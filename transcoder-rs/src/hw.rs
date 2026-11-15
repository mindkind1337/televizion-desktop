//! Hardware encoder detection (NVENC > AMF > QSV > VAAPI, per codec) with software fallback.
//!
//! Phase 1 (before the server listens, a few seconds at most): which encoders ffmpeg has, a
//! one-frame test encode of each candidate, and whether CUDA decoding works.
//! Phase 2 (background thread): the 1080p encode/decode benchmarks shown on the stats page.
//! The complete result is cached in the temp folder, keyed by ffmpeg build, GPUs and options;
//! set HWDETECT_FRESH=1 to ignore the cache.

use crate::config::Config;
use crate::ffargs::Encoders;
use crate::proc::{run, NULL_DEVICE};
use crate::util::now_ms;
use serde_json::{json, Map, Value};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
#[allow(unused_imports)]
use std::sync::RwLock;
use std::time::Duration;

#[derive(Default)]
pub struct HwInfo {
    pub log: Vec<Value>,
    pub benchmark: Map<String, Value>,
    pub decode_benchmark: Map<String, Value>,
}
pub type SharedHw = Arc<Mutex<HwInfo>>;

pub use crate::util::iso_now;

fn logm(hw: &SharedHw, msg: String) {
    crate::logi!("{}", msg);
    if let Ok(mut h) = hw.lock() {
        h.log.push(json!({ "t": iso_now(), "msg": msg }));
    }
}

/// `\bname\b` in ffmpeg's encoder list.
fn encoder_exists(list: &str, name: &str) -> bool {
    let word = |c: Option<char>| c.map(|c| c.is_ascii_alphanumeric() || c == '_').unwrap_or(false);
    let mut from = 0;
    while let Some(i) = list[from..].find(name) {
        let s = from + i;
        let e = s + name.len();
        if !word(list[..s].chars().next_back()) && !word(list[e..].chars().next()) {
            return true;
        }
        from = e;
    }
    false
}

struct Test {
    ok: bool,
    detail: String,
    ms: u64,
}

fn run_test(hw: &SharedHw, label: &str, args: &[String]) -> Test {
    logm(hw, format!("[hwaccel] {} cmd: ffmpeg {}", label, args.join(" ")));
    let t0 = now_ms();
    let a: Vec<&str> = args.iter().map(|x| x.as_str()).collect();
    match run("ffmpeg", &a, Duration::from_secs(15)) {
        Some(o) if o.ok => Test { ok: true, detail: String::new(), ms: now_ms() - t0 },
        Some(o) => {
            let d = if o.timed_out { "timed out".to_string() } else { o.stderr.replace('\n', " ") };
            Test { ok: false, detail: crate::util::prefix(&d, 200).to_string(), ms: now_ms() - t0 }
        }
        None => Test { ok: false, detail: "could not start ffmpeg".to_string(), ms: now_ms() - t0 },
    }
}

fn sv(a: &[&str]) -> Vec<String> {
    a.iter().map(|x| x.to_string()).collect()
}

fn test_encode(hw: &SharedHw, name: &str, extra: &[&str]) -> Test {
    // One black 256x128 frame (NVENC rejects very small frames) through the candidate encoder.
    let mut a = sv(&["-hide_banner", "-loglevel", "error", "-f", "lavfi", "-i", "color=black:s=256x128:d=0.04:r=25", "-frames:v", "1", "-c:v", name]);
    a.extend(sv(extra));
    a.extend(sv(&["-f", "null", NULL_DEVICE]));
    run_test(hw, "test", &a)
}

fn test_hw_decode(hw: &SharedHw, method: &str, extra: &[&str]) -> Test {
    let mut a = sv(&["-hide_banner", "-loglevel", "error", "-hwaccel", method]);
    a.extend(sv(extra));
    a.extend(sv(&["-f", "lavfi", "-i", "color=black:s=256x128:d=0.04:r=25", "-frames:v", "1", "-f", "null", NULL_DEVICE]));
    run_test(hw, "decode test", &a)
}

struct Candidate {
    name: &'static str,
    /// hwaccel settings that select it besides "auto".
    family: &'static str,
    test_args: &'static [&'static str],
}

const H264: [Candidate; 4] = [
    Candidate { name: "h264_nvenc", family: "nvenc", test_args: &["-gpu", "0"] },
    Candidate { name: "h264_amf", family: "amf", test_args: &[] },
    Candidate { name: "h264_qsv", family: "qsv", test_args: &[] },
    Candidate { name: "h264_vaapi", family: "vaapi", test_args: &[] },
];
const HEVC: [Candidate; 4] = [
    Candidate { name: "hevc_nvenc", family: "nvenc", test_args: &["-gpu", "0"] },
    Candidate { name: "hevc_amf", family: "amf", test_args: &[] },
    Candidate { name: "hevc_qsv", family: "qsv", test_args: &[] },
    Candidate { name: "hevc_vaapi", family: "vaapi", test_args: &[] },
];
const VAAPI_INPUT: [&str; 4] = ["-init_hw_device", "vaapi=va:/dev/dri/renderD128", "-filter_hw_device", "va"];

fn cache_file() -> PathBuf {
    std::env::temp_dir().join("televizion_hwdetect_cache_rs.json")
}

fn cache_key(cfg: &Config, ffmpeg_version_line: &str) -> String {
    let gpus = run("nvidia-smi", &["-L"], Duration::from_secs(3)).filter(|o| o.ok).map(|o| o.stdout.trim().to_string()).unwrap_or_else(|| "none".to_string());
    json!({
        "v": 1, "platform": std::env::consts::OS, "arch": std::env::consts::ARCH,
        "ffmpeg": ffmpeg_version_line, "gpus": gpus,
        "opts": { "hwaccel": cfg.hwaccel, "hwdecode": cfg.hwdecode, "forceNvidiaDecode": cfg.force_nvidia_decode }
    })
    .to_string()
}

fn strs(v: Option<&Value>) -> Vec<String> {
    v.and_then(|x| x.as_array()).map(|a| a.iter().filter_map(|s| s.as_str().map(|s| s.to_string())).collect()).unwrap_or_default()
}

/// Runs detection on a background thread so the server can listen at once. Until it finishes
/// `enc` holds no hardware encoder, i.e. sessions started meanwhile use libx264/libx265.
pub fn start_detection(cfg: Config, ffmpeg_version_line: String, enc: Arc<std::sync::RwLock<Encoders>>, hw: SharedHw) {
    let _ = std::thread::Builder::new().name("hw-detect".into()).spawn(move || {
        let found = detect(&cfg, &ffmpeg_version_line, &hw);
        if let Ok(mut e) = enc.write() {
            *e = found;
        }
    });
}

/// Detects encoders (or loads the cached result) and starts the background benchmarks.
pub fn detect(cfg: &Config, ffmpeg_version_line: &str, hw: &SharedHw) -> Encoders {
    let hw: SharedHw = hw.clone();
    let key = cache_key(cfg, ffmpeg_version_line);

    if std::env::var("HWDETECT_FRESH").ok().as_deref() != Some("1") {
        if let Some(c) = std::fs::read_to_string(cache_file()).ok().and_then(|t| serde_json::from_str::<Value>(&t).ok()) {
            if c.get("key").and_then(|k| k.as_str()) == Some(key.as_str()) && c.get("result").is_some() {
                let r = &c["result"];
                let enc = Encoders {
                    h264: r["encoders"]["h264"].as_str().map(|s| s.to_string()),
                    hevc: r["encoders"]["hevc"].as_str().map(|s| s.to_string()),
                    h264_input: strs(r["inputArgs"].get("h264")),
                    hevc_input: strs(r["inputArgs"].get("hevc")),
                };
                let age_min = (now_ms().saturating_sub(c["ts"].as_u64().unwrap_or(0)) + 30000) / 60000;
                {
                    let mut h = hw.lock().unwrap();
                    h.log = r["log"].as_array().cloned().unwrap_or_default();
                    h.benchmark = r["benchmark"].as_object().cloned().unwrap_or_default();
                    h.decode_benchmark = r["decodeBenchmark"].as_object().cloned().unwrap_or_default();
                }
                logm(&hw, format!("[hwaccel] loaded cached detection (age {}m; set HWDETECT_FRESH=1 to re-run benchmarks)", age_min));
                return enc;
            }
        }
    }

    let start = now_ms();
    let mut enc = Encoders::default();
    logm(&hw, format!("[hwaccel] config: hwaccel={}, hwdecode={}, forceNvidiaDecode={}", cfg.hwaccel, cfg.hwdecode, cfg.force_nvidia_decode));
    logm(&hw, format!("[hwaccel] OS: {}/{}, cpus: {}", std::env::consts::OS, std::env::consts::ARCH, std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1)));

    if cfg.hwaccel == "off" {
        logm(&hw, "[hwaccel] disabled in config — using CPU encoders".to_string());
        return enc;
    }

    let list = match run("ffmpeg", &["-hide_banner", "-encoders"], Duration::from_secs(5)) {
        Some(o) if o.ok => o.stdout,
        _ => {
            logm(&hw, "[hwaccel] failed to query ffmpeg encoders — using CPU".to_string());
            return enc;
        }
    };
    let gpu_lines: Vec<&str> = list.lines().map(|l| l.trim()).filter(|l| ["nvenc", "amf", "qsv", "vaapi", "videotoolbox"].iter().any(|k| l.to_ascii_lowercase().contains(k))).collect();
    logm(&hw, format!("[hwaccel] ffmpeg GPU encoders found: {}", if gpu_lines.is_empty() { "none".to_string() } else { format!("\n  {}", gpu_lines.join("\n  ")) }));

    match run("nvidia-smi", &["-L"], Duration::from_secs(3)) {
        Some(o) if o.ok => logm(&hw, format!("[hwaccel] nvidia-smi -L: {}", o.stdout.trim().lines().collect::<Vec<_>>().join(", "))),
        _ => logm(&hw, "[hwaccel] nvidia-smi not available or no NVIDIA GPU".to_string()),
    }
    if !cfg!(windows) && std::path::Path::new("/dev/dri/renderD128").exists() {
        logm(&hw, "[hwaccel] /dev/dri/renderD128 present".to_string());
    }

    let selected = |family: &str| cfg.hwaccel == "auto" || cfg.hwaccel == family;
    let force = cfg.force_nvidia_decode;
    let decode_extra: &[&str] = if force { &["-hwaccel_device", "0"] } else { &[] };

    // CUDA decoding for NVENC, only when a test decode works.
    let mut nvenc_input: Vec<String> = Vec::new();
    // Only worth testing when ffmpeg has an NVENC encoder at all.
    let has_nvenc = encoder_exists(&list, "h264_nvenc") || encoder_exists(&list, "hevc_nvenc");
    if cfg.hwdecode && selected("nvenc") && has_nvenc {
        let t = test_hw_decode(&hw, "cuda", decode_extra);
        if t.ok {
            nvenc_input = sv(&["-hwaccel", "cuda"]);
            if force {
                nvenc_input.extend(sv(&["-hwaccel_device", "0"]));
            }
            logm(&hw, format!("[hwaccel] CUDA decode test OK ✓ ({}ms) — enabled ({})", t.ms, nvenc_input.join(" ")));
        } else {
            logm(&hw, format!("[hwaccel] CUDA decode test FAILED ({}ms) — {}", t.ms, t.detail));
            logm(&hw, "[hwaccel] falling back to CPU decoding (NVENC encoding may still be available)".to_string());
        }
    } else if !selected("nvenc") {
        logm(&hw, format!("[hwaccel] skipping CUDA decode test — NVENC not in scope for hwaccel={}", cfg.hwaccel));
    } else if !has_nvenc {
        logm(&hw, "[hwaccel] skipping CUDA decode test — ffmpeg has no NVENC encoder".to_string());
    } else {
        logm(&hw, "[hwaccel] hardware decoding disabled in config".to_string());
    }

    let mut tested: Vec<(&'static str, bool)> = Vec::new();
    for (codec, cands) in [("h264", &H264), ("hevc", &HEVC)] {
        let mut chosen: Option<&Candidate> = None;
        for c in cands.iter() {
            if !selected(c.family) {
                continue;
            }
            if !encoder_exists(&list, c.name) {
                logm(&hw, format!("[hwaccel] {} not found in ffmpeg — skipped", c.name));
                continue;
            }
            logm(&hw, format!("[hwaccel] testing {}...", c.name));
            let t = test_encode(&hw, c.name, c.test_args);
            tested.push((c.name, t.ok));
            if !t.ok {
                logm(&hw, format!("[hwaccel] {} FAILED ({}ms) — {}", c.name, t.ms, t.detail));
                continue;
            }
            logm(&hw, format!("[hwaccel] {} test encode OK ✓ ({}ms) — enabled for {}", c.name, t.ms, codec.to_ascii_uppercase()));
            chosen = Some(c);
            break;
        }
        match chosen {
            Some(c) => {
                let input = match c.family {
                    "nvenc" => nvenc_input.clone(),
                    "vaapi" => sv(&VAAPI_INPUT),
                    _ => vec![],
                };
                if codec == "h264" {
                    enc.h264 = Some(c.name.to_string());
                    enc.h264_input = input;
                } else {
                    enc.hevc = Some(c.name.to_string());
                    enc.hevc_input = input;
                }
            }
            None => logm(&hw, format!("[hwaccel] no GPU encoder available for {} — using CPU", codec.to_ascii_uppercase())),
        }
    }
    logm(
        &hw,
        format!(
            "[hwaccel] === RESULT: H.264={}, HEVC={} (detected in {}ms; benchmarks continue in the background) ===",
            enc.h264.clone().unwrap_or_else(|| "libx264 (CPU)".into()),
            enc.hevc.clone().unwrap_or_else(|| "libx265 (CPU)".into()),
            now_ms() - start
        ),
    );

    // Phase 2 in the background; the cache is written when it completes.
    {
        let hw2 = hw.clone();
        let enc2 = enc.clone();
        let force = cfg.force_nvidia_decode;
        let _ = std::thread::Builder::new().name("hw-bench".into()).spawn(move || {
            benchmarks(&hw2, &enc2, &list, &tested, force);
            let h = hw2.lock().unwrap();
            let result = json!({
                "encoders": { "h264": enc2.h264, "hevc": enc2.hevc },
                "inputArgs": { "h264": enc2.h264_input, "hevc": enc2.hevc_input },
                "benchmark": h.benchmark, "decodeBenchmark": h.decode_benchmark, "log": h.log,
            });
            if let Err(e) = std::fs::write(cache_file(), json!({ "key": key, "ts": now_ms(), "result": result }).to_string()) {
                crate::logi!("[hwaccel] could not write detection cache: {}", e);
            }
        });
    }
    enc
}

const BENCH_SRC: &str = "testsrc2=s=1920x1080:d=3:r=30,noise=c0s=40:allf=t";
const BENCH_FRAMES: u32 = 90;

fn number_after(text: &str, key: &str) -> Option<f64> {
    let i = text.find(key)? + key.len();
    let rest = text[i..].trim_start();
    let end = rest.find(|c: char| !(c.is_ascii_digit() || c == '.')).unwrap_or(rest.len());
    rest[..end].parse::<f64>().ok()
}

/// Reads ffmpeg's `-benchmark` output: speed, user/system/real time, peak memory.
pub fn parse_bench(text: &str, wall_ms: u64) -> Option<Value> {
    // speed=  12.3x (first occurrence followed by an 'x')
    let speed = {
        let mut found = None;
        let mut from = 0;
        while let Some(i) = text[from..].find("speed=") {
            let s = from + i + 6;
            let rest = text[s..].trim_start();
            let end = rest.find(|c: char| !(c.is_ascii_digit() || c == '.')).unwrap_or(rest.len());
            if end > 0 && rest[end..].starts_with('x') {
                found = rest[..end].parse::<f64>().ok();
                break;
            }
            from = s;
        }
        found
    };
    let bench = text.find("bench:").map(|i| &text[i..]);
    let (utime, stime, rtime) = match bench {
        Some(b) => (number_after(b, "utime="), number_after(b, "stime="), number_after(b, "rtime=")),
        None => (None, None, None),
    };
    let has_bench = utime.is_some() && stime.is_some() && rtime.is_some();
    if speed.is_none() && !has_bench {
        return None;
    }
    let rss = number_after(text, "maxrss=").map(|k| (k / 1024.0).round());
    Some(json!({
        "speed": speed, "utime": utime, "stime": stime, "rtime": rtime, "maxrssMB": rss.map(crate::util::num),
        "wallMs": wall_ms, "frames": BENCH_FRAMES, "resolution": "1920x1080"
    }))
}

fn bench(hw: &SharedHw, what: &str, label: &str, args: &[String]) -> Option<Value> {
    logm(hw, format!("[hwaccel] {} cmd: ffmpeg {}", what, args.join(" ")));
    let t0 = now_ms();
    let a: Vec<&str> = args.iter().map(|x| x.as_str()).collect();
    let o = run("ffmpeg", &a, Duration::from_secs(30))?;
    let wall = now_ms() - t0;
    let text = format!("{}{}", o.stderr, o.stdout);
    match parse_bench(&text, wall) {
        Some(p) => {
            logm(hw, format!("[hwaccel] {} {}: speed={}x, rtime={}s, maxrss={}MB, wall={}ms", what, label, p["speed"], p["rtime"], p["maxrssMB"], wall));
            Some(p)
        }
        None => {
            logm(hw, format!("[hwaccel] {} {} FAILED: no benchmark data in output", what, label));
            None
        }
    }
}

fn benchmarks(hw: &SharedHw, enc: &Encoders, list: &str, tested: &[(&'static str, bool)], force_nvidia: bool) {
    // Encoders to time: the selected ones, the other working hardware ones, then software.
    let mut targets: Vec<(String, &'static str, Vec<String>, &'static str)> = Vec::new();
    let all: Vec<(&'static str, &Candidate)> = H264.iter().map(|c| ("h264", c)).chain(HEVC.iter().map(|c| ("hevc", c))).collect();
    for (codec, sel) in [("h264", &enc.h264), ("hevc", &enc.hevc)] {
        if let Some(name) = sel {
            let extra = all.iter().find(|(_, c)| c.name == name).map(|(_, c)| sv(c.test_args)).unwrap_or_default();
            targets.push((name.clone(), codec, extra, "selected"));
        }
    }
    for (codec, c) in &all {
        if targets.iter().any(|t| t.0 == c.name) {
            continue;
        }
        if !encoder_exists(list, c.name) {
            continue;
        }
        let ok = match tested.iter().find(|(n, _)| *n == c.name) {
            Some((_, ok)) => *ok,
            None => {
                logm(hw, format!("[hwaccel] testing {} for bench...", c.name));
                test_encode(hw, c.name, c.test_args).ok
            }
        };
        if ok {
            logm(hw, format!("[hwaccel] {} available ✓ — adding to bench", c.name));
            targets.push((c.name.to_string(), codec, sv(c.test_args), "available"));
        }
    }
    for (name, codec) in [("libx264", "h264"), ("libx265", "hevc")] {
        if encoder_exists(list, name) {
            targets.push((name.to_string(), codec, sv(&["-preset", "ultrafast"]), "cpu"));
        }
    }
    for (name, codec, extra, label) in &targets {
        logm(hw, format!("[hwaccel] benchmarking {} at 1080p ({} frames, noisy testsrc2)...", name, BENCH_FRAMES));
        let mut a = sv(&["-hide_banner", "-benchmark", "-f", "lavfi", "-i", BENCH_SRC, "-frames:v", &BENCH_FRAMES.to_string(), "-c:v", name]);
        a.extend(extra.iter().cloned());
        a.extend(sv(&["-f", "null", NULL_DEVICE]));
        if let Some(mut p) = bench(hw, "bench", name, &a) {
            p["codec"] = json!(codec);
            p["label"] = json!(label);
            hw.lock().unwrap().benchmark.insert(name.clone(), p);
        }
    }

    // Decoders: software always, then each hardware method whose one-frame test works.
    let tmp = std::env::temp_dir().join(format!("hwdetect_bench_{}.mp4", std::process::id()));
    let tmp_s = tmp.to_string_lossy().to_string();
    logm(hw, "[hwaccel] creating temp H.264 file for decode benchmark...".to_string());
    let made = run(
        "ffmpeg",
        &["-hide_banner", "-loglevel", "error", "-y", "-f", "lavfi", "-i", BENCH_SRC, "-frames:v", &BENCH_FRAMES.to_string(), "-c:v", "libx264", "-preset", "ultrafast", "-pix_fmt", "yuv420p", &tmp_s],
        Duration::from_secs(30),
    );
    if made.map(|o| o.ok).unwrap_or(false) {
        let dec = |method: Option<&str>, extra: &[&str]| -> Option<Value> {
            let mut a = sv(&["-hide_banner", "-benchmark"]);
            if let Some(m) = method {
                a.extend(sv(&["-hwaccel", m]));
                a.extend(sv(extra));
            }
            a.extend(sv(&["-i", &tmp_s, "-f", "null", NULL_DEVICE]));
            bench(hw, "decode bench", method.unwrap_or("software"), &a)
        };
        logm(hw, "[hwaccel] benchmarking decode: software (CPU)...".to_string());
        if let Some(mut p) = dec(None, &[]) {
            p["label"] = json!("software");
            hw.lock().unwrap().decode_benchmark.insert("software".into(), p);
        }
        let cuda_extra: &[&str] = if force_nvidia { &["-hwaccel_device", "0"] } else { &[] };
        let methods: [(&str, &str, &[&str]); 6] = [("cuda", "NVIDIA CUDA", cuda_extra), ("qsv", "Intel QSV", &[]), ("d3d11va", "D3D11VA", &[]), ("dxva2", "DXVA2", &[]), ("vaapi", "VA-API", &[]), ("vulkan", "Vulkan", &[])];
        for (name, label, extra) in methods {
            if test_hw_decode(hw, name, extra).ok {
                logm(hw, format!("[hwaccel] benchmarking decode: {}...", label));
                if let Some(mut p) = dec(Some(name), extra) {
                    p["label"] = json!(label);
                    hw.lock().unwrap().decode_benchmark.insert(name.to_string(), p);
                }
            } else {
                logm(hw, format!("[hwaccel] decode {} not available — skipped", name));
            }
        }
    } else {
        logm(hw, "[hwaccel] decode temp file creation FAILED".to_string());
    }
    let _ = std::fs::remove_file(&tmp);
    logm(hw, "[hwaccel] benchmarks done".to_string());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encoder_names_match_whole_words() {
        let list = " V....D h264_nvenc           NVIDIA NVENC H.264 encoder (codec h264)\n V....D libx264              libx264 H.264\n V..... libx264rgb           libx264 RGB";
        assert!(encoder_exists(list, "h264_nvenc"));
        assert!(encoder_exists(list, "libx264"));
        assert!(!encoder_exists(list, "hevc_nvenc"));
        assert!(!encoder_exists(" V..... libx264rgb x", "libx264"));
        assert!(!encoder_exists(" V..... xh264_qsv x", "h264_qsv"));
    }

    #[test]
    fn benchmark_output_is_parsed() {
        let text = "frame=   90 fps= 45 q=-0.0 Lsize=N/A time=00:00:02.96 bitrate=N/A speed=1.48x\nbench: utime=5.210s stime=0.330s rtime=2.030s\nbench: maxrss=204800KiB\n";
        let p = parse_bench(text, 2100).unwrap();
        assert_eq!(p["speed"], json!(1.48));
        assert_eq!(p["utime"], json!(5.21));
        assert_eq!(p["rtime"], json!(2.03));
        assert_eq!(p["maxrssMB"], json!(200));
        assert_eq!(p["frames"], json!(90));
        assert!(parse_bench("nothing useful", 1).is_none());
        let p = parse_bench("speed= 0.5x", 1).unwrap();
        assert_eq!(p["speed"], json!(0.5));
        assert!(p["utime"].is_null());
    }

    #[test]
    fn iso_timestamp_shape() {
        let t = iso_now();
        assert_eq!(t.len(), 24);
        assert!(t.ends_with('Z') && t.as_bytes()[10] == b'T');
        assert!(t.starts_with("20"));
    }
}
