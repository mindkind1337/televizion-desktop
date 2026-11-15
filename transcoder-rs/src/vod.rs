//! /vod: copy-mode playback of files the browser could play if only they were segmented.
//!
//!   ?sid=…&segment=index.m3u8      a playlist built earlier (kept in memory for two hours)
//!   ?url=…&seg=N&seg_dur=10&bframes=0   segment N as MPEG-TS, cut by ffmpeg with -c copy
//!   ?url=…&init=1&filesize=…       fMP4 init segment of an MKV
//!   ?url=….mkv [&duration=…]       builds the playlist and returns where to fetch it

use crate::app::App;
use crate::downloader::fetch_range;
use crate::ffargs;
use crate::fmp4::{Event, Fmp4Parser};
use crate::mkv;
use crate::routes::{json_resp, Params};
use crate::ssrf;
use crate::util::{contains_ci, now_ms, sha1_16, value_str};
use crate::{loge, logi, logw};
use axum::body::Body;
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::Response;
use bytes::Bytes;
use serde_json::json;
use std::net::SocketAddr;
use std::pin::Pin;
use std::process::Stdio;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;

/// Response body fed from a channel. Dropping it (the client went away) drops the receiver,
/// which makes the producing task stop and its ffmpeg die (kill_on_drop).
struct ChanBody(mpsc::Receiver<Bytes>);

impl futures_core::Stream for ChanBody {
    type Item = Result<Bytes, std::io::Error>;
    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.0.poll_recv(cx).map(|o| o.map(Ok))
    }
}

fn status(code: StatusCode) -> Response {
    let mut r = Response::new(Body::empty());
    *r.status_mut() = code;
    r
}

fn err(code: StatusCode, msg: &str) -> Response {
    json_resp(code, json!({ "error": msg }))
}

pub async fn handle(app: &Arc<App>, addr: &SocketAddr, headers: &HeaderMap, p: &Params) -> Response {
    let url = p.s("url");
    let host = headers.get("host").and_then(|v| v.to_str().ok()).unwrap_or("127.0.0.1");
    let base = format!("http://{}/vod", host);
    if !url.is_empty() {
        if !ssrf::scheme_ok(&url) {
            return err(StatusCode::BAD_REQUEST, "invalid url scheme");
        }
        if !ssrf::frontend_allowed(addr, headers) {
            return err(StatusCode::FORBIDDEN, "forbidden");
        }
        if !ssrf::host_is_public(&url).await {
            return err(StatusCode::FORBIDDEN, "url not allowed");
        }
    }

    // A playlist built earlier.
    let sid = p.s("sid");
    if !sid.is_empty() && p.s("segment") == "index.m3u8" {
        let entry = app.playlists.lock().unwrap().get(&sid).map(|e| e.0.clone());
        return match entry {
            Some(m3u8) => {
                let mut r = Response::new(Body::from(m3u8));
                r.headers_mut().insert("content-type", HeaderValue::from_static("application/vnd.apple.mpegurl; charset=utf-8"));
                r.headers_mut().insert("cache-control", HeaderValue::from_static("no-store"));
                r
            }
            None => status(StatusCode::NOT_FOUND),
        };
    }
    if url.is_empty() {
        return err(StatusCode::BAD_REQUEST, "Missing url parameter");
    }
    let ua = app.cfg.user_agent.clone();

    // fMP4 init segment for an MKV.
    if p.s("init") == "1" && contains_ci(&url, ".mkv") && p.i("filesize") > 0 {
        return init_segment(app, &url, p.i("audio"), &p.s_or("bitrate", "320k")).await;
    }

    // One copy-mode segment.
    if p.has("seg") {
        let seg = value_str(p.get("seg")).unwrap_or_default();
        if p.s("bframes") != "0" {
            logw!("[VOD:seg={}] non-copy per-segment request rejected - use /transcode route", seg);
            return err(StatusCode::BAD_REQUEST, "Non-copy VOD must use /transcode route");
        }
        let seg_dur = match p.f("seg_dur") {
            d if d > 0.0 => d,
            _ => 10.0,
        };
        let seg_index = crate::util::parse_float(&seg).unwrap_or(f64::NAN);
        if !seg_index.is_finite() || seg_index < 0.0 {
            return err(StatusCode::BAD_REQUEST, "invalid seg");
        }
        // ffmpeg gets the final URL (better Range/seek behaviour than following redirects itself).
        let resolved = crate::probe::resolve_url(app, &url, 5, "HEAD").await;
        if resolved != url && !ssrf::host_is_public(&resolved).await {
            return err(StatusCode::FORBIDDEN, "url not allowed");
        }
        let args = ffargs::vod_segment_args(&ua, &resolved, seg_index, seg_dur, p.i("audio"));
        logi!("[VOD:seg={}] copy-mode: dur={} url={}", seg, seg_dur, crate::probe::redact(&resolved));
        let mut child = match crate::proc::tokio_command("ffmpeg").args(&args).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn() {
            Ok(c) => c,
            Err(e) => {
                loge!("[VOD:seg={}] cannot start ffmpeg: {}", seg, e);
                return status(StatusCode::INTERNAL_SERVER_ERROR);
            }
        };
        let (tx, rx) = mpsc::channel::<Bytes>(8);
        let mut stdout = child.stdout.take();
        let mut stderr = child.stderr.take();
        tokio::spawn(async move {
            let err_task = tokio::spawn(async move {
                let mut text = Vec::new();
                if let Some(e) = stderr.as_mut() {
                    let _ = e.read_to_end(&mut text).await;
                }
                text
            });
            let pump = async {
                let mut buf = vec![0u8; 64 * 1024];
                if let Some(out) = stdout.as_mut() {
                    loop {
                        match out.read(&mut buf).await {
                            Ok(0) | Err(_) => break,
                            Ok(n) => {
                                if tx.send(Bytes::copy_from_slice(&buf[..n])).await.is_err() {
                                    return false; // client went away
                                }
                            }
                        }
                    }
                }
                true
            };
            // An unseekable source can hang ffmpeg: 30 s at most.
            match tokio::time::timeout(Duration::from_secs(30), pump).await {
                Err(_) => {
                    logw!("[VOD:seg={}] ffmpeg timed out after 30s - killing", seg);
                    let _ = child.start_kill();
                }
                Ok(false) => {
                    let _ = child.start_kill();
                }
                Ok(true) => {}
            }
            let st = child.wait().await.ok();
            let text = err_task.await.unwrap_or_default();
            if st.map(|s| !s.success()).unwrap_or(true) && !text.is_empty() {
                loge!("[VOD:seg={}] ffmpeg exit {:?}: {}", seg, st.and_then(|s| s.code()), crate::util::prefix(String::from_utf8_lossy(&text).trim(), 200));
            }
        });
        let mut r = Response::new(Body::from_stream(ChanBody(rx)));
        r.headers_mut().insert("content-type", HeaderValue::from_static("video/MP2T"));
        return r;
    }

    // Build the playlist.
    if ffargs::is_vod_container(&url) || p.s("remux") == "1" {
        let mut duration = p.f("duration");
        let audio = p.s_or("audio", "0");
        let bitrate = p.s_or("bitrate", "320k");
        if duration <= 0.0 {
            let probed = crate::probe::run_ffprobe(&ffargs::probe_duration_args(&ua, &url), None, Duration::from_secs(30)).await;
            duration = probed.and_then(|v| v["format"]["duration"].as_str().and_then(crate::util::parse_float)).unwrap_or(0.0);
            if duration <= 0.0 {
                duration = 7200.0;
                loge!("Duration probe failed for {} - Using fallback 7200s", crate::probe::redact(&url));
            }
        }
        let playlist = ffargs::build_vod_playlist(&base, &url, duration, 10.0, &audio, &bitrate, &p.s("tk"));
        let id = sha1_16(&format!("{}{}", url, audio));
        app.playlists.lock().unwrap().insert(id.clone(), (playlist, now_ms()));
        return json_resp(
            StatusCode::OK,
            json!({ "status": "started", "playlist": format!("{}?sid={}&segment=index.m3u8", base, id), "v_codec": "h264", "a_codec": "aac", "duration": crate::util::num(duration) }),
        );
    }

    err(StatusCode::BAD_REQUEST, "invalid vod request")
}

/// MKV header + the first megabyte and a half through a tiny ffmpeg run: answers with the
/// ftyp+moov it writes first.
async fn init_segment(app: &Arc<App>, url: &str, audio_track: i64, bitrate: &str) -> Response {
    let ua = &app.cfg.user_agent;
    logi!("[VOD:init] generating fMP4 init segment for {}", crate::probe::redact(url));
    let raw = match fetch_range(&app.clients.follow, ua, url, 0, 524_287, 524_288).await {
        Ok(r) => r,
        Err(_) => return status(StatusCode::BAD_GATEWAY),
    };
    let cluster = match mkv::find_mkv_cluster(&raw, 0).filter(|c| c.offset > 0) {
        Some(c) => c,
        None => {
            loge!("[VOD:init] no cluster in header");
            return status(StatusCode::BAD_GATEWAY);
        }
    };
    let header = mkv::patch_mkv_segment_length(&raw[..cluster.offset]);
    let mut child = match crate::proc::tokio_command("ffmpeg").args(ffargs::vod_init_args(audio_track, bitrate)).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn() {
        Ok(c) => c,
        Err(_) => return status(StatusCode::INTERNAL_SERVER_ERROR),
    };
    let stdin = child.stdin.take();
    let stdout = child.stdout.take();
    let (client, ua2, url2) = (app.clients.follow.clone(), ua.clone(), url.to_string());
    let feeder = tokio::spawn(async move {
        if let Some(mut si) = stdin {
            if si.write_all(&header).await.is_err() || si.write_all(&raw[cluster.offset..]).await.is_err() {
                return;
            }
            if let Ok(more) = fetch_range(&client, &ua2, &url2, 524_288, 1_572_863, 1_048_576).await {
                let _ = si.write_all(&more).await;
            }
            let _ = si.shutdown().await;
        }
    });
    let read = async {
        let mut out = stdout?;
        let mut parser = Fmp4Parser::new(app.cfg.seg_duration);
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            match out.read(&mut buf).await {
                Ok(0) | Err(_) => return None,
                Ok(n) => {
                    for ev in parser.push(&buf[..n]) {
                        if let Event::Init(b) = ev {
                            return Some(b);
                        }
                    }
                }
            }
        }
    };
    let init = tokio::time::timeout(Duration::from_secs(30), read).await.ok().flatten();
    feeder.abort();
    let _ = child.start_kill();
    let mut r = match init {
        Some(b) => {
            logi!("[VOD:init] init segment: {} bytes", b.len());
            Response::new(Body::from(b))
        }
        None => Response::new(Body::empty()),
    };
    r.headers_mut().insert("content-type", HeaderValue::from_static("video/mp4"));
    r
}
