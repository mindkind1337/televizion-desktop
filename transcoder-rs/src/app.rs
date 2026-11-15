//! Process-wide state shared by the routes, the sessions and the background sweepers.

use crate::config::Config;
use crate::ffargs::Encoders;
use crate::hw::SharedHw;
use crate::session::Session;
use crate::ssrf::Clients;
use crate::sysinfo::Monitor;
use bytes::Bytes;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;

pub const PROBE_CACHE_TTL_MS: u64 = 120_000;
pub const PROBE_CACHE_SIZE: usize = 5 * 1024 * 1024;
pub const PROBE_CACHE_MAX: usize = 8;

/// First bytes of a file, downloaded while probing, handed to ffmpeg when playback starts.
pub struct ProbeEntry {
    pub url: String,
    pub data: Bytes,
    pub ts: u64,
}

#[derive(Default)]
pub struct GpuState {
    pub stats: Value,
    /// (pid, used MiB, encoder %, decoder %)
    pub processes: Vec<(u32, u64, Option<u64>, Option<u64>)>,
    pub last_poll: u64,
    /// None = not looked for yet.
    pub has_nvidia: Option<bool>,
    pub polling: bool,
}

pub struct App {
    pub cfg: Config,
    /// Filled in by the background hardware detection; software encoders until then.
    pub enc: Arc<std::sync::RwLock<Encoders>>,
    pub hw: SharedHw,
    pub clients: Clients,
    pub ffmpeg_version: String,
    pub started: Instant,
    pub sessions: Mutex<HashMap<String, Arc<Session>>>,
    /// Oldest first; at most PROBE_CACHE_MAX entries.
    pub probe_cache: Mutex<Vec<ProbeEntry>>,
    /// /vod playlists by id: (m3u8, created at).
    pub playlists: Mutex<HashMap<String, (String, u64)>>,
    pub monitor: Mutex<Monitor>,
    pub gpu: Mutex<GpuState>,
    /// CPU time of this process at the previous stats call: (microseconds, at ms).
    pub prev_self_cpu: Mutex<(u64, u64)>,
}

impl App {
    pub fn session(&self, sid: &str) -> Option<Arc<Session>> {
        self.sessions.lock().unwrap().get(sid).cloned()
    }

    pub fn remove_session(&self, sid: &str) -> Option<Arc<Session>> {
        self.sessions.lock().unwrap().remove(sid)
    }

    pub fn session_count(&self) -> usize {
        self.sessions.lock().unwrap().len()
    }

    /// Stops every session and deletes its files (shutdown).
    pub fn stop_all(&self) {
        let all: Vec<Arc<Session>> = self.sessions.lock().unwrap().drain().map(|(_, s)| s).collect();
        for s in all {
            s.stop();
        }
        self.probe_cache.lock().unwrap().clear();
    }

    pub fn probe_cache_put(&self, url: &str, data: Bytes) {
        let mut c = self.probe_cache.lock().unwrap();
        c.retain(|e| e.url != url);
        c.push(ProbeEntry { url: url.to_string(), data, ts: crate::util::now_ms() });
        while c.len() > PROBE_CACHE_MAX {
            c.remove(0);
        }
    }

    /// Removes and returns the cached bytes for a URL (one-time use).
    pub fn probe_cache_take(&self, url: &str) -> Option<ProbeEntry> {
        let mut c = self.probe_cache.lock().unwrap();
        let i = c.iter().position(|e| e.url == url)?;
        Some(c.remove(i))
    }

    pub fn probe_cache_sweep(&self) {
        let now = crate::util::now_ms();
        self.probe_cache.lock().unwrap().retain(|e| now.saturating_sub(e.ts) <= PROBE_CACHE_TTL_MS);
    }
}
