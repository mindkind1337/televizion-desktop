//! CPU and memory figures for the stats page: this process, each ffmpeg, and the whole system.
//! Linux reads /proc; Windows calls kernel32 directly (no dependency); elsewhere zeros.

use std::collections::HashMap;

#[derive(Clone, Copy, Default, Debug)]
pub struct ProcSample {
    /// User + kernel CPU time, microseconds.
    pub cpu_us: u64,
    /// Resident memory, bytes.
    pub rss: u64,
}

#[derive(Clone, Copy, Default, Debug)]
pub struct SysSample {
    /// Total and idle CPU time over all cores, arbitrary but consistent units.
    pub cpu_total: u64,
    pub cpu_idle: u64,
    pub total_mem: u64,
    pub free_mem: u64,
    pub load: [f64; 3],
}

#[cfg(target_os = "linux")]
mod imp {
    use super::*;

    pub fn process(pid: u32) -> Option<ProcSample> {
        let stat = std::fs::read_to_string(format!("/proc/{}/stat", pid)).ok()?;
        // Fields after the ")" that closes the command name: state is field 3.
        let rest = &stat[stat.rfind(')')? + 1..];
        let f: Vec<&str> = rest.split_whitespace().collect();
        let utime: u64 = f.get(11)?.parse().ok()?;
        let stime: u64 = f.get(12)?.parse().ok()?;
        let rss_pages: u64 = f.get(21)?.parse().ok()?;
        // USER_HZ is 100 and pages are 4 KiB on every Linux this runs on.
        Some(ProcSample { cpu_us: (utime + stime) * 10_000, rss: rss_pages * 4096 })
    }

    pub fn system() -> SysSample {
        let mut s = SysSample::default();
        if let Ok(stat) = std::fs::read_to_string("/proc/stat") {
            if let Some(line) = stat.lines().next() {
                let v: Vec<u64> = line.split_whitespace().skip(1).filter_map(|x| x.parse().ok()).collect();
                s.cpu_total = v.iter().take(8).sum();
                s.cpu_idle = v.get(3).copied().unwrap_or(0);
            }
        }
        if let Ok(mem) = std::fs::read_to_string("/proc/meminfo") {
            for line in mem.lines() {
                let kb = || line.split_whitespace().nth(1).and_then(|x| x.parse::<u64>().ok()).unwrap_or(0) * 1024;
                if line.starts_with("MemTotal:") {
                    s.total_mem = kb();
                } else if line.starts_with("MemAvailable:") {
                    s.free_mem = kb();
                }
            }
        }
        if let Ok(l) = std::fs::read_to_string("/proc/loadavg") {
            for (i, x) in l.split_whitespace().take(3).enumerate() {
                s.load[i] = x.parse().unwrap_or(0.0);
            }
        }
        s
    }
}

#[cfg(windows)]
mod imp {
    use super::*;
    use std::ffi::c_void;
    type Handle = *mut c_void;

    #[repr(C)]
    #[derive(Default, Clone, Copy)]
    struct FileTime {
        low: u32,
        high: u32,
    }
    impl FileTime {
        /// 100-nanosecond units.
        fn ticks(&self) -> u64 {
            ((self.high as u64) << 32) | self.low as u64
        }
    }
    #[repr(C)]
    #[derive(Default)]
    struct ProcessMemoryCounters {
        cb: u32,
        page_fault_count: u32,
        peak_working_set_size: usize,
        working_set_size: usize,
        quota_peak_paged_pool_usage: usize,
        quota_paged_pool_usage: usize,
        quota_peak_non_paged_pool_usage: usize,
        quota_non_paged_pool_usage: usize,
        pagefile_usage: usize,
        peak_pagefile_usage: usize,
    }
    #[repr(C)]
    #[derive(Default)]
    struct MemoryStatusEx {
        length: u32,
        memory_load: u32,
        total_phys: u64,
        avail_phys: u64,
        total_page_file: u64,
        avail_page_file: u64,
        total_virtual: u64,
        avail_virtual: u64,
        avail_extended_virtual: u64,
    }
    const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;

    #[link(name = "kernel32")]
    extern "system" {
        fn OpenProcess(access: u32, inherit: i32, pid: u32) -> Handle;
        fn CloseHandle(h: Handle) -> i32;
        fn GetProcessTimes(h: Handle, creation: *mut FileTime, exit: *mut FileTime, kernel: *mut FileTime, user: *mut FileTime) -> i32;
        fn K32GetProcessMemoryInfo(h: Handle, counters: *mut ProcessMemoryCounters, cb: u32) -> i32;
        fn GetSystemTimes(idle: *mut FileTime, kernel: *mut FileTime, user: *mut FileTime) -> i32;
        fn GlobalMemoryStatusEx(status: *mut MemoryStatusEx) -> i32;
    }

    pub fn process(pid: u32) -> Option<ProcSample> {
        // SAFETY: Win32 calls with valid out-pointers to correctly laid out structures; the
        // handle is closed before returning.
        unsafe {
            let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if h.is_null() {
                return None;
            }
            let (mut c, mut e, mut k, mut u) = (FileTime::default(), FileTime::default(), FileTime::default(), FileTime::default());
            let ok_times = GetProcessTimes(h, &mut c, &mut e, &mut k, &mut u) != 0;
            let mut pmc = ProcessMemoryCounters::default();
            pmc.cb = std::mem::size_of::<ProcessMemoryCounters>() as u32;
            let ok_mem = K32GetProcessMemoryInfo(h, &mut pmc, pmc.cb) != 0;
            CloseHandle(h);
            if !ok_times {
                return None;
            }
            Some(ProcSample { cpu_us: (k.ticks() + u.ticks()) / 10, rss: if ok_mem { pmc.working_set_size as u64 } else { 0 } })
        }
    }

    pub fn system() -> SysSample {
        let mut s = SysSample::default();
        // SAFETY: as above.
        unsafe {
            let (mut idle, mut kernel, mut user) = (FileTime::default(), FileTime::default(), FileTime::default());
            if GetSystemTimes(&mut idle, &mut kernel, &mut user) != 0 {
                // Kernel time includes idle time.
                s.cpu_total = kernel.ticks() + user.ticks();
                s.cpu_idle = idle.ticks();
            }
            let mut m = MemoryStatusEx::default();
            m.length = std::mem::size_of::<MemoryStatusEx>() as u32;
            if GlobalMemoryStatusEx(&mut m) != 0 {
                s.total_mem = m.total_phys;
                s.free_mem = m.avail_phys;
            }
        }
        s
    }
}

#[cfg(not(any(target_os = "linux", windows)))]
mod imp {
    use super::*;
    pub fn process(_pid: u32) -> Option<ProcSample> {
        None
    }
    pub fn system() -> SysSample {
        SysSample::default()
    }
}

pub use imp::{process, system};

pub fn cpu_cores() -> usize {
    std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1)
}

#[derive(Clone, Copy, Default)]
pub struct ProcStats {
    /// 0–100 per core in use (two busy cores = 200).
    pub cpu: f64,
    pub memory: u64,
}

/// Remembers the previous sample of each process so CPU can be reported as a percentage.
#[derive(Default)]
pub struct Monitor {
    prev: HashMap<u32, (u64, u64)>,
    cache: HashMap<u32, ProcStats>,
    last_poll: u64,
    prev_sys: Option<(u64, u64)>,
    sys_cpu_pct: f64,
}

impl Monitor {
    /// Samples the given processes, at most once every 1.5 s.
    pub fn poll(&mut self, pids: &[u32], now_ms: u64) {
        if now_ms.saturating_sub(self.last_poll) < 1500 {
            return;
        }
        self.last_poll = now_ms;
        for &pid in pids {
            if let Some(s) = process(pid) {
                let mut cpu = self.cache.get(&pid).map(|c| c.cpu).unwrap_or(0.0);
                if let Some(&(prev_us, prev_ms)) = self.prev.get(&pid) {
                    let wall_us = now_ms.saturating_sub(prev_ms) * 1000;
                    if wall_us > 0 {
                        cpu = s.cpu_us.saturating_sub(prev_us) as f64 / wall_us as f64 * 100.0;
                    }
                }
                self.prev.insert(pid, (s.cpu_us, now_ms));
                self.cache.insert(pid, ProcStats { cpu, memory: s.rss });
            }
        }
        self.prev.retain(|pid, _| pids.contains(pid));
        self.cache.retain(|pid, _| pids.contains(pid));
    }

    pub fn get(&self, pid: u32) -> Option<ProcStats> {
        self.cache.get(&pid).copied()
    }

    /// System-wide CPU use (0–100) since the previous call.
    pub fn system_cpu_pct(&mut self, s: &SysSample) -> f64 {
        if let Some((pt, pi)) = self.prev_sys {
            let dt = s.cpu_total.saturating_sub(pt);
            let di = s.cpu_idle.saturating_sub(pi);
            if dt > 0 {
                self.sys_cpu_pct = crate::util::js_round((1.0 - di as f64 / dt as f64) * 100.0).clamp(0.0, 100.0);
            }
        }
        if s.cpu_total > 0 {
            self.prev_sys = Some((s.cpu_total, s.cpu_idle));
        }
        self.sys_cpu_pct
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg(any(target_os = "linux", windows))]
    fn own_process_is_readable() {
        let s = process(std::process::id()).expect("own process stats");
        assert!(s.rss > 0);
        let sys = system();
        assert!(sys.total_mem > 0 && sys.cpu_total > 0);
        assert!(cpu_cores() >= 1);
    }

    #[test]
    fn cpu_percent_from_two_samples() {
        let mut m = Monitor::default();
        let pid = std::process::id();
        m.poll(&[pid], 10_000);
        // burn a little CPU so the second sample differs
        let mut x = 0u64;
        for i in 0..20_000_000u64 {
            x = x.wrapping_add(i * i);
        }
        assert!(x != 1);
        m.poll(&[pid], 10_500); // too soon: ignored
        m.poll(&[pid], 12_000);
        if let Some(s) = m.get(pid) {
            assert!(s.cpu >= 0.0);
            assert!(s.memory > 0);
        }
        m.poll(&[], 20_000);
        assert!(m.get(pid).is_none());
    }
}
