//! A transcode session: one source file, one ffmpeg at a time, HLS segments on disk.
//!
//! Lifecycle: start → (segments are produced, the player fetches the playlist and segments) →
//! seek (ffmpeg is restarted at another position, numbering restarts at 0) → pause/resume →
//! stop (files deleted). Sessions nobody fetches from are stopped by the sweeper (main.rs).
//!
//! Every ffmpeg start is a "run" with its own number; tasks of an older run notice the number
//! changed and do nothing, which is how a seek cleanly replaces a running ffmpeg.

use crate::app::App;
use crate::downloader::{fetch_range, DlEvent, DlShared, Downloader};
use crate::ffargs::{self, SessionSpec};
use crate::fmp4::{extract_tfdt, Event, Fmp4Parser};
use crate::mkv::{self, CueEntry, SCAN_OVERLAP};
use crate::util::{contains_ci, now_ms, prefix};
use crate::{loge, logi, logw};
use bytes::Bytes;
use serde_json::Value;
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{watch, Notify};

/// Stops the tasks of one ffmpeg run.
#[derive(Clone)]
pub struct Cancel(Arc<watch::Sender<bool>>);

impl Cancel {
    pub fn new() -> Self {
        Cancel(Arc::new(watch::channel(false).0))
    }
    pub fn cancel(&self) {
        self.0.send_replace(true);
    }
    pub async fn cancelled(&self) {
        let mut rx = self.0.subscribe();
        let _ = rx.wait_for(|v| *v).await;
    }
}

#[derive(Clone, Debug)]
pub struct Seg {
    pub seq: u64,
    pub duration: f64,
    pub path: PathBuf,
    pub start_time: f64,
    pub size: u64,
}

/// One audio-only output (sources with several audio tracks): its own folder, playlist, segments.
pub struct AudioOut {
    pub dir: PathBuf,
    pub segments: VecDeque<Seg>,
    pub next_seq: u64,
    pub has_init: bool,
    acc_time: f64,
    timescale: u32,
}

/// What is written to ffmpeg's stdin before the downloaded data.
#[derive(Clone)]
pub enum HeaderMode {
    /// Nothing: the download starts at byte 0 and is a complete file.
    None,
    /// The container header, then data from an estimated offset; for MKV the data is scanned
    /// for the first Cluster boundary before it is passed on.
    Scan(Arc<Vec<u8>>),
    /// The container header, then data exactly as downloaded (offset from the Cues index).
    Raw(Arc<Vec<u8>>),
}

pub struct Inner {
    pub url: String,
    pub audio_track: i64,
    pub audio_bitrate: String,
    pub strategy: String,
    pub start_time: f64,
    pub file_size: u64,
    pub duration: f64,
    pub range_method: String,
    pub segments: VecDeque<Seg>,
    pub seq_base: u64,
    pub next_seq: u64,
    /// Start time of the newest segment the player fetched (-1 = none yet).
    pub consumed_time: f64,
    pub total_produced_sec: f64,
    pub last_access: u64,
    /// starting | running | seeking | paused | ended | stopped
    pub state: &'static str,
    pub has_init: bool,

    // current ffmpeg run
    pub run_id: u64,
    cancel: Option<Cancel>,
    pub ffmpeg_pid: Option<u32>,
    pub ffmpeg_alive: bool,
    /// The current ffmpeg exited on its own and its output was read to the end.
    pub run_done: bool,
    timescale: u32,
    pub dl: Option<Arc<DlShared>>,
    pub dl_throttled: bool,

    // seeking aids
    pub file_header: Option<Arc<Vec<u8>>>,
    pub cue_table: Option<Arc<Vec<CueEntry>>>,
    cues_fetching: bool,
    cluster_map: Vec<CueEntry>,
    last_track_offset: u64,
    pub probe_cached_data: Option<Bytes>,
    probe_data_buf: Option<Bytes>,

    // codec info (from the player's probe) and what ffmpeg was asked to produce
    pub source_video_codec: String,
    pub source_audio_codec: String,
    pub resolution: String,
    pub output_resolution: String,
    pub output_video_codec: String,
    pub output_audio_codec: String,
    pub video_encoder_name: String,
    pub source_video_bitrate: i64,
    pub source_audio_bitrate: i64,
    pub source_video_profile: Value,
    pub source_video_level: Value,
    pub source_pix_fmt: String,
    pub source_audio_channels: i64,
    pub source_audio_sample_rate: Value,
    pub source_has_b_frames: Value,
    pub source_format_name: String,
    pub audio_track_count: i64,
    pub audio_track_labels: Vec<Value>,
    pub audio_outputs: Vec<AudioOut>,

    pub ffmpeg_exit_code: Option<i32>,
    pub ffmpeg_signal: Option<String>,
    pub ffmpeg_args: String,
    pub ebml_errors: u64,
    pub logs: VecDeque<(u64, &'static str, String)>,
    pub client_ip: String,
    pub total_bytes_downloaded: u64,
    pub disk_usage_bytes: i64,
    /// (start byte, end byte, seek method) of earlier downloads.
    pub downloaded_ranges: Vec<(u64, u64, &'static str)>,
    /// (start second, end second) of earlier ffmpeg runs.
    pub produced_ranges: Vec<(f64, f64)>,
    pub seek_method: &'static str,
    pub seek_gen: u64,
}

pub struct Session {
    pub sid: String,
    pub original_url: String,
    pub tmp_dir: PathBuf,
    pub created_at: u64,
    pub app: Arc<App>,
    pub inner: Mutex<Inner>,
    /// Woken on every new segment, end of stream, ffmpeg exit, seek and stop.
    pub notify: Notify,
    /// true while the download is held back because the player has plenty buffered.
    throttle: watch::Sender<bool>,
}

fn write_atomic(dir: &Path, name: &str, buf: &[u8]) -> std::io::Result<PathBuf> {
    let tmp = dir.join(format!("{}.tmp", name));
    let fin = dir.join(name);
    let attempt = || -> std::io::Result<()> {
        std::fs::write(&tmp, buf)?;
        std::fs::rename(&tmp, &fin)
    };
    match attempt() {
        Ok(()) => Ok(fin),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            // The folder was deleted under us: recreate it and try once more.
            std::fs::create_dir_all(dir)?;
            attempt()?;
            Ok(fin)
        }
        Err(e) => Err(e),
    }
}

#[cfg(unix)]
fn exit_signal(status: &std::process::ExitStatus) -> Option<String> {
    use std::os::unix::process::ExitStatusExt;
    status.signal().map(|s| match s {
        9 => "SIGKILL".to_string(),
        15 => "SIGTERM".to_string(),
        2 => "SIGINT".to_string(),
        11 => "SIGSEGV".to_string(),
        13 => "SIGPIPE".to_string(),
        n => format!("SIG{}", n),
    })
}

#[cfg(not(unix))]
fn exit_signal(_status: &std::process::ExitStatus) -> Option<String> {
    None
}

pub struct NewSession {
    pub sid: String,
    pub url: String,
    pub audio_track: i64,
    pub audio_bitrate: String,
    pub start_time: f64,
    pub file_size: u64,
    pub duration: f64,
    pub strategy: String,
}

impl Session {
    pub fn new(app: Arc<App>, n: NewSession) -> Arc<Session> {
        let tmp_dir = app.cfg.streams_dir.join(&n.sid);
        let _ = std::fs::create_dir_all(&tmp_dir);
        let now = now_ms();
        Arc::new(Session {
            sid: n.sid,
            original_url: n.url.clone(),
            tmp_dir,
            created_at: now,
            app,
            notify: Notify::new(),
            throttle: watch::channel(false).0,
            inner: Mutex::new(Inner {
                url: n.url,
                audio_track: n.audio_track,
                audio_bitrate: if n.audio_bitrate.is_empty() { "128k".to_string() } else { n.audio_bitrate },
                strategy: n.strategy,
                start_time: n.start_time,
                file_size: n.file_size,
                duration: n.duration,
                range_method: String::new(),
                segments: VecDeque::new(),
                seq_base: 0,
                next_seq: 0,
                consumed_time: -1.0,
                total_produced_sec: 0.0,
                last_access: now,
                state: "starting",
                has_init: false,
                run_id: 0,
                cancel: None,
                ffmpeg_pid: None,
                ffmpeg_alive: false,
                run_done: false,
                timescale: 0,
                dl: None,
                dl_throttled: false,
                file_header: None,
                cue_table: None,
                cues_fetching: false,
                cluster_map: Vec::new(),
                last_track_offset: 0,
                probe_cached_data: None,
                probe_data_buf: None,
                source_video_codec: String::new(),
                source_audio_codec: String::new(),
                resolution: String::new(),
                output_resolution: String::new(),
                output_video_codec: String::new(),
                output_audio_codec: String::new(),
                video_encoder_name: String::new(),
                source_video_bitrate: 0,
                source_audio_bitrate: 0,
                source_video_profile: Value::String(String::new()),
                source_video_level: Value::from(0),
                source_pix_fmt: String::new(),
                source_audio_channels: 0,
                source_audio_sample_rate: Value::String(String::new()),
                source_has_b_frames: Value::from(0),
                source_format_name: String::new(),
                audio_track_count: 1,
                audio_track_labels: Vec::new(),
                audio_outputs: Vec::new(),
                ffmpeg_exit_code: None,
                ffmpeg_signal: None,
                ffmpeg_args: String::new(),
                ebml_errors: 0,
                logs: VecDeque::new(),
                client_ip: String::new(),
                total_bytes_downloaded: 0,
                disk_usage_bytes: 0,
                downloaded_ranges: Vec::new(),
                produced_ranges: Vec::new(),
                seek_method: "initial",
                seek_gen: 0,
            }),
        })
    }

    fn is_mkv(&self) -> bool {
        contains_ci(&self.original_url, ".mkv")
    }
    fn is_avi(&self) -> bool {
        contains_ci(&self.original_url, ".avi")
    }

    pub fn add_log(&self, level: &'static str, msg: impl Into<String>) {
        let mut i = self.inner.lock().unwrap();
        Self::log_locked(&mut i, level, msg.into());
    }

    fn log_locked(i: &mut Inner, level: &'static str, msg: String) {
        i.logs.push_back((now_ms(), level, msg));
        while i.logs.len() > 200 {
            i.logs.pop_front();
        }
    }

    pub fn touch(&self) {
        self.inner.lock().unwrap().last_access = now_ms();
    }

    // ── files ──

    fn clean_files(&self, i: &mut Inner) {
        if let Ok(rd) = std::fs::read_dir(&self.tmp_dir) {
            for e in rd.flatten() {
                let p = e.path();
                if p.is_dir() {
                    let _ = std::fs::remove_dir_all(&p);
                } else {
                    let _ = std::fs::remove_file(&p);
                }
            }
        }
        i.disk_usage_bytes = 0;
        i.has_init = false;
    }

    pub fn read_file(&self, rel: &str) -> Option<Vec<u8>> {
        std::fs::read(self.tmp_dir.join(rel)).ok()
    }

    // ── run control ──

    /// Ends the current ffmpeg and download, if any. Tasks of that run stop on their own.
    fn kill_locked(&self, i: &mut Inner) {
        if let Some(c) = i.cancel.take() {
            c.cancel();
        }
        i.run_id += 1;
        i.dl = None;
        i.dl_throttled = false;
        i.ffmpeg_alive = false;
        i.ffmpeg_pid = None;
        i.run_done = false;
        self.throttle.send_replace(false);
    }

    /// Holds the download back (true) or lets it continue (false).
    fn set_throttle(&self, i: &mut Inner, on: bool) {
        i.dl_throttled = on;
        if let Some(dl) = &i.dl {
            dl.paused.store(on, Ordering::Relaxed);
        }
        self.throttle.send_replace(on);
    }

    /// Lets a throttled download continue once the player has caught up.
    fn maybe_resume_download(&self, i: &mut Inner) {
        if i.dl_throttled && i.dl.is_some() && Self::ahead_sec(i) < self.app.cfg.throttle_resume_sec {
            self.set_throttle(i, false);
        }
    }

    /// Starts (or restarts) ffmpeg at `seek_time` seconds.
    ///   0: the whole file from byte 0.
    ///   > 0, MKV/AVI of known size and duration: byte-range seek with the cached header.
    ///   > 0, anything else: the whole file from byte 0 with `-ss` before the input.
    pub fn spawn_ffmpeg(self: &Arc<Self>, seek_time: f64) {
        let byte_range = {
            let mut i = self.inner.lock().unwrap();
            self.kill_locked(&mut i);
            i.state = "starting";
            i.start_time = seek_time;
            let br = (self.is_mkv() || self.is_avi()) && i.file_size > 0 && i.duration > 0.0 && seek_time > 0.0;
            if !br {
                i.seek_method = if seek_time > 0.0 { "full-redownload" } else { "initial" };
            }
            br
        };
        if byte_range {
            self.spawn_byte_range_seek(seek_time);
        } else {
            self.launch_piped(HeaderMode::None, 0, seek_time, None);
        }
    }

    fn cues_lookup(i: &Inner, seek_ms: u64) -> Option<CueEntry> {
        match &i.cue_table {
            Some(t) => mkv::cues_lookup(t, seek_ms),
            None => mkv::cues_lookup(&i.cluster_map, seek_ms),
        }
    }

    fn spawn_byte_range_seek(self: &Arc<Self>, seek_time: f64) {
        let is_mkv = self.is_mkv();
        let seek_ms = (seek_time * 1000.0).floor() as u64;
        let (data_offset, header, run_id) = {
            let mut i = self.inner.lock().unwrap();
            let cue = if is_mkv { Self::cues_lookup(&i, seek_ms) } else { None };
            // An index entry is only trusted within two minutes of the target.
            let usable = cue.filter(|c| seek_ms - c.time_ms < 120_000);
            let off = match usable {
                Some(c) => {
                    i.seek_method = "byte-range-cues";
                    logi!("[LIVE:{}] byte-range seek: t={}s → Cues: byte {} (cue_ts={}ms, gap={}ms)", self.sid, seek_time, c.byte_offset, c.time_ms, seek_ms - c.time_ms);
                    c.byte_offset
                }
                None => {
                    if let Some(c) = cue {
                        logi!("[LIVE:{}] Cues entry too far: cue_ts={}ms vs target={}ms, using linear", self.sid, c.time_ms, seek_ms);
                    }
                    let mut off = ((seek_time / i.duration) * i.file_size as f64).floor() as u64;
                    let max = (i.file_size as f64 * 0.8).floor() as u64;
                    if off > max {
                        logi!("[LIVE:{}] byte-range: capping offset {} → {} (80% safety)", self.sid, off, max);
                        off = max;
                    }
                    i.seek_method = "byte-range-linear";
                    logi!("[LIVE:{}] byte-range seek: t={}s, est={}/{} fmt={}", self.sid, seek_time, off, i.file_size, if is_mkv { "mkv" } else { "avi" });
                    off
                }
            };
            (off, i.file_header.clone(), i.run_id)
        };

        if let Some(h) = header {
            logi!("[LIVE:{}] byte-range: using cached header ({}B), ss={}", self.sid, h.len(), seek_time);
            self.launch_piped(HeaderMode::Raw(h), data_offset, seek_time, Some(run_id));
            return;
        }

        // No header cached yet: fetch the first 2 MB (three attempts), then start.
        let s = self.clone();
        tokio::spawn(async move {
            let app = s.app.clone();
            for attempt in 1..=3u64 {
                if s.inner.lock().unwrap().run_id != run_id {
                    return;
                }
                logi!("[LIVE:{}] byte-range: fetching header (attempt {}/3)", s.sid, attempt);
                match fetch_range(&app.clients.follow, &app.cfg.user_agent, &s.original_url, 0, 2_097_151, 2_097_152).await {
                    Ok(raw) if !raw.is_empty() => {
                        let header = if is_mkv {
                            match mkv::find_mkv_cluster(&raw, 0) {
                                Some(c) if c.offset > 0 => mkv::patch_mkv_segment_length(&raw[..c.offset]),
                                _ => {
                                    logw!("[LIVE:{}] byte-range: no Cluster in first {}B, using full buffer as header", s.sid, raw.len());
                                    mkv::patch_mkv_segment_length(&raw)
                                }
                            }
                        } else {
                            match mkv::find_avi_movi_offset(&raw, 0) {
                                Some(o) if o > 0 => raw[..o].to_vec(),
                                _ => {
                                    logw!("[LIVE:{}] byte-range: no movi found, using full buffer as header", s.sid);
                                    raw
                                }
                            }
                        };
                        let header = Arc::new(header);
                        logi!("[LIVE:{}] byte-range: header={}B (cached, {})", s.sid, header.len(), if is_mkv { "mkv" } else { "avi" });
                        s.inner.lock().unwrap().file_header = Some(header.clone());
                        s.launch_piped(HeaderMode::Scan(header), data_offset, seek_time, Some(run_id));
                        return;
                    }
                    other => {
                        let why = match other {
                            Err(e) => e,
                            _ => "no data".to_string(),
                        };
                        if attempt < 3 {
                            logw!("[LIVE:{}] byte-range: header request failed ({}), retry in {}ms", s.sid, why, attempt * 1000);
                            tokio::time::sleep(Duration::from_millis(attempt * 1000)).await;
                        }
                    }
                }
            }
            loge!("[LIVE:{}] byte-range: header fetch failed after 3 attempts, piping raw from offset", s.sid);
            s.launch_piped(HeaderMode::None, data_offset, seek_time, Some(run_id));
        });
    }

    /// Starts ffmpeg reading stdin and the task that feeds it the download from `data_offset`.
    /// `expected_run`: only start when no other run was started in the meantime.
    pub fn launch_piped(self: &Arc<Self>, mode: HeaderMode, data_offset: u64, seek_time: f64, expected_run: Option<u64>) {
        let cfg = &self.app.cfg;
        let has_header = !matches!(mode, HeaderMode::None);
        let input = ffargs::input_args(cfg, seek_time, has_header, data_offset);

        let mut i = self.inner.lock().unwrap();
        if let Some(r) = expected_run {
            if i.run_id != r {
                return;
            }
        }
        if i.state == "stopped" {
            return;
        }
        self.kill_locked(&mut i);
        let run_id = i.run_id;

        let spec = SessionSpec {
            strategy: i.strategy.clone(),
            audio_track: i.audio_track,
            audio_bitrate: i.audio_bitrate.clone(),
            audio_track_count: i.audio_track_count,
            source_audio_codec: i.source_audio_codec.clone(),
            source_audio_channels: i.source_audio_channels,
            source_video_codec: i.source_video_codec.clone(),
            prev_output_video_codec: i.output_video_codec.clone(),
        };

        // Separate audio outputs are written by ffmpeg to loopback TCP ports we listen on
        // (Node used inherited pipes 3, 4, …, which a Windows child cannot be given portably).
        let extra = if spec.audio_track_count > 1 { spec.audio_track_count as usize } else { 0 };
        let mut listeners = Vec::new();
        let mut targets = Vec::new();
        for _ in 0..extra {
            let l = std::net::TcpListener::bind(("127.0.0.1", 0)).and_then(|l| {
                l.set_nonblocking(true)?;
                let port = l.local_addr()?.port();
                Ok((tokio::net::TcpListener::from_std(l)?, port))
            });
            match l {
                Ok((l, port)) => {
                    targets.push(format!("tcp://127.0.0.1:{}", port));
                    listeners.push(l);
                }
                Err(e) => {
                    Self::log_locked(&mut i, "error", format!("audio output socket: {}", e));
                    loge!("[LIVE:{}] audio output socket: {}", self.sid, e);
                }
            }
        }

        let built = ffargs::build(cfg, &self.app.enc.read().map(|e| e.clone()).unwrap_or_default(), &spec, input, Some(&targets));
        for w in &built.warnings {
            logw!("[LIVE:{}] {}", self.sid, w);
            Self::log_locked(&mut i, "warn", w.clone());
        }
        i.video_encoder_name = built.video_encoder.clone();
        i.output_video_codec = built.output_video_codec.clone();
        i.output_audio_codec = built.output_audio_codec.clone();
        if extra > 0 {
            logi!("[LIVE:{}] multi-audio: {} tracks → video-only main + {} audio outputs", self.sid, spec.audio_track_count, extra);
            Self::log_locked(&mut i, "info", format!("multi-audio: {} tracks, main=video-only, audio on separate pipes", spec.audio_track_count));
        }
        let strat = if i.strategy.is_empty() { "transcode-audio".to_string() } else { i.strategy.clone() };
        logi!("[LIVE:{}] spawn ffmpeg: seek={} strategy={}", self.sid, i.start_time, strat);
        let start_time = i.start_time;
        Self::log_locked(&mut i, "info", format!("spawn ffmpeg: seek={} strategy={}", start_time, strat));
        i.ffmpeg_exit_code = None;
        i.ffmpeg_signal = None;
        i.ffmpeg_args = built.args.join(" ");

        let mut child = match crate::proc::tokio_command("ffmpeg").args(&built.args).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn() {
            Ok(c) => c,
            Err(e) => {
                Self::log_locked(&mut i, "error", format!("ffmpeg process error: {}", e));
                loge!("[LIVE:{}] ffmpeg process error: {}", self.sid, e);
                i.run_done = true;
                drop(i);
                self.notify.notify_waiters();
                return;
            }
        };
        let cancel = Cancel::new();
        i.cancel = Some(cancel.clone());
        i.ffmpeg_pid = child.id();
        i.ffmpeg_alive = true;
        i.timescale = 0;

        // Audio outputs of this run.
        i.audio_outputs.clear();
        for idx in 0..extra {
            let dir = self.tmp_dir.join(format!("a{}", idx));
            let _ = std::fs::create_dir_all(&dir);
            i.audio_outputs.push(AudioOut { dir, segments: VecDeque::new(), next_seq: 0, has_init: false, acc_time: 0.0, timescale: 0 });
        }
        drop(i);

        let stdin = child.stdin.take();
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let seg_dur = cfg.seg_duration;

        // stderr → logs
        if let Some(mut stderr) = stderr {
            let s = self.clone();
            tokio::spawn(async move {
                let mut buf = vec![0u8; 8192];
                while let Ok(n) = stderr.read(&mut buf).await {
                    if n == 0 {
                        break;
                    }
                    let text = String::from_utf8_lossy(&buf[..n]);
                    let msg = text.trim();
                    if msg.is_empty() {
                        continue;
                    }
                    // Harmless Matroska demuxer complaints about stray bytes: count, don't print.
                    let noise = msg.contains("invalid as first byte of an EBML number");
                    let mut i = s.inner.lock().unwrap();
                    if i.run_id != run_id {
                        continue;
                    }
                    if noise {
                        i.ebml_errors += 1;
                    } else {
                        loge!("[LIVE:{}] ffmpeg: {}", s.sid, prefix(msg, 200));
                    }
                    Self::log_locked(&mut i, if noise { "debug" } else { "ffmpeg" }, msg.to_string());
                }
            });
        }

        // audio sockets → audio segments
        for (idx, listener) in listeners.into_iter().enumerate() {
            let s = self.clone();
            let c = cancel.clone();
            tokio::spawn(async move {
                let work = async {
                    let (mut sock, _) = match listener.accept().await {
                        Ok(x) => x,
                        Err(_) => return,
                    };
                    drop(listener);
                    let mut parser = Fmp4Parser::new(seg_dur);
                    let mut buf = vec![0u8; 64 * 1024];
                    loop {
                        match sock.read(&mut buf).await {
                            Ok(0) | Err(_) => break,
                            Ok(n) => {
                                for ev in parser.push(&buf[..n]) {
                                    s.on_audio_event(run_id, idx, ev, parser.timescale);
                                }
                            }
                        }
                    }
                    for ev in parser.flush() {
                        s.on_audio_event(run_id, idx, ev, parser.timescale);
                    }
                };
                tokio::select! { _ = c.cancelled() => {}, _ = work => {} }
            });
        }

        // stdout → init + segments; then the exit status
        {
            let s = self.clone();
            let c = cancel.clone();
            tokio::spawn(async move {
                let reader = async {
                    let mut stdout = match stdout {
                        Some(o) => o,
                        None => return,
                    };
                    let mut parser = Fmp4Parser::new(seg_dur);
                    let mut buf = vec![0u8; 256 * 1024];
                    loop {
                        match stdout.read(&mut buf).await {
                            Ok(0) | Err(_) => break,
                            Ok(n) => {
                                for ev in parser.push(&buf[..n]) {
                                    s.on_video_event(run_id, ev, parser.timescale);
                                }
                            }
                        }
                    }
                    for ev in parser.flush() {
                        s.on_video_event(run_id, ev, parser.timescale);
                    }
                    s.on_stdout_end(run_id);
                };
                let waiter = async {
                    tokio::select! {
                        st = child.wait() => st.ok(),
                        _ = c.cancelled() => {
                            let _ = child.start_kill();
                            child.wait().await.ok()
                        }
                    }
                };
                let (_, status) = tokio::join!(reader, waiter);
                s.on_exit(run_id, status);
            });
        }

        // download → stdin
        if let Some(stdin) = stdin {
            let s = self.clone();
            let c = cancel.clone();
            tokio::spawn(async move {
                tokio::select! {
                    _ = c.cancelled() => {}
                    _ = s.feed(run_id, stdin, mode, data_offset, seek_time) => {}
                }
            });
        }
    }

    fn set_file_header(self: &Arc<Self>, header: Vec<u8>, what: &str) {
        logi!("[LIVE:{}] cached {} header: {}B", self.sid, what, header.len());
        self.inner.lock().unwrap().file_header = Some(Arc::new(header));
    }

    /// Feeds ffmpeg: optional cached header, optional bytes kept from the probe, then the
    /// download. Also caches the container header and builds the seek index along the way.
    async fn feed(self: Arc<Self>, run_id: u64, mut stdin: tokio::process::ChildStdin, mode: HeaderMode, data_offset: u64, seek_time: f64) {
        let is_mkv = self.is_mkv();
        let is_avi = self.is_avi();
        let app = self.app.clone();
        let scan_header = match &mode {
            HeaderMode::Scan(h) => Some(h.clone()),
            _ => None,
        };
        match &mode {
            HeaderMode::Scan(h) | HeaderMode::Raw(h) => {
                if stdin.write_all(h).await.is_err() {
                    return;
                }
            }
            HeaderMode::None => {}
        }

        // MKV with an estimated offset: hold data back until the first Cluster boundary.
        let mut scan_buf: Option<Vec<u8>> = (scan_header.is_some() && is_mkv && data_offset > 0).then(Vec::new);
        // First playback from byte 0: accumulate until the container header can be cached.
        let (mut header_buf, cached, file_size) = {
            let mut i = self.inner.lock().unwrap();
            if i.run_id != run_id {
                return;
            }
            let hb: Option<Vec<u8>> = (data_offset == 0 && i.file_header.is_none() && (is_mkv || is_avi)).then(Vec::new);
            let cached = if data_offset == 0 && matches!(mode, HeaderMode::None) { i.probe_cached_data.take() } else { None };
            if let Some(c) = &cached {
                i.probe_data_buf = Some(c.clone());
            }
            (hb, cached, i.file_size)
        };

        // Bytes downloaded while probing go to ffmpeg first; the download continues after them.
        let mut dl_offset = data_offset;
        if let Some(cached) = cached {
            logi!("[LIVE:{}] writing {}B probe cache to FFmpeg stdin", self.sid, cached.len());
            let mut direct: Option<Vec<u8>> = None;
            match header_buf.as_mut() {
                Some(hb) => {
                    hb.extend_from_slice(&cached);
                    if is_mkv {
                        if let Some(c) = mkv::find_mkv_cluster(hb, 0).filter(|c| c.offset > 0) {
                            self.set_file_header(mkv::patch_mkv_segment_length(&hb[..c.offset]), "MKV");
                            self.fetch_cues();
                            direct = Some(mkv::patch_mkv_segment_length(hb));
                        }
                    } else if is_avi {
                        if let Some(o) = mkv::find_avi_movi_offset(hb, 0).filter(|o| *o > 0) {
                            self.set_file_header(hb[..o].to_vec(), "AVI");
                            direct = Some(hb.clone());
                        }
                    }
                    if direct.is_some() {
                        header_buf = None;
                    }
                }
                None => direct = Some(cached.to_vec()),
            }
            if let Some(d) = direct {
                if stdin.write_all(&d).await.is_err() {
                    return;
                }
            }
            dl_offset = cached.len() as u64;
        }

        let mut dl = Downloader::new(app.clients.follow.clone(), &app.cfg.user_agent, &self.original_url, dl_offset, &format!("LIVE:{}", self.sid), app.cfg.dl_max_retries, file_size);
        {
            let mut i = self.inner.lock().unwrap();
            if i.run_id != run_id {
                return;
            }
            i.dl = Some(dl.shared.clone());
        }
        let mut throttle_rx = self.throttle.subscribe();

        loop {
            // Held back while the player has plenty buffered.
            if *throttle_rx.borrow() {
                dl.was_paused = true;
                if throttle_rx.wait_for(|v| !*v).await.is_err() {
                    return;
                }
                dl.shared.paused.store(false, Ordering::Relaxed);
            }
            let mut chunk: Vec<u8> = match dl.next().await {
                DlEvent::Data(b) => b.to_vec(),
                DlEvent::End => break,
                DlEvent::Error(e) => {
                    loge!("[LIVE:{}] download failed: {}", self.sid, e);
                    self.add_log("error", format!("download failed: {}", e));
                    break;
                }
            };

            if let Some(hb) = header_buf.as_mut() {
                let scan_from = hb.len().saturating_sub(SCAN_OVERLAP);
                hb.extend_from_slice(&chunk);
                if is_mkv {
                    match mkv::find_mkv_cluster(hb, scan_from).filter(|c| c.offset > 0) {
                        Some(c) => {
                            self.set_file_header(mkv::patch_mkv_segment_length(&hb[..c.offset]), "MKV");
                            self.fetch_cues();
                            chunk = mkv::patch_mkv_segment_length(hb);
                        }
                        None if hb.len() > 2 * 1024 * 1024 => {
                            logw!("[LIVE:{}] no Cluster in first 2MB, skipping header cache", self.sid);
                            chunk = mkv::patch_mkv_segment_length(hb);
                        }
                        None => continue,
                    }
                } else {
                    match mkv::find_avi_movi_offset(hb, scan_from).filter(|o| *o > 0) {
                        Some(o) => {
                            self.set_file_header(hb[..o].to_vec(), "AVI");
                            chunk = std::mem::take(hb);
                        }
                        None if hb.len() > 2 * 1024 * 1024 => {
                            logw!("[LIVE:{}] no movi in first 2MB, skipping header cache", self.sid);
                            chunk = std::mem::take(hb);
                        }
                        None => continue,
                    }
                }
                header_buf = None;
            }

            if let Some(sb) = scan_buf.as_mut() {
                let scan_from = sb.len().saturating_sub(SCAN_OVERLAP);
                sb.extend_from_slice(&chunk);
                match mkv::find_mkv_cluster(sb, scan_from) {
                    Some(c) => {
                        logi!("[LIVE:{}] byte-range: found cluster at +{}B, ts={}ms", self.sid, c.offset, c.timestamp_ms);
                        // A cluster far earlier than the target means the file's duration is
                        // wrong (truncated file): play from here instead.
                        if seek_time > 0.0 && c.timestamp_ms > 0 && (c.timestamp_ms as f64) < seek_time * 500.0 {
                            logw!("[LIVE:{}] byte-range: cluster ts={}ms << seek={}s — metadata duration likely wrong, restarting without -ss", self.sid, c.timestamp_ms, seek_time);
                            let (s, h) = (self.clone(), scan_header.clone());
                            tokio::spawn(async move {
                                let mode = h.map(HeaderMode::Scan).unwrap_or(HeaderMode::None);
                                s.launch_piped(mode, data_offset, 0.0, Some(run_id));
                            });
                            return;
                        }
                        chunk = sb[c.offset..].to_vec();
                        scan_buf = None;
                    }
                    None if sb.len() > 8 * 1024 * 1024 => {
                        logw!("[LIVE:{}] byte-range: no cluster in 8MB, fallback to -ss", self.sid);
                        let s = self.clone();
                        tokio::spawn(async move {
                            s.launch_piped(HeaderMode::None, 0, seek_time, Some(run_id));
                        });
                        return;
                    }
                    None => continue,
                }
            }

            if stdin.write_all(&chunk).await.is_err() {
                return; // ffmpeg is gone
            }

            // First playback of an MKV without Cues: note a cluster every ~10 s as a seek index.
            if is_mkv && data_offset == 0 && dl.offset > 0 {
                let mut i = self.inner.lock().unwrap();
                if i.cue_table.is_none() && (i.last_track_offset == 0 || dl.offset.saturating_sub(i.last_track_offset) > 256 * 1024) {
                    if let Some(c) = mkv::find_mkv_cluster(&chunk, 0).filter(|c| c.timestamp_ms > 0) {
                        let abs = dl.offset.saturating_sub(chunk.len() as u64) + c.offset as u64;
                        let ts = c.timestamp_ms as u64;
                        let far_enough = i.cluster_map.last().map(|l| ts.saturating_sub(l.time_ms) >= 10_000).unwrap_or(true);
                        if far_enough {
                            i.cluster_map.push(CueEntry { time_ms: ts, byte_offset: abs });
                        }
                    }
                    i.last_track_offset = dl.offset;
                }
            }
        }
        // End of the file (or the download gave up): closing stdin lets ffmpeg finish.
        let _ = stdin.shutdown().await;
        drop(stdin);
    }

    /// Reads the MKV Cues (seek index) in the background: from the bytes kept from the probe
    /// when they reach that far, else with a Range request; the end of the file is scanned when
    /// the SeekHead pointer is stale.
    fn fetch_cues(self: &Arc<Self>) {
        let (header, file_size, probe_buf) = {
            let mut i = self.inner.lock().unwrap();
            if i.cue_table.is_some() || i.cues_fetching {
                return;
            }
            let h = match &i.file_header {
                Some(h) => h.clone(),
                None => return,
            };
            i.cues_fetching = true;
            (h, i.file_size, i.probe_data_buf.clone())
        };
        let s = self.clone();
        tokio::spawn(async move {
            let app = s.app.clone();
            let found: Option<Vec<CueEntry>> = async {
                let seg_start = mkv::segment_data_start(&header)?;
                let cues_at = match mkv::parse_mkv_seek_head(&header).filter(|o| *o > 0) {
                    Some(o) => o,
                    None => {
                        logi!("[LIVE:{}] no Cues pointer in SeekHead — using linear estimation", s.sid);
                        return None;
                    }
                };
                let buf: Vec<u8> = if file_size > 0 && cues_at >= file_size {
                    let start = file_size.saturating_sub(512 * 1024);
                    logi!("[LIVE:{}] Cues pointer ({}) beyond file size ({}) — stale, scanning end of file", s.sid, cues_at, file_size);
                    let tail = fetch_range(&app.clients.follow, &app.cfg.user_agent, &s.original_url, start, file_size - 1, 4 * 1024 * 1024).await.ok()?;
                    match mkv::find(&tail, &mkv::CUES_ID, 0) {
                        Some(i) => tail[i..].to_vec(),
                        None => {
                            logi!("[LIVE:{}] no Cues element found in last {}B — file has no Cues", s.sid, tail.len());
                            return None;
                        }
                    }
                } else {
                    logi!("[LIVE:{}] SeekHead says Cues at byte {}", s.sid, cues_at);
                    match probe_buf.filter(|b| b.len() as u64 > cues_at) {
                        Some(b) => b[cues_at as usize..].to_vec(),
                        None => {
                            let mut end = cues_at + 2 * 1024 * 1024;
                            if file_size > 0 {
                                end = end.min(file_size);
                            }
                            match fetch_range(&app.clients.follow, &app.cfg.user_agent, &s.original_url, cues_at, end - 1, 2 * 1024 * 1024).await {
                                Ok(b) => b,
                                Err(e) => {
                                    logw!("[LIVE:{}] Cues fetch failed: {}", s.sid, e);
                                    return None;
                                }
                            }
                        }
                    }
                };
                let entries = mkv::parse_mkv_cues(&buf, seg_start);
                if entries.is_empty() {
                    logw!("[LIVE:{}] Cues element found but no entries parsed", s.sid);
                    None
                } else {
                    Some(entries)
                }
            }
            .await;
            let mut i = s.inner.lock().unwrap();
            if let Some(e) = found {
                logi!("[LIVE:{}] Cues parsed: {} entries, range {}ms—{}ms", s.sid, e.len(), e[0].time_ms, e[e.len() - 1].time_ms);
                i.cue_table = Some(Arc::new(e));
            }
            i.cues_fetching = false;
            i.probe_data_buf = None;
        });
    }

    // ── output of ffmpeg ──

    fn on_video_event(&self, run_id: u64, ev: Event, timescale: u32) {
        let mut i = self.inner.lock().unwrap();
        if i.run_id != run_id {
            return;
        }
        i.timescale = timescale;
        match ev {
            Event::Init(buf) => {
                if let Err(e) = write_atomic(&self.tmp_dir, "init.mp4", &buf) {
                    loge!("[LIVE:{}] cannot write init segment: {}", self.sid, e);
                    return;
                }
                i.has_init = true;
                i.disk_usage_bytes += buf.len() as i64;
                i.state = "running";
                logi!("[LIVE:{}] init segment: {} bytes (disk)", self.sid, buf.len());
                Self::log_locked(&mut i, "info", format!("init segment: {} bytes", buf.len()));
            }
            Event::Segment(buf, duration) => {
                self.on_segment(&mut i, buf, duration);
                drop(i);
                self.notify.notify_waiters();
            }
        }
    }

    fn on_segment(&self, i: &mut Inner, buf: Vec<u8>, duration: f64) {
        let cfg = &self.app.cfg;
        let seq = i.next_seq;
        let path = match write_atomic(&self.tmp_dir, &format!("{}.m4s", seq), &buf) {
            Ok(p) => p,
            Err(e) => {
                loge!("[LIVE:{}] cannot write segment {}: {}", self.sid, seq, e);
                return;
            }
        };
        i.next_seq += 1;
        // The fragment's own decode time when present, else the running total.
        let start_time = extract_tfdt(&buf, i.timescale).unwrap_or(i.total_produced_sec);
        if let Some(prev) = i.segments.back() {
            let prev_end = prev.start_time + prev.duration;
            let gap = start_time - prev_end;
            if gap > 0.5 {
                let prev_seq = prev.seq;
                logw!("[LIVE:{}] VIDEO GAP: {:.3}s between seg {} ({:.2}s) and seg {} ({:.2}s)", self.sid, gap, prev_seq, prev_end, seq, start_time);
                Self::log_locked(i, "warn", format!("VIDEO GAP: {:.3}s at {:.2}s-{:.2}s (seq {}→{})", gap, prev_end, start_time, prev_seq, seq));
            }
        }
        i.segments.push_back(Seg { seq, duration, path, start_time, size: buf.len() as u64 });
        i.total_produced_sec = start_time + duration;
        i.disk_usage_bytes += buf.len() as i64;

        // Delete what the player has certainly finished with. consumed_time is the newest
        // segment it FETCHED; it buffers ahead, so the playhead is about buffer_window behind.
        if i.consumed_time >= 0.0 && !i.segments.is_empty() {
            let playhead = (i.consumed_time - cfg.buffer_window_sec).max(0.0);
            let cutoff = (playhead - cfg.keep_behind_sec).max(0.0);
            while let Some(oldest) = i.segments.front() {
                if oldest.start_time + oldest.duration >= cutoff {
                    break;
                }
                let old = i.segments.pop_front().unwrap();
                i.disk_usage_bytes -= old.size as i64;
                let _ = std::fs::remove_file(&old.path);
            }
            let mut freed: i64 = 0;
            for ao in i.audio_outputs.iter_mut() {
                while let Some(a) = ao.segments.front() {
                    if a.start_time + a.duration >= cutoff {
                        break;
                    }
                    let old = ao.segments.pop_front().unwrap();
                    freed += old.size as i64;
                    let _ = std::fs::remove_file(&old.path);
                }
            }
            i.disk_usage_bytes -= freed;
        }

        // Far enough ahead of the player: hold the download back.
        if i.dl.is_some() && Self::ahead_sec(i) >= cfg.throttle_pause_sec {
            self.set_throttle(i, true);
        }
    }

    fn on_audio_event(&self, run_id: u64, idx: usize, ev: Event, timescale: u32) {
        let mut guard = self.inner.lock().unwrap();
        let i = &mut *guard;
        if i.run_id != run_id {
            return;
        }
        let ao = match i.audio_outputs.get_mut(idx) {
            Some(a) => a,
            None => return,
        };
        ao.timescale = timescale;
        match ev {
            Event::Init(buf) => {
                if write_atomic(&ao.dir, "init.mp4", &buf).is_err() {
                    return;
                }
                ao.has_init = true;
                i.disk_usage_bytes += buf.len() as i64;
                logi!("[LIVE:{}] audio track {} init: {} bytes", self.sid, idx, buf.len());
                Self::log_locked(i, "info", format!("audio track {} init: {} bytes", idx, buf.len()));
            }
            Event::Segment(buf, duration) => {
                let seq = ao.next_seq;
                let start_time = extract_tfdt(&buf, ao.timescale).unwrap_or(ao.acc_time);
                let path = match write_atomic(&ao.dir, &format!("{}.m4s", seq), &buf) {
                    Ok(p) => p,
                    Err(_) => return,
                };
                ao.next_seq += 1;
                ao.acc_time = start_time + duration;
                let mut gap_log = None;
                if let Some(prev) = ao.segments.back() {
                    let prev_end = prev.start_time + prev.duration;
                    let gap = start_time - prev_end;
                    if gap > 0.5 {
                        gap_log = Some(format!("AUDIO[{}] GAP: {:.3}s at {:.2}s-{:.2}s (seq {}→{})", idx, gap, prev_end, start_time, prev.seq, seq));
                    }
                }
                ao.segments.push_back(Seg { seq, duration, path, start_time, size: buf.len() as u64 });
                i.disk_usage_bytes += buf.len() as i64;
                if let Some(g) = gap_log {
                    logw!("[LIVE:{}] {}", self.sid, g);
                    Self::log_locked(i, "warn", g);
                }
            }
        }
    }

    fn on_stdout_end(&self, run_id: u64) {
        let mut i = self.inner.lock().unwrap();
        if i.run_id != run_id {
            return;
        }
        if i.state == "running" {
            i.state = "ended";
            Self::log_locked(&mut i, "info", "ffmpeg stdout ended — stream complete".to_string());
        }
        drop(i);
        self.notify.notify_waiters();
    }

    fn on_exit(&self, run_id: u64, status: Option<std::process::ExitStatus>) {
        let mut i = self.inner.lock().unwrap();
        if i.run_id != run_id {
            return;
        }
        let code = status.as_ref().and_then(|s| s.code());
        let signal = status.as_ref().and_then(exit_signal);
        i.ffmpeg_exit_code = code;
        i.ffmpeg_signal = signal.clone();
        i.ffmpeg_alive = false;
        i.run_done = true;
        if i.state != "seeking" && i.state != "stopped" {
            let c = code.map(|c| c.to_string()).unwrap_or_else(|| "null".into());
            let sg = signal.unwrap_or_else(|| "null".into());
            logi!("[LIVE:{}] ffmpeg exited (code={} signal={})", self.sid, c, sg);
            Self::log_locked(&mut i, if code == Some(0) { "info" } else { "error" }, format!("ffmpeg exited code={} signal={}", c, sg));
        }
        drop(i);
        self.notify.notify_waiters();
    }

    // ── operations ──

    /// Restarts at `time` seconds. Numbering restarts at 0: the player reloads the playlist.
    pub fn seek(self: &Arc<Self>, time: f64) {
        {
            let mut i = self.inner.lock().unwrap();
            i.seek_gen += 1;
            i.state = "seeking";
            let gen = i.seek_gen;
            Self::log_locked(&mut i, "info", format!("seek to {}s (gen={})", time, gen));
            // Remember what was downloaded and produced so far (progress bars on the stats page).
            if let Some(dl) = &i.dl {
                let end = dl.offset.load(Ordering::Relaxed);
                let read = dl.bytes_read.load(Ordering::Relaxed);
                let start = end.saturating_sub(read);
                let method = i.seek_method;
                i.total_bytes_downloaded += read;
                if end > start {
                    i.downloaded_ranges.push((start, end, method));
                }
            }
            if i.total_produced_sec > 0.0 {
                let r = (i.start_time, i.start_time + i.total_produced_sec);
                i.produced_ranges.push(r);
            }
            self.kill_locked(&mut i);
            self.clean_files(&mut i);
            i.segments.clear();
            i.audio_outputs.clear();
            i.consumed_time = -1.0;
            i.total_produced_sec = 0.0;
            i.seq_base = 0;
            i.next_seq = 0;
            i.start_time = time;
        }
        self.notify.notify_waiters();
        self.spawn_ffmpeg(time);
    }

    /// Stops ffmpeg but keeps the segments.
    pub fn pause(&self) {
        let mut i = self.inner.lock().unwrap();
        if i.state == "paused" {
            return;
        }
        Self::log_locked(&mut i, "info", "session paused (segments kept)".to_string());
        i.state = "paused";
        self.kill_locked(&mut i);
    }

    /// Continues after the last segment. False when the session was not paused.
    pub fn resume(self: &Arc<Self>) -> bool {
        let at = {
            let mut i = self.inner.lock().unwrap();
            if i.state != "paused" {
                return false;
            }
            Self::log_locked(&mut i, "info", "session resumed".to_string());
            i.state = "running";
            i.segments.back().map(|s| s.start_time + s.duration).unwrap_or(i.start_time)
        };
        self.spawn_ffmpeg(at);
        true
    }

    /// Stops everything and deletes the session's folder.
    pub fn stop(&self) {
        {
            let mut i = self.inner.lock().unwrap();
            i.state = "stopped";
            Self::log_locked(&mut i, "info", "session stopped".to_string());
            self.kill_locked(&mut i);
            self.clean_files(&mut i);
            i.segments.clear();
            i.probe_cached_data = None;
            i.probe_data_buf = None;
        }
        let _ = std::fs::remove_dir_all(&self.tmp_dir);
        self.notify.notify_waiters();
    }

    // ── playlists ──

    pub fn ahead_sec(i: &Inner) -> f64 {
        let last = match i.segments.back() {
            Some(l) => l,
            None => return 0.0,
        };
        let end = last.start_time + last.duration;
        if i.consumed_time < 0.0 {
            return end - i.start_time; // nothing fetched yet: everything is ahead
        }
        (end - i.consumed_time).max(0.0)
    }

    pub fn behind_sec(i: &Inner) -> f64 {
        match i.segments.front() {
            Some(f) if i.consumed_time >= 0.0 => (i.consumed_time - f.start_time).max(0.0),
            _ => 0.0,
        }
    }

    /// Media playlist of the main output: from the player's position to about a minute ahead,
    /// so it cannot race ahead when ffmpeg is faster than real time.
    pub fn playlist(&self, base: &str) -> String {
        let cfg = &self.app.cfg;
        let mut i = self.inner.lock().unwrap();
        i.last_access = now_ms();
        self.maybe_resume_download(&mut i);
        let mut lines = vec!["#EXTM3U".to_string(), "#EXT-X-VERSION:7".to_string()];
        let n = i.segments.len();
        let mut start = 0usize;
        if i.consumed_time >= 0.0 && n > 0 {
            for (k, s) in i.segments.iter().enumerate() {
                start = k;
                if s.start_time + s.duration >= i.consumed_time {
                    break;
                }
            }
        }
        let mut end = start;
        let mut acc = 0.0;
        while end < n && acc < cfg.playlist_window_sec {
            acc += i.segments[end].duration;
            end += 1;
        }
        let window: Vec<&Seg> = i.segments.range(start..end).collect();
        let max_dur = window.iter().fold(cfg.seg_duration, |m, s| m.max(s.duration));
        lines.push(format!("#EXT-X-TARGETDURATION:{}", max_dur.ceil() as i64));
        lines.push(format!("#EXT-X-MEDIA-SEQUENCE:{}", window.first().map(|s| s.seq).unwrap_or(i.seq_base)));
        if i.has_init {
            lines.push(format!("#EXT-X-MAP:URI=\"{}?sid={}&live_seg=init.mp4\"", base, self.sid));
        }
        for s in &window {
            lines.push(format!("#EXTINF:{:.6},", s.duration));
            lines.push(format!("{}?sid={}&live_seg={}.m4s", base, self.sid, s.seq));
        }
        if i.state == "ended" && !window.is_empty() {
            lines.push("#EXT-X-ENDLIST".to_string());
        }
        lines.join("\n") + "\n"
    }

    /// True when the playlist to hand out is the master playlist (several audio tracks).
    pub fn is_multi_audio(&self) -> bool {
        let i = self.inner.lock().unwrap();
        i.audio_track_count > 1 && !i.audio_outputs.is_empty()
    }

    /// Master playlist: one video-only variant, every audio track as its own rendition.
    pub fn master_playlist(&self, base: &str) -> String {
        let i = self.inner.lock().unwrap();
        let mut lines = vec!["#EXTM3U".to_string(), "#EXT-X-INDEPENDENT-SEGMENTS".to_string()];
        for t in 0..i.audio_track_count.max(0) as usize {
            let lbl = i.audio_track_labels.get(t);
            let field = |k: &str| lbl.and_then(|l| l.get(k)).and_then(|v| v.as_str()).filter(|s| !s.is_empty()).map(|s| s.to_string());
            let lang = field("lang");
            let name = field("name").or_else(|| lang.clone()).unwrap_or_else(|| format!("Track {}", t + 1));
            lines.push(format!(
                "#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"audio\",NAME=\"{}\"{}{},CHANNELS=\"2\",URI=\"{}?sid={}&live_seg=a{}.m3u8\"",
                name,
                if t == 0 { ",DEFAULT=YES,AUTOSELECT=YES" } else { ",DEFAULT=NO" },
                lang.map(|l| format!(",LANGUAGE=\"{}\"", l)).unwrap_or_default(),
                base,
                self.sid,
                t
            ));
        }
        lines.push("#EXT-X-STREAM-INF:BANDWIDTH=2000000,AUDIO=\"audio\"".to_string());
        lines.push(format!("{}?sid={}&live_seg=main.m3u8", base, self.sid));
        lines.join("\n") + "\n"
    }

    /// Media playlist of one audio track: every segment still on disk (the player may switch
    /// tracks at a position behind what it last fetched for video).
    pub fn audio_playlist(&self, track: usize, base: &str) -> String {
        let cfg = &self.app.cfg;
        let mut i = self.inner.lock().unwrap();
        if track >= i.audio_outputs.len() {
            return String::new();
        }
        i.last_access = now_ms();
        let ended = i.state == "ended";
        let ao = &i.audio_outputs[track];
        let mut lines = vec!["#EXTM3U".to_string(), "#EXT-X-VERSION:7".to_string()];
        let max_dur = ao.segments.iter().fold(cfg.seg_duration, |m, s| m.max(s.duration));
        lines.push(format!("#EXT-X-TARGETDURATION:{}", max_dur.ceil() as i64));
        lines.push(format!("#EXT-X-MEDIA-SEQUENCE:{}", ao.segments.front().map(|s| s.seq).unwrap_or(0)));
        if ao.has_init {
            lines.push(format!("#EXT-X-MAP:URI=\"{}?sid={}&live_seg=a{}_init.mp4\"", base, self.sid, track));
        }
        for s in &ao.segments {
            lines.push(format!("#EXTINF:{:.6},", s.duration));
            lines.push(format!("{}?sid={}&live_seg=a{}_{}.m4s", base, self.sid, track, s.seq));
        }
        if ended && !ao.segments.is_empty() {
            lines.push("#EXT-X-ENDLIST".to_string());
        }
        lines.join("\n") + "\n"
    }

    /// Called when the player fetched main segment `seq`: moves the consumption mark, lets a
    /// throttled download continue when the player is catching up.
    pub fn mark_consumed(&self, seq: u64) {
        let mut i = self.inner.lock().unwrap();
        if let Some(first) = i.segments.front().map(|s| s.seq) {
            if seq >= first {
                if let Some(seg) = i.segments.get((seq - first) as usize) {
                    let t = seg.start_time;
                    if t > i.consumed_time {
                        i.consumed_time = t;
                    }
                }
            }
        }
        self.maybe_resume_download(&mut i);
        i.last_access = now_ms();
    }

    pub fn main_playlist_touch(&self) {
        let mut i = self.inner.lock().unwrap();
        i.last_access = now_ms();
        self.maybe_resume_download(&mut i);
    }

    // ── waiting ──

    /// Returns when at least `count` segments exist, the stream ended or was stopped, ffmpeg
    /// exited without getting there, or the timeout passed.
    pub async fn wait_for_segments(&self, count: usize, timeout_ms: u64) {
        let deadline = tokio::time::Instant::now() + Duration::from_millis(timeout_ms);
        loop {
            let notified = self.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            {
                let i = self.inner.lock().unwrap();
                if i.segments.len() >= count || i.state == "stopped" || i.state == "ended" || i.run_done {
                    return;
                }
            }
            tokio::select! {
                _ = &mut notified => {}
                _ = tokio::time::sleep_until(deadline) => return,
            }
        }
    }

    /// Waits for main segment `seq` to be produced: until the next event of the session (new
    /// segment, end, stop) or 15 s, whichever comes first.
    pub async fn wait_for_segment_seq(&self, seq: u64) {
        let notified = self.notify.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        {
            let i = self.inner.lock().unwrap();
            if i.next_seq > seq || i.state == "ended" || i.state == "stopped" {
                return;
            }
        }
        tokio::select! {
            _ = &mut notified => {}
            _ = tokio::time::sleep(Duration::from_secs(15)) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    //! End-to-end with a real ffmpeg (skipped when ffmpeg is not installed): a generated MKV
    //! with two audio tracks is served from a local HTTP server and played through a session.
    //! Mirrors transcode/test_seek_audio.js (multi-audio output before and after a seek).
    use super::*;
    use crate::app::{App, GpuState};
    use std::collections::HashMap;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    async fn serve_file(data: Arc<Vec<u8>>) -> u16 {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            loop {
                let (mut sock, _) = match listener.accept().await {
                    Ok(x) => x,
                    Err(_) => return,
                };
                let data = data.clone();
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 4096];
                    let n = sock.read(&mut buf).await.unwrap_or(0);
                    let req = String::from_utf8_lossy(&buf[..n]).to_ascii_lowercase();
                    let range = req.split("range: bytes=").nth(1).and_then(|r| r.lines().next()).unwrap_or("0-").trim().to_string();
                    let mut it = range.split('-');
                    let start: usize = it.next().and_then(|v| v.parse().ok()).unwrap_or(0).min(data.len());
                    let end: usize = it.next().and_then(|v| v.parse::<usize>().ok()).map(|e| e + 1).unwrap_or(data.len()).min(data.len());
                    let body = &data[start..end.max(start)];
                    let head = format!("HTTP/1.1 206 Partial Content\r\nContent-Range: bytes {}-{}/{}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", start, end.saturating_sub(1), data.len(), body.len());
                    let _ = sock.write_all(head.as_bytes()).await;
                    let _ = sock.write_all(body).await;
                    let _ = sock.shutdown().await;
                });
            }
        });
        port
    }

    fn test_app(dir: &Path) -> Arc<App> {
        let mut cfg = crate::config::Config::defaults();
        cfg.streams_dir = dir.to_path_buf();
        // Plain clients: the production ones refuse loopback, where the test server lives.
        let plain = reqwest::Client::builder().no_proxy().build().unwrap();
        Arc::new(App {
            cfg,
            enc: Arc::new(std::sync::RwLock::new(Default::default())),
            hw: Arc::new(Mutex::new(Default::default())),
            clients: crate::ssrf::Clients { follow: plain.clone(), manual: plain },
            ffmpeg_version: String::new(),
            started: std::time::Instant::now(),
            sessions: Mutex::new(HashMap::new()),
            probe_cache: Mutex::new(Vec::new()),
            playlists: Mutex::new(HashMap::new()),
            monitor: Mutex::new(Default::default()),
            gpu: Mutex::new(GpuState::default()),
            prev_self_cpu: Mutex::new((0, 0)),
        })
    }

    fn have_ffmpeg() -> bool {
        crate::proc::run("ffmpeg", &["-version"], Duration::from_secs(5)).map(|o| o.ok).unwrap_or(false)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn multi_audio_session_start_seek_stop() {
        if !have_ffmpeg() {
            eprintln!("ffmpeg not installed: skipped");
            return;
        }
        let dir = std::env::temp_dir().join(format!("tvz-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("two_audio.mkv");
        let made = crate::proc::run(
            "ffmpeg",
            &[
                "-hide_banner", "-loglevel", "error", "-y", "-f", "lavfi", "-i", "testsrc2=s=320x180:r=24:d=40", "-f", "lavfi", "-i", "sine=frequency=440:duration=40", "-f", "lavfi", "-i",
                "sine=frequency=880:duration=40", "-map", "0:v", "-map", "1:a", "-map", "2:a", "-c:v", "libx264", "-preset", "ultrafast", "-g", "24", "-c:a", "ac3", "-metadata:s:a:0", "language=eng",
                "-metadata:s:a:1", "language=fre", src.to_str().unwrap(),
            ],
            Duration::from_secs(60),
        );
        assert!(made.map(|o| o.ok).unwrap_or(false), "could not generate the test file");
        let data = Arc::new(std::fs::read(&src).unwrap());
        let port = serve_file(data.clone()).await;

        let app = test_app(&dir.join("streams"));
        let s = Session::new(
            app.clone(),
            NewSession { sid: "testsid000000001".into(), url: format!("http://localhost:{}/two_audio.mkv", port), audio_track: 0, audio_bitrate: "128k".into(), start_time: 0.0, file_size: data.len() as u64, duration: 40.0, strategy: "copy-video-transcode-audio".into() },
        );
        {
            let mut i = s.inner.lock().unwrap();
            i.source_video_codec = "h264".into();
            i.source_audio_codec = "ac3".into();
            i.source_audio_channels = 1;
            i.audio_track_count = 2;
            i.audio_track_labels = vec![serde_json::json!({ "index": 0, "lang": "eng", "name": "", "codec": "ac3", "channels": 1 }), serde_json::json!({ "index": 1, "lang": "fre", "name": "French", "codec": "ac3", "channels": 1 })];
        }
        s.spawn_ffmpeg(0.0);
        s.wait_for_segments(2, 30_000).await;
        // give the audio outputs a moment to catch up with the video
        tokio::time::sleep(Duration::from_millis(1500)).await;

        let base = "http://127.0.0.1:1/transcode";
        {
            let i = s.inner.lock().unwrap();
            assert!(i.segments.len() >= 2, "video segments: {} (logs: {:?})", i.segments.len(), i.logs);
            assert_eq!(i.audio_outputs.len(), 2);
            assert!(i.ffmpeg_args.contains("tcp://127.0.0.1:"), "{}", i.ffmpeg_args);
            assert!(i.ffmpeg_args.contains("-c:v copy -an"));
            assert!(i.file_header.is_some(), "MKV header cached for seeking");
        }
        assert!(s.is_multi_audio());
        let master = s.master_playlist(base);
        assert!(master.contains("#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"audio\",NAME=\"eng\",DEFAULT=YES,AUTOSELECT=YES,LANGUAGE=\"eng\",CHANNELS=\"2\",URI=\"http://127.0.0.1:1/transcode?sid=testsid000000001&live_seg=a0.m3u8\""), "{master}");
        assert!(master.contains("NAME=\"French\",DEFAULT=NO,LANGUAGE=\"fre\""), "{master}");
        assert!(master.contains("#EXT-X-STREAM-INF:BANDWIDTH=2000000,AUDIO=\"audio\"\nhttp://127.0.0.1:1/transcode?sid=testsid000000001&live_seg=main.m3u8\n"));
        let video = s.playlist(base);
        assert!(video.starts_with("#EXTM3U\n#EXT-X-VERSION:7\n#EXT-X-TARGETDURATION:"));
        assert!(video.contains("#EXT-X-MAP:URI=\"http://127.0.0.1:1/transcode?sid=testsid000000001&live_seg=init.mp4\""));
        assert!(video.contains("live_seg=0.m4s"));
        assert!(!video.contains("PROGRAM-DATE-TIME"));
        for t in 0..2 {
            let a = s.audio_playlist(t, base);
            assert!(a.contains("#EXTINF:"), "audio {t} has no segments: {a}");
            assert!(a.contains(&format!("live_seg=a{}_init.mp4", t)));
            assert!(!a.contains("PROGRAM-DATE-TIME"));
            let seg = s.read_file(&format!("a{}/0.m4s", t)).expect("audio segment on disk");
            assert!(crate::mkv::find(&seg, b"moof", 0).is_some());
            assert!(s.read_file(&format!("a{}/init.mp4", t)).is_some());
        }
        assert_eq!(s.audio_playlist(5, base), "");
        let seg0 = s.read_file("0.m4s").expect("video segment on disk");
        assert_eq!(&seg0[4..8], b"moof");
        s.mark_consumed(0);
        assert_eq!(s.inner.lock().unwrap().consumed_time, 0.0);

        // Seek: numbering restarts, both audio outputs come back (test_seek_audio.js).
        s.seek(20.0);
        s.wait_for_segments(1, 30_000).await;
        tokio::time::sleep(Duration::from_millis(1500)).await;
        {
            let i = s.inner.lock().unwrap();
            assert!(!i.segments.is_empty(), "segments after seek (logs: {:?})", i.logs);
            assert_eq!(i.segments.front().unwrap().seq, 0);
            assert!(i.seek_method.starts_with("byte-range"), "{}", i.seek_method);
            assert!(!i.ffmpeg_args.contains(" -ss "), "byte-range seek has no -ss");
            assert_eq!(i.audio_track_count, 2);
            assert_eq!(i.audio_outputs.len(), 2);
            assert_eq!(i.produced_ranges.len(), 1);
            assert_eq!(i.seek_gen, 1);
        }
        assert!(s.master_playlist(base).contains("a1.m3u8"));
        for t in 0..2 {
            assert!(s.audio_playlist(t, base).contains("#EXTINF:"), "audio {t} after seek");
            assert!(s.read_file(&format!("a{}/init.mp4", t)).is_some());
        }

        // Pause keeps the files, resume restarts ffmpeg, stop deletes everything.
        s.pause();
        assert_eq!(s.inner.lock().unwrap().state, "paused");
        assert!(s.read_file("init.mp4").is_some());
        assert!(s.resume());
        assert!(!s.resume());
        s.stop();
        assert_eq!(s.inner.lock().unwrap().state, "stopped");
        assert!(!s.tmp_dir.exists(), "session folder removed");
        tokio::time::sleep(Duration::from_millis(300)).await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_source_that_cannot_be_played_fails_fast() {
        if !have_ffmpeg() {
            return;
        }
        let dir = std::env::temp_dir().join(format!("tvz-test-bad-{}", std::process::id()));
        let port = serve_file(Arc::new(vec![0x55u8; 50_000])).await;
        let app = test_app(&dir);
        let s = Session::new(app, NewSession { sid: "testsid000000002".into(), url: format!("http://localhost:{}/junk.bin", port), audio_track: 0, audio_bitrate: String::new(), start_time: 0.0, file_size: 0, duration: 0.0, strategy: String::new() });
        s.spawn_ffmpeg(0.0);
        let t0 = std::time::Instant::now();
        s.wait_for_segments(1, 15_000).await;
        assert!(t0.elapsed() < Duration::from_secs(10), "returns when ffmpeg exits, not at the timeout");
        {
            let i = s.inner.lock().unwrap();
            assert!(i.segments.is_empty());
            assert!(i.run_done);
            assert!(i.ffmpeg_exit_code.is_some());
        }
        s.stop();
        assert!(!s.tmp_dir.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
