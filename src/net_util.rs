//! Network and platform helpers.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::process::Command;

use axum::http::HeaderMap;

/// Resolves the client address, honouring reverse-proxy headers like the reference:
/// first entry of `X-Forwarded-For`, then `X-Real-IP`, then the TCP peer.
pub fn real_ip(headers: &HeaderMap, peer: SocketAddr) -> String {
    if let Some(xff) = headers.get("x-forwarded-for").and_then(|v| v.to_str().ok()) {
        let first = xff.split(',').next().unwrap_or("").trim();
        if !first.is_empty() {
            return first.to_string();
        }
    }
    if let Some(xri) = headers.get("x-real-ip").and_then(|v| v.to_str().ok()) {
        let xri = xri.trim();
        if !xri.is_empty() {
            return xri.to_string();
        }
    }
    match peer.ip() {
        IpAddr::V6(v6) => v6
            .to_ipv4_mapped()
            .map(|v4| v4.to_string())
            .unwrap_or_else(|| v6.to_string()),
        ip => ip.to_string(),
    }
}

fn is_private(ip: &Ipv4Addr) -> bool {
    ip.is_private()
}

/// Picks the LAN IPv4 address to advertise: private ranges first, never
/// loopback or link-local (169.254/16, assigned when DHCP fails).
pub fn local_ipv4() -> Option<Ipv4Addr> {
    let ifaces = if_addrs::get_if_addrs().ok()?;
    let mut candidates: Vec<Ipv4Addr> = ifaces
        .iter()
        .filter(|i| !i.is_loopback())
        .filter_map(|i| match i.ip() {
            IpAddr::V4(v4) if !v4.is_loopback() && !v4.is_link_local() && !v4.is_unspecified() => {
                Some(v4)
            }
            _ => None,
        })
        .collect();
    candidates.sort_by_key(|ip| !is_private(ip));
    candidates.into_iter().next()
}

/// `LOCAL_CLIPBOARD_HOST` if set (Docker / NAT), otherwise the LAN IP.
pub fn advertised_host() -> Option<String> {
    match std::env::var("LOCAL_CLIPBOARD_HOST") {
        Ok(h) if !h.trim().is_empty() => Some(h.trim().to_string()),
        _ => local_ipv4().map(|ip| ip.to_string()),
    }
}

/// Whether launching a browser makes sense (not in containers or headless Linux).
pub fn can_open_browser() -> bool {
    if std::path::Path::new("/.dockerenv").exists() {
        return false;
    }
    if cfg!(target_os = "linux")
        && std::env::var_os("DISPLAY").is_none()
        && std::env::var_os("WAYLAND_DISPLAY").is_none()
    {
        return false;
    }
    true
}

/// Opens `url` in the default browser.
pub fn open_browser(url: &str) -> std::io::Result<()> {
    let mut cmd = if cfg!(target_os = "macos") {
        let mut c = Command::new("open");
        c.arg(url);
        c
    } else if cfg!(target_os = "windows") {
        let mut c = Command::new("rundll32");
        c.args(["url.dll,FileProtocolHandler", url]);
        c
    } else {
        let mut c = Command::new("xdg-open");
        c.arg(url);
        c
    };
    let mut child = cmd.spawn()?;
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

/// Builds an RFC 6266 `Content-Disposition` value with an ASCII fallback and an
/// RFC 5987 UTF-8 `filename*`, so arbitrary names cannot inject header content.
pub fn content_disposition(name: &str) -> String {
    let fallback: String = name
        .chars()
        .map(|c| {
            if (c.is_ascii_graphic() && c != '"' && c != '\\') || c == ' ' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let mut encoded = String::new();
    for b in name.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                encoded.push(*b as char)
            }
            _ => encoded.push_str(&format!("%{b:02X}")),
        }
    }
    format!("attachment; filename=\"{fallback}\"; filename*=UTF-8''{encoded}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn peer() -> SocketAddr {
        "10.0.0.9:5555".parse().unwrap()
    }

    #[test]
    fn real_ip_precedence() {
        let mut h = HeaderMap::new();
        assert_eq!(real_ip(&h, peer()), "10.0.0.9");
        h.insert("x-real-ip", HeaderValue::from_static(" 192.168.1.7 "));
        assert_eq!(real_ip(&h, peer()), "192.168.1.7");
        h.insert("x-forwarded-for", HeaderValue::from_static("192.168.1.5, 172.18.0.1"));
        assert_eq!(real_ip(&h, peer()), "192.168.1.5");
        let v6: SocketAddr = "[::ffff:192.168.1.3]:80".parse().unwrap();
        assert_eq!(real_ip(&HeaderMap::new(), v6), "192.168.1.3");
    }

    #[test]
    fn disposition_is_safe() {
        assert_eq!(
            content_disposition("a b.txt"),
            "attachment; filename=\"a b.txt\"; filename*=UTF-8''a%20b.txt"
        );
        let d = content_disposition("é\"\r\n.txt");
        assert!(d.starts_with("attachment; filename=\"____.txt\""));
        assert!(d.ends_with("filename*=UTF-8''%C3%A9%22%0D%0A.txt"));
        assert!(!d.contains('\r') && !d.contains('\n'));
    }
}
