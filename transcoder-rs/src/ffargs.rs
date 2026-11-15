//! ffmpeg / ffprobe argument lists. Pure functions: no I/O, no globals, so the same inputs can be
//! fed to the Node service's code and compared (tests/compare_args.js).

use crate::config::Config;
use crate::util::js_num;

/// Hardware encoders found at startup, per output codec (None = software).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Encoders {
    pub h264: Option<String>,
    pub hevc: Option<String>,
    /// Arguments placed before `-i` (hardware decoding), per output codec.
    pub h264_input: Vec<String>,
    pub hevc_input: Vec<String>,
}

/// What a session knows when it starts ffmpeg.
#[derive(Clone, Debug, Default)]
pub struct SessionSpec {
    /// "hls-copy" | "transcode-video-copy-audio" | "h264-aac" | "copy-video-transcode-audio" | "" …
    pub strategy: String,
    pub audio_track: i64,
    /// Requested audio bitrate for this session ("128k"); empty falls back to the config.
    pub audio_bitrate: String,
    pub audio_track_count: i64,
    pub source_audio_codec: String,
    pub source_audio_channels: i64,
    pub source_video_codec: String,
    /// Output codec chosen by a previous run of the same session (seek keeps it).
    pub prev_output_video_codec: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Built {
    pub args: Vec<String>,
    pub video_encoder: String,
    pub output_video_codec: String,
    pub output_audio_codec: String,
    /// Number of separate audio-only outputs (0 unless the source has several audio tracks).
    pub extra_audio_count: usize,
    /// Warnings for the session log (strategy overrides).
    pub warnings: Vec<String>,
}

fn s(v: &str) -> String {
    v.to_string()
}

fn push(args: &mut Vec<String>, items: &[&str]) {
    args.extend(items.iter().map(|x| x.to_string()));
}

/// Audio codecs that cannot be copied into fMP4 for the browser's MediaSource.
pub fn incompatible_audio(codec: &str) -> bool {
    let c = codec.to_ascii_lowercase();
    ["ac3", "eac3", "dts", "dts_hd", "truehd", "pcm_", "flac", "vorbis", "opus", "wmav", "cook", "atrac", "mlp", "tak", "mp3", "mp2"].iter().any(|p| c.starts_with(p))
}

fn safe_audio(codec: &str) -> bool {
    codec.eq_ignore_ascii_case("aac")
}

/// Input half: everything up to and including `-i pipe:0`.
///
/// `has_header`: a cached container header is written to stdin first (byte-range seek), in which
/// case no `-ss` is used. `data_offset`: where the download starts in the file.
pub fn input_args(cfg: &Config, seek_time: f64, has_header: bool, data_offset: u64) -> Vec<String> {
    let mut a = vec![s("-hide_banner"), s("-loglevel"), s("error"), s("-threads"), cfg.threads.to_string()];
    if seek_time > 0.0 && !has_header {
        a.push(s("-ss"));
        a.push(js_num(seek_time));
    }
    let (analyze, probe) = if has_header && data_offset > 0 {
        (cfg.seek_analyze_duration.clone(), cfg.seek_probe_size.clone())
    } else {
        (cfg.start_analyze_duration.clone(), cfg.start_probe_size.clone())
    };
    a.extend([s("-analyzeduration"), analyze, s("-probesize"), probe, s("-err_detect"), s("ignore_err"), s("-i"), s("pipe:0")]);
    a
}

fn frag_args(cfg: &Config) -> [String; 6] {
    [
        s("-movflags"),
        s("frag_keyframe+empty_moov+default_base_moof"),
        s("-frag_duration"),
        js_num(cfg.seg_duration * 1_000_000.0),
        s("-min_frag_duration"),
        js_num((cfg.seg_duration * 0.5 * 1_000_000.0).floor()),
    ]
}

/// Complete argument list: `input` (from `input_args`) plus mapping, codecs and outputs.
///
/// `audio_targets`: where each separate audio output is written when the source has several
/// audio tracks. None = "pipe:3", "pipe:4", … as the Node service does; the Rust service passes
/// loopback TCP URLs instead (extra pipes cannot be inherited portably).
pub fn build(cfg: &Config, enc: &Encoders, spec: &SessionSpec, input: Vec<String>, audio_targets: Option<&[String]>) -> Built {
    let mut args = input;
    let mut warnings = Vec::new();

    let preset = cfg.preset.as_str();
    let gop = cfg.gop_size.to_string();
    let keyint = cfg.keyint_min.to_string();
    let a_bitrate = if !spec.audio_bitrate.is_empty() { spec.audio_bitrate.clone() } else { cfg.audio_bitrate.clone() };
    let a_rate = cfg.audio_sample_rate.to_string();
    let a_ch = cfg.audio_channels.to_string();
    let strategy = spec.strategy.as_str();

    // Output codec: H.264 unless the strategy (or a previous run) says HEVC, or the config forces one.
    let mut requested = "h264";
    if strategy.to_ascii_lowercase().contains("hevc") || spec.prev_output_video_codec.to_ascii_lowercase().contains("hevc") {
        requested = "hevc";
    }
    if let Some(c) = cfg.codec.as_deref() {
        requested = if c == "hevc" { "hevc" } else { "h264" };
    }
    let hevc = requested == "hevc";
    let v_encoder = if hevc { enc.hevc.clone().unwrap_or_else(|| s("libx265")) } else { enc.h264.clone().unwrap_or_else(|| s("libx264")) };
    let crf = cfg.crf.unwrap_or(if hevc { 28 } else { 23 }).to_string();
    let is_vaapi = v_encoder.ends_with("_vaapi");

    // Hardware input arguments go right before -i.
    let hw_in = if hevc { &enc.hevc_input } else { &enc.h264_input };
    if !hw_in.is_empty() {
        match args.iter().position(|x| x == "-i") {
            Some(i) => {
                args.splice(i..i, hw_in.iter().cloned());
            }
            None => {
                args.splice(0..0, hw_in.iter().cloned());
            }
        }
    }

    // Quality arguments per encoder family.
    let v_quality: Vec<String> = if v_encoder.ends_with("_nvenc") {
        let p = match preset {
            "ultrafast" => "p1",
            "superfast" => "p2",
            "veryfast" => "p3",
            "faster" => "p4",
            "fast" | "medium" => "p5",
            "slow" => "p6",
            "slower" => "p7",
            _ => "p4",
        };
        vec![s("-preset"), s(p), s("-rc"), s("vbr"), s("-cq"), crf.clone(), s("-b:v"), s("0"), s("-gpu"), s("0")]
    } else if v_encoder.ends_with("_amf") {
        let q = match preset {
            "ultrafast" | "superfast" | "veryfast" | "faster" => "speed",
            "fast" | "medium" => "balanced",
            "slow" | "slower" => "quality",
            _ => "speed",
        };
        vec![s("-quality"), s(q), s("-rc"), s("cqp"), s("-qp_i"), crf.clone(), s("-qp_p"), crf.clone()]
    } else if v_encoder.ends_with("_qsv") {
        vec![s("-preset"), s(preset), s("-global_quality"), crf.clone()]
    } else if is_vaapi {
        vec![s("-qp"), crf.clone()]
    } else {
        vec![s("-preset"), s(preset), s("-crf"), crf.clone()]
    };

    push(&mut args, &["-map", "0:v:0?"]);
    if spec.audio_track_count <= 1 {
        args.push(s("-map"));
        args.push(format!("0:a:{}?", spec.audio_track));
    }
    push(&mut args, &["-map", "-0:s"]);

    // Downscale filter (vaapi needs hwupload).
    let needs_scale = cfg.max_height > 0 && strategy != "hls-copy";
    let scale: Vec<String> = if is_vaapi && needs_scale {
        vec![s("-vf"), format!("format=nv12,hwupload,scale_vaapi=w=-2:h='min({},ih)'", cfg.max_height)]
    } else if is_vaapi {
        vec![s("-vf"), s("format=nv12,hwupload")]
    } else if needs_scale {
        vec![s("-vf"), format!("scale=-2:'min({},ih)'", cfg.max_height)]
    } else {
        vec![]
    };

    let a_codec = spec.source_audio_codec.as_str();
    let channels = spec.source_audio_channels;
    let needs_audio_transcode = incompatible_audio(a_codec)
        || (!safe_audio(a_codec) && strategy == "transcode-video-copy-audio")
        || (channels > 2 && (strategy == "transcode-video-copy-audio" || strategy == "copy-video-transcode-audio"));
    let multi = spec.audio_track_count > 1;

    let src_v = if spec.source_video_codec.is_empty() { s("copy") } else { spec.source_video_codec.clone() };
    let src_a = if spec.source_audio_codec.is_empty() { s("copy") } else { spec.source_audio_codec.clone() };

    let aac: Vec<String> = vec![s("-c:a"), s("aac"), s("-ac"), a_ch.clone(), s("-b:a"), a_bitrate.clone(), s("-ar"), a_rate.clone()];
    let encode_video = |args: &mut Vec<String>| {
        args.extend(scale.iter().cloned());
        args.push(s("-c:v"));
        args.push(v_encoder.clone());
        args.extend(v_quality.iter().cloned());
        args.extend([s("-pix_fmt"), s("yuv420p"), s("-g"), gop.clone(), s("-keyint_min"), keyint.clone()]);
    };
    let copy_video_aac = |args: &mut Vec<String>| {
        if multi {
            push(args, &["-c:v", "copy", "-an"]);
        } else {
            push(args, &["-c:v", "copy"]);
            args.extend(aac.iter().cloned());
        }
    };

    let (out_v, out_a): (String, String);
    if strategy == "hls-copy" && channels > 2 {
        // Copy video, bring multichannel audio down to stereo AAC.
        warnings.push(format!("{}ch audio in copy-mode → forcing stereo AAC transcode", channels));
        copy_video_aac(&mut args);
        out_v = src_v;
        out_a = s("aac");
    } else if strategy == "hls-copy" {
        if multi {
            push(&mut args, &["-c:v", "copy", "-an"]);
        } else {
            push(&mut args, &["-c", "copy"]);
        }
        out_v = src_v;
        out_a = src_a;
    } else if strategy == "transcode-video-copy-audio" && needs_audio_transcode {
        // The audio cannot go into fMP4 as is: transcode both.
        let reason = if channels > 2 { format!("{}ch audio", channels) } else { format!("codec \"{}\"", spec.source_audio_codec) };
        warnings.push(format!("{} incompatible with fMP4 — forcing AAC transcode", reason));
        encode_video(&mut args);
        if multi {
            args.push(s("-an"));
        } else {
            args.extend(aac.iter().cloned());
        }
        out_v = s(requested);
        out_a = s("aac");
    } else if strategy == "transcode-video-copy-audio" {
        encode_video(&mut args);
        if multi {
            args.push(s("-an"));
        } else {
            push(&mut args, &["-c:a", "copy"]);
        }
        out_v = s(requested);
        out_a = if multi { s("aac") } else { src_a };
    } else if strategy == "h264-aac" {
        encode_video(&mut args);
        if multi {
            args.push(s("-an"));
        } else {
            args.extend(aac.iter().cloned());
        }
        out_v = s(requested);
        out_a = s("aac");
    } else {
        // copy-video-transcode-audio, and the default: copy video, AAC audio.
        copy_video_aac(&mut args);
        out_v = src_v;
        out_a = s("aac");
    }

    push(&mut args, &["-start_at_zero", "-max_muxing_queue_size", "200000"]);
    args.extend(frag_args(cfg));
    push(&mut args, &["-metadata", "comment=tv-transcoder", "-f", "mp4", "pipe:1"]);

    // One audio-only fMP4 output per track when the source has several.
    let extra = if multi { spec.audio_track_count as usize } else { 0 };
    for i in 0..extra {
        args.push(s("-map"));
        args.push(format!("0:a:{}?", i));
        args.extend(aac.iter().cloned());
        push(&mut args, &["-max_muxing_queue_size", "200000"]);
        args.extend(frag_args(cfg));
        push(&mut args, &["-f", "mp4"]);
        args.push(match audio_targets.and_then(|t| t.get(i)) {
            Some(t) => t.clone(),
            None => format!("pipe:{}", i + 3),
        });
    }

    Built { args, video_encoder: v_encoder, output_video_codec: out_v, output_audio_codec: out_a, extra_audio_count: extra, warnings }
}

// ── ffprobe ──

pub fn is_mp4_url(url: &str) -> bool {
    let l = url.to_ascii_lowercase();
    [".mp4", ".m4v", ".mov"].iter().any(|ext| {
        let mut from = 0;
        while let Some(i) = l[from..].find(ext) {
            let end = from + i + ext.len();
            if end == l.len() || l.as_bytes()[end] == b'?' {
                return true;
            }
            from = end;
        }
        false
    })
}

/// /vod treats these as files to remux: .mkv .avi .flv .wmv (before a query string or at the end).
pub fn is_vod_container(url: &str) -> bool {
    let l = url.to_ascii_lowercase();
    [".mkv", ".avi", ".flv", ".wmv"].iter().any(|ext| {
        let mut from = 0;
        while let Some(i) = l[from..].find(ext) {
            let end = from + i + ext.len();
            if end == l.len() || l.as_bytes()[end] == b'?' {
                return true;
            }
            from = end;
        }
        false
    })
}

/// Frame probe from stdin (first 5 frames: B-frame detection, stream basics).
pub fn probe_frame_args() -> Vec<String> {
    [
        "-v", "error", "-select_streams", "v:0", "-read_intervals", "%+#5", "-show_entries",
        "stream=codec_name,profile,width,height,avg_frame_rate,bit_rate,level,pix_fmt,has_b_frames,channels,sample_rate:frame=pict_type,key_frame:format=format_name,duration,size,bit_rate",
        "-show_frames", "-show_streams", "-show_format", "-of", "json", "-analyzeduration", "2000000", "-probesize", "2000000", "pipe:0",
    ]
    .iter()
    .map(|x| x.to_string())
    .collect()
}

/// Full stream/format/chapter probe from stdin.
pub fn probe_all_args() -> Vec<String> {
    ["-v", "quiet", "-print_format", "json", "-show_streams", "-show_format", "-show_chapters", "-analyzeduration", "3000000", "-probesize", "3000000", "pipe:0"]
        .iter()
        .map(|x| x.to_string())
        .collect()
}

/// MP4/MOV probe straight from the URL (ffprobe seeks to the moov atom over HTTP).
pub fn probe_mp4_args(user_agent: &str, url: &str) -> Vec<String> {
    let mut a: Vec<String> = ["-v", "error", "-protocol_whitelist", "file,http,https,tcp,tls,crypto", "-show_streams", "-show_format", "-of", "json", "-user_agent"].iter().map(|x| x.to_string()).collect();
    a.push(user_agent.to_string());
    a.extend([s("-timeout"), s("8000000"), url.to_string()]);
    a
}

/// Duration probe used by /vod when the caller did not pass one.
pub fn probe_duration_args(user_agent: &str, url: &str) -> Vec<String> {
    let mut a: Vec<String> = [
        "-v", "quiet", "-print_format", "json", "-show_format", "-protocol_whitelist", "file,http,https,tcp,tls,crypto", "-analyzeduration", "10000000", "-probesize", "10000000", "-find_stream_info",
        "-user_agent",
    ]
    .iter()
    .map(|x| x.to_string())
    .collect();
    a.push(user_agent.to_string());
    a.push(url.to_string());
    a
}

// ── /vod ──

/// One copy-mode MPEG-TS segment cut straight from the source URL.
pub fn vod_segment_args(user_agent: &str, resolved_url: &str, seg_index: f64, seg_dur: f64, audio_track: i64) -> Vec<String> {
    let start = seg_index * seg_dur;
    let pre = (start - 5.0).max(0.0);
    let fine = start - pre;
    let mut a: Vec<String> = ["-hide_banner", "-loglevel", "error", "-reconnect", "1", "-reconnect_streamed", "1", "-reconnect_on_network_error", "1", "-reconnect_delay_max", "5", "-user_agent"].iter().map(|x| x.to_string()).collect();
    a.push(user_agent.to_string());
    a.extend([s("-protocol_whitelist"), s("file,http,https,tcp,tls,crypto"), s("-ss"), js_num(pre), s("-i"), resolved_url.to_string(), s("-ss"), js_num(fine), s("-t"), js_num(seg_dur)]);
    a.extend([s("-map"), s("0:v:0?"), s("-map"), format!("0:a:{}?", audio_track), s("-map"), s("-0:s")]);
    push(&mut a, &["-c", "copy", "-copyts", "-bsf:v", "h264_mp4toannexb", "-start_at_zero", "-mpegts_flags", "+resend_headers", "-f", "mpegts", "pipe:1"]);
    a
}

/// fMP4 init segment for an MKV (header piped on stdin).
pub fn vod_init_args(audio_track: i64, audio_bitrate: &str) -> Vec<String> {
    let mut a: Vec<String> = ["-hide_banner", "-loglevel", "error", "-analyzeduration", "10000000", "-probesize", "10000000", "-i", "pipe:0", "-t", "0.001", "-map", "0:v:0?", "-map"].iter().map(|x| x.to_string()).collect();
    a.push(format!("0:a:{}?", audio_track));
    a.extend([s("-map"), s("-0:s"), s("-c:v"), s("copy"), s("-c:a"), s("aac"), s("-ac"), s("2"), s("-b:a"), audio_bitrate.to_string(), s("-ar"), s("48000")]);
    push(&mut a, &["-movflags", "frag_keyframe+empty_moov+default_base_moof", "-f", "mp4", "pipe:1"]);
    a
}

/// The full per-segment VOD playlist /vod hands out (every segment listed up front).
pub fn build_vod_playlist(base: &str, source_url: &str, duration: f64, seg_dur: f64, audio_track: &str, audio_bitrate: &str, tk: &str) -> String {
    use crate::util::encode_uri_component as enc;
    let count = (duration / seg_dur).ceil() as i64;
    let mut lines = vec![s("#EXTM3U"), s("#EXT-X-VERSION:3"), s("#EXT-X-PLAYLIST-TYPE:VOD"), format!("#EXT-X-TARGETDURATION:{}", js_num(seg_dur.ceil())), s("#EXT-X-MEDIA-SEQUENCE:0")];
    let encoded = enc(source_url);
    for i in 0..count {
        let remaining = duration - (i as f64 * seg_dur);
        let this = seg_dur.min(remaining);
        lines.push(format!("#EXTINF:{:.6},", this));
        lines.push(format!("{}?tk={}&url={}&seg={}&seg_dur={}&audio={}&bitrate={}&bframes=0", base, enc(tk), encoded, i, js_num(seg_dur), audio_track, audio_bitrate));
    }
    lines.push(s("#EXT-X-ENDLIST"));
    lines.join("\n") + "\n"
}

#[cfg(test)]
mod tests {
    //! Ported from transcode/test_ffmpeg_args.js: the same strategy × audio matrix and the same
    //! assertions, run against the real builder with software encoders (as the Node test does).
    use super::*;

    #[derive(Clone)]
    struct Opts {
        strategy: &'static str,
        audio_track_count: i64,
        audio_track: i64,
        source_audio_codec: &'static str,
        source_audio_channels: i64,
        seek_time: f64,
    }
    impl Default for Opts {
        fn default() -> Self {
            Opts { strategy: "", audio_track_count: 1, audio_track: 0, source_audio_codec: "aac", source_audio_channels: 2, seek_time: 0.0 }
        }
    }

    fn build_ff(o: &Opts) -> Built {
        let cfg = Config::defaults();
        let spec = SessionSpec {
            strategy: o.strategy.to_string(),
            audio_track: o.audio_track,
            audio_bitrate: String::new(),
            audio_track_count: o.audio_track_count,
            source_audio_codec: o.source_audio_codec.to_string(),
            source_audio_channels: o.source_audio_channels,
            source_video_codec: "h264".to_string(),
            prev_output_video_codec: String::new(),
        };
        build(&cfg, &Encoders::default(), &spec, input_args(&cfg, o.seek_time, false, 0), None)
    }

    const STRATEGIES: [&str; 6] = ["hls-copy", "transcode-video-copy-audio", "h264-aac", "copy-video-transcode-audio", "transcode-audio", ""];

    fn audio_configs() -> Vec<(&'static str, Opts)> {
        let o = |n, c, ch| Opts { audio_track_count: n, source_audio_codec: c, source_audio_channels: ch, ..Default::default() };
        vec![
            ("1-track AAC stereo", o(1, "aac", 2)),
            ("1-track EAC3 5.1", o(1, "eac3", 6)),
            ("1-track AAC 5.1", o(1, "aac", 6)),
            ("1-track DTS stereo", o(1, "dts", 2)),
            ("2-track AAC stereo", o(2, "aac", 2)),
            ("3-track EAC3 5.1", o(3, "eac3", 6)),
            ("2-track DTS 5.1", o(2, "dts", 6)),
        ]
    }

    fn matrix() -> Vec<(String, Opts)> {
        let mut v = Vec::new();
        for st in STRATEGIES {
            for (label, ac) in audio_configs() {
                v.push((format!("strategy={} + {}", if st.is_empty() { "default" } else { st }, label), Opts { strategy: st, ..ac }));
            }
        }
        v
    }

    /// Everything after `pipe:0` up to (not including) `pipe:1`.
    fn main_before_p1(args: &[String]) -> Vec<String> {
        let i = args.iter().position(|a| a == "pipe:0").unwrap();
        let p1 = args.iter().position(|a| a == "pipe:1").unwrap();
        args[i + 1..p1].to_vec()
    }
    fn extras(args: &[String]) -> Vec<String> {
        let p1 = args.iter().position(|a| a == "pipe:1").unwrap();
        args[p1 + 1..].to_vec()
    }
    fn count(args: &[String], v: &str) -> usize {
        args.iter().filter(|a| *a == v).count()
    }
    fn has(args: &[String], v: &str) -> bool {
        args.iter().any(|a| a == v)
    }
    fn after<'a>(args: &'a [String], flag: &str) -> Option<&'a str> {
        args.iter().position(|a| a == flag).and_then(|i| args.get(i + 1)).map(|x| x.as_str())
    }

    #[test]
    fn t01_no_contradictory_args() {
        for (label, o) in matrix() {
            let m = main_before_p1(&build_ff(&o).args);
            assert!(!(has(&m, "-an") && has(&m, "-c:a")), "{label}: -an and -c:a both in main output: {m:?}");
        }
    }

    #[test]
    fn t02_no_duplicate_codec_flags_in_main_output() {
        for (label, o) in matrix() {
            let m = main_before_p1(&build_ff(&o).args);
            for flag in ["-c:v", "-c:a", "-an", "-c"] {
                assert!(count(&m, flag) <= 1, "{label}: duplicate {flag}: {m:?}");
            }
        }
    }

    #[test]
    fn t03_multi_audio_main_is_video_only_and_extras_have_audio() {
        for (label, o) in matrix().into_iter().filter(|(_, o)| o.audio_track_count > 1) {
            let b = build_ff(&o);
            let m = main_before_p1(&b.args);
            assert!(has(&m, "-an"), "{label}: main output missing -an");
            assert!(!has(&m, "-c:a"), "{label}: main output has -c:a");
            let e = extras(&b.args);
            assert!(!e.is_empty(), "{label}: no extra audio outputs");
            assert_eq!(count(&e, "-c:a"), o.audio_track_count as usize, "{label}");
        }
    }

    #[test]
    fn t04_single_audio_has_no_an_and_no_extras() {
        for (label, o) in matrix().into_iter().filter(|(_, o)| o.audio_track_count == 1) {
            let b = build_ff(&o);
            assert!(!has(&main_before_p1(&b.args), "-an"), "{label}");
            assert!(extras(&b.args).is_empty(), "{label}");
        }
    }

    #[test]
    fn t05_hls_copy_aac_stereo_is_c_copy() {
        let b = build_ff(&Opts { strategy: "hls-copy", ..Default::default() });
        let m = main_before_p1(&b.args);
        assert_eq!(after(&m, "-c"), Some("copy"));
        assert!(!has(&m, "-c:v"));
        assert!(!has(&m, "-c:a"));
    }

    #[test]
    fn t06_hls_copy_multichannel_transcodes_audio() {
        let b = build_ff(&Opts { strategy: "hls-copy", source_audio_channels: 6, ..Default::default() });
        let m = main_before_p1(&b.args);
        assert_eq!(after(&m, "-c:v"), Some("copy"));
        assert_eq!(after(&m, "-c:a"), Some("aac"));
    }

    #[test]
    fn t07_incompatible_audio_forces_transcode() {
        for codec in ["eac3", "dts", "truehd", "flac", "vorbis", "opus", "pcm_s16le"] {
            let b = build_ff(&Opts { strategy: "transcode-video-copy-audio", source_audio_codec: codec, ..Default::default() });
            let m = main_before_p1(&b.args);
            assert_eq!(after(&m, "-c:a"), Some("aac"), "{codec}");
            assert_eq!(b.output_audio_codec, "aac", "{codec}");
        }
    }

    #[test]
    fn t08_safe_audio_passes_through_with_copy_strategy() {
        let b = build_ff(&Opts { strategy: "transcode-video-copy-audio", ..Default::default() });
        assert_eq!(after(&main_before_p1(&b.args), "-c:a"), Some("copy"));
    }

    #[test]
    fn t09_h264_aac_transcodes_both() {
        let b = build_ff(&Opts { strategy: "h264-aac", ..Default::default() });
        let m = main_before_p1(&b.args);
        assert_eq!(after(&m, "-c:v"), Some("libx264"));
        assert_eq!(after(&m, "-c:a"), Some("aac"));
        assert_eq!(b.output_video_codec, "h264");
        assert_eq!(b.output_audio_codec, "aac");
    }

    #[test]
    fn t10_extra_audio_outputs_have_max_muxing_queue_size() {
        for st in STRATEGIES {
            let b = build_ff(&Opts { strategy: st, audio_track_count: 3, ..Default::default() });
            assert_eq!(count(&extras(&b.args), "-max_muxing_queue_size"), 3, "{st}");
        }
    }

    #[test]
    fn t11_extra_output_count_matches_track_count() {
        // Node: stdio array has 3 + N entries when N > 1 tracks, else 3.
        for n in [1i64, 2, 3, 5] {
            let b = build_ff(&Opts { audio_track_count: n, ..Default::default() });
            assert_eq!(b.extra_audio_count, if n > 1 { n as usize } else { 0 });
        }
    }

    #[test]
    fn t12_pipe_references_are_sequential() {
        let b = build_ff(&Opts { audio_track_count: 3, ..Default::default() });
        assert!(has(&b.args, "pipe:3") && has(&b.args, "pipe:4") && has(&b.args, "pipe:5"));
        assert!(!has(&b.args, "pipe:6"));
    }

    #[test]
    fn t13_mandatory_args_everywhere() {
        for (label, o) in matrix() {
            let a = build_ff(&o).args;
            for m in ["-hide_banner", "-loglevel", "-i", "pipe:0", "-start_at_zero", "-movflags", "-f", "mp4", "pipe:1"] {
                assert!(has(&a, m), "{label}: missing {m}");
            }
        }
    }

    #[test]
    fn t14_map_selection() {
        let b = build_ff(&Opts { audio_track: 2, ..Default::default() });
        assert!(has(&b.args, "-map") && has(&b.args, "0:a:2?"));
        let b = build_ff(&Opts { audio_track_count: 3, ..Default::default() });
        let m = main_before_p1(&b.args);
        assert!(!m.iter().any(|a| a.starts_with("0:a:")), "main must not map audio: {m:?}");
        let e = extras(&b.args);
        assert!(has(&e, "0:a:0?") && has(&e, "0:a:1?") && has(&e, "0:a:2?"));
    }

    // ── Beyond the Node test file: exact lists, seek, hardware encoders ──

    fn sv(a: &[&str]) -> Vec<String> {
        a.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn exact_default_copy_video_aac_audio() {
        let b = build_ff(&Opts { source_audio_codec: "ac3", source_audio_channels: 6, ..Default::default() });
        assert_eq!(
            b.args,
            sv(&[
                "-hide_banner", "-loglevel", "error", "-threads", "0", "-analyzeduration", "5000000", "-probesize", "5000000", "-err_detect", "ignore_err", "-i", "pipe:0", "-map", "0:v:0?", "-map", "0:a:0?",
                "-map", "-0:s", "-c:v", "copy", "-c:a", "aac", "-ac", "2", "-b:a", "128k", "-ar", "48000", "-start_at_zero", "-max_muxing_queue_size", "200000", "-movflags",
                "frag_keyframe+empty_moov+default_base_moof", "-frag_duration", "3000000", "-min_frag_duration", "1500000", "-metadata", "comment=tv-transcoder", "-f", "mp4", "pipe:1",
            ])
        );
        assert_eq!(b.video_encoder, "libx264");
        assert_eq!(b.output_video_codec, "h264");
    }

    #[test]
    fn exact_software_transcode_with_scale() {
        let b = build_ff(&Opts { strategy: "h264-aac", source_audio_codec: "dts", ..Default::default() });
        let m = main_before_p1(&b.args);
        assert_eq!(
            m,
            sv(&[
                "-map", "0:v:0?", "-map", "0:a:0?", "-map", "-0:s", "-vf", "scale=-2:'min(1080,ih)'", "-c:v", "libx264", "-preset", "veryfast", "-crf", "23", "-pix_fmt", "yuv420p", "-g", "72", "-keyint_min", "24",
                "-c:a", "aac", "-ac", "2", "-b:a", "128k", "-ar", "48000", "-start_at_zero", "-max_muxing_queue_size", "200000", "-movflags", "frag_keyframe+empty_moov+default_base_moof", "-frag_duration", "3000000",
                "-min_frag_duration", "1500000", "-metadata", "comment=tv-transcoder", "-f", "mp4",
            ])
        );
    }

    #[test]
    fn seek_adds_ss_only_without_a_cached_header() {
        let cfg = Config::defaults();
        let a = input_args(&cfg, 42.5, false, 0);
        assert_eq!(&a[5..7], &sv(&["-ss", "42.5"])[..]);
        let a = input_args(&cfg, 600.0, false, 0);
        assert_eq!(&a[5..7], &sv(&["-ss", "600"])[..]);
        let a = input_args(&cfg, 42.5, true, 123456);
        assert!(!has(&a, "-ss"));
        assert_eq!(a.last().unwrap(), "pipe:0");
        assert!(!has(&input_args(&cfg, 0.0, false, 0), "-ss"));
    }

    #[test]
    fn session_bitrate_and_audio_targets() {
        let cfg = Config::defaults();
        let spec = SessionSpec { audio_bitrate: "192k".into(), audio_track_count: 2, source_audio_codec: "ac3".into(), ..Default::default() };
        let targets = vec!["tcp://127.0.0.1:5001".to_string(), "tcp://127.0.0.1:5002".to_string()];
        let b = build(&cfg, &Encoders::default(), &spec, input_args(&cfg, 0.0, false, 0), Some(&targets));
        let e = extras(&b.args);
        assert_eq!(count(&e, "192k"), 2);
        assert!(has(&e, "tcp://127.0.0.1:5001") && has(&e, "tcp://127.0.0.1:5002"));
        assert!(!b.args.iter().any(|a| a == "pipe:3"));
        assert_eq!(b.extra_audio_count, 2);
    }

    fn hw(h264: &str, input: &[&str]) -> Encoders {
        Encoders { h264: Some(h264.to_string()), hevc: None, h264_input: sv(input), hevc_input: vec![] }
    }

    #[test]
    fn nvenc_quality_and_cuda_input() {
        let cfg = Config::defaults();
        let spec = SessionSpec { strategy: "h264-aac".into(), audio_track_count: 1, source_audio_codec: "ac3".into(), ..Default::default() };
        let b = build(&cfg, &hw("h264_nvenc", &["-hwaccel", "cuda", "-hwaccel_device", "0"]), &spec, input_args(&cfg, 0.0, false, 0), None);
        let i = b.args.iter().position(|a| a == "-i").unwrap();
        assert_eq!(&b.args[i - 4..i], &sv(&["-hwaccel", "cuda", "-hwaccel_device", "0"])[..]);
        let m = main_before_p1(&b.args);
        let v = m.iter().position(|a| a == "-c:v").unwrap();
        assert_eq!(&m[v..v + 12], &sv(&["-c:v", "h264_nvenc", "-preset", "p3", "-rc", "vbr", "-cq", "23", "-b:v", "0", "-gpu", "0"])[..]);
        assert_eq!(b.video_encoder, "h264_nvenc");
    }

    #[test]
    fn amf_qsv_vaapi_quality() {
        let cfg = Config::defaults();
        let spec = SessionSpec { strategy: "h264-aac".into(), audio_track_count: 1, ..Default::default() };
        let m = |e: Encoders| main_before_p1(&build(&cfg, &e, &spec, input_args(&cfg, 0.0, false, 0), None).args);

        let a = m(hw("h264_amf", &[]));
        let v = a.iter().position(|x| x == "-c:v").unwrap();
        assert_eq!(&a[v..v + 10], &sv(&["-c:v", "h264_amf", "-quality", "speed", "-rc", "cqp", "-qp_i", "23", "-qp_p", "23"])[..]);

        let a = m(hw("h264_qsv", &[]));
        let v = a.iter().position(|x| x == "-c:v").unwrap();
        assert_eq!(&a[v..v + 6], &sv(&["-c:v", "h264_qsv", "-preset", "veryfast", "-global_quality", "23"])[..]);

        let a = m(hw("h264_vaapi", &["-init_hw_device", "vaapi=va:/dev/dri/renderD128", "-filter_hw_device", "va"]));
        assert_eq!(after(&a, "-vf"), Some("format=nv12,hwupload,scale_vaapi=w=-2:h='min(1080,ih)'"));
        let v = a.iter().position(|x| x == "-c:v").unwrap();
        assert_eq!(&a[v..v + 4], &sv(&["-c:v", "h264_vaapi", "-qp", "23"])[..]);
    }

    #[test]
    fn hevc_strategy_uses_the_hevc_encoder() {
        let mut cfg = Config::defaults();
        cfg.crf = None;
        let spec = SessionSpec { strategy: "hevc-aac".into(), audio_track_count: 1, ..Default::default() };
        let b = build(&cfg, &Encoders::default(), &spec, input_args(&cfg, 0.0, false, 0), None);
        assert_eq!(b.video_encoder, "libx265");
        // an unknown strategy name falls to the default branch: copy video, AAC audio
        assert_eq!(after(&main_before_p1(&b.args), "-c:v"), Some("copy"));
        cfg.codec = Some("hevc".into());
        let spec = SessionSpec { strategy: "h264-aac".into(), audio_track_count: 1, ..Default::default() };
        let b = build(&cfg, &Encoders::default(), &spec, input_args(&cfg, 0.0, false, 0), None);
        let m = main_before_p1(&b.args);
        assert_eq!(after(&m, "-c:v"), Some("libx265"));
        assert_eq!(after(&m, "-crf"), Some("28"));
        assert_eq!(b.output_video_codec, "hevc");
    }

    #[test]
    fn url_kinds() {
        assert!(is_mp4_url("http://a/b/201.mp4"));
        assert!(is_mp4_url("http://a/b/x.MOV?token=1"));
        assert!(!is_mp4_url("http://a/b/x.mp4.mkv"));
        assert!(!is_mp4_url("http://a/b/202.mkv"));
        assert!(is_vod_container("http://a/b/202.mkv"));
        assert!(is_vod_container("http://a/b/x.avi?x=1"));
        assert!(!is_vod_container("http://a/b/x.mp4"));
    }

    #[test]
    fn vod_segment_and_playlist() {
        let a = vod_segment_args("UA", "http://h/x.mp4", 3.0, 10.0, 1);
        let i = a.iter().position(|x| x == "-i").unwrap();
        assert_eq!(&a[i - 2..i + 2], &sv(&["-ss", "25", "-i", "http://h/x.mp4"])[..]);
        assert_eq!(&a[i + 2..i + 6], &sv(&["-ss", "5", "-t", "10"])[..]);
        assert!(has(&a, "0:a:1?"));
        assert_eq!(a.last().unwrap(), "pipe:1");
        let a0 = vod_segment_args("UA", "http://h/x.mp4", 0.0, 10.0, 0);
        let i = a0.iter().position(|x| x == "-i").unwrap();
        assert_eq!(&a0[i - 2..i], &sv(&["-ss", "0"])[..]);
        assert_eq!(a0[i + 3], "0");

        let pl = build_vod_playlist("http://127.0.0.1:3002/vod", "http://h/a b.mkv", 25.0, 10.0, "0", "320k", "");
        let lines: Vec<&str> = pl.lines().collect();
        assert_eq!(&lines[..5], &["#EXTM3U", "#EXT-X-VERSION:3", "#EXT-X-PLAYLIST-TYPE:VOD", "#EXT-X-TARGETDURATION:10", "#EXT-X-MEDIA-SEQUENCE:0"]);
        assert_eq!(lines[5], "#EXTINF:10.000000,");
        assert_eq!(lines[6], "http://127.0.0.1:3002/vod?tk=&url=http%3A%2F%2Fh%2Fa%20b.mkv&seg=0&seg_dur=10&audio=0&bitrate=320k&bframes=0");
        assert_eq!(lines[9], "#EXTINF:5.000000,");
        assert_eq!(lines[11], "#EXT-X-ENDLIST");
        assert!(pl.ends_with("\n"));
    }
}
