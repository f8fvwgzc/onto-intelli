//! Outbound connections whose target is chosen by someone other than the
//! operator: the SSRF rules of docs/CONTROL_PLANE.md section 14 and
//! docs/design/EVENTS.md B.5.
//!
//! * [`public_ip`]: loopback, private, link-local, CGNAT, multicast,
//!   unspecified, broadcast, documentation and reserved ranges are not
//!   public, IPv4 and IPv6 (with IPv4-mapped and NAT64 addresses judged by
//!   the IPv4 address inside).
//! * [`always_denied`]: link-local and cloud metadata ranges, `0.0.0.0/8`,
//!   multicast and reserved ranges (and their IPv4-mapped and NAT64 forms)
//!   are refused even to operator allow rules.
//! * [`Reach`]: what one outbound target may reach. An operator rule
//!   ([`AllowRule`], `[[webhooks.allow]]`) lets a named host resolve into its
//!   listed CIDRs only; rules never apply to `restricted` targets (those a
//!   project registered), which reach public addresses only.
//! * With the `outbound` feature: the guarded HTTP client ([`client`],
//!   [`guarded_client`]), whose own resolver checks every answer right
//!   before connecting (no rebinding window), never follows redirects and
//!   uses no proxy; IP literals are checked before a request is built; URL
//!   and header checks ([`check_url`], [`check_target_url`], [`join_route`],
//!   [`check_headers`]). onto-control re-exports these.

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

/// 64:ff9b:1::/48, the local-use NAT64 prefix (RFC 8215).
fn local_nat64(ip: Ipv6Addr) -> bool {
    let s = ip.segments();
    s[0] == 0x64 && s[1] == 0xff9b && s[2] == 1
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
    // Local-use NAT64 (64:ff9b:1::/48, RFC 8215) embeds IPv4 at the RFC 6052
    // positions of its prefix length, which this side cannot know: never public.
    if local_nat64(ip) {
        return false;
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

/// One `(address, prefix length)` block, `10.20.0.0/16` or `fd00::/8`. A
/// bare address is a single host (`/32` or `/128`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cidr {
    pub addr: IpAddr,
    pub prefix: u8,
}

impl Cidr {
    pub fn parse(raw: &str) -> Result<Self> {
        let raw = raw.trim();
        let bad = || Error::Config(format!("'{raw}' is not a CIDR block (for example 10.20.0.0/16)"));
        let (a, p) = match raw.split_once('/') {
            Some((a, p)) => (a, Some(p)),
            None => (raw, None),
        };
        let addr: IpAddr = a.parse().map_err(|_| bad())?;
        let max = if addr.is_ipv4() { 32 } else { 128 };
        let prefix = match p {
            Some(p) => p.parse::<u8>().map_err(|_| bad())?,
            None => max,
        };
        if prefix > max {
            return Err(bad());
        }
        Ok(Self { addr, prefix })
    }

    /// `ip` is in the block (an IPv4-mapped address is matched as IPv4).
    pub fn contains(&self, ip: IpAddr) -> bool {
        match (self.addr, canonical(ip)) {
            (IpAddr::V4(net), IpAddr::V4(ip)) => {
                let mask = if self.prefix == 0 { 0 } else { u32::MAX << (32 - self.prefix) };
                (u32::from(net) & mask) == (u32::from(ip) & mask)
            }
            (IpAddr::V6(net), IpAddr::V6(ip)) => {
                let mask = if self.prefix == 0 { 0 } else { u128::MAX << (128 - self.prefix) };
                (u128::from(net) & mask) == (u128::from(ip) & mask)
            }
            _ => false,
        }
    }
}

impl std::fmt::Display for Cidr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}", self.addr, self.prefix)
    }
}

/// The IPv4 address inside an IPv4-mapped or NAT64 address, else `ip`.
pub fn canonical(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return IpAddr::V4(v4);
            }
            let s = v6.segments();
            if s[..6] == [0x64, 0xff9b, 0, 0, 0, 0] {
                return IpAddr::V4(Ipv4Addr::new((s[6] >> 8) as u8, s[6] as u8, (s[7] >> 8) as u8, s[7] as u8));
            }
            ip
        }
        v4 => v4,
    }
}

/// Addresses no outbound target may reach, allow-listed or not: link-local
/// (169.254.0.0/16, fe80::/10), cloud metadata (100.100.100.200,
/// fd00:ec2::254), 0.0.0.0/8, multicast, broadcast and reserved
/// (240.0.0.0/4), and their IPv4-mapped and NAT64 forms. The local-use NAT64
/// prefix 64:ff9b:1::/48 is denied whole (fail closed: where its embedded
/// address sits depends on the translator's prefix length).
pub fn always_denied(ip: IpAddr) -> bool {
    match canonical(ip) {
        IpAddr::V4(v4) => {
            let [a, ..] = v4.octets();
            v4.is_link_local()
                || v4 == Ipv4Addr::new(100, 100, 100, 200)
                || a == 0
                || v4.is_multicast()
                || v4.is_broadcast()
                || a >= 240
        }
        IpAddr::V6(v6) => {
            let s = v6.segments();
            local_nat64(v6)
                || (s[0] & 0xffc0) == 0xfe80
                || v6 == Ipv6Addr::new(0xfd00, 0x0ec2, 0, 0, 0, 0, 0, 0x0254)
                || v6.is_multicast()
                || v6.is_unspecified()
        }
    }
}

/// A host pattern of an allow rule: `hooks.internal.example` (exactly) or
/// `*.internal.example` (any name under it, not the bare suffix).
fn host_matches(pattern: &str, host: &str) -> bool {
    let (p, h) =
        (pattern.trim().trim_end_matches('.').to_ascii_lowercase(), host.trim_end_matches('.').to_ascii_lowercase());
    let h = h.trim_start_matches('[').trim_end_matches(']');
    match p.strip_prefix("*.") {
        Some(suffix) => h.len() > suffix.len() + 1 && h.ends_with(suffix) && h[..h.len() - suffix.len()].ends_with('.'),
        None => p == h,
    }
}

/// An operator rule (`[[webhooks.allow]]`): a host that may resolve into
/// these CIDRs, and only into them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AllowRule {
    pub host: String,
    pub cidrs: Vec<Cidr>,
}

impl AllowRule {
    /// `host` is a name, `*.suffix` or an IP literal; `cidrs` is not empty.
    pub fn new(host: &str, cidrs: &[String]) -> Result<Self> {
        let host = host.trim().to_ascii_lowercase();
        let name = host.strip_prefix("*.").unwrap_or(&host);
        let ok_name = !name.is_empty()
            && !name.contains('*')
            && name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | ':' | '[' | ']'));
        if !ok_name {
            return Err(Error::Config(format!("webhooks.allow host '{host}' is not a host name, *.suffix or address")));
        }
        if cidrs.is_empty() {
            return Err(Error::Config(format!(
                "webhooks.allow host '{host}' needs cidrs (the addresses it may reach)"
            )));
        }
        let cidrs = cidrs.iter().map(|c| Cidr::parse(c)).collect::<Result<Vec<_>>>()?;
        if let Some(c) = cidrs.iter().find(|c| always_denied(c.addr)) {
            return Err(Error::Config(format!(
                "webhooks.allow host '{host}': {c} is link-local, metadata or reserved and is never reachable"
            )));
        }
        Ok(Self { host, cidrs })
    }

    pub fn matches(&self, host: &str) -> bool {
        host_matches(&self.host, host)
    }
}

/// What one outbound target may reach.
#[derive(Clone, Debug, Default)]
pub struct Reach {
    /// Operator allow rules (ignored when `restricted`).
    pub allow: std::sync::Arc<Vec<AllowRule>>,
    /// The target was registered by a project credential: public addresses only.
    pub restricted: bool,
    /// `console.allow_private_urls` (development): loopback and private
    /// ranges are reachable (link-local and metadata never are).
    pub allow_private: bool,
}

impl Reach {
    /// The allow rules that apply to `host` (none for restricted targets).
    pub fn rules_for(&self, host: &str) -> Vec<&AllowRule> {
        if self.restricted { Vec::new() } else { self.allow.iter().filter(|r| r.matches(host)).collect() }
    }

    /// May `host` be reached at all of `addrs`? An allow-listed host must
    /// resolve only into its rules' CIDRs; any other host only to public
    /// addresses (or, with `allow_private`, private ones). The error names
    /// the host and the refused address.
    pub fn judge(&self, host: &str, addrs: &[IpAddr]) -> std::result::Result<(), String> {
        if addrs.is_empty() {
            return Err(format!("{host} has no addresses"));
        }
        let rules = self.rules_for(host);
        for ip in addrs {
            if always_denied(*ip) {
                return Err(format!("refused: {host} resolves to {ip}, a link-local, metadata or reserved address"));
            }
            let ok = if rules.is_empty() {
                public_ip(*ip) || self.allow_private
            } else {
                rules.iter().any(|r| r.cidrs.iter().any(|c| c.contains(*ip)))
            };
            if !ok {
                let why = if !rules.is_empty() {
                    "outside the CIDRs its [[webhooks.allow]] rule lists"
                } else if self.restricted {
                    "not a public address (a project-registered target reaches public addresses only)"
                } else {
                    "not a public address (allow it with [[webhooks.allow]])"
                };
                return Err(format!("refused: {host} resolves to {ip}, {why}"));
            }
        }
        Ok(())
    }

    /// Plain `http://` is allowed for allow-listed hosts and with `allow_private`.
    pub fn allows_http(&self, host: &str) -> bool {
        self.allow_private || !self.rules_for(host).is_empty()
    }
}

/// Check a delivery URL (`what` names it in errors, never its value when it
/// came from a reference): `https://` (or `http://` for an allow-listed host
/// or with `allow_private`), a host, no userinfo, no fragment, and an IP
/// literal host that `reach` allows.
pub fn check_target_url(raw: &str, what: &str, reach: &Reach) -> Result<url::Url> {
    let url = url::Url::parse(raw.trim()).map_err(|e| Error::invalid(format!("{what} is not a URL: {e}")))?;
    let host = url.host_str().map(str::to_string).ok_or_else(|| Error::invalid(format!("{what} has no host")))?;
    match url.scheme() {
        "https" => {}
        "http" if reach.allows_http(&host) => {}
        "http" => return Err(Error::invalid(format!("{what} must be an https URL"))),
        other => return Err(Error::invalid(format!("{what}: scheme {other} is not https"))),
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(Error::invalid(format!("{what} may not carry credentials (userinfo)")));
    }
    if url.fragment().is_some() {
        return Err(Error::invalid(format!("{what} may not have a fragment")));
    }
    let ip = match url.host() {
        Some(url::Host::Ipv4(v4)) => Some(IpAddr::V4(v4)),
        Some(url::Host::Ipv6(v6)) => Some(IpAddr::V6(v6)),
        _ => None,
    };
    if let Some(ip) = ip {
        reach.judge(&host, &[ip]).map_err(|e| Error::invalid(format!("{what}: {e}")))?;
    }
    Ok(url)
}

/// The URL of `route` under `base` (writeback targets): `route` is a
/// relative path without `//`, `@`, `\` or `..` segments, the result is
/// `base.join(route)`, and its scheme, host and port must equal the base's.
pub fn join_route(base: &url::Url, route: &str) -> Result<url::Url> {
    let r = route.trim();
    let bad = |why: &str| Err(Error::invalid(format!("route '{r}' {why}")));
    if r.contains("//") || r.contains('@') || r.contains('\\') {
        return bad("may not contain //, @ or \\");
    }
    if r.split(['/', '?', '#']).any(|seg| seg == ".." || seg.eq_ignore_ascii_case("%2e%2e")) {
        return bad("may not contain ..");
    }
    if r.contains(':') && r.split('/').next().is_some_and(|first| first.contains(':')) {
        return bad("must be a relative path");
    }
    let url = base.join(r).map_err(|e| Error::invalid(format!("route '{r}': {e}")))?;
    if url.scheme() != base.scheme()
        || url.host_str() != base.host_str()
        || url.port_or_known_default() != base.port_or_known_default()
    {
        return bad("leaves the target's host");
    }
    Ok(url)
}

#[cfg(feature = "outbound")]
pub use outbound::*;

#[cfg(feature = "outbound")]
mod outbound {
    use std::net::{IpAddr, SocketAddr};
    use std::time::Duration;

    use reqwest::dns::{Addrs, Name, Resolve, Resolving};

    use super::{Reach, public_ip};
    use onto_core::{Error, Result};

    /// Header names a project may not set: they carry credentials. So does
    /// a name with a part (between `-`, `_` or `.`) that is one of
    /// [`CREDENTIAL_WORDS`], starts with `auth`, or ends with `token`,
    /// `secret`, `password` or `apikey` (`x-goog-api-key`,
    /// `x-portkey-api-key`, `helicone-auth`, `x-auth-token`,
    /// `x-accesstoken`, …).
    pub const CREDENTIAL_HEADERS: [&str; 5] =
        ["authorization", "proxy-authorization", "x-api-key", "api-key", "cookie"];

    /// Parts of header names that mark a credential.
    pub const CREDENTIAL_WORDS: [&str; 9] =
        ["key", "token", "secret", "password", "passwd", "credential", "credentials", "cookie", "signature"];

    /// Does the header `name` (any case) carry credentials? Extra headers
    /// are stored in plaintext and shown to every project member, so such
    /// headers are refused; the provider's key goes in `api_key`, sealed.
    pub fn credential_header(name: &str) -> bool {
        let n = name.trim().to_ascii_lowercase();
        CREDENTIAL_HEADERS.contains(&n.as_str())
            || n.split(['-', '_', '.']).any(|part| {
                CREDENTIAL_WORDS.contains(&part)
                    || part.starts_with("auth")
                    || ["token", "secret", "password", "apikey"].iter().any(|w| part.ends_with(w))
            })
    }

    /// A DNS resolver that answers only with public addresses: a name with
    /// any non-public address is refused.
    #[derive(Clone, Copy, Debug, Default)]
    pub struct PublicOnly;

    impl Resolve for PublicOnly {
        fn resolve(&self, name: Name) -> Resolving {
            let host = name.as_str().to_string();
            Box::pin(async move {
                let addrs: Vec<SocketAddr> = tokio::net::lookup_host((host.as_str(), 0)).await?.collect();
                if addrs.is_empty() {
                    return Err(format!("{host} has no addresses").into());
                }
                if let Some(bad) = addrs.iter().find(|a| !public_ip(a.ip())) {
                    return Err(format!(
                        "refused: {host} resolves to {}, not a public address (console.allow_private_urls is off)",
                        bad.ip()
                    )
                    .into());
                }
                Ok(Box::new(addrs.into_iter()) as Addrs)
            })
        }
    }

    /// A DNS resolver that answers only with addresses [`Reach::judge`]
    /// allows, checked right before connecting.
    #[derive(Clone, Debug, Default)]
    pub struct Guarded {
        pub reach: Reach,
    }

    impl Resolve for Guarded {
        fn resolve(&self, name: Name) -> Resolving {
            let host = name.as_str().to_string();
            let reach = self.reach.clone();
            Box::pin(async move {
                let addrs: Vec<SocketAddr> = tokio::net::lookup_host((host.as_str(), 0)).await?.collect();
                let ips: Vec<IpAddr> = addrs.iter().map(|a| a.ip()).collect();
                reach.judge(&host, &ips)?;
                Ok(Box::new(addrs.into_iter()) as Addrs)
            })
        }
    }

    /// Check a project-chosen base URL: `https` (or `http` with
    /// `allow_private`), a host, no credentials, query or fragment, and an
    /// IP literal host only when public (or `allow_private`). Returns it
    /// without a trailing `/`.
    ///
    /// No error quotes `raw` or any piece of it: a refused value may hold a
    /// password, even one that does not parse, and errors reach traces and
    /// logs. Only an IP address the URL parsed to is named.
    pub fn check_url(raw: &str, allow_private: bool) -> Result<String> {
        let raw = raw.trim();
        let url = url::Url::parse(raw).map_err(|e| Error::invalid(format!("base_url is not a URL: {e}")))?;
        if !url.username().is_empty() || url.password().is_some() {
            return Err(Error::invalid("base_url may not carry credentials; use the api_key field"));
        }
        match url.scheme() {
            "https" => {}
            "http" if allow_private => {}
            "http" => {
                return Err(Error::invalid("base_url must be https:// (http:// needs console.allow_private_urls)"));
            }
            _ => return Err(Error::invalid("base_url: its scheme is not http(s)")),
        }
        if url.query().is_some() || url.fragment().is_some() {
            return Err(Error::invalid("base_url may not have a query or a fragment"));
        }
        let ip = match url.host() {
            None => return Err(Error::invalid("base_url has no host")),
            Some(url::Host::Ipv4(v4)) => Some(IpAddr::V4(v4)),
            Some(url::Host::Ipv6(v6)) => Some(IpAddr::V6(v6)),
            Some(url::Host::Domain(_)) => None,
        };
        if let Some(ip) = ip
            && !allow_private
            && !public_ip(ip)
        {
            return Err(Error::invalid(format!(
                "base_url: {ip} is not a public address (console.allow_private_urls is off)"
            )));
        }
        Ok(raw.trim_end_matches('/').to_string())
    }

    /// Check extra request headers: valid names and values, and none that
    /// carries credentials.
    pub fn check_headers(headers: &std::collections::BTreeMap<String, String>) -> Result<()> {
        if headers.len() > 20 {
            return Err(Error::invalid("at most 20 extra headers"));
        }
        for (k, v) in headers {
            let name = reqwest::header::HeaderName::from_bytes(k.trim().as_bytes())
                .map_err(|_| Error::invalid(format!("'{k}' is not a valid header name")))?;
            if credential_header(name.as_str()) {
                return Err(Error::invalid(format!(
                    "header {k} carries credentials and may not be set (the API key goes in api_key)"
                )));
            }
            if reqwest::header::HeaderValue::from_str(v).is_err() || v.len() > 1024 {
                return Err(Error::invalid(format!("the value of header {k} is not a valid header value")));
            }
        }
        Ok(())
    }

    fn builder(timeout: Duration) -> reqwest::ClientBuilder {
        reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .timeout(timeout)
            .connect_timeout(timeout.min(Duration::from_secs(10)))
    }

    /// The HTTP client of project providers: no redirects, no proxy, the
    /// given timeout, and (unless `allow_private`) the [`PublicOnly`] resolver.
    pub fn client(allow_private: bool, timeout: Duration) -> Result<reqwest::Client> {
        let mut b = builder(timeout);
        if !allow_private {
            b = b.dns_resolver(PublicOnly);
        }
        b.build().map_err(|e| Error::Config(format!("http client: {e}")))
    }

    /// The HTTP client of one webhook endpoint or writeback target: no
    /// redirects, no proxy, the given timeout, and the [`Guarded`] resolver
    /// for `reach` (IP literals are checked by [`super::check_target_url`]).
    pub fn guarded_client(reach: &Reach, timeout: Duration) -> Result<reqwest::Client> {
        builder(timeout)
            .dns_resolver(Guarded { reach: reach.clone() })
            .build()
            .map_err(|e| Error::Config(format!("http client: {e}")))
    }
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
            // Local-use NAT64 (RFC 8215): 169.254.169.254 and 127.0.0.1 at the
            // /48 positions, and at the /96 ones.
            "64:ff9b:1:a9fe:a9:fe00::",
            "64:ff9b:1::a9fe:a9fe",
            "64:ff9b:1:7f00:0:100::",
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

    fn rules(v: &[(&str, &[&str])]) -> std::sync::Arc<Vec<AllowRule>> {
        std::sync::Arc::new(
            v.iter()
                .map(|(h, c)| AllowRule::new(h, &c.iter().map(|s| s.to_string()).collect::<Vec<_>>()).unwrap())
                .collect(),
        )
    }

    fn ips(v: &[&str]) -> Vec<IpAddr> {
        v.iter().map(|s| s.parse().unwrap()).collect()
    }

    #[test]
    fn cidrs() {
        let c = Cidr::parse("10.20.0.0/16").unwrap();
        assert!(c.contains("10.20.3.4".parse().unwrap()) && !c.contains("10.21.0.1".parse().unwrap()));
        assert!(c.contains("::ffff:10.20.0.9".parse().unwrap()), "IPv4-mapped is matched as IPv4");
        assert_eq!(Cidr::parse("127.0.0.1").unwrap().to_string(), "127.0.0.1/32");
        let v6 = Cidr::parse("fd12:3456::/32").unwrap();
        assert!(v6.contains("fd12:3456::1".parse().unwrap()) && !v6.contains("fd12:3457::1".parse().unwrap()));
        assert!(Cidr::parse("0.0.0.0/0").unwrap().contains("8.8.8.8".parse().unwrap()));
        for bad in ["10.0.0.0/33", "x/8", "10.0.0/8", "::/129", ""] {
            assert!(Cidr::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn metadata_and_link_local_are_always_denied() {
        for bad in [
            "169.254.169.254",
            "169.254.0.1",
            "fe80::1",
            "100.100.100.200",
            "fd00:ec2::254",
            "0.0.0.0",
            "0.1.2.3",
            "224.0.0.1",
            "ff02::1",
            "255.255.255.255",
            "240.0.0.1",
            "::ffff:169.254.169.254",
            "64:ff9b::a9fe:a9fe",
            "64:ff9b:1:a9fe:a9:fe00::",
            "64:ff9b:1::a9fe:a9fe",
            "64:ff9b:1:7f00:0:100::",
            "::",
        ] {
            assert!(always_denied(bad.parse().unwrap()), "{bad}");
        }
        for ok in ["10.0.0.1", "127.0.0.1", "8.8.8.8", "fd00::1", "::1", "100.100.100.201"] {
            assert!(!always_denied(ok.parse().unwrap()), "{ok}");
        }
        // Rules may not open them.
        assert!(AllowRule::new("meta.internal", &["169.254.0.0/16".into()]).is_err());
        assert!(AllowRule::new("x.internal", &[]).is_err());
        assert!(AllowRule::new("*", &["10.0.0.0/8".into()]).is_err());
    }

    #[test]
    fn the_guard_applies_exact_and_suffix_rules_within_their_cidrs() {
        let allow = rules(&[
            ("hooks.internal.example", &["10.20.0.0/16"]),
            ("*.svc.example", &["10.30.0.0/16", "fd12::/16"]),
            ("127.0.0.1", &["127.0.0.1/32"]),
        ]);
        let reach = Reach { allow: allow.clone(), restricted: false, allow_private: false };
        assert!(reach.judge("hooks.internal.example", &ips(&["10.20.1.2"])).is_ok());
        assert!(reach.judge("HOOKS.internal.example.", &ips(&["10.20.1.2"])).is_ok(), "case and the root dot");
        let e = reach.judge("hooks.internal.example", &ips(&["10.20.1.2", "10.99.0.1"])).unwrap_err();
        assert!(e.contains("10.99.0.1") && e.contains("outside the CIDRs"), "{e}");
        assert!(
            reach.judge("hooks.internal.example", &ips(&["8.8.8.8"])).is_err(),
            "a rule's host resolves only into its CIDRs"
        );
        assert!(reach.judge("a.svc.example", &ips(&["10.30.0.5", "fd12::5"])).is_ok());
        assert!(reach.judge("a.b.svc.example", &ips(&["10.30.0.5"])).is_ok());
        assert!(reach.judge("svc.example", &ips(&["10.30.0.5"])).is_err(), "the bare suffix is not under it");
        assert!(reach.judge("evilsvc.example", &ips(&["10.30.0.5"])).is_err());
        // An allow-listed name resolving to metadata is refused.
        let e = reach.judge("hooks.internal.example", &ips(&["169.254.169.254"])).unwrap_err();
        assert!(e.contains("metadata"), "{e}");
        // Without a rule: public only.
        assert!(reach.judge("example.com", &ips(&["93.184.216.34"])).is_ok());
        let e = reach.judge("rebind.example", &ips(&["10.0.0.1"])).unwrap_err();
        assert!(e.contains("not a public address"), "{e}");
        assert!(reach.judge("localhost", &ips(&["127.0.0.1"])).is_err(), "localhost is not the allow-listed literal");
        assert!(reach.judge("::ffff:127.0.0.1", &ips(&["::ffff:127.0.0.1"])).is_err());
        assert!(reach.judge("[64:ff9b::7f00:1]", &ips(&["64:ff9b::7f00:1"])).is_err(), "NAT64 of loopback");
        // Local-use NAT64 (RFC 8215) of metadata and loopback, with no rule: refused.
        let open = Reach { allow: Default::default(), restricted: false, allow_private: false };
        for a in ["64:ff9b:1:a9fe:a9:fe00::", "64:ff9b:1::a9fe:a9fe", "64:ff9b:1:7f00:0:100::"] {
            assert!(open.judge("nat64.example", &ips(&[a])).is_err(), "{a}");
        }
        assert!(reach.judge("127.0.0.1", &ips(&["127.0.0.1"])).is_ok());
        assert!(reach.judge("x", &[]).is_err());
        // Restricted targets ignore every rule.
        let restricted = Reach { allow, restricted: true, allow_private: false };
        let e = restricted.judge("hooks.internal.example", &ips(&["10.20.1.2"])).unwrap_err();
        assert!(e.contains("public addresses only"), "{e}");
        assert!(restricted.judge("127.0.0.1", &ips(&["127.0.0.1"])).is_err());
        assert!(!restricted.allows_http("127.0.0.1"));
        // Development: private ranges open, metadata still closed.
        let dev = Reach { allow_private: true, ..Default::default() };
        assert!(dev.judge("localhost", &ips(&["127.0.0.1", "::1"])).is_ok());
        assert!(dev.judge("m", &ips(&["169.254.169.254"])).is_err());
    }

    #[test]
    fn target_urls() {
        let reach = Reach { allow: rules(&[("127.0.0.1", &["127.0.0.1/32"])]), ..Default::default() };
        assert!(check_target_url("https://hooks.example.com/in?k=1", "url", &reach).is_ok());
        assert!(check_target_url("http://127.0.0.1:9/in", "url", &reach).is_ok(), "allow-listed: http allowed");
        for bad in [
            "http://hooks.example.com/in",
            "https://user:pw@hooks.example.com/",
            "https://hooks.example.com/#frag",
            "https://169.254.169.254/latest",
            "https://[::ffff:127.0.0.1]/",
            "https://10.0.0.1/",
            "ftp://hooks.example.com/",
            "not a url",
        ] {
            assert!(check_target_url(bad, "url", &reach).is_err(), "{bad}");
        }
        let e = check_target_url("http://hooks.example.com/x", "the URL in env:HOOK_URL", &reach).unwrap_err();
        assert!(e.to_string().contains("env:HOOK_URL") && !e.to_string().contains("hooks.example.com"), "{e}");
    }

    /// A refused base URL may hold a password, so no error quotes it: every
    /// path of `check_url`, with the password wherever that path can meet it.
    /// "has no host" is not here: an http(s) URL without a host does not parse.
    #[cfg(feature = "outbound")]
    #[test]
    fn base_url_errors_do_not_quote_the_value() {
        const CREDENTIALS: &str = "may not carry credentials";
        const SCHEME: &str = "its scheme is not http(s)";
        for (raw, allow_private, expected) in [
            // Does not parse, so nothing can say where the password is.
            ("user s3cret@api.example.com", false, "is not a URL"),
            ("//user:s3cret@api.example.com/v1", false, "is not a URL"),
            ("https://user:s3cret@api.example.com:port/v1", false, "is not a URL"),
            ("https://user:s3cret@[::1/v1", false, "is not a URL"),
            ("https://:s3cret@/v1", false, "is not a URL"),
            // Credentials are refused before any rule that looks at the rest.
            ("http://user:s3cret@host/v1", false, CREDENTIALS),
            ("http://user:s3cret@127.0.0.1/v1", true, CREDENTIALS),
            ("https://user:s3cret@api.example.com/v1", false, CREDENTIALS),
            ("https://:s3cret@api.example.com/v1", false, CREDENTIALS),
            ("https://user:s3cret@10.0.0.1/v1", false, CREDENTIALS),
            ("https://user:s3cret@api.example.com/v1?x=1", false, CREDENTIALS),
            ("ftp://user:s3cret@api.example.com/", false, CREDENTIALS),
            // Not https.
            ("http://api.example.com/s3cret/v1", false, "must be https://"),
            // Another scheme. Without `//` a URL has no userinfo, and what is
            // before the first colon is the scheme, so that is not printed either.
            ("ftp://api.example.com/s3cret", true, SCHEME),
            ("user:s3cret@api.example.com/v1", false, SCHEME),
            ("s3cret:user@api.example.com/v1", false, SCHEME),
            // A query or a fragment.
            ("https://api.example.com/v1?key=s3cret", false, "a query or a fragment"),
            ("https://api.example.com/v1#s3cret", false, "a query or a fragment"),
            // An address that is not public; the address itself is still named.
            ("https://10.0.0.1/s3cret/v1", false, "10.0.0.1 is not a public address"),
        ] {
            let e = check_url(raw, allow_private).unwrap_err();
            assert!(matches!(e, Error::Invalid(_)), "{raw}: {e:?}");
            let e = e.to_string();
            assert!(e.contains(expected), "{raw}: {e}");
            assert!(!e.contains("s3cret"), "{raw}: {e}");
        }
    }

    #[test]
    fn routes_stay_on_the_target_host() {
        let base = url::Url::parse("https://api.example.com/v1/").unwrap();
        assert_eq!(join_route(&base, "orders/42").unwrap().as_str(), "https://api.example.com/v1/orders/42");
        assert_eq!(join_route(&base, "orders?x=1").unwrap().as_str(), "https://api.example.com/v1/orders?x=1");
        for bad in [
            "//evil.example/x",
            "@evil.example",
            "x@evil",
            "../admin",
            "a/../../b",
            "a\\b",
            "https://evil/",
            "%2e%2e/x",
        ] {
            assert!(join_route(&base, bad).is_err(), "{bad}");
        }
        assert!(join_route(&base, "/root").is_ok(), "an absolute path stays on the host");
    }
}
