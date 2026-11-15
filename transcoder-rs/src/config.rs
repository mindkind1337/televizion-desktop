//! Settings. The defaults are the values of the Node service's config.json (config.default.json,
//! compiled in). An optional config.json next to the executable, or the file named by TVZ_CONFIG,
//! overrides them key by key. Environment: TVZ_PORT, TVZ_STREAMS_DIR.

use serde_json::Value;
use std::path::PathBuf;

const DEFAULTS: &str = include_str!("../config.default.json");

#[derive(Clone, Debug)]
pub struct Config {
    pub port: u16,
    pub user_agent: String,
    // ffmpeg
    pub threads: i64,
    pub preset: String,
    /// None when absent from the config: 23 for H.264, 28 for HEVC.
    pub crf: Option<i64>,
    pub max_height: i64,
    pub gop_size: i64,
    pub keyint_min: i64,
    pub audio_bitrate: String,
    pub audio_sample_rate: i64,
    pub audio_channels: i64,
    /// Optional forced output codec ("h264" or "hevc").
    pub codec: Option<String>,
    /// "auto" | "nvenc" | "amf" | "qsv" | "vaapi" | "off".
    pub hwaccel: String,
    pub hwdecode: bool,
    pub force_nvidia_decode: bool,
    // segments
    pub seg_duration: f64,
    pub keep_behind_sec: f64,
    pub throttle_pause_sec: f64,
    pub throttle_resume_sec: f64,
    pub playlist_window_sec: f64,
    pub buffer_window_sec: f64,
    // limits
    pub dl_max_retries: u32,
    pub max_sessions: usize,
    // session
    pub ttl_ms: u64,
    pub startup_timeout_transcode: u64,
    pub startup_timeout_copy: u64,
    pub seek_timeout_transcode: u64,
    pub seek_timeout_copy: u64,
    // probe sizes for the ffmpeg input
    pub start_analyze_duration: String,
    pub start_probe_size: String,
    pub seek_analyze_duration: String,
    pub seek_probe_size: String,
    // where segments are kept
    pub streams_dir: PathBuf,
}

fn merge(base: &mut Value, over: &Value) {
    match (base, over) {
        (Value::Object(b), Value::Object(o)) => {
            for (k, v) in o {
                if k.starts_with('_') {
                    continue;
                }
                match b.get_mut(k) {
                    Some(bv) if bv.is_object() && v.is_object() => merge(bv, v),
                    _ => {
                        b.insert(k.clone(), v.clone());
                    }
                }
            }
        }
        (b, o) => *b = o.clone(),
    }
}

// `x || default` on numbers and strings (0 and "" fall back).
fn n_or(v: Option<&Value>, d: f64) -> f64 {
    match v.and_then(|x| x.as_f64()) {
        Some(f) if f != 0.0 && f.is_finite() => f,
        _ => d,
    }
}
fn s_or(v: Option<&Value>, d: &str) -> String {
    match v {
        Some(Value::String(s)) if !s.is_empty() => s.clone(),
        Some(Value::Number(n)) => n.to_string(),
        _ => d.to_string(),
    }
}

impl Config {
    pub fn from_value(c: &Value) -> Config {
        let ff = c.get("ffmpeg");
        let g = |sec: Option<&Value>, k: &str| -> Option<Value> { sec.and_then(|s| s.get(k)).cloned() };
        let seg = c.get("segments");
        let ses = c.get("session");
        let pr = c.get("probe");

        let hwaccel = match g(ff, "hwaccel") {
            Some(Value::Bool(false)) => "off".to_string(),
            Some(Value::Bool(true)) => "auto".to_string(),
            Some(Value::String(s)) if s == "none" => "off".to_string(),
            Some(Value::String(s)) if !s.is_empty() => s,
            _ => "auto".to_string(),
        };
        let codec = match g(ff, "codec") {
            Some(Value::String(s)) if s == "h264" || s == "hevc" => Some(s),
            _ => None,
        };
        let analyze = s_or(g(pr, "analyzeDuration").as_ref(), "5000000");
        let probesize = s_or(g(pr, "probeSize").as_ref(), "5000000");

        Config {
            port: n_or(c.get("port"), 3002.0) as u16,
            user_agent: s_or(c.get("userAgent"), "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36"),
            threads: g(ff, "threads").and_then(|v| v.as_i64()).unwrap_or(0),
            preset: s_or(g(ff, "preset").as_ref(), "veryfast"),
            crf: g(ff, "crf").and_then(|v| v.as_i64()),
            max_height: n_or(g(ff, "maxHeight").as_ref(), 0.0) as i64,
            gop_size: n_or(g(ff, "gopSize").as_ref(), 72.0) as i64,
            keyint_min: n_or(g(ff, "keyintMin").as_ref(), 24.0) as i64,
            audio_bitrate: s_or(g(ff, "audioBitrate").as_ref(), "128k"),
            audio_sample_rate: n_or(g(ff, "audioSampleRate").as_ref(), 48000.0) as i64,
            audio_channels: n_or(g(ff, "audioChannels").as_ref(), 2.0) as i64,
            codec,
            hwaccel,
            hwdecode: !matches!(g(ff, "hwdecode"), Some(Value::Bool(false))),
            force_nvidia_decode: matches!(g(ff, "forceNvidiaDecode"), Some(Value::Bool(true))),
            seg_duration: n_or(g(seg, "duration").as_ref(), 3.0),
            keep_behind_sec: n_or(g(seg, "keepBehindSec").as_ref(), 60.0),
            throttle_pause_sec: n_or(g(seg, "throttlePauseSec").as_ref(), 60.0),
            throttle_resume_sec: n_or(g(seg, "throttleResumeSec").as_ref(), 30.0),
            playlist_window_sec: n_or(g(seg, "playlistWindowSec").as_ref(), 60.0),
            buffer_window_sec: n_or(g(seg, "bufferWindowSec").as_ref(), 30.0),
            dl_max_retries: n_or(c.get("downloader").and_then(|d| d.get("maxRetries")), 5.0) as u32,
            max_sessions: n_or(c.get("limits").and_then(|d| d.get("maxSessions")), 8.0) as usize,
            ttl_ms: (n_or(g(ses, "ttlMinutes").as_ref(), 5.0) * 60000.0) as u64,
            startup_timeout_transcode: n_or(g(ses, "startupTimeoutTranscode").as_ref(), 45000.0) as u64,
            startup_timeout_copy: n_or(g(ses, "startupTimeoutCopy").as_ref(), 15000.0) as u64,
            seek_timeout_transcode: n_or(g(ses, "seekTimeoutTranscode").as_ref(), 30000.0) as u64,
            seek_timeout_copy: n_or(g(ses, "seekTimeoutCopy").as_ref(), 8000.0) as u64,
            start_analyze_duration: s_or(g(pr, "startAnalyzeDuration").as_ref(), &analyze),
            start_probe_size: s_or(g(pr, "startProbeSize").as_ref(), &probesize),
            seek_analyze_duration: s_or(g(pr, "seekAnalyzeDuration").as_ref(), "5000000"),
            seek_probe_size: s_or(g(pr, "seekProbeSize").as_ref(), "5000000"),
            streams_dir: std::env::temp_dir().join("televizion-streams"),
        }
    }

    /// The compiled-in defaults only (what the tests use).
    pub fn defaults() -> Config {
        let v: Value = serde_json::from_str(DEFAULTS).expect("config.default.json is valid JSON");
        Config::from_value(&v)
    }

    /// Defaults, then the optional override file, then the environment.
    pub fn load() -> Config {
        let mut v: Value = serde_json::from_str(DEFAULTS).expect("config.default.json is valid JSON");
        let file = std::env::var_os("TVZ_CONFIG").map(PathBuf::from).or_else(|| {
            let p = std::env::current_exe().ok()?.parent()?.join("config.json");
            p.exists().then_some(p)
        });
        if let Some(path) = file {
            match std::fs::read_to_string(&path).map_err(|e| e.to_string()).and_then(|t| serde_json::from_str::<Value>(&t).map_err(|e| e.to_string())) {
                Ok(over) => {
                    merge(&mut v, &over);
                    crate::logi!("[config] loaded {}", path.display());
                }
                Err(e) => crate::logw!("[config] failed to load {}, using defaults: {}", path.display(), e),
            }
        }
        let mut cfg = Config::from_value(&v);
        if let Some(p) = std::env::var("TVZ_PORT").ok().and_then(|p| p.parse::<u16>().ok()) {
            cfg.port = p;
        }
        if let Some(d) = std::env::var_os("TVZ_STREAMS_DIR").filter(|d| !d.is_empty()) {
            cfg.streams_dir = PathBuf::from(d);
        }
        cfg
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_the_node_config() {
        let c = Config::defaults();
        assert_eq!(c.port, 3002);
        assert_eq!(c.preset, "veryfast");
        assert_eq!(c.crf, Some(23));
        assert_eq!(c.max_height, 1080);
        assert_eq!(c.seg_duration, 3.0);
        assert_eq!(c.ttl_ms, 600_000);
        assert_eq!(c.startup_timeout_copy, 15000);
        assert_eq!(c.seek_timeout_copy, 8000);
        assert_eq!(c.start_probe_size, "5000000");
        assert_eq!(c.hwaccel, "auto");
        assert!(c.force_nvidia_decode);
    }

    #[test]
    fn override_merges_by_key() {
        let mut v: Value = serde_json::from_str(DEFAULTS).unwrap();
        merge(&mut v, &serde_json::json!({ "ffmpeg": { "maxHeight": 720, "hwaccel": false }, "port": 4000 }));
        let c = Config::from_value(&v);
        assert_eq!(c.max_height, 720);
        assert_eq!(c.hwaccel, "off");
        assert_eq!(c.port, 4000);
        assert_eq!(c.preset, "veryfast");
    }
}
