//! /probe: what is in a file (ffprobe), whether the server honours Range, and the decision the
//! player follows (play directly, remux, transcode audio, transcode everything).

use crate::app::{App, PROBE_CACHE_SIZE};
use crate::downloader::{DlEvent, Downloader};
use crate::ffargs;
use crate::routes::{json_resp, Params};
use crate::ssrf;
use crate::util::{prefix, truthy};
use crate::logi;
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use bytes::Bytes;
use serde_json::{json, Map, Value};
use std::net::SocketAddr;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::AsyncWriteExt;

/// A URL without its path and query (provider URLs carry logins): for log lines.
pub fn redact(u: &str) -> String {
    match reqwest::Url::parse(u) {
        Ok(x) => {
            let last = x.path_segments().and_then(|mut s| s.next_back().map(|l| l.to_string())).unwrap_or_default();
            format!("{}://{}/.../{}", x.scheme(), x.host_str().unwrap_or(""), last)
        }
        Err(_) => String::new(),
    }
}

/// Follows redirects by hand (HEAD, or GET with a one-byte Range) and returns the final URL.
/// Any failure returns the URL reached so far.
pub async fn resolve_url(app: &App, input: &str, max_redirects: usize, prefer: &str) -> String {
    if !ssrf::scheme_ok(input) {
        return input.to_string();
    }
    let mut url = input.to_string();
    let mut method = if prefer == "GET" { "GET" } else { "HEAD" };
    let mut remaining = max_redirects;
    while remaining > 0 {
        let parsed = match reqwest::Url::parse(&url) {
            Ok(u) if ssrf::url_target_ok(&u) => u,
            _ => return url,
        };
        let mut req = if method == "GET" { app.clients.manual.get(parsed.clone()).header("range", "bytes=0-0") } else { app.clients.manual.head(parsed.clone()) };
        req = req.header("user-agent", &app.cfg.user_agent);
        match tokio::time::timeout(Duration::from_secs(5), req.send()).await {
            Ok(Ok(resp)) => {
                let st = resp.status().as_u16();
                let loc = resp.headers().get("location").and_then(|v| v.to_str().ok()).map(|s| s.to_string());
                match loc {
                    Some(l) if [301, 302, 303, 307, 308].contains(&st) => match parsed.join(&l) {
                        Ok(next) => {
                            logi!("[resolveUrl] {} {} -> {}", method, st, redact(next.as_str()));
                            url = next.to_string();
                            remaining -= 1;
                        }
                        Err(_) => return url,
                    },
                    _ => return url,
                }
            }
            _ => {
                if method == "HEAD" {
                    method = "GET"; // some servers refuse HEAD
                } else {
                    return url;
                }
            }
        }
    }
    url
}

/// One ranged GET that follows redirects by hand: tells whether Range works, the file size, and
/// where the first redirect pointed. Used for MP4/MOV (ffprobe then reads that URL itself).
async fn detect_range_and_redirect(app: &App, input: &str) -> Value {
    let redirect = std::sync::Mutex::new(json!({ "redirected": false, "redirectCode": null, "redirectUrl": null }));
    let with = |mut base: Value, redirect: &Value| -> Value {
        for (k, v) in redirect.as_object().unwrap() {
            base[k] = v.clone();
        }
        base
    };
    let work = async {
        let mut url = match reqwest::Url::parse(input) {
            Ok(u) => u,
            Err(e) => return json!({ "supported": false, "method": "get-range", "reason": e.to_string(), "fileSize": 0 }),
        };
        for _ in 0..10 {
            if !ssrf::url_target_ok(&url) {
                return json!({ "supported": false, "method": "get-range", "reason": "url not allowed", "fileSize": 0 });
            }
            let req = app.clients.manual.get(url.clone()).header("user-agent", &app.cfg.user_agent).header("range", "bytes=0-1");
            let resp = match tokio::time::timeout(Duration::from_secs(8), req.send()).await {
                Err(_) => return json!({ "supported": false, "method": "get-range", "reason": "timeout", "fileSize": 0 }),
                Ok(Err(e)) => return json!({ "supported": false, "method": "get-range", "reason": crate::downloader::describe(&e), "fileSize": 0 }),
                Ok(Ok(r)) => r,
            };
            let st = resp.status().as_u16();
            let hv = |name: &str| resp.headers().get(name).and_then(|v| v.to_str().ok()).map(|s| s.to_string());
            if let (true, Some(loc)) = ([301, 302, 303, 307, 308].contains(&st), hv("location")) {
                let next = match url.join(&loc) {
                    Ok(n) => n,
                    Err(_) => return json!({ "supported": false, "method": "get-range", "reason": "bad redirect", "fileSize": 0 }),
                };
                {
                    let mut r = redirect.lock().unwrap();
                    if r["redirected"] == false {
                        *r = json!({ "redirected": true, "redirectCode": st, "redirectUrl": next.to_string() });
                    }
                }
                url = next;
                continue;
            }
            let mut size: u64 = hv("content-range").and_then(|cr| cr.rsplit('/').next().and_then(|t| t.parse().ok())).unwrap_or(0);
            if size == 0 && st == 200 {
                size = hv("content-length").and_then(|v| v.parse().ok()).unwrap_or(0);
            }
            if st == 206 {
                return json!({ "supported": true, "method": "get-range", "reason": "206 Partial Content", "statusCode": 206, "fileSize": size });
            }
            if hv("accept-ranges").map(|a| a.to_ascii_lowercase().contains("bytes")).unwrap_or(false) {
                return json!({ "supported": true, "method": "get-range", "reason": "accept-ranges: bytes", "statusCode": st, "fileSize": size });
            }
            return json!({ "supported": false, "method": "get-range", "reason": format!("server returned {}", st), "statusCode": st, "fileSize": size });
        }
        json!({ "supported": false, "method": "get-range", "reason": "too many redirects", "fileSize": 0 })
    };
    let base = match tokio::time::timeout(Duration::from_secs(10), work).await {
        Ok(v) => v,
        Err(_) => json!({ "supported": false, "method": "get-range", "reason": "timeout" }),
    };
    let r = redirect.lock().unwrap().clone();
    with(base, &r)
}

/// Downloads the first bytes of the file (up to 5 MB, 10 s): the HTTP status tells whether Range
/// works, ffprobe reads the bytes from stdin, and they are kept for the start of playback.
async fn detect_and_download(app: &Arc<App>, url: &str) -> (Value, Bytes) {
    let mut dl = Downloader::new(app.clients.follow.clone(), &app.cfg.user_agent, url, 0, "probe-cache", 1, 0);
    let mut data: Vec<u8> = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        match tokio::time::timeout_at(deadline, dl.next()).await {
            Ok(DlEvent::Data(b)) => {
                data.extend_from_slice(&b);
                if data.len() >= PROBE_CACHE_SIZE {
                    break;
                }
            }
            _ => break, // end of file, error, or out of time: use what we have
        }
    }
    let supported = dl.status_code == 206;
    let range = json!({
        "supported": supported,
        "method": "get-range",
        "statusCode": dl.status_code,
        "reason": if supported { "206 Partial Content".to_string() } else if dl.status_code > 0 { format!("server returned {}", dl.status_code) } else { "server returned unknown".to_string() },
        "redirected": false, "redirectCode": null, "redirectUrl": null,
        "fileSize": dl.file_size,
    });
    drop(dl);
    let data = Bytes::from(data);
    if !data.is_empty() {
        app.probe_cache_put(url, data.clone());
        logi!("[probe-cache] cached {}B for {}", data.len(), prefix(&redact(url), 80));
    }
    (range, data)
}

/// Runs ffprobe (12 s at most) and parses its JSON. `stdin_data`: fed to its stdin.
pub async fn run_ffprobe(args: &[String], stdin_data: Option<Bytes>, timeout: Duration) -> Option<Value> {
    let mut cmd = crate::proc::tokio_command("ffprobe");
    cmd.args(args).stdout(Stdio::piped()).stderr(Stdio::null());
    cmd.stdin(if stdin_data.is_some() { Stdio::piped() } else { Stdio::null() });
    let mut child = cmd.spawn().ok()?;
    if let (Some(mut stdin), Some(data)) = (child.stdin.take(), stdin_data) {
        // ffprobe stops reading once it has enough: a broken pipe here is expected.
        tokio::spawn(async move {
            let _ = stdin.write_all(&data).await;
            let _ = stdin.shutdown().await;
        });
    }
    let out = tokio::time::timeout(timeout, child.wait_with_output()).await.ok()?.ok()?;
    serde_json::from_slice::<Value>(&out.stdout).ok()
}

fn or_val(v: Option<&Value>, d: Value) -> Value {
    if truthy(v) {
        v.cloned().unwrap_or(d)
    } else {
        d
    }
}

/// Fills in audio_streams, chapters and the decision. `frame`: the frame probe (None for MP4),
/// `all`: the full probe.
pub fn build_probe_result(frame: Option<Value>, all: Option<Value>, is_mp4: bool) -> Value {
    let mut result: Map<String, Value> = match frame {
        Some(Value::Object(o)) => o,
        _ => Map::new(),
    };
    let src = all.unwrap_or_else(|| json!({}));
    if !result.contains_key("format") {
        if let Some(f) = src.get("format") {
            result.insert("format".into(), f.clone());
        }
    }
    if !result.contains_key("streams") {
        if let Some(s) = src.get("streams") {
            result.insert("streams".into(), s.clone());
        }
    }
    let src_streams: Vec<Value> = src.get("streams").and_then(|s| s.as_array()).cloned().unwrap_or_default();

    let audio: Vec<Value> = src_streams
        .iter()
        .filter(|s| s["codec_type"] == "audio")
        .map(|s| {
            let tags = s.get("tags");
            let tag = |k: &str| tags.and_then(|t| t.get(k)).filter(|v| truthy(Some(v))).cloned();
            let codec = or_val(s.get("codec_name"), json!("unknown"));
            json!({
                "index": s.get("index").cloned().unwrap_or(Value::Null),
                "codec": codec, "codec_name": codec,
                "channels": or_val(s.get("channels"), json!(2)),
                "language": tag("language").or_else(|| tag("handler_name")).or_else(|| tag("title")).unwrap_or_else(|| json!("Unknown")),
                "title": tag("title").unwrap_or_else(|| json!("")),
                "bitrate": or_val(s.get("bit_rate"), json!("")),
                "sample_rate": or_val(s.get("sample_rate"), json!("")),
                "default": truthy(s.get("disposition").and_then(|d| d.get("default"))),
            })
        })
        .collect();
    result.insert("audio_streams".into(), Value::Array(audio.clone()));

    if let Some(ch) = src.get("chapters").and_then(|c| c.as_array()).filter(|c| !c.is_empty()) {
        let chapters: Vec<Value> = ch
            .iter()
            .map(|c| {
                json!({
                    "id": or_val(c.get("id"), json!(0)),
                    "start": or_val(c.get("start_time"), json!("0")),
                    "end": or_val(c.get("end_time"), json!("0")),
                    "title": c.get("tags").and_then(|t| t.get("title")).filter(|v| truthy(Some(v))).cloned().unwrap_or_else(|| json!("")),
                })
            })
            .collect();
        result.insert("chapters".into(), Value::Array(chapters));
    }

    // Decision
    let video = match result.get("streams").and_then(|s| s.as_array()) {
        Some(st) => st.iter().find(|s| s["codec_type"] == "video").cloned(),
        None => src_streams.iter().find(|s| s["codec_type"] == "video").cloned(),
    };
    let vc = video.as_ref().and_then(|v| v.get("codec_name")).and_then(|c| c.as_str()).unwrap_or("").to_string();
    let first_audio = audio.first();
    let ac = first_audio.and_then(|a| a.get("codec")).and_then(|c| c.as_str()).unwrap_or("").to_string();
    let video_ok = vc == "h264";
    let is_hevc = vc == "hevc" || vc == "h265";
    let no_audio = first_audio.is_none();
    // Only AAC is safe in fMP4 for MediaSource.
    let audio_ok = no_audio || ac == "aac";

    let (mut b, mut total) = (0u32, 0u32);
    if let Some(frames) = result.get("frames").and_then(|f| f.as_array()) {
        for fr in frames {
            if let Some(t) = fr.get("pict_type").and_then(|t| t.as_str()).filter(|t| !t.is_empty()) {
                total += 1;
                if t == "B" {
                    b += 1;
                }
            }
        }
    }
    let seek_friendly = b == 0 || (total > 0 && (b as f64 / total as f64) < 0.1);
    let d = |action: &str, strategy: &str, reason: String| json!({ "action": action, "strategy": strategy, "reason": reason });
    let decision = if video_ok && audio_ok && seek_friendly && is_mp4 {
        d("direct", "direct", if no_audio { "H264 video-only MP4, browser-native".into() } else { "H264 + AAC/MP3 MP4, browser-native — no proxy needed".into() })
    } else if video_ok && audio_ok && seek_friendly {
        d("remux", "hls-copy", if no_audio { "H264 video-only MKV — linear remux".into() } else { "H264 + AAC/MP3 MKV — linear remux (native seek in buffer)".into() })
    } else if video_ok && audio_ok {
        d("remux", "hls-copy", if no_audio { "H264 video-only, B-frames may affect seeking".into() } else { "Compatible codecs, B-frames may affect seeking".into() })
    } else if video_ok {
        d("partial-transcode", "copy-video-transcode-audio", format!("Video OK but audio codec \"{}\" not browser-native", ac))
    } else if audio_ok {
        d("transcode", "transcode-video-copy-audio", format!("Video codec \"{}\" not browser-native, audio OK", vc))
    } else if is_hevc {
        d("transcode", "h264-aac", format!("HEVC not browser-compatible, audio \"{}\" also needs transcode", ac))
    } else {
        d("transcode", "h264-aac", format!("Unsupported codecs: V={} A={}", if vc.is_empty() { "unknown" } else { &vc }, if ac.is_empty() { "unknown" } else { &ac }))
    };
    result.insert("decision".into(), decision);
    Value::Object(result)
}

pub async fn handle(app: &Arc<App>, addr: &SocketAddr, headers: &HeaderMap, p: &Params) -> Response {
    let url = p.s("url");
    if url.is_empty() {
        return json_resp(StatusCode::BAD_REQUEST, json!({ "error": "Missing url parameter" }));
    }
    if !ssrf::scheme_ok(&url) {
        return json_resp(StatusCode::BAD_REQUEST, json!({ "error": "invalid url scheme" }));
    }
    if !ssrf::frontend_allowed(addr, headers) {
        return json_resp(StatusCode::FORBIDDEN, json!({ "error": "forbidden" }));
    }
    if !ssrf::host_is_public(&url).await {
        return json_resp(StatusCode::FORBIDDEN, json!({ "error": "url not allowed" }));
    }
    let is_mp4 = ffargs::is_mp4_url(&url);
    let t = Duration::from_secs(12);

    let (range, frame, all) = if is_mp4 {
        let range = detect_range_and_redirect(app, &url).await;
        let final_url = range["redirectUrl"].as_str().filter(|_| range["redirected"] == true).unwrap_or(&url).to_string();
        let all = run_ffprobe(&ffargs::probe_mp4_args(&app.cfg.user_agent, &final_url), None, t).await;
        if all.is_none() {
            // Broken file or server trouble: answer with plausible defaults so playback is tried.
            logi!("[probe] MP4 probe returned no data for {}", prefix(&redact(&url), 80));
            return json_resp(
                StatusCode::OK,
                json!({
                    "range": range,
                    "probe": {
                        "streams": [{ "index": 0, "codec_name": "h264", "codec_type": "video", "width": 1920, "height": 1080 }],
                        "audio_streams": [{ "index": 1, "codec": "aac", "codec_name": "aac", "channels": 2, "language": "und", "title": "", "bitrate": "" }],
                        "format": { "format_name": "mov,mp4", "duration": "0", "size": "0" },
                        "chapters": [], "estimated": true
                    }
                }),
            );
        }
        (range, None, all)
    } else {
        let (range, data) = detect_and_download(app, &url).await;
        if data.is_empty() {
            (range, None, None)
        } else {
            let (fa, aa) = (ffargs::probe_frame_args(), ffargs::probe_all_args());
            let (f, a) = tokio::join!(run_ffprobe(&fa, Some(data.clone()), t), run_ffprobe(&aa, Some(data.clone()), t));
            (range, f, a)
        }
    };

    if frame.is_none() && all.is_none() {
        return json_resp(StatusCode::OK, json!({ "range": range, "probe": null }));
    }
    json_resp(StatusCode::OK, json!({ "range": range, "probe": build_probe_result(frame, all, is_mp4) }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn all(v: &str, a: Option<&str>) -> Value {
        let mut streams = vec![json!({ "index": 0, "codec_name": v, "codec_type": "video", "width": 1920, "height": 1080 })];
        if let Some(a) = a {
            streams.push(json!({ "index": 1, "codec_name": a, "codec_type": "audio", "channels": 6, "bit_rate": "640000", "tags": { "language": "eng", "title": "Surround" }, "disposition": { "default": 1 } }));
        }
        json!({ "streams": streams, "format": { "format_name": "matroska,webm", "duration": "888.0" }, "chapters": [{ "id": 1, "start_time": "0.0", "end_time": "10.0", "tags": { "title": "One" } }] })
    }
    fn frames(types: &[&str]) -> Value {
        json!({ "frames": types.iter().map(|t| json!({ "pict_type": t, "key_frame": 0 })).collect::<Vec<_>>(), "streams": [{ "codec_name": "h264", "codec_type": "video" }], "format": { "format_name": "matroska,webm" } })
    }
    fn decide(frame: Option<Value>, a: Value, mp4: bool) -> (String, String) {
        let r = build_probe_result(frame, Some(a), mp4);
        (r["decision"]["action"].as_str().unwrap().to_string(), r["decision"]["strategy"].as_str().unwrap().to_string())
    }

    #[test]
    fn decisions() {
        let s = |a: &str, b: &str| (a.to_string(), b.to_string());
        assert_eq!(decide(None, all("h264", Some("aac")), true), s("direct", "direct"));
        assert_eq!(decide(Some(frames(&["I", "P", "P", "P", "P"])), all("h264", Some("aac")), false), s("remux", "hls-copy"));
        assert_eq!(decide(Some(frames(&["I", "B", "B", "P", "B"])), all("h264", Some("aac")), false), s("remux", "hls-copy"));
        assert_eq!(decide(Some(frames(&["I", "P"])), all("h264", Some("ac3")), false), s("partial-transcode", "copy-video-transcode-audio"));
        assert_eq!(decide(None, all("h264", Some("ac3")), true), s("partial-transcode", "copy-video-transcode-audio"));
        assert_eq!(decide(None, all("hevc", Some("aac")), false), s("transcode", "transcode-video-copy-audio"));
        assert_eq!(decide(None, all("hevc", Some("eac3")), false), s("transcode", "h264-aac"));
        assert_eq!(decide(None, all("mpeg4", Some("mp3")), false), s("transcode", "h264-aac"));
        assert_eq!(decide(None, all("h264", None), true), s("direct", "direct"));
    }

    #[test]
    fn audio_streams_and_chapters_shape() {
        let r = build_probe_result(Some(frames(&["I"])), Some(all("h264", Some("ac3"))), false);
        let a = &r["audio_streams"][0];
        assert_eq!(a["codec"], "ac3");
        assert_eq!(a["codec_name"], "ac3");
        assert_eq!(a["channels"], 6);
        assert_eq!(a["language"], "eng");
        assert_eq!(a["title"], "Surround");
        assert_eq!(a["bitrate"], "640000");
        assert_eq!(a["sample_rate"], "");
        assert_eq!(a["default"], true);
        assert_eq!(r["chapters"][0], json!({ "id": 1, "start": "0.0", "end": "10.0", "title": "One" }));
        // the frame probe's own streams/format are kept
        assert_eq!(r["streams"].as_array().unwrap().len(), 1);
        assert_eq!(r["format"]["format_name"], "matroska,webm");
        assert!(r["decision"]["reason"].as_str().unwrap().contains("\"ac3\""));
        // without a frame probe the full probe supplies them
        let r = build_probe_result(None, Some(all("h264", Some("aac"))), true);
        assert_eq!(r["streams"].as_array().unwrap().len(), 2);
        assert_eq!(r["format"]["duration"], "888.0");
    }

    #[test]
    fn urls_are_redacted_for_logs() {
        assert_eq!(redact("http://host:8080/movie/user/pass/123.mkv?token=abc"), "http://host/.../123.mkv");
        assert_eq!(redact("nonsense"), "");
    }
}
