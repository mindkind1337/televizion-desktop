//! HTTP surface: the same routes, parameters and JSON as the Node service.
//!
//!   /probe        what is in a file, and how to play it
//!   /transcode    session start, playlists, segments, seek, pause, resume, stop
//!   /vod          copy-mode segments straight from the source, VOD playlists
//!   /stats        dashboard page;  /stats/json  figures;  /ws/stats  the same, pushed every second
//!
//! Parameters come from the query string and, on top of it, from a JSON body (sent as
//! application/json or as text/plain, which the player uses to avoid a CORS preflight).

use crate::app::App;
use crate::probe;
use crate::session::{HeaderMode, NewSession, Session};
use crate::ssrf;
use crate::util::{now_ms, parse_float, parse_int, prefix, sha1_16, truthy, value_int, value_str, value_str_or_empty};
use crate::{logi, logw};
use axum::body::{Body, Bytes};
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{ConnectInfo, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use serde_json::{json, Map, Value};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

const STATS_HTML: &str = include_str!("stats.html");
const M3U8: &str = "application/vnd.apple.mpegurl; charset=utf-8";

pub struct Params(pub Map<String, Value>);

impl Params {
    pub fn parse(uri: &Uri, headers: &HeaderMap, body: &[u8]) -> Params {
        let mut m = Map::new();
        for (k, v) in crate::util::parse_query(uri.query().unwrap_or("")) {
            m.insert(k, Value::String(v));
        }
        let ct = headers.get("content-type").and_then(|v| v.to_str().ok()).unwrap_or("").to_ascii_lowercase();
        let is_json = ct.starts_with("application/json");
        let is_text = ct.starts_with("text/plain");
        if !body.is_empty() && (is_json || (is_text && body[0] == b'{')) {
            if let Ok(Value::Object(o)) = serde_json::from_slice::<Value>(body) {
                for (k, v) in o {
                    m.insert(k, v);
                }
            }
        }
        Params(m)
    }
    pub fn get(&self, k: &str) -> Option<&Value> {
        self.0.get(k)
    }
    pub fn has(&self, k: &str) -> bool {
        self.0.contains_key(k)
    }
    pub fn truthy(&self, k: &str) -> bool {
        truthy(self.0.get(k))
    }
    /// `params.k || ''`
    pub fn s(&self, k: &str) -> String {
        value_str_or_empty(self.0.get(k))
    }
    /// `params.k || default`
    pub fn s_or(&self, k: &str, d: &str) -> String {
        let v = self.s(k);
        if v.is_empty() {
            d.to_string()
        } else {
            v
        }
    }
    /// `parseFloat(params.k) || 0`
    pub fn f(&self, k: &str) -> f64 {
        value_str(self.0.get(k)).and_then(|s| parse_float(&s)).filter(|v| v.is_finite()).unwrap_or(0.0)
    }
    /// `parseInt(params.k) || 0`
    pub fn i(&self, k: &str) -> i64 {
        value_str(self.0.get(k)).and_then(|s| parse_int(&s)).unwrap_or(0)
    }
}

pub fn json_resp(status: StatusCode, v: Value) -> Response {
    let mut r = Response::new(Body::from(v.to_string()));
    *r.status_mut() = status;
    r.headers_mut().insert("content-type", HeaderValue::from_static("application/json; charset=utf-8"));
    r
}

fn empty(status: StatusCode) -> Response {
    let mut r = Response::new(Body::empty());
    *r.status_mut() = status;
    r
}

fn bytes_resp(ct: &'static str, cache: &'static str, body: Vec<u8>) -> Response {
    let mut r = Response::new(Body::from(body));
    r.headers_mut().insert("content-type", HeaderValue::from_static(ct));
    r.headers_mut().insert("cache-control", HeaderValue::from_static(cache));
    r
}

fn err(status: StatusCode, msg: &str) -> Response {
    json_resp(status, json!({ "error": msg }))
}

fn host_of(headers: &HeaderMap) -> String {
    headers.get("host").and_then(|v| v.to_str().ok()).unwrap_or("127.0.0.1").to_string()
}

pub fn router(app: Arc<App>) -> axum::Router {
    axum::Router::new().route("/ws/stats", axum::routing::get(ws_stats)).fallback(dispatch).with_state(app)
}

/// Every plain HTTP request: CORS headers first (so errors carry them too), then the route.
async fn dispatch(State(app): State<Arc<App>>, ConnectInfo(addr): ConnectInfo<SocketAddr>, method: Method, uri: Uri, headers: HeaderMap, body: Bytes) -> Response {
    let origin = headers.get("origin").and_then(|v| v.to_str().ok()).map(|s| s.to_string());
    let mut resp = if method == Method::OPTIONS {
        empty(StatusCode::OK)
    } else {
        let params = Params::parse(&uri, &headers, &body);
        match uri.path() {
            "/probe" => probe::handle(&app, &addr, &headers, &params).await,
            "/vod" => crate::vod::handle(&app, &addr, &headers, &params).await,
            "/transcode" => transcode(&app, &addr, &headers, &params).await,
            "/stats" if method == Method::GET || method == Method::HEAD => stats_page(&addr),
            "/stats/json" if method == Method::GET || method == Method::HEAD => stats_json(&app, &addr, &params),
            _ => empty(StatusCode::NOT_FOUND),
        }
    };
    let h = resp.headers_mut();
    if let Some(o) = origin {
        if o != "null" && !o.is_empty() && ssrf::host_allowed(Some(&o)) {
            if let Ok(v) = HeaderValue::from_str(&o) {
                h.insert("access-control-allow-origin", v);
                h.insert("vary", HeaderValue::from_static("Origin"));
            }
        }
    }
    h.insert("access-control-allow-methods", HeaderValue::from_static("GET, OPTIONS"));
    h.insert("access-control-allow-headers", HeaderValue::from_static("*"));
    resp
}

fn stats_page(addr: &SocketAddr) -> Response {
    if !ssrf::is_loopback(addr) {
        return (StatusCode::FORBIDDEN, "Forbidden").into_response();
    }
    let mut r = Response::new(Body::from(STATS_HTML));
    let h = r.headers_mut();
    h.insert("content-type", HeaderValue::from_static("text/html; charset=utf-8"));
    h.insert("cache-control", HeaderValue::from_static("no-store, no-cache, must-revalidate"));
    h.insert("pragma", HeaderValue::from_static("no-cache"));
    r
}

fn stats_json(app: &Arc<App>, addr: &SocketAddr, params: &Params) -> Response {
    if params.has("brief") {
        return json_resp(StatusCode::OK, crate::stats::brief(app));
    }
    // The service only listens on loopback; anything else is refused outright.
    if !ssrf::is_loopback(addr) {
        return err(StatusCode::FORBIDDEN, "forbidden");
    }
    let sid = params.s("sid");
    json_resp(StatusCode::OK, crate::stats::gather(app, if sid.is_empty() { None } else { Some(&sid) }))
}

async fn ws_stats(State(app): State<Arc<App>>, ConnectInfo(addr): ConnectInfo<SocketAddr>, ws: WebSocketUpgrade) -> Response {
    if !ssrf::is_loopback(&addr) {
        return empty(StatusCode::FORBIDDEN);
    }
    ws.on_upgrade(move |socket| ws_loop(app, socket))
}

/// Pushes the full stats once a second until the client goes away.
async fn ws_loop(app: Arc<App>, mut socket: WebSocket) {
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    loop {
        tokio::select! {
            _ = tick.tick() => {
                let text = crate::stats::gather(&app, None).to_string();
                if socket.send(Message::Text(text.into())).await.is_err() {
                    return;
                }
            }
            msg = socket.recv() => {
                match msg {
                    Some(Ok(Message::Close(_))) | Some(Err(_)) | None => return,
                    _ => {}
                }
            }
        }
    }
}

/// Removes a session that was being started when the player went away before the answer.
struct StartGuard {
    app: Arc<App>,
    sid: String,
    done: bool,
}
impl Drop for StartGuard {
    fn drop(&mut self) {
        if !self.done {
            if let Some(s) = self.app.remove_session(&self.sid) {
                s.stop();
                logi!("[LIVE:{}] client disconnected before start completed - session discarded", self.sid);
            }
        }
    }
}

fn is_video_transcode(strategy: &str) -> bool {
    strategy.contains("transcode-video") || strategy == "h264-aac"
}

/// "a12.m3u8" → 12, with the given prefix/suffix around digits only.
fn audio_index(name: &str, suffix: &str) -> Option<usize> {
    let mid = name.strip_prefix('a')?.strip_suffix(suffix)?;
    if mid.is_empty() || !mid.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    mid.parse().ok()
}

/// "a1_25.m4s" → (1, 25)
fn audio_segment(name: &str) -> Option<(usize, u64)> {
    let mid = name.strip_prefix('a')?.strip_suffix(".m4s")?;
    let (t, s) = mid.split_once('_')?;
    if t.is_empty() || s.is_empty() || !t.bytes().all(|b| b.is_ascii_digit()) || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some((t.parse().ok()?, s.parse().ok()?))
}

fn js_or<'a>(a: Option<&'a Value>, b: Option<&'a Value>) -> Option<&'a Value> {
    if truthy(a) {
        a
    } else {
        b
    }
}

async fn transcode(app: &Arc<App>, addr: &SocketAddr, headers: &HeaderMap, p: &Params) -> Response {
    let cfg = &app.cfg;
    let base = format!("http://{}/transcode", host_of(headers));
    let url = p.s("url");
    let live_seg = p.s("live_seg");
    let sid = p.s("sid");
    if !url.is_empty() && !ssrf::scheme_ok(&url) {
        return err(StatusCode::BAD_REQUEST, "invalid url scheme");
    }

    // ── Start a session ──
    if p.truthy("live_start") && !url.is_empty() {
        if !ssrf::frontend_allowed(addr, headers) {
            return err(StatusCode::FORBIDDEN, "forbidden");
        }
        if !ssrf::host_is_public(&url).await {
            return err(StatusCode::FORBIDDEN, "url not allowed");
        }
        let audio_track = p.s_or("audio", "0");
        let strategy = p.s("strategy");
        let start_time = p.f("t");
        let range_method = p.s("range_method");
        let new_sid = sha1_16(&format!("{}{}", url, now_ms()));

        // Follow redirects once up front and make sure the final host is public too.
        let resolved = probe::resolve_url(app, &url, 5, if range_method == "get-range" { "GET" } else { "HEAD" }).await;
        if resolved != url && !ssrf::host_is_public(&resolved).await {
            return err(StatusCode::FORBIDDEN, "url not allowed");
        }
        logi!("[LIVE:{}] original: {}", new_sid, prefix(&crate::probe::redact(&url), 80));

        let active = app.session_count();
        if active >= cfg.max_sessions {
            logw!("[LIVE:{}] rejected: {} active sessions (max {})", new_sid, active, cfg.max_sessions);
            let mut r = json_resp(StatusCode::SERVICE_UNAVAILABLE, json!({ "error": "too many active sessions", "active": active, "max": cfg.max_sessions }));
            r.headers_mut().insert("retry-after", HeaderValue::from_static("10"));
            return r;
        }

        let session = Session::new(
            app.clone(),
            NewSession {
                sid: new_sid.clone(),
                url: url.clone(),
                audio_track: parse_int(&audio_track).unwrap_or(0),
                audio_bitrate: p.s_or("bitrate", "128k"),
                start_time,
                file_size: p.i("filesize").max(0) as u64,
                duration: p.f("duration"),
                strategy: strategy.clone(),
            },
        );

        // Codec details from the player's probe (JSON body), with URL parameters as fallback.
        let no = json!({});
        let probe_v = p.get("probe").filter(|v| v.is_object()).unwrap_or(&no);
        let streams: Vec<Value> = probe_v.get("streams").and_then(|v| v.as_array()).cloned().unwrap_or_default();
        let audios: Vec<Value> = probe_v.get("audio_streams").and_then(|v| v.as_array()).cloned().unwrap_or_default();
        let format = probe_v.get("format").cloned().unwrap_or_else(|| json!({}));
        let v_stream = streams.iter().find(|s| s["codec_type"] == "video").cloned().unwrap_or_else(|| json!({}));
        let a_stream = audios
            .get(parse_int(&audio_track).unwrap_or(0).max(0) as usize)
            .or_else(|| audios.first())
            .or_else(|| streams.iter().find(|s| s["codec_type"] == "audio"))
            .cloned()
            .unwrap_or_else(|| json!({}));
        let client_ip = headers.get("x-forwarded-for").and_then(|v| v.to_str().ok()).and_then(|v| v.split(',').next()).map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).unwrap_or_else(|| addr.ip().to_string());
        let cached = app.probe_cache_take(&url);
        {
            let mut i = session.inner.lock().unwrap();
            i.url = resolved;
            i.range_method = range_method;
            i.client_ip = client_ip.clone();
            i.source_video_codec = value_str_or_empty(js_or(v_stream.get("codec_name"), p.get("vcodec")));
            i.source_audio_codec = value_str_or_empty(js_or(js_or(a_stream.get("codec_name"), a_stream.get("codec")), p.get("acodec")));
            let (w, h) = (value_int(v_stream.get("width")).unwrap_or(0), value_int(v_stream.get("height")).unwrap_or(0));
            i.resolution = if w != 0 && h != 0 { format!("{}x{}", w, h) } else { p.s("resolution") };
            if !i.resolution.is_empty() && cfg.max_height > 0 && strategy != "hls-copy" {
                let mut it = i.resolution.split('x').map(|x| parse_int(x).unwrap_or(0));
                let (rw, rh) = (it.next().unwrap_or(0), it.next().unwrap_or(0));
                if rh > cfg.max_height {
                    let out_w = (crate::util::js_round(rw as f64 * (cfg.max_height as f64 / rh as f64) / 2.0) * 2.0) as i64;
                    i.output_resolution = format!("{}x{}", out_w, cfg.max_height);
                }
            }
            let int = |v: Option<&Value>| value_int(v).filter(|x| *x != 0);
            i.source_video_bitrate = int(v_stream.get("bit_rate")).or_else(|| int(format.get("bit_rate"))).unwrap_or(0);
            i.source_audio_bitrate = int(js_or(a_stream.get("bit_rate"), a_stream.get("bitrate"))).unwrap_or(0);
            i.source_video_profile = js_or(v_stream.get("profile"), None).cloned().unwrap_or_else(|| json!(""));
            i.source_video_level = js_or(v_stream.get("level"), None).cloned().unwrap_or_else(|| json!(0));
            i.source_pix_fmt = value_str_or_empty(v_stream.get("pix_fmt"));
            i.source_audio_channels = value_int(a_stream.get("channels")).unwrap_or(0);
            i.source_audio_sample_rate = js_or(a_stream.get("sample_rate"), None).cloned().unwrap_or_else(|| json!(""));
            i.source_has_b_frames = js_or(v_stream.get("has_b_frames"), None).cloned().unwrap_or_else(|| json!(0));
            i.source_format_name = value_str_or_empty(format.get("format_name"));
            i.audio_track_count = if audios.is_empty() { 1 } else { audios.len() as i64 };
            i.audio_track_labels = audios
                .iter()
                .enumerate()
                .map(|(k, a)| {
                    json!({
                        "index": k,
                        "lang": value_str_or_empty(js_or(a.get("language"), a.get("lang"))),
                        "name": value_str_or_empty(js_or(a.get("title"), a.get("name"))),
                        "codec": value_str_or_empty(js_or(a.get("codec_name"), a.get("codec"))),
                        "channels": value_int(a.get("channels")).unwrap_or(0),
                    })
                })
                .collect();
            // Bytes downloaded while probing go to ffmpeg first (only when starting at 0).
            let mut cache_note = None;
            if let Some(c) = cached {
                if !c.data.is_empty() && start_time == 0.0 {
                    logi!("[LIVE:{}] probe cache HIT: {}B pre-downloaded", new_sid, c.data.len());
                    cache_note = Some(format!("probe cache: {}B pre-downloaded", c.data.len()));
                    i.probe_cached_data = Some(c.data);
                }
            }
            let kbps = if i.source_video_bitrate > 0 { format!("{}kbps", crate::util::js_round(i.source_video_bitrate as f64 / 1000.0) as i64) } else { "?".to_string() };
            let line = format!(
                "session started from {}: {} | {}/{} {} @ {}",
                client_ip,
                if strategy.is_empty() { "transcode-audio" } else { &strategy },
                i.source_video_codec,
                i.source_audio_codec,
                i.resolution,
                kbps
            );
            let now = now_ms();
            if let Some(n) = cache_note {
                i.logs.push_back((now, "info", n));
            }
            i.logs.push_back((now, "info", line));
            if i.file_size > 0 {
                logi!("[LIVE:{}] fileSize={}, duration={} (byte-range seek enabled)", new_sid, i.file_size, i.duration);
            }
        }
        app.sessions.lock().unwrap().insert(new_sid.clone(), session.clone());
        let mut guard = StartGuard { app: app.clone(), sid: new_sid.clone(), done: false };
        session.spawn_ffmpeg(start_time);

        // One segment is enough for the playlist to be playable.
        let timeout = if is_video_transcode(&strategy) { cfg.startup_timeout_transcode } else { cfg.startup_timeout_copy };
        session.wait_for_segments(1, timeout).await;

        let (nsegs, track_count, labels) = {
            let i = session.inner.lock().unwrap();
            (i.segments.len(), i.audio_track_count, i.audio_track_labels.clone())
        };
        if nsegs == 0 {
            guard.done = true;
            app.remove_session(&new_sid);
            session.stop();
            logi!("[LIVE:{}] FFmpeg produced 0 segments - returning error", new_sid);
            return json_resp(StatusCode::INTERNAL_SERVER_ERROR, json!({ "status": "error", "error": "FFmpeg failed to produce segments" }));
        }
        guard.done = true;
        let mut out = json!({ "status": "started", "sid": new_sid, "playlist": format!("{}?sid={}&live_seg=playlist.m3u8", base, new_sid) });
        if track_count > 1 {
            out["audioTracks"] = Value::Array(labels);
        }
        return json_resp(StatusCode::OK, out);
    }

    let has_sid = !sid.is_empty();
    let not_found = || err(StatusCode::NOT_FOUND, "session not found");
    let m3u8 = |text: String| bytes_resp(M3U8, "no-store", text.into_bytes());

    // ── Playlist (master playlist when the source has several audio tracks) ──
    if has_sid && live_seg == "playlist.m3u8" {
        let session = match app.session(&sid) {
            Some(s) => s,
            None => return not_found(),
        };
        session.touch();
        let (n, state) = {
            let i = session.inner.lock().unwrap();
            (i.segments.len(), i.state)
        };
        if n == 0 && state != "stopped" && state != "ended" {
            session.wait_for_segments(1, 5000).await;
        }
        let (n, state) = {
            let i = session.inner.lock().unwrap();
            (i.segments.len(), i.state)
        };
        if n == 0 {
            if state == "ended" {
                return err(StatusCode::INTERNAL_SERVER_ERROR, "FFmpeg ended with no segments");
            }
            let mut r = (StatusCode::SERVICE_UNAVAILABLE, "Buffering — segments not ready yet").into_response();
            r.headers_mut().insert("content-type", HeaderValue::from_static("text/html; charset=utf-8"));
            r.headers_mut().insert("retry-after", HeaderValue::from_static("2"));
            return r;
        }
        session.main_playlist_touch();
        if session.is_multi_audio() {
            return m3u8(session.master_playlist(&base));
        }
        return m3u8(session.playlist(&base));
    }

    // ── Video playlist of a multi-audio session ──
    if has_sid && live_seg == "main.m3u8" {
        return match app.session(&sid) {
            Some(s) => m3u8(s.playlist(&base)),
            None => not_found(),
        };
    }

    // ── Audio track playlist / init / segment ──
    if has_sid {
        if let Some(track) = audio_index(&live_seg, ".m3u8") {
            return match app.session(&sid) {
                Some(s) => {
                    s.touch();
                    m3u8(s.audio_playlist(track, &base))
                }
                None => not_found(),
            };
        }
        if let Some(track) = audio_index(&live_seg, "_init.mp4") {
            let s = match app.session(&sid) {
                Some(s) => s,
                None => return empty(StatusCode::NOT_FOUND),
            };
            if track >= s.inner.lock().unwrap().audio_outputs.len() {
                return empty(StatusCode::NOT_FOUND);
            }
            return match s.read_file(&format!("a{}/init.mp4", track)) {
                Some(d) => {
                    s.touch();
                    bytes_resp("video/mp4", "max-age=3600", d)
                }
                None => empty(StatusCode::NOT_FOUND),
            };
        }
        if let Some((track, seq)) = audio_segment(&live_seg) {
            let s = match app.session(&sid) {
                Some(s) => s,
                None => return empty(StatusCode::NOT_FOUND),
            };
            if track >= s.inner.lock().unwrap().audio_outputs.len() {
                return empty(StatusCode::NOT_FOUND);
            }
            s.touch();
            return match s.read_file(&format!("a{}/{}.m4s", track, seq)) {
                Some(d) => bytes_resp("video/mp4", "no-store", d),
                None => empty(StatusCode::NOT_FOUND),
            };
        }
    }

    // ── Init segment ──
    if has_sid && live_seg == "init.mp4" {
        let s = match app.session(&sid) {
            Some(s) => s,
            None => return empty(StatusCode::NOT_FOUND),
        };
        return match s.read_file("init.mp4") {
            Some(d) => {
                s.touch();
                bytes_resp("video/mp4", "max-age=3600", d)
            }
            None => empty(StatusCode::NOT_FOUND),
        };
    }

    // ── Media segment N.m4s ──
    if has_sid && live_seg.ends_with(".m4s") {
        let s = match app.session(&sid) {
            Some(s) => s,
            None => return empty(StatusCode::NOT_FOUND),
        };
        let seq = match parse_int(&live_seg).filter(|v| *v >= 0) {
            Some(v) => v as u64,
            None => return empty(StatusCode::NOT_FOUND),
        };
        let name = format!("{}.m4s", seq);
        let mut data = s.read_file(&name);
        if data.is_none() {
            // Not produced yet: wait for ffmpeg (the wait ends early if the player hangs up,
            // because this future is dropped with the connection).
            let state = s.inner.lock().unwrap().state;
            if state == "starting" || state == "running" || state == "seeking" {
                s.wait_for_segment_seq(seq).await;
                data = s.read_file(&name);
            }
        }
        return match data {
            Some(d) => {
                s.mark_consumed(seq);
                bytes_resp("video/mp4", "no-store", d)
            }
            None => empty(StatusCode::NOT_FOUND),
        };
    }

    // ── Seek: same session, ffmpeg restarted at the new position ──
    if has_sid && p.has("live_seek") {
        let s = match app.session(&sid) {
            Some(s) => s,
            None => return not_found(),
        };
        let seek_time = p.f("live_seek");
        logi!("[LIVE:{}] seek to {:.1}s", sid, seek_time);
        let gen = s.inner.lock().unwrap().seek_gen + 1;
        s.seek(seek_time);
        let strat = s.inner.lock().unwrap().strategy.clone();
        let timeout = if is_video_transcode(&strat) { cfg.seek_timeout_transcode } else { cfg.seek_timeout_copy };
        s.wait_for_segments(1, timeout).await;
        let superseded = |s: &Arc<Session>| s.inner.lock().unwrap().seek_gen != gen;
        if superseded(&s) {
            logi!("[LIVE:{}] seek gen {} superseded - aborting", sid, gen);
            return json_resp(StatusCode::OK, json!({ "status": "superseded", "time": crate::util::num(seek_time), "sid": sid }));
        }
        // Nothing came out (e.g. a bad Cues offset): retry from a position estimated from the
        // file size, scanning for a cluster boundary.
        let empty_now = s.inner.lock().unwrap().segments.is_empty();
        if empty_now && seek_time > 0.0 {
            logw!("[LIVE:{}] seek produced 0 segments - retrying with linear estimation", sid);
            let (header, off) = {
                let i = s.inner.lock().unwrap();
                let off = if i.duration > 0.0 && i.file_size > 0 { ((seek_time / i.duration) * i.file_size as f64).floor().max(0.0) as u64 } else { 0 };
                (i.file_header.clone(), off)
            };
            let mode = match header {
                Some(h) if off > 0 => HeaderMode::Scan(h),
                _ => HeaderMode::None,
            };
            s.launch_piped(mode, off, seek_time, None);
            s.wait_for_segments(1, timeout).await;
            if superseded(&s) {
                return json_resp(StatusCode::OK, json!({ "status": "superseded", "time": crate::util::num(seek_time), "sid": sid }));
            }
        }
        let ready = s.inner.lock().unwrap().segments.len();
        logi!("[LIVE:{}] seek ready: {} segments after wait", sid, ready);
        return json_resp(
            StatusCode::OK,
            json!({ "status": "seeked", "time": crate::util::num(seek_time), "sid": sid, "segments": ready, "playlist": format!("{}?sid={}&live_seg=playlist.m3u8", base, sid) }),
        );
    }

    // ── Pause / resume / stop ──
    if has_sid && p.truthy("live_pause") {
        return match app.session(&sid) {
            Some(s) => {
                s.pause();
                json_resp(StatusCode::OK, json!({ "status": "paused", "sid": sid }))
            }
            None => json_resp(StatusCode::OK, json!({ "status": "not_found" })),
        };
    }
    if has_sid && p.truthy("live_resume") {
        return match app.session(&sid) {
            Some(s) => {
                if s.resume() {
                    json_resp(StatusCode::OK, json!({ "status": "resumed", "sid": sid }))
                } else {
                    json_resp(StatusCode::OK, json!({ "status": "not_paused" }))
                }
            }
            None => json_resp(StatusCode::OK, json!({ "status": "not_found" })),
        };
    }
    if has_sid && p.truthy("live_stop") {
        if let Some(s) = app.remove_session(&sid) {
            s.stop();
        }
        return json_resp(StatusCode::OK, json!({ "status": "stopped" }));
    }

    err(StatusCode::BAD_REQUEST, "invalid transcode request")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audio_names() {
        assert_eq!(audio_index("a0.m3u8", ".m3u8"), Some(0));
        assert_eq!(audio_index("a12.m3u8", ".m3u8"), Some(12));
        assert_eq!(audio_index("a.m3u8", ".m3u8"), None);
        assert_eq!(audio_index("a1x.m3u8", ".m3u8"), None);
        assert_eq!(audio_index("a1_init.mp4", "_init.mp4"), Some(1));
        assert_eq!(audio_index("main.m3u8", ".m3u8"), None);
        assert_eq!(audio_segment("a1_25.m4s"), Some((1, 25)));
        assert_eq!(audio_segment("a1_.m4s"), None);
        assert_eq!(audio_segment("a../x_1.m4s"), None);
        assert_eq!(audio_segment("7.m4s"), None);
    }

    #[test]
    fn params_merge_query_and_body() {
        let uri: Uri = "/transcode?url=http%3A%2F%2Fh%2Fa.mkv&live_start=1&audio=1&t=12.5&brief".parse().unwrap();
        let mut h = HeaderMap::new();
        h.insert("content-type", HeaderValue::from_static("text/plain;charset=UTF-8"));
        let p = Params::parse(&uri, &h, br#"{"probe":{"streams":[]},"audio":2}"#);
        assert_eq!(p.s("url"), "http://h/a.mkv");
        assert!(p.truthy("live_start"));
        assert_eq!(p.i("audio"), 2, "the body wins over the query");
        assert_eq!(p.f("t"), 12.5);
        assert!(p.has("brief") && !p.truthy("brief"));
        assert!(p.get("probe").unwrap().is_object());
        assert_eq!(p.s_or("bitrate", "128k"), "128k");
        // a non-JSON text body is ignored
        let p = Params::parse(&uri, &h, b"hello");
        assert_eq!(p.i("audio"), 1);
        // and so is JSON without a JSON or text content type
        let p = Params::parse(&uri, &HeaderMap::new(), br#"{"audio":5}"#);
        assert_eq!(p.i("audio"), 1);
    }
}
