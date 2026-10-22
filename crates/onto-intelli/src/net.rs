//! Public addresses: the SSRF rule of docs/CONTROL_PLANE.md section 14 for
//! outbound connections whose target a project chooses (LLM base URLs, git
//! URLs and PostgreSQL connector URLs registered by API keys and console
//! sessions).
//!
//! Loopback, private, link-local, CGNAT, multicast, unspecified, broadcast,
//! documentation and reserved ranges are not public, IPv4 and IPv6 (with
//! IPv4-mapped and NAT64 addresses judged by the IPv4 address inside).

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use onto_core::{Error, Result};

fn public_v4(ip: Ipv4Addr) -> bool {
    let [a, b, c, _] = ip.octets();
    !(ip.is_private()
        || ip.is_loopback()
        || ip.is_link_local()
        || ip.is_multicast()
        || ip.is_unspecified()
        || ip.is_broadcast()
        || ip.is_documentation()
        || a == 0                                   // 0.0.0.0/8 "this network"
        || (a == 100 && (64..128).contains(&b))     // 100.64.0.0/10 CGNAT
        || (a == 192 && b == 0 && c == 0)           // 192.0.0.0/24 protocol assignments
        || (a == 198 && (b == 18 || b == 19))       // 198.18.0.0/15 benchmarking
        || a >= 240) // 240.0.0.0/4 reserved
}

fn public_v6(ip: Ipv6Addr) -> bool {
    if let Some(v4) = ip.to_ipv4_mapped() {
        return public_v4(v4);
    }
    let s = ip.segments();
    // NAT64 (64:ff9b::/96): the IPv4 address in the last 32 bits.
    if s[..6] == [0x64, 0xff9b, 0, 0, 0, 0] {
        return public_v4(Ipv4Addr::new((s[6] >> 8) as u8, s[6] as u8, (s[7] >> 8) as u8, s[7] as u8));
    }
    !(ip.is_loopback()
        || ip.is_unspecified()
        || ip.is_multicast()
        || (s[0] & 0xfe00) == 0xfc00                // fc00::/7 unique local
        || (s[0] & 0xffc0) == 0xfe80                // fe80::/10 link-local
        || (s[0] & 0xffc0) == 0xfec0                // fec0::/10 site-local (deprecated)
        || (s[0] == 0x2001 && s[1] == 0x0db8)       // 2001:db8::/32 documentation
        || s[..6] == [0, 0, 0, 0, 0, 0]) // ::/96 IPv4-compatible (deprecated)
}

/// The address is on the public internet.
pub fn public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => public_v4(v4),
        IpAddr::V6(v6) => public_v6(v6),
    }
}

/// The addresses `host` resolved to, when there are some and all are
/// public; `what` names the URL in errors.
pub fn only_public(what: &str, host: &str, addrs: Vec<IpAddr>) -> Result<Vec<IpAddr>> {
    if addrs.is_empty() {
        return Err(Error::invalid(format!("{what}: {host} has no addresses")));
    }
    if let Some(bad) = addrs.iter().find(|ip| !public_ip(**ip)) {
        return Err(Error::invalid(format!(
            "{what}: {host} resolves to {bad}, not a public address (console.allow_private_urls is off)"
        )));
    }
    Ok(addrs)
}

/// Resolve `host` (an IP literal is taken as is) and require public
/// addresses only. Blocking.
pub fn resolve_public_blocking(what: &str, host: &str, port: u16) -> Result<Vec<IpAddr>> {
    use std::net::ToSocketAddrs as _;
    let addrs = match host.parse::<IpAddr>() {
        Ok(ip) => vec![ip],
        Err(_) => (host, port)
            .to_socket_addrs()
            .map_err(|e| Error::invalid(format!("{what}: {host} does not resolve: {e}")))?
            .map(|a| a.ip())
            .collect(),
    };
    only_public(what, host, addrs)
}

/// [`resolve_public_blocking`] on the async runtime.
pub async fn resolve_public(what: &str, host: &str, port: u16) -> Result<Vec<IpAddr>> {
    let addrs = match host.parse::<IpAddr>() {
        Ok(ip) => vec![ip],
        Err(_) => tokio::net::lookup_host((host, port))
            .await
            .map_err(|e| Error::invalid(format!("{what}: {host} does not resolve: {e}")))?
            .map(|a| a.ip())
            .collect(),
    };
    only_public(what, host, addrs)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addresses() {
        for bad in [
            "127.0.0.1",
            "10.1.2.3",
            "172.16.0.1",
            "192.168.1.1",
            "169.254.169.254",
            "100.64.0.1",
            "224.0.0.1",
            "0.0.0.0",
            "255.255.255.255",
            "::1",
            "::",
            "fd00::1",
            "fe80::1",
            "ff02::1",
            "::ffff:127.0.0.1",
            "::ffff:10.0.0.1",
            "64:ff9b::a9fe:a9fe",
            "2001:db8::1",
        ] {
            assert!(!public_ip(bad.parse().unwrap()), "{bad}");
        }
        for good in ["8.8.8.8", "1.1.1.1", "100.128.0.1", "2606:4700:4700::1111", "::ffff:8.8.8.8"] {
            assert!(public_ip(good.parse().unwrap()), "{good}");
        }
    }

    #[tokio::test]
    async fn resolution() {
        assert!(resolve_public("u", "localhost", 443).await.is_err());
        assert!(resolve_public_blocking("u", "127.0.0.1", 443).is_err());
        assert_eq!(resolve_public("u", "8.8.8.8", 443).await.unwrap(), vec!["8.8.8.8".parse::<IpAddr>().unwrap()]);
    }
}
