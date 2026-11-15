//! Small helpers shared by every module: time, JS-compatible number handling, URL coding, logging.

use serde_json::Value;
use std::collections::VecDeque;
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

pub fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

/// `new Date().toISOString()`.
pub fn iso_now() -> String {
    let ms = now_ms();
    let secs = (ms / 1000) as i64;
    let days = secs.div_euclid(86400);
    let rem = secs.rem_euclid(86400);
    // civil-from-days (Howard Hinnant)
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z.rem_euclid(146097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z", y, m, d, rem / 3600, (rem % 3600) / 60, rem % 60, ms % 1000)
}

/// `String(number)` as JavaScript prints it for the values used here (30 → "30", 12.5 → "12.5").
pub fn js_num(v: f64) -> String {
    format!("{}", v)
}

/// `Math.round` (halves go up).
pub fn js_round(v: f64) -> f64 {
    (v + 0.5).floor()
}

/// A JSON number that prints like JavaScript does: whole values without a ".0".
pub fn num(v: f64) -> Value {
    if v.is_finite() && v == v.trunc() && v.abs() < 9.0e15 {
        Value::from(v as i64)
    } else if v.is_finite() {
        Value::from(v)
    } else {
        Value::Null
    }
}

/// `parseFloat`: the longest numeric prefix, or None (NaN).
pub fn parse_float(s: &str) -> Option<f64> {
    let t = s.trim_start();
    let b = t.as_bytes();
    let mut i = 0;
    if i < b.len() && (b[i] == b'+' || b[i] == b'-') {
        i += 1;
    }
    let ds = i;
    while i < b.len() && b[i].is_ascii_digit() {
        i += 1;
    }
    let mut digits = i - ds;
    if i < b.len() && b[i] == b'.' {
        let fs = i + 1;
        let mut j = fs;
        while j < b.len() && b[j].is_ascii_digit() {
            j += 1;
        }
        if j > fs || digits > 0 {
            digits += j - fs;
            i = j;
        }
    }
    if digits == 0 {
        return None;
    }
    if i < b.len() && (b[i] == b'e' || b[i] == b'E') {
        let mut j = i + 1;
        if j < b.len() && (b[j] == b'+' || b[j] == b'-') {
            j += 1;
        }
        let es = j;
        while j < b.len() && b[j].is_ascii_digit() {
            j += 1;
        }
        if j > es {
            i = j;
        }
    }
    t[..i].parse::<f64>().ok()
}

/// `parseInt(s, 10)`: the leading integer, or None (NaN).
pub fn parse_int(s: &str) -> Option<i64> {
    let t = s.trim_start();
    let b = t.as_bytes();
    let mut i = 0;
    if i < b.len() && (b[i] == b'+' || b[i] == b'-') {
        i += 1;
    }
    let ds = i;
    while i < b.len() && b[i].is_ascii_digit() {
        i += 1;
    }
    if i == ds {
        return None;
    }
    t[..i].parse::<i64>().ok().or(Some(i64::MAX))
}

/// `parseInt(value)` on a JSON value (numbers are truncated, strings parsed).
pub fn value_int(v: Option<&Value>) -> Option<i64> {
    match v? {
        Value::Number(n) => n.as_i64().or_else(|| n.as_f64().map(|f| f.trunc() as i64)),
        Value::String(s) => parse_int(s),
        _ => None,
    }
}

/// JavaScript truthiness of a JSON value.
pub fn truthy(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().map(|f| f != 0.0 && !f.is_nan()).unwrap_or(false),
        Some(Value::String(s)) => !s.is_empty(),
        Some(_) => true,
    }
}

/// `String(value)` for scalars; None for missing/null/objects.
pub fn value_str(v: Option<&Value>) -> Option<String> {
    match v? {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

/// `value || ''` as a string: empty when the value is falsy.
pub fn value_str_or_empty(v: Option<&Value>) -> String {
    if truthy(v) {
        value_str(v).unwrap_or_default()
    } else {
        String::new()
    }
}

pub fn hex(bytes: &[u8]) -> String {
    const H: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(H[(b >> 4) as usize] as char);
        s.push(H[(b & 15) as usize] as char);
    }
    s
}

/// First 16 hex characters of SHA-1(text), as the Node service names sessions and playlists.
pub fn sha1_16(text: &str) -> String {
    use sha1::{Digest, Sha1};
    let d = Sha1::digest(text.as_bytes());
    hex(&d)[..16].to_string()
}

/// `encodeURIComponent`.
pub fn encode_uri_component(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 3);
    for &b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'!' | b'~' | b'*' | b'\'' | b'(' | b')' => out.push(b as char),
            _ => {
                out.push('%');
                out.push_str(&format!("{:02X}", b));
            }
        }
    }
    out
}

fn hexval(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Query-string component decoding ('+' is a space, %XX bytes, lossy UTF-8).
pub fn decode_component(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' => {
                if i + 2 < b.len() {
                    if let (Some(h), Some(l)) = (hexval(b[i + 1]), hexval(b[i + 2])) {
                        out.push(h * 16 + l);
                        i += 3;
                        continue;
                    }
                }
                out.push(b'%');
                i += 1;
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Parses "a=1&b=2" into pairs (first value wins on repeats, like the code that consumes it expects).
pub fn parse_query(q: &str) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    for part in q.split('&') {
        if part.is_empty() {
            continue;
        }
        let (k, v) = match part.find('=') {
            Some(i) => (&part[..i], &part[i + 1..]),
            None => (part, ""),
        };
        let k = decode_component(k);
        if out.iter().any(|(ek, _)| *ek == k) {
            continue;
        }
        out.push((k, decode_component(v)));
    }
    out
}

/// Byte-safe prefix of a string (never splits a UTF-8 character).
pub fn prefix(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

pub fn contains_ci(hay: &str, needle: &str) -> bool {
    hay.to_ascii_lowercase().contains(&needle.to_ascii_lowercase())
}

// ── Global log ring (warnings and errors, shown on the stats page) ──

pub struct LogEntry {
    pub t: u64,
    pub level: &'static str,
    pub msg: String,
}

fn ring() -> &'static Mutex<VecDeque<LogEntry>> {
    static RING: OnceLock<Mutex<VecDeque<LogEntry>>> = OnceLock::new();
    RING.get_or_init(|| Mutex::new(VecDeque::new()))
}

pub fn global_log(level: &'static str, msg: &str) {
    if let Ok(mut r) = ring().lock() {
        r.push_back(LogEntry { t: now_ms(), level, msg: prefix(msg, 1000).to_string() });
        while r.len() > 500 {
            r.pop_front();
        }
    }
}

pub fn global_logs_json(last: usize) -> Value {
    let r = match ring().lock() {
        Ok(r) => r,
        Err(_) => return Value::Array(vec![]),
    };
    let skip = r.len().saturating_sub(last);
    Value::Array(
        r.iter()
            .skip(skip)
            .map(|e| serde_json::json!({ "t": e.t, "level": e.level, "msg": e.msg }))
            .collect(),
    )
}

// ── Console + log file ──
// Log text is kept ASCII: a Windows console or Notepad shows UTF-8 dashes and arrows as mojibake.

struct LogFile {
    file: std::fs::File,
    path: std::path::PathBuf,
    written: u64,
}
const LOG_CAP: u64 = 4 * 1024 * 1024;

fn log_file() -> &'static Mutex<Option<LogFile>> {
    static F: OnceLock<Mutex<Option<LogFile>>> = OnceLock::new();
    F.get_or_init(|| Mutex::new(None))
}

/// Opens (truncating) the log file. Called once at startup.
pub fn open_log_file(path: &std::path::Path) {
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(file) = std::fs::File::create(path) {
        *log_file().lock().unwrap() = Some(LogFile { file, path: path.to_path_buf(), written: 0 });
    }
}

pub fn to_ascii(s: &str) -> String {
    if s.is_ascii() {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            c if c.is_ascii() => out.push(c),
            '\u{2014}' | '\u{2013}' | '\u{2500}' | '\u{2550}' => out.push('-'),
            '\u{2192}' => out.push_str("->"),
            '\u{2713}' | '\u{2705}' => out.push_str("OK"),
            '\u{2026}' => out.push_str("..."),
            '\u{26A0}' | '\u{FE0F}' => {}
            _ => out.push('?'),
        }
    }
    out
}

fn to_file(line: &str) {
    use std::io::Write;
    let mut g = match log_file().lock() {
        Ok(g) => g,
        Err(_) => return,
    };
    if let Some(lf) = g.as_mut() {
        if lf.written > LOG_CAP {
            // Size cap: start over, keeping the most recent output.
            if let Ok(f) = std::fs::File::create(&lf.path) {
                lf.file = f;
                lf.written = 0;
                let _ = writeln!(lf.file, "[log] size cap reached, file restarted");
            }
        }
        let text = format!("{} {}\n", iso_now(), line);
        if lf.file.write_all(text.as_bytes()).is_ok() {
            lf.written += text.len() as u64;
        }
    }
}

pub fn out_line(s: &str) {
    use std::io::Write;
    let s = to_ascii(s);
    to_file(&s);
    let _ = writeln!(std::io::stdout(), "{}", s);
}

pub fn err_line(s: &str) {
    use std::io::Write;
    let s = to_ascii(s);
    to_file(&s);
    let _ = writeln!(std::io::stderr(), "{}", s);
}

#[macro_export]
macro_rules! logi {
    ($($a:tt)*) => { $crate::util::out_line(&format!($($a)*)) };
}
#[macro_export]
macro_rules! logw {
    ($($a:tt)*) => {{ let m = format!($($a)*); $crate::util::global_log("warn", &m); $crate::util::err_line(&m); }};
}
#[macro_export]
macro_rules! loge {
    ($($a:tt)*) => {{ let m = format!($($a)*); $crate::util::global_log("error", &m); $crate::util::err_line(&m); }};
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers_print_like_javascript() {
        assert_eq!(js_num(30.0), "30");
        assert_eq!(js_num(12.5), "12.5");
        assert_eq!(js_num(3.0 * 1_000_000.0), "3000000");
        assert_eq!(js_round(-10.0), -10.0);
        assert_eq!(js_round(2.5), 3.0);
        assert_eq!(num(5.0).to_string(), "5");
        assert_eq!(num(5.25).to_string(), "5.25");
    }

    #[test]
    fn parse_prefixes() {
        assert_eq!(parse_float("12.5abc"), Some(12.5));
        assert_eq!(parse_float(""), None);
        assert_eq!(parse_float("abc"), None);
        assert_eq!(parse_float("596.48"), Some(596.48));
        assert_eq!(parse_int("42.9"), Some(42));
        assert_eq!(parse_int("x"), None);
        assert_eq!(parse_int("7.m4s"), Some(7));
    }

    #[test]
    fn url_coding() {
        assert_eq!(encode_uri_component("http://a/b?c=d&e=f g"), "http%3A%2F%2Fa%2Fb%3Fc%3Dd%26e%3Df%20g");
        assert_eq!(decode_component("http%3A%2F%2Fa%2Fb+c%zz%4"), "http://a/b c%zz%4");
        let q = parse_query("url=http%3A%2F%2Fx%2Fy.mkv&live_start=1&brief");
        assert_eq!(q[0], ("url".to_string(), "http://x/y.mkv".to_string()));
        assert_eq!(q[2], ("brief".to_string(), "".to_string()));
    }

    #[test]
    fn log_text_is_ascii() {
        assert_eq!(to_ascii("a \u{2014} b \u{2192} c \u{2713} \u{e9}"), "a - b -> c OK ?");
        assert_eq!(to_ascii("plain"), "plain");
    }

    #[test]
    fn sha1_prefix() {
        assert_eq!(sha1_16("abc"), "a9993e364706816a");
    }
}
