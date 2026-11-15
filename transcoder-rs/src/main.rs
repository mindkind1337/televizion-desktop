//! Televizion desktop transcoder.
//!
//! A small local HTTP service the web player talks to (inside the desktop app): it wraps ffmpeg
//! and ffprobe to turn files the browser cannot play into HLS. It listens on loopback only and
//! serves only requests that come from the Televizion site.
//!
//! Environment:
//!   TVZ_PORT          port to listen on (default: config, 3002). If it cannot be bound, any free
//!                     port is used instead. The port in use is printed as `TVZ_LISTENING <port>`.
//!   TVZ_STREAMS_DIR   where segments are kept (default: <temp>/televizion-streams)
//!   TVZ_LOG_FILE      log file (default: <temp>/televizion-transcoder.log), restarted each run
//!   TVZ_CONFIG        optional JSON file overriding config.default.json
//!   HWDETECT_FRESH=1  ignore the cached hardware-encoder detection
//!
//! ffmpeg and ffprobe are found through PATH.

mod app;
mod config;
mod downloader;
mod ffargs;
mod fmp4;
mod hw;
mod mkv;
mod probe;
mod proc;
mod routes;
mod session;
mod ssrf;
mod stats;
mod sysinfo;
mod util;
mod vod;

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

/// `--ffargs`: reads a JSON array of cases on stdin, prints the ffmpeg argument list of each.
/// Used by tests/compare_args.js to diff this builder against the Node service's.
fn print_ffargs() {
    use serde_json::Value;
    let mut text = String::new();
    let _ = std::io::Read::read_to_string(&mut std::io::stdin(), &mut text);
    let cases: Vec<Value> = serde_json::from_str(&text).unwrap_or_default();
    let cfg = config::Config::defaults();
    let strs = |v: &Value| -> Vec<String> { v.as_array().map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect()).unwrap_or_default() };
    let out: Vec<Value> = cases
        .iter()
        .map(|c| {
            let s = |k: &str| c[k].as_str().unwrap_or("").to_string();
            let enc = ffargs::Encoders { h264: c["h264Encoder"].as_str().map(String::from), hevc: c["hevcEncoder"].as_str().map(String::from), h264_input: strs(&c["h264Input"]), hevc_input: strs(&c["hevcInput"]) };
            let spec = ffargs::SessionSpec {
                strategy: s("strategy"),
                audio_track: c["audioTrack"].as_i64().unwrap_or(0),
                audio_bitrate: s("audioBitrate"),
                audio_track_count: c["audioTrackCount"].as_i64().unwrap_or(1),
                source_audio_codec: s("sourceAudioCodec"),
                source_audio_channels: c["sourceAudioChannels"].as_i64().unwrap_or(0),
                source_video_codec: s("sourceVideoCodec"),
                prev_output_video_codec: s("prevOutputVideoCodec"),
            };
            let input = ffargs::input_args(&cfg, c["seekTime"].as_f64().unwrap_or(0.0), c["hasHeader"].as_bool().unwrap_or(false), c["dataOffset"].as_u64().unwrap_or(0));
            let b = ffargs::build(&cfg, &enc, &spec, input, None);
            serde_json::json!({ "args": b.args, "videoEncoder": b.video_encoder, "outputVideoCodec": b.output_video_codec, "outputAudioCodec": b.output_audio_codec })
        })
        .collect();
    println!("{}", Value::Array(out));
}

fn main() {
    if std::env::args().any(|a| a == "--ffargs") {
        print_ffargs();
        return;
    }
    proc::tie_children_to_this_process();

    let log_path = std::env::var_os("TVZ_LOG_FILE").filter(|p| !p.is_empty()).map(std::path::PathBuf::from).unwrap_or_else(|| std::env::temp_dir().join("televizion-transcoder.log"));
    util::open_log_file(&log_path);
    logi!("[startup] televizion-transcoder {} ({}/{})", env!("CARGO_PKG_VERSION"), std::env::consts::OS, std::env::consts::ARCH);

    let cfg = config::Config::load();

    // ffmpeg and ffprobe must be there.
    let mut ffmpeg_version = String::new();
    let mut version_line = String::new();
    let mut missing = Vec::new();
    for bin in ["ffmpeg", "ffprobe"] {
        match proc::run(bin, &["-version"], Duration::from_secs(5)) {
            Some(o) if o.ok => {
                let line = o.stdout.lines().next().unwrap_or("").trim().to_string();
                logi!("[startup] {}", line);
                if bin == "ffmpeg" {
                    ffmpeg_version = line.split("version").nth(1).and_then(|r| r.split_whitespace().next()).unwrap_or(&line).to_string();
                    version_line = line;
                }
            }
            _ => missing.push(bin),
        }
    }
    if !missing.is_empty() {
        loge!("[startup] FATAL: {} not found. Install FFmpeg: https://ffmpeg.org/download.html", missing.join(", "));
        std::process::exit(1);
    }

    // Leftovers of a previous run.
    let _ = std::fs::create_dir_all(&cfg.streams_dir);
    if let Ok(rd) = std::fs::read_dir(&cfg.streams_dir) {
        let mut n = 0;
        for e in rd.flatten() {
            let p = e.path();
            let _ = if p.is_dir() { std::fs::remove_dir_all(&p) } else { std::fs::remove_file(&p) };
            n += 1;
        }
        if n > 0 {
            logi!("[STARTUP] cleaned {} stale session(s) from {}", n, cfg.streams_dir.display());
        }
    }

    let enc = Arc::new(RwLock::new(ffargs::Encoders::default()));
    let hw_info: hw::SharedHw = Arc::new(Mutex::new(hw::HwInfo::default()));
    // In the background: the server listens at once, sessions use software encoders until done.
    hw::start_detection(cfg.clone(), version_line, enc.clone(), hw_info.clone());

    let app = Arc::new(app::App {
        cfg,
        enc,
        hw: hw_info,
        clients: ssrf::build_clients(),
        ffmpeg_version,
        started: Instant::now(),
        sessions: Mutex::new(HashMap::new()),
        probe_cache: Mutex::new(Vec::new()),
        playlists: Mutex::new(HashMap::new()),
        monitor: Mutex::new(sysinfo::Monitor::default()),
        gpu: Mutex::new(app::GpuState::default()),
        prev_self_cpu: Mutex::new((0, 0)),
    });

    let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(4).enable_all().build().expect("tokio runtime");
    let code = rt.block_on(serve(app));
    rt.shutdown_timeout(Duration::from_secs(2));
    std::process::exit(code);
}

async fn serve(app: Arc<app::App>) -> i32 {
    // Loopback only. Windows can reserve port ranges (Hyper-V, WSL) so the wanted port may be
    // refused: fall back to any free port and tell the desktop app which one.
    let wanted = app.cfg.port;
    let listener = match tokio::net::TcpListener::bind(("127.0.0.1", wanted)).await {
        Ok(l) => l,
        Err(e) => {
            logw!("[startup] cannot listen on 127.0.0.1:{} ({}), using a free port instead", wanted, e);
            match tokio::net::TcpListener::bind(("127.0.0.1", 0)).await {
                Ok(l) => l,
                Err(e2) => {
                    loge!("[startup] FATAL: cannot listen on loopback: {}", e2);
                    return 1;
                }
            }
        }
    };
    let port = listener.local_addr().map(|a| a.port()).unwrap_or(wanted);
    logi!("Transcode (v4.0, rust) listening on 127.0.0.1:{}", port);
    // Machine-readable, read by the desktop app.
    logi!("TVZ_LISTENING {}", port);

    // Sweepers: idle sessions, probe cache, old VOD playlists.
    {
        let app = app.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(30));
            let mut n = 0u64;
            loop {
                tick.tick().await;
                n += 1;
                sweep_sessions(&app);
                app.probe_cache_sweep();
                if n % 10 == 0 {
                    let now = util::now_ms();
                    app.playlists.lock().unwrap().retain(|_, e| now.saturating_sub(e.1) <= 7_200_000);
                }
            }
        });
    }

    let router = routes::router(app.clone());
    // On SIGTERM/Ctrl-C: stop at once (no waiting for idle keep-alive connections to drain).
    let res = tokio::select! {
        r = axum::serve(listener, router.into_make_service_with_connect_info::<SocketAddr>()) => r,
        _ = shutdown_signal() => Ok(()),
    };
    logi!("[SHUTDOWN] stopping {} live session(s)", app.session_count());
    app.stop_all();
    match res {
        Ok(()) => 0,
        Err(e) => {
            loge!("server error: {}", e);
            1
        }
    }
}

/// Stops sessions nobody has fetched from for the idle limit (one minute for sessions whose
/// first segment was never fetched).
fn sweep_sessions(app: &Arc<app::App>) {
    let now = util::now_ms();
    let expired: Vec<String> = {
        let sessions = app.sessions.lock().unwrap();
        sessions
            .iter()
            .filter(|(_, s)| {
                let i = s.inner.lock().unwrap();
                let limit = if i.consumed_time < 0.0 { app.cfg.ttl_ms.min(60_000) } else { app.cfg.ttl_ms };
                now.saturating_sub(i.last_access) > limit
            })
            .map(|(sid, _)| sid.clone())
            .collect()
    };
    for sid in expired {
        if let Some(s) = app.remove_session(&sid) {
            logi!("[LIVE:{}] session expired - cleaning up", sid);
            s.stop();
        }
    }
}

/// Stops on a termination signal, or when the app that started this process is gone.
async fn shutdown_signal() {
    tokio::select! {
        _ = os_signal() => {}
        _ = parent_gone() => { logi!("[shutdown] the app that started the transcoder is gone - stopping"); }
    }
}

/// With TVZ_EXIT_WITH_PARENT=1 the app keeps this process's standard input open as a pipe and
/// never writes to it. The pipe closes when the app ends, however it ends (closed, crashed,
/// killed), and reading it then returns end-of-file. Without the variable this never resolves.
async fn parent_gone() {
    if std::env::var("TVZ_EXIT_WITH_PARENT").map(|v| v == "1").unwrap_or(false) {
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        std::thread::spawn(move || {
            use std::io::Read;
            let mut buf = [0u8; 64];
            let mut stdin = std::io::stdin();
            loop {
                match stdin.read(&mut buf) {
                    Ok(0) | Err(_) => break, // end-of-file or a broken pipe: the parent is gone
                    Ok(_) => {}              // ignore anything written
                }
            }
            let _ = tx.send(());
        });
        let _ = rx.await;
    } else {
        std::future::pending::<()>().await;
    }
}

async fn os_signal() {
    #[cfg(unix)]
    {
        let mut term = match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(t) => t,
            Err(_) => {
                let _ = tokio::signal::ctrl_c().await;
                return;
            }
        };
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}
