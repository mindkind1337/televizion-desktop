//! HTTP download of the source file, pulled chunk by chunk by whoever feeds ffmpeg.
//!
//! ffmpeg never fetches the source itself: providers answer with redirects that carry short-lived
//! tokens, and ffmpeg's own reconnect cannot re-resolve them. This downloader does: it always
//! sends a Range header, follows redirects on every (re)connect and resumes from the byte it
//! reached. "Pausing" is simply not asking for the next chunk (TCP back-pressure does the rest).

use bytes::Bytes;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// What the stats page shows about a download, readable while the download runs elsewhere.
#[derive(Default)]
pub struct DlShared {
    pub paused: AtomicBool,
    pub bytes_read: AtomicU64,
    pub offset: AtomicU64,
    pub retries: AtomicU32,
    pub destroyed: AtomicBool,
}

pub enum DlEvent {
    Data(Bytes),
    End,
    Error(String),
}

pub struct Downloader {
    client: reqwest::Client,
    user_agent: String,
    pub url: String,
    /// Next byte to request (start offset + bytes received).
    pub offset: u64,
    pub bytes_read: u64,
    /// Status of the first response (206 = the server honours Range).
    pub status_code: u16,
    pub range_supported: bool,
    /// Total size when known: an early end of stream then triggers a reconnect.
    pub file_size: u64,
    pub max_retries: u32,
    pub label: String,
    retries: u32,
    resp: Option<reqwest::Response>,
    /// Set by the owner when it held the download back; an upstream timeout is then expected.
    pub was_paused: bool,
    /// Bytes still to discard: the server ignored Range and is sending the file from byte 0.
    skip: u64,
    pub shared: Arc<DlShared>,
}

impl Drop for Downloader {
    fn drop(&mut self) {
        self.shared.destroyed.store(true, Ordering::Relaxed);
    }
}

fn total_from_content_range(v: &str) -> Option<u64> {
    // "bytes X-Y/TOTAL"
    let total = v.rsplit('/').next()?;
    if total.is_empty() || !total.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    total.parse().ok()
}

impl Downloader {
    pub fn new(client: reqwest::Client, user_agent: &str, url: &str, offset: u64, label: &str, max_retries: u32, file_size: u64) -> Self {
        let shared = Arc::new(DlShared::default());
        shared.offset.store(offset, Ordering::Relaxed);
        Downloader {
            client,
            user_agent: user_agent.to_string(),
            url: url.to_string(),
            offset,
            bytes_read: 0,
            status_code: 0,
            range_supported: false,
            file_size,
            max_retries,
            label: label.to_string(),
            retries: 0,
            resp: None,
            was_paused: false,
            skip: 0,
            shared,
        }
    }

    /// Opens the connection if needed. Ok(false) = the server said 416 (already at the end).
    async fn connect(&mut self) -> Result<bool, (String, bool)> {
        // (message, retryable)
        let url = reqwest::Url::parse(&self.url).map_err(|e| (e.to_string(), false))?;
        if !crate::ssrf::url_target_ok(&url) {
            return Err(("SSRF blocked: non-public address".to_string(), false));
        }
        let req = self.client.get(url).header("user-agent", &self.user_agent).header("range", format!("bytes={}-", self.offset));
        // 10 s to connect (client setting) plus 15 s for the response headers.
        let resp = match tokio::time::timeout(Duration::from_secs(25), req.send()).await {
            Err(_) => return Err(("headers timeout".to_string(), true)),
            Ok(Err(e)) => return Err((describe(&e), true)),
            Ok(Ok(r)) => r,
        };
        let status = resp.status().as_u16();
        if self.status_code == 0 {
            self.status_code = status;
        }
        if status == 206 {
            self.range_supported = true;
        }
        if self.file_size == 0 {
            if let Some(t) = resp.headers().get("content-range").and_then(|v| v.to_str().ok()).and_then(total_from_content_range) {
                self.file_size = t;
            }
            if self.file_size == 0 && status == 200 {
                if let Some(cl) = resp.headers().get("content-length").and_then(|v| v.to_str().ok()).and_then(|v| v.parse::<u64>().ok()) {
                    if cl > 0 {
                        self.file_size = cl;
                    }
                }
            }
        }
        if status >= 400 {
            if status == 416 {
                crate::logi!("[{}] HTTP 416 at offset {} — already at end of file", self.label, self.offset);
                return Ok(false);
            }
            // 5xx (and rate limits like 509) may recover; 4xx will not.
            if status >= 500 {
                crate::loge!("[{}] server error {}, will retry", self.label, status);
                return Err((format!("HTTP {}", status), true));
            }
            return Err((format!("HTTP {}", status), false));
        }
        // Asked for bytes from an offset but got the whole file: drop what we already have.
        self.skip = if status == 200 && self.offset > 0 { self.offset } else { 0 };
        self.resp = Some(resp);
        self.retries = 0;
        self.shared.retries.store(0, Ordering::Relaxed);
        Ok(true)
    }

    /// Decides whether to try again after an error; sleeps before the retry.
    async fn retry_or(&mut self, err: String) -> Option<DlEvent> {
        self.resp = None;
        if !self.range_supported && self.offset > 0 {
            crate::loge!("[{}] can't reconnect at offset {}: server doesn't support Range", self.label, self.offset);
            return Some(DlEvent::Error(err));
        }
        if self.retries >= self.max_retries {
            crate::loge!("[{}] giving up after {} retries", self.label, self.retries);
            return Some(DlEvent::Error(err));
        }
        self.retries += 1;
        self.shared.retries.store(self.retries, Ordering::Relaxed);
        let delay = (1000 * self.retries as u64).min(5000);
        crate::logi!("[{}] reconnecting (retry {}/{}) from byte {} in {}ms", self.label, self.retries, self.max_retries, self.offset, delay);
        tokio::time::sleep(Duration::from_millis(delay)).await;
        None
    }

    /// The next chunk, the clean end of the file, or a final error (retries exhausted).
    pub async fn next(&mut self) -> DlEvent {
        loop {
            if self.resp.is_none() {
                match self.connect().await {
                    Ok(true) => {}
                    Ok(false) => return DlEvent::End,
                    Err((msg, retryable)) => {
                        if !retryable {
                            return DlEvent::Error(msg);
                        }
                        crate::loge!("[{}] connect error: {}", self.label, msg);
                        if let Some(ev) = self.retry_or(msg).await {
                            return ev;
                        }
                        continue;
                    }
                }
            }
            let resp = self.resp.as_mut().expect("connected");
            match resp.chunk().await {
                Ok(Some(mut chunk)) => {
                    if self.skip > 0 {
                        let n = (self.skip as usize).min(chunk.len());
                        self.skip -= n as u64;
                        if n == chunk.len() {
                            continue;
                        }
                        chunk = chunk.slice(n..);
                    }
                    self.was_paused = false;
                    self.bytes_read += chunk.len() as u64;
                    self.offset += chunk.len() as u64;
                    self.shared.bytes_read.store(self.bytes_read, Ordering::Relaxed);
                    self.shared.offset.store(self.offset, Ordering::Relaxed);
                    return DlEvent::Data(chunk);
                }
                Ok(None) => {
                    if self.file_size > 0 && self.offset < self.file_size {
                        crate::logi!("[{}] stream ended early at {}/{}, reconnecting", self.label, self.offset, self.file_size);
                        if self.was_paused {
                            self.retries = 0; // held back on purpose: the upstream timeout is expected
                        }
                        if let Some(ev) = self.retry_or("premature end".to_string()).await {
                            return ev;
                        }
                        continue;
                    }
                    self.resp = None;
                    return DlEvent::End;
                }
                Err(e) => {
                    let msg = describe(&e);
                    if self.was_paused {
                        self.retries = 0;
                    } else {
                        crate::loge!("[{}] download error: {}", self.label, msg);
                    }
                    if let Some(ev) = self.retry_or(msg).await {
                        return ev;
                    }
                }
            }
        }
    }
}

/// A short description of a client error. reqwest's own text includes the URL, and source URLs
/// can carry provider credentials, so only the kind and the underlying causes are kept.
pub fn describe(e: &reqwest::Error) -> String {
    use std::error::Error;
    let mut s = if e.is_timeout() {
        "timeout"
    } else if e.is_connect() {
        "connect error"
    } else if e.is_redirect() {
        "redirect error"
    } else if e.is_body() || e.is_decode() {
        "body error"
    } else {
        "request error"
    }
    .to_string();
    let mut src = e.source();
    while let Some(c) = src {
        let t = c.to_string();
        if !t.contains("http://") && !t.contains("https://") && !s.contains(&t) {
            s.push_str(": ");
            s.push_str(&t);
        }
        src = c.source();
    }
    s
}

/// GET bytes start..=end of a URL (redirects followed), at most `cap` bytes kept.
pub async fn fetch_range(client: &reqwest::Client, user_agent: &str, url: &str, start: u64, end: u64, cap: usize) -> Result<Vec<u8>, String> {
    let u = reqwest::Url::parse(url).map_err(|e| e.to_string())?;
    if !crate::ssrf::url_target_ok(&u) {
        return Err("SSRF blocked: non-public address".to_string());
    }
    let req = client.get(u).header("user-agent", user_agent).header("range", format!("bytes={}-{}", start, end));
    let mut resp = match tokio::time::timeout(Duration::from_secs(25), req.send()).await {
        Err(_) => return Err("timeout".to_string()),
        Ok(Err(e)) => return Err(describe(&e)),
        Ok(Ok(r)) => r,
    };
    let status = resp.status().as_u16();
    if status >= 400 {
        return Err(format!("HTTP {}", status));
    }
    // A server that ignores Range sends the file from byte 0: skip to the wanted offset.
    let mut skip = if status == 200 { start } else { 0 };
    let want = ((end - start + 1) as usize).min(cap);
    let mut out = Vec::new();
    while out.len() < want {
        match resp.chunk().await {
            Ok(Some(c)) => {
                let mut c = &c[..];
                if skip > 0 {
                    let n = (skip as usize).min(c.len());
                    skip -= n as u64;
                    c = &c[n..];
                }
                let room = want - out.len();
                out.extend_from_slice(&c[..c.len().min(room)]);
            }
            Ok(None) => break,
            Err(e) => {
                if out.is_empty() {
                    return Err(describe(&e));
                }
                break;
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[test]
    fn content_range_total() {
        assert_eq!(total_from_content_range("bytes 0-1/12345"), Some(12345));
        assert_eq!(total_from_content_range("bytes 0-1/*"), None);
        assert_eq!(total_from_content_range("nonsense"), None);
    }

    #[tokio::test]
    async fn loopback_sources_are_refused() {
        // The SSRF guard refuses a loopback literal before any connection is made.
        let c = crate::ssrf::build_clients();
        let mut d = Downloader::new(c.follow, "UA", "http://127.0.0.1:9/x.mkv", 0, "t", 1, 0);
        match d.next().await {
            DlEvent::Error(m) => assert!(m.contains("SSRF"), "{m}"),
            _ => panic!("expected an SSRF error"),
        }
    }

    /// A tiny HTTP server on loopback that serves `data` with Range support and drops the
    /// connection after `cut` bytes on the first request, to exercise resume.
    async fn serve(data: Vec<u8>, cut: usize) -> u16 {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let mut first = true;
            loop {
                let (mut sock, _) = match listener.accept().await {
                    Ok(x) => x,
                    Err(_) => return,
                };
                let mut buf = vec![0u8; 4096];
                let n = sock.read(&mut buf).await.unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]).to_ascii_lowercase();
                let start: usize = req.split("range: bytes=").nth(1).and_then(|r| r.split('-').next()).and_then(|v| v.trim().parse().ok()).unwrap_or(0);
                let body = &data[start.min(data.len())..];
                let head = format!("HTTP/1.1 206 Partial Content\r\nContent-Range: bytes {}-{}/{}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", start, data.len() - 1, data.len(), body.len());
                let _ = sock.write_all(head.as_bytes()).await;
                if first {
                    first = false;
                    let _ = sock.write_all(&body[..cut.min(body.len())]).await;
                    let _ = sock.shutdown().await;
                } else {
                    let _ = sock.write_all(body).await;
                    let _ = sock.shutdown().await;
                }
            }
        });
        port
    }

    #[tokio::test]
    async fn resumes_with_range_after_an_early_end() {
        let data: Vec<u8> = (0..50_000u32).map(|i| (i % 251) as u8).collect();
        let port = serve(data.clone(), 12_345).await;
        // A plain client: the production one refuses loopback, which is what the test server is.
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let mut d = Downloader::new(client, "UA", &format!("http://localhost:{}/f.mkv", port), 0, "t", 3, 0);
        let mut got = Vec::new();
        loop {
            match d.next().await {
                DlEvent::Data(b) => got.extend_from_slice(&b),
                DlEvent::End => break,
                DlEvent::Error(e) => panic!("download failed: {e}"),
            }
        }
        assert_eq!(got.len(), data.len());
        assert!(got == data, "resumed data must be contiguous");
        assert_eq!(d.status_code, 206);
        assert!(d.range_supported);
        assert_eq!(d.file_size, 50_000);
        assert_eq!(d.offset, 50_000);
        assert_eq!(d.shared.bytes_read.load(Ordering::Relaxed), 50_000);
    }
}
