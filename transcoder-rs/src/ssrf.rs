//! Who may call this service, and which hosts it may fetch from.
//!
//! - `from_allowed_site`: Origin/Referer, when present, must be one of our own sites.
//! - `host_is_public`: ffmpeg/ffprobe do their own DNS, so the target host is checked up front:
//!   every address it resolves to must be public.
//! - `SafeResolver`: the DNS resolver of our own HTTP client; it drops non-public addresses and
//!   fails when nothing public remains, on every redirect hop.

use axum::http::HeaderMap;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::Duration;

pub const ALLOWED_HOSTS: [&str; 2] = ["televizion.tv", "tv.afterliferp.ca"];

pub fn v4_is_public(ip: Ipv4Addr) -> bool {
    let [a, b, c, _d] = ip.octets();
    let n = u32::from(ip);
    let in_cidr = |net: [u8; 4], bits: u32| -> bool {
        let net = u32::from(Ipv4Addr::from(net));
        let mask = if bits == 0 { 0 } else { u32::MAX << (32 - bits) };
        (n & mask) == (net & mask)
    };
    if a == 0 || a == 10 || a == 127 {
        return false; // this-net, private, loopback
    }
    if a == 169 && b == 254 {
        return false; // link-local
    }
    if a == 172 && (16..=31).contains(&b) {
        return false; // private
    }
    if a == 192 && b == 168 {
        return false; // private
    }
    let _ = c;
    if in_cidr([100, 64, 0, 0], 10) // CGNAT
        || in_cidr([192, 0, 0, 0], 24) // IETF protocol
        || in_cidr([192, 0, 2, 0], 24) // TEST-NET-1
        || in_cidr([198, 18, 0, 0], 15) // benchmarking
        || in_cidr([198, 51, 100, 0], 24) // TEST-NET-2
        || in_cidr([203, 0, 113, 0], 24)
    // TEST-NET-3
    {
        return false;
    }
    if a >= 224 {
        return false; // multicast + reserved
    }
    true
}

pub fn v6_is_public(ip: Ipv6Addr) -> bool {
    let w = ip.segments();
    let v4 = |hi: u16, lo: u16| v4_is_public(Ipv4Addr::new((hi >> 8) as u8, (hi & 255) as u8, (lo >> 8) as u8, (lo & 255) as u8));
    if w[0] == 0 && w[1] == 0 && w[2] == 0 && w[3] == 0 && w[4] == 0 {
        // v4-mapped / v4-compatible (includes :: and ::1)
        return if w[5] == 0xffff || w[5] == 0 { v4(w[6], w[7]) } else { false };
    }
    if w[0] == 0x64 && w[1] == 0xff9b {
        return false; // NAT64 (may map to a private v4)
    }
    if w[0] == 0x2002 {
        return v4(w[1], w[2]); // 6to4 embeds a v4 address
    }
    if (w[0] & 0xffc0) == 0xfe80 {
        return false; // link-local
    }
    if (w[0] & 0xfe00) == 0xfc00 {
        return false; // unique-local
    }
    if (w[0] & 0xff00) == 0xff00 {
        return false; // multicast
    }
    true
}

pub fn ip_is_public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v) => v4_is_public(v),
        IpAddr::V6(v) => v6_is_public(v),
    }
}

pub fn scheme_ok(u: &str) -> bool {
    let l = u.get(..8).unwrap_or(u).to_ascii_lowercase();
    l.starts_with("http://") || l.starts_with("https://")
}

/// Every address the URL's host resolves to is public.
pub async fn host_is_public(u: &str) -> bool {
    let url = match reqwest::Url::parse(u) {
        Ok(x) => x,
        Err(_) => return false,
    };
    match url.host() {
        None => false,
        Some(url::Host::Ipv4(ip)) => v4_is_public(ip),
        Some(url::Host::Ipv6(ip)) => v6_is_public(ip),
        Some(url::Host::Domain(d)) => {
            if d.is_empty() {
                return false;
            }
            match tokio::net::lookup_host((d.to_string(), 0)).await {
                Ok(addrs) => {
                    let all: Vec<SocketAddr> = addrs.collect();
                    !all.is_empty() && all.iter().all(|a| ip_is_public(a.ip()))
                }
                Err(_) => false,
            }
        }
    }
}

/// Origin or Referer value: absent or "null" passes, anything else must be one of our sites.
pub fn host_allowed(header: Option<&str>) -> bool {
    match header {
        None => true,
        Some(v) if v.is_empty() || v == "null" => true,
        Some(v) => match reqwest::Url::parse(v) {
            Ok(u) => u.host_str().map(|h| ALLOWED_HOSTS.contains(&h.to_ascii_lowercase().as_str())).unwrap_or(false),
            Err(_) => false,
        },
    }
}

fn header<'a>(h: &'a HeaderMap, name: &str) -> Option<&'a str> {
    h.get(name).and_then(|v| v.to_str().ok())
}

pub fn from_allowed_site(h: &HeaderMap) -> bool {
    host_allowed(header(h, "origin")) && host_allowed(header(h, "referer"))
}

pub fn is_loopback(addr: &SocketAddr) -> bool {
    match addr.ip() {
        IpAddr::V4(v) => v == Ipv4Addr::LOCALHOST,
        IpAddr::V6(v) => v == Ipv6Addr::LOCALHOST || v.to_ipv4_mapped() == Some(Ipv4Addr::LOCALHOST),
    }
}

/// Desktop rule: only this computer (loopback) and only from our own site may make the service
/// fetch a new URL.
pub fn frontend_allowed(addr: &SocketAddr, h: &HeaderMap) -> bool {
    is_loopback(addr) && from_allowed_site(h)
}

// ── HTTP client plumbing ──

struct SafeResolver;

impl reqwest::dns::Resolve for SafeResolver {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        Box::pin(async move {
            let host = name.as_str().to_string();
            let addrs: Vec<SocketAddr> = tokio::net::lookup_host((host.as_str(), 0)).await?.collect();
            let safe: Vec<SocketAddr> = addrs.into_iter().filter(|a| ip_is_public(a.ip())).collect();
            if safe.is_empty() {
                let e: Box<dyn std::error::Error + Send + Sync> = format!("SSRF blocked: {} resolves only to non-public addresses", host).into();
                return Err(e);
            }
            let it: reqwest::dns::Addrs = Box::new(safe.into_iter());
            Ok(it)
        })
    }
}

fn literal_ip_blocked(url: &reqwest::Url) -> bool {
    match url.host() {
        Some(url::Host::Ipv4(ip)) => !v4_is_public(ip),
        Some(url::Host::Ipv6(ip)) => !v6_is_public(ip),
        _ => false,
    }
}

/// True when a URL may be requested by our own client (an IP literal skips DNS, so the resolver
/// above never sees it).
pub fn url_target_ok(url: &reqwest::Url) -> bool {
    (url.scheme() == "http" || url.scheme() == "https") && !literal_ip_blocked(url)
}

#[derive(Clone)]
pub struct Clients {
    /// Follows up to 5 redirects (downloads, range fetches).
    pub follow: reqwest::Client,
    /// Never follows redirects (the callers walk the hops themselves).
    pub manual: reqwest::Client,
}

pub fn build_clients() -> Clients {
    let base = || {
        reqwest::Client::builder()
            .dns_resolver(std::sync::Arc::new(SafeResolver))
            .connect_timeout(Duration::from_secs(10))
            .no_proxy()
    };
    let follow = base()
        .read_timeout(Duration::from_secs(30))
        .redirect(reqwest::redirect::Policy::custom(|attempt| {
            if attempt.previous().len() > 5 {
                attempt.error("too many redirects")
            } else if !url_target_ok(attempt.url()) {
                attempt.error("SSRF blocked: redirect to a non-public address")
            } else {
                attempt.follow()
            }
        }))
        .build()
        .expect("http client");
    let manual = base().redirect(reqwest::redirect::Policy::none()).build().expect("http client");
    Clients { follow, manual }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> bool {
        ip_is_public(s.parse().unwrap())
    }

    #[test]
    fn private_and_special_v4_are_refused() {
        for ip in ["0.1.2.3", "10.0.0.1", "127.0.0.1", "169.254.169.254", "172.16.0.1", "172.31.255.255", "192.168.1.1", "100.64.0.1", "100.127.255.255", "192.0.0.5", "192.0.2.1", "198.18.0.1", "198.19.255.255", "198.51.100.7", "203.0.113.9", "224.0.0.1", "255.255.255.255"] {
            assert!(!p(ip), "{ip} should be refused");
        }
        for ip in ["8.8.8.8", "1.1.1.1", "172.15.0.1", "172.32.0.1", "100.63.255.255", "100.128.0.1", "198.20.0.1", "83.149.95.28", "223.255.255.255"] {
            assert!(p(ip), "{ip} should be allowed");
        }
    }

    #[test]
    fn v6_spellings_are_caught() {
        for ip in ["::", "::1", "::ffff:127.0.0.1", "::ffff:7f00:1", "0:0:0:0:0:ffff:a00:1", "::10.0.0.1", "64:ff9b::808:808", "2002:7f00:1::", "2002:c0a8:101::1", "fe80::1", "febf::1", "fc00::1", "fd12:3456::1", "ff02::1", "::fffe:808:808"] {
            assert!(!p(ip), "{ip} should be refused");
        }
        for ip in ["2606:4700:4700::1111", "::ffff:8.8.8.8", "2002:808:808::1", "2001:db8::1"] {
            assert!(p(ip), "{ip} should be allowed");
        }
    }

    #[test]
    fn origin_and_referer() {
        assert!(host_allowed(None));
        assert!(host_allowed(Some("null")));
        assert!(host_allowed(Some("http://televizion.tv")));
        assert!(host_allowed(Some("https://TELEVIZION.tv:8443/browse/")));
        assert!(host_allowed(Some("https://tv.afterliferp.ca/x")));
        assert!(!host_allowed(Some("http://evil.example")));
        assert!(!host_allowed(Some("http://televizion.tv.evil.example")));
        assert!(!host_allowed(Some("not a url")));
    }

    #[test]
    fn loopback_only() {
        assert!(is_loopback(&"127.0.0.1:5".parse().unwrap()));
        assert!(is_loopback(&"[::1]:5".parse().unwrap()));
        assert!(is_loopback(&"[::ffff:127.0.0.1]:5".parse().unwrap()));
        assert!(!is_loopback(&"192.168.1.4:5".parse().unwrap()));
    }

    #[test]
    fn schemes_and_literal_targets() {
        assert!(scheme_ok("http://a/b"));
        assert!(scheme_ok("HTTPS://a/b"));
        assert!(!scheme_ok("file:///etc/passwd"));
        assert!(!scheme_ok("ftp://a"));
        assert!(!url_target_ok(&reqwest::Url::parse("http://127.0.0.1/x").unwrap()));
        assert!(!url_target_ok(&reqwest::Url::parse("http://[::1]/x").unwrap()));
        assert!(url_target_ok(&reqwest::Url::parse("http://example.com/x").unwrap()));
    }

    #[tokio::test]
    async fn literal_hosts_are_checked_without_dns() {
        assert!(!host_is_public("http://127.0.0.1:3002/x").await);
        assert!(!host_is_public("http://[::1]/x").await);
        assert!(!host_is_public("http://192.168.0.10/movie.mkv").await);
        assert!(!host_is_public("http://localhost/x").await);
        assert!(host_is_public("http://8.8.8.8/x").await);
        assert!(!host_is_public("nonsense").await);
    }
}
