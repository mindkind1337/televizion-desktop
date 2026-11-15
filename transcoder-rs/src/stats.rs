//! The JSON behind /stats/json and /ws/stats (same field names as the Node service, so the
//! web player's stats panel and the /stats dashboard work unchanged).

use crate::app::App;
use crate::session::{Inner, Session};
use crate::sysinfo;
use crate::util::{js_round, now_ms, num};
use serde_json::{json, Map, Value};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

fn r1(v: f64) -> Value {
    num(js_round(v * 10.0) / 10.0)
}

pub fn brief(app: &App) -> Value {
    json!({ "version": "v4.0", "activeSessions": app.session_count(), "uptime": app.started.elapsed().as_secs_f64().round() as u64 })
}

/// Asks nvidia-smi in the background (at most every 3 s); results show up on a later call.
fn poll_gpu(app: &Arc<App>) {
    let now = now_ms();
    {
        let mut g = app.gpu.lock().unwrap();
        if g.polling || g.has_nvidia == Some(false) || now.saturating_sub(g.last_poll) < 3000 {
            return;
        }
        g.polling = true;
        g.last_poll = now;
    }
    let app = app.clone();
    tokio::spawn(async move {
        let run = |args: &'static [&'static str]| async move {
            let fut = crate::proc::tokio_command("nvidia-smi").args(args).stdin(std::process::Stdio::null()).stderr(std::process::Stdio::null()).output();
            match tokio::time::timeout(Duration::from_secs(5), fut).await {
                Ok(Ok(o)) if o.status.success() => Some(String::from_utf8_lossy(&o.stdout).into_owned()),
                _ => None,
            }
        };
        let q = run(&["--query-gpu=name,utilization.gpu,utilization.memory,memory.used,memory.total,temperature.gpu,utilization.encoder,utilization.decoder", "--format=csv,noheader,nounits"]).await;
        let mut stats = Value::Null;
        let mut procs = Vec::new();
        let has = q.is_some();
        if let Some(out) = q {
            let gpus: Vec<Value> = out
                .trim()
                .lines()
                .filter(|l| !l.trim().is_empty())
                .map(|line| {
                    let p: Vec<&str> = line.split(',').map(|s| s.trim()).collect();
                    let n = |i: usize| p.get(i).and_then(|v| v.parse::<i64>().ok()).unwrap_or(0);
                    json!({ "name": p.first().filter(|s| !s.is_empty()).copied().unwrap_or("Unknown GPU"), "gpuUtil": n(1), "memUtil": n(2), "memUsed": n(3), "memTotal": n(4), "temp": n(5), "encUtil": n(6), "decUtil": n(7) })
                })
                .collect();
            if !gpus.is_empty() {
                stats = Value::Array(gpus);
            }
            // Per-process encoder/decoder use: "gpu pid type sm mem enc dec ..."
            if let Some(pmon) = run(&["pmon", "-c", "1", "-s", "u"]).await {
                for line in pmon.lines() {
                    if line.trim().is_empty() || line.starts_with('#') {
                        continue;
                    }
                    let f: Vec<&str> = line.split_whitespace().collect();
                    if f.len() < 7 {
                        continue;
                    }
                    if let Ok(pid) = f[1].parse::<u32>() {
                        let pct = |v: &str| if v == "-" { None } else { Some(v.parse::<u64>().unwrap_or(0)) };
                        procs.push((pid, 0u64, pct(f[5]), pct(f[6])));
                    }
                }
            }
            // Per-process memory from the plain table: "|  0  N/A  N/A  1234  C  ffmpeg  123MiB |"
            if let Some(full) = run(&[]).await {
                if let Some(at) = full.find("Processes:") {
                    for line in full[at..].lines() {
                        let f: Vec<&str> = line.split_whitespace().collect();
                        if f.len() < 8 || f[0] != "|" || f[f.len() - 1] != "|" {
                            continue;
                        }
                        let mem = f[f.len() - 2].strip_suffix("MiB").and_then(|m| m.parse::<u64>().ok());
                        if let (Ok(_gpu), Ok(pid), Some(mem)) = (f[1].parse::<u32>(), f[4].parse::<u32>(), mem) {
                            match procs.iter_mut().find(|p| p.0 == pid) {
                                Some(p) => p.1 = mem,
                                None => procs.push((pid, mem, None, None)),
                            }
                        }
                    }
                }
            }
        }
        let mut g = app.gpu.lock().unwrap();
        if g.has_nvidia.is_none() {
            g.has_nvidia = Some(has);
        }
        g.stats = stats;
        g.processes = procs;
        g.polling = false;
    });
}

fn session_json(sid: &str, s: &Session, i: &Inner, app: &App, cores: usize, active_gpu_sessions: usize) -> Value {
    let n = i.segments.len();
    let total_buffered: f64 = i.segments.iter().map(|x| x.duration).sum();
    let alive = i.ffmpeg_alive && i.ffmpeg_pid.is_some();
    let pid = if alive { i.ffmpeg_pid } else { None };

    let (mut cpu_pct, mut mem_mb, mut gpu_mem, mut gpu_pct) = (0i64, 0i64, 0u64, 0u64);
    if let Some(pid) = pid {
        if let Some(p) = app.monitor.lock().unwrap().get(pid) {
            cpu_pct = js_round(p.cpu / cores as f64) as i64;
            mem_mb = js_round(p.memory as f64 / 1048576.0) as i64;
        }
        let g = app.gpu.lock().unwrap();
        if let Some(p) = g.processes.iter().find(|p| p.0 == pid) {
            gpu_mem = p.1;
            if p.2.is_some() || p.3.is_some() {
                gpu_pct = p.2.unwrap_or(0).max(p.3.unwrap_or(0));
            } else if let Some(g0) = g.stats.get(0) {
                let m = g0["encUtil"].as_u64().unwrap_or(0).max(g0["decUtil"].as_u64().unwrap_or(0));
                gpu_pct = if active_gpu_sessions > 0 { js_round(m as f64 / active_gpu_sessions as f64) as u64 } else { m };
            }
        }
    }

    let mut dl_bytes = 0u64;
    let mut dl_offset = 0u64;
    let dl = match &i.dl {
        Some(d) => {
            dl_bytes = d.bytes_read.load(Ordering::Relaxed);
            dl_offset = d.offset.load(Ordering::Relaxed);
            json!({
                "paused": d.paused.load(Ordering::Relaxed), "bytesRead": dl_bytes, "offset": dl_offset,
                "retries": d.retries.load(Ordering::Relaxed), "destroyed": d.destroyed.load(Ordering::Relaxed), "throttled": i.dl_throttled
            })
        }
        None => Value::Null,
    };

    let enc = i.video_encoder_name.to_ascii_lowercase();
    let encoder_type = if enc.contains("nvenc") {
        "NVENC"
    } else if enc.contains("qsv") {
        "QSV"
    } else if enc.contains("amf") {
        "AMF"
    } else if enc.contains("vaapi") {
        "VAAPI"
    } else {
        "CPU"
    };

    let mut dranges: Vec<Value> = i.downloaded_ranges.iter().map(|(a, b, m)| json!({ "startByte": a, "endByte": b, "seekMethod": m })).collect();
    if i.dl.is_some() && dl_bytes > 0 {
        dranges.push(json!({ "startByte": dl_offset.saturating_sub(dl_bytes), "endByte": dl_offset, "seekMethod": i.seek_method }));
    }
    let mut pranges: Vec<Value> = i.produced_ranges.iter().map(|(a, b)| json!({ "startSec": num(*a), "endSec": num(*b) })).collect();
    if i.total_produced_sec > 0.0 {
        pranges.push(json!({ "startSec": num(i.start_time), "endSec": num(i.start_time + i.total_produced_sec) }));
    }

    let audio_outputs: Vec<Value> = i
        .audio_outputs
        .iter()
        .enumerate()
        .map(|(t, ao)| {
            let total: f64 = ao.segments.iter().map(|x| x.duration).sum();
            let st = ao.segments.front().map(|x| x.start_time).unwrap_or(0.0);
            let en = ao.segments.back().map(|x| x.start_time + x.duration).unwrap_or(0.0);
            json!({
                "track": t,
                "label": i.audio_track_labels.get(t).cloned().unwrap_or_else(|| json!({})),
                "segmentsInMemory": ao.segments.len(),
                "nextSeq": ao.next_seq,
                "segmentRange": match (ao.segments.front(), ao.segments.back()) { (Some(a), Some(b)) => format!("{}-{}", a.seq, b.seq), _ => "none".to_string() },
                "bufferedSec": r1(total),
                "timeRange": format!("{:.1}s-{:.1}s", st, en),
            })
        })
        .collect();

    let logs: Vec<Value> = i.logs.iter().map(|(t, level, msg)| json!({ "t": t, "level": level, "msg": msg })).collect();
    let now = now_ms();
    let or_res = if i.output_resolution.is_empty() { i.resolution.clone() } else { i.output_resolution.clone() };

    let mut m = Map::new();
    let mut put = |k: &str, v: Value| {
        m.insert(k.to_string(), v);
    };
    put("sid", json!(sid));
    put("state", json!(i.state));
    put("strategy", json!(if i.strategy.is_empty() { "transcode-audio" } else { i.strategy.as_str() }));
    put("url", json!(s.original_url));
    put("startTime", num(i.start_time));
    put("fileDuration", num(i.duration));
    put("fileSize", json!(i.file_size));
    put("audioTrack", json!(i.audio_track));
    put("audioBitrate", json!(i.audio_bitrate));
    put("segmentsInMemory", json!(n));
    put("nextSeq", json!(i.next_seq));
    put("seqBase", json!(i.seq_base));
    put("segmentRange", json!(match (i.segments.front(), i.segments.back()) { (Some(a), Some(b)) => format!("{}-{}", a.seq, b.seq), _ => "none".to_string() }));
    put("totalBufferedSec", r1(total_buffered));
    put("avgSegDuration", if n > 0 { num(js_round(total_buffered / n as f64 * 100.0) / 100.0) } else { json!(0) });
    put("consumedTime", r1(i.consumed_time));
    put("totalProducedSec", r1(i.total_produced_sec));
    put("aheadSec", r1(Session::ahead_sec(i)));
    put("behindSec", r1(Session::behind_sec(i)));
    put("ffmpegAlive", json!(alive));
    put("ffmpegPid", json!(pid));
    put("ffmpegCpuPct", json!(cpu_pct));
    put("ffmpegMemoryMB", json!(mem_mb));
    put("ffmpegGpuMemMB", json!(gpu_mem));
    put("ffmpegGpuPct", json!(gpu_pct));
    put("ffmpegGpuIdx", Value::Null);
    put("encoderType", json!(encoder_type));
    put("downloader", dl);
    put("totalDownloaded", json!(i.total_bytes_downloaded + dl_bytes));
    put("downloadedRanges", Value::Array(dranges));
    put("producedRanges", Value::Array(pranges));
    put("diskUsageBytes", json!(i.disk_usage_bytes.max(0)));
    put("idleSec", json!(js_round(now.saturating_sub(i.last_access) as f64 / 1000.0) as i64));
    put("ageSec", json!(js_round(now.saturating_sub(s.created_at) as f64 / 1000.0) as i64));
    put("diskFiles", json!(n + if i.has_init { 1 } else { 0 }));
    put("seekMethod", json!(i.seek_method));
    put("hasCues", json!(i.cue_table.is_some()));
    put("cueEntries", json!(i.cue_table.as_ref().map(|c| c.len()).unwrap_or(0)));
    put("hasHeader", json!(i.file_header.is_some()));
    put("headerSize", json!(i.file_header.as_ref().map(|h| h.len()).unwrap_or(0)));
    put("tmpDir", json!(s.tmp_dir.to_string_lossy()));
    put("createdAt", json!(s.created_at));
    put("clientIp", json!(i.client_ip));
    put("sourceVideoCodec", json!(i.source_video_codec));
    put("sourceAudioCodec", json!(i.source_audio_codec));
    put("outputVideoCodec", json!(i.output_video_codec));
    put("outputAudioCodec", json!(i.output_audio_codec));
    put("resolution", json!(i.resolution));
    put("outputResolution", json!(or_res));
    put("sourceVideoBitrate", json!(i.source_video_bitrate));
    put("sourceAudioBitrate", json!(i.source_audio_bitrate));
    put("sourceVideoProfile", i.source_video_profile.clone());
    put("sourceVideoLevel", i.source_video_level.clone());
    put("sourcePixFmt", json!(i.source_pix_fmt));
    put("sourceAudioChannels", json!(i.source_audio_channels));
    put("sourceAudioSampleRate", i.source_audio_sample_rate.clone());
    put("sourceHasBFrames", i.source_has_b_frames.clone());
    put("sourceFormatName", json!(i.source_format_name));
    put("ffmpegExitCode", json!(i.ffmpeg_exit_code));
    put("ffmpegSignal", json!(i.ffmpeg_signal));
    put("ffmpegArgs", json!(i.ffmpeg_args));
    put("ebmlErrors", json!(i.ebml_errors));
    put("audioTrackCount", json!(i.audio_track_count.max(1)));
    put("audioOutputs", Value::Array(audio_outputs));
    put("logs", Value::Array(logs));
    Value::Object(m)
}

/// Everything: system figures, encoder detection, every session. `only_sid` limits the session
/// list to one session (the player's own buffer readout).
pub fn gather(app: &Arc<App>, only_sid: Option<&str>) -> Value {
    let now = now_ms();
    let cores = sysinfo::cpu_cores();
    let mut sessions: Vec<Arc<Session>> = app.sessions.lock().unwrap().values().cloned().collect();
    sessions.sort_by_key(|s| s.created_at);
    let active = sessions.len();

    let own_pid = std::process::id();
    let mut pids: Vec<u32> = sessions.iter().filter_map(|s| s.inner.lock().unwrap().ffmpeg_pid).collect();
    pids.push(own_pid);
    app.monitor.lock().unwrap().poll(&pids, now);
    poll_gpu(app);

    let gpu_sessions = {
        let g = app.gpu.lock().unwrap();
        pids.iter().filter(|p| g.processes.iter().any(|x| x.0 == **p)).count()
    };

    let list: Vec<Value> = sessions
        .iter()
        .filter(|s| only_sid.map(|o| o == s.sid).unwrap_or(true))
        .map(|s| {
            let i = s.inner.lock().unwrap();
            session_json(&s.sid, s, &i, app, cores, gpu_sessions)
        })
        .collect();

    // This process: CPU since the previous call, resident memory.
    let own = sysinfo::process(own_pid).unwrap_or_default();
    let self_cpu_pct = {
        let mut prev = app.prev_self_cpu.lock().unwrap();
        let wall_ms = now.saturating_sub(prev.1);
        let pct = if prev.0 > 0 && wall_ms > 0 { js_round(own.cpu_us.saturating_sub(prev.0) as f64 / (wall_ms as f64 * 1000.0 * cores as f64) * 1000.0) / 10.0 } else { 0.0 };
        *prev = (own.cpu_us, now);
        pct
    };
    let sys = sysinfo::system();
    let cpu_pct = app.monitor.lock().unwrap().system_cpu_pct(&sys);

    let ff_cpu: i64 = list.iter().map(|s| s["ffmpegCpuPct"].as_i64().unwrap_or(0)).sum();
    let ff_mem: i64 = list.iter().map(|s| s["ffmpegMemoryMB"].as_i64().unwrap_or(0)).sum();
    let enc = app.enc.read().map(|e| e.clone()).unwrap_or_default();
    let (hw_log, hw_bench, hw_dec) = {
        let h = app.hw.lock().unwrap();
        (Value::Array(h.log.clone()), Value::Object(h.benchmark.clone()), Value::Object(h.decode_benchmark.clone()))
    };
    let gpu = app.gpu.lock().unwrap().stats.clone();

    json!({
        "version": "v4.0",
        "platform": if cfg!(windows) { "win32" } else { std::env::consts::OS },
        "ffmpegVersion": app.ffmpeg_version,
        "videoEncoder": enc.h264.unwrap_or_else(|| "libx264".into()),
        "hevcEncoder": enc.hevc.unwrap_or_else(|| "libx265".into()),
        "activeSessions": active,
        "uptime": app.started.elapsed().as_secs_f64().round() as u64,
        "memoryMB": js_round(own.rss as f64 / 1048576.0) as i64,
        "heapMB": 0,
        "cpuPct": num(cpu_pct),
        "nodeCpuPct": num(self_cpu_pct),
        "ffmpegTotalCpuPct": ff_cpu,
        "ffmpegTotalMemoryMB": ff_mem,
        "loadAvg": sys.load.iter().map(|l| num(js_round(l * 100.0) / 100.0)).collect::<Vec<_>>(),
        "cpuCores": cores,
        "freeMem": js_round(sys.free_mem as f64 / 1048576.0) as i64,
        "totalMem": js_round(sys.total_mem as f64 / 1048576.0) as i64,
        "gpu": gpu,
        "hwDetectLog": hw_log,
        "hwBenchmark": hw_bench,
        "hwDecodeBenchmark": hw_dec,
        "timestamp": now,
        "sessions": list,
        "globalLogs": crate::util::global_logs_json(100),
    })
}
