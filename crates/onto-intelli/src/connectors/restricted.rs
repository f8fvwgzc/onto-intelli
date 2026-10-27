//! The SSRF rules for connectors a project credential registers (an API key
//! or a console session: [`ConnectorSpec::restricted`](super::ConnectorSpec)),
//! shared by the MySQL, SQL Server and MongoDB connectors (PostgreSQL has its
//! own, [`super::postgres::check_url`]). A restricted URL names one TCP host
//! (no socket, no host list), a user, and only the query parameters the
//! connector allows; nothing is read from the server's environment or files.
//! Unless `console.allow_private_urls`, the host must resolve only to public
//! addresses, and the connector connects to the first of them, so a name
//! that resolves differently later cannot move the connection.

use std::collections::BTreeMap;
use std::net::IpAddr;

use onto_core::{Error, Result};

/// A checked connection target.
#[derive(Clone, Debug, PartialEq)]
pub struct Target {
    /// The host as written (for TLS name checks).
    pub host: String,
    pub port: u16,
    pub user: String,
    pub password: String,
    /// The path's first segment (a database), percent-decoded.
    pub database: String,
    pub params: BTreeMap<String, String>,
    /// The address to connect to: the first checked public address, or
    /// `None` when private addresses are allowed (connect by name).
    pub addr: Option<IpAddr>,
}

impl Target {
    /// Where to connect: the checked address, else the host.
    pub fn connect_host(&self) -> String {
        self.addr.map(|a| a.to_string()).unwrap_or_else(|| self.host.clone())
    }
}

/// The URL's parts, without network checks (also for unrestricted specs).
pub fn parse(what: &str, raw: &str, schemes: &[&str], default_port: u16, need_user: bool) -> Result<Target> {
    let bad = |why: String| Error::invalid(format!("{what} URL: {why}"));
    let url = split_url(raw).map_err(|e| bad(format!("not a URL ({e})")))?;
    if !schemes.contains(&url.scheme.as_str()) {
        return Err(bad(format!("scheme {} is not one of {}", url.scheme, schemes.join(", "))));
    }
    if url.host.is_empty() || url.host.contains(['/', '\\', ',']) {
        return Err(bad("one TCP host is required (sockets and host lists are refused)".into()));
    }
    if need_user && url.user.is_empty() {
        return Err(bad("a user name is required".into()));
    }
    Ok(Target {
        host: url.host,
        port: url.port.unwrap_or(default_port),
        user: url.user,
        password: url.password,
        database: url.database,
        params: url.params,
        addr: None,
    })
}

/// [`parse`], then the restricted rules: only `allowed` parameters, and
/// unless `allow_private`, public addresses only (pinned in `addr`).
pub async fn check(
    what: &str,
    raw: &str,
    schemes: &[&str],
    default_port: u16,
    need_user: bool,
    allowed: &[&str],
    allow_private: bool,
) -> Result<Target> {
    let mut t = parse(what, raw, schemes, default_port, need_user)?;
    if let Some(p) = t.params.keys().find(|k| !allowed.contains(&k.as_str())) {
        return Err(Error::invalid(format!(
            "{what} URL: the parameter {p} is refused for connectors registered by API keys and console sessions \
             (only {} are accepted)",
            if allowed.is_empty() { "none".to_string() } else { allowed.join(", ") }
        )));
    }
    if !allow_private {
        let addrs = crate::net::resolve_public(&format!("{what} URL"), &t.host, t.port).await?;
        t.addr = addrs.first().copied();
    }
    Ok(t)
}

/// A URL split into what connectors need: no dependency on an HTTP stack.
struct Parts {
    scheme: String,
    user: String,
    password: String,
    host: String,
    port: Option<u16>,
    database: String,
    params: BTreeMap<String, String>,
}

fn decode(s: &str) -> std::result::Result<String, String> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).map_err(|_| "bad percent-encoding".to_string())?;
                out.push(u8::from_str_radix(hex, 16).map_err(|_| "bad percent-encoding".to_string())?);
                i += 3;
            }
            b'%' => return Err("bad percent-encoding".into()),
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8(out).map_err(|_| "not valid UTF-8 after percent-decoding".into())
}

fn split_url(raw: &str) -> std::result::Result<Parts, String> {
    let raw = raw.trim();
    let (scheme, rest) = raw.split_once("://").ok_or("no scheme")?;
    let scheme = scheme.to_ascii_lowercase();
    let (rest, query) = rest.split_once('?').unwrap_or((rest, ""));
    let (authority, path) = rest.split_once('/').unwrap_or((rest, ""));
    let (userinfo, hostport) = match authority.rsplit_once('@') {
        Some((u, h)) => (u, h),
        None => ("", authority),
    };
    let (user, password) = userinfo.split_once(':').unwrap_or((userinfo, ""));
    let (host, port) = if let Some(v6) = hostport.strip_prefix('[') {
        let (h, after) = v6.split_once(']').ok_or("unclosed [")?;
        let port = after.strip_prefix(':').map(|p| p.parse::<u16>().map_err(|_| "bad port")).transpose()?;
        (h.to_string(), port)
    } else {
        match hostport.rsplit_once(':') {
            Some((h, p)) if !h.contains(':') => (h.to_string(), Some(p.parse::<u16>().map_err(|_| "bad port")?)),
            _ => (hostport.to_string(), None),
        }
    };
    let mut params = BTreeMap::new();
    for pair in query.split('&').filter(|p| !p.is_empty()) {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        params.insert(decode(k)?, decode(v)?);
    }
    Ok(Parts {
        scheme,
        user: decode(user)?,
        password: decode(password)?,
        host: decode(&host)?.to_ascii_lowercase(),
        port,
        database: decode(path.split('/').next().unwrap_or_default())?,
        params,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_split_into_their_parts() {
        let t = parse("mysql", "mysql://app:p%40ss@db.example:3307/shop?ssl-mode=REQUIRED", &["mysql"], 3306, true)
            .unwrap();
        assert_eq!(
            (t.host.as_str(), t.port, t.user.as_str(), t.password.as_str()),
            ("db.example", 3307, "app", "p@ss")
        );
        assert_eq!(t.database, "shop");
        assert_eq!(t.params["ssl-mode"], "REQUIRED");
        let t = parse("x", "mongodb://[2606:4700::1]/", &["mongodb"], 27017, false).unwrap();
        assert_eq!((t.host.as_str(), t.port), ("2606:4700::1", 27017));
        for bad in ["mysql://u@/db", "mysql:///tmp/sock", "postgres://u@h/db", "mysql://u@a,b/db", "no scheme"] {
            assert!(parse("mysql", bad, &["mysql"], 3306, true).is_err(), "{bad}");
        }
        assert!(parse("mysql", "mysql://h/db", &["mysql"], 3306, true).is_err(), "a user is required");
    }

    #[tokio::test]
    async fn restricted_urls_are_public_hosts_with_allowed_parameters_only() {
        let schemes = ["mysql"];
        let ok = check("mysql", "mysql://u:p@8.8.8.8/db?ssl-mode=REQUIRED", &schemes, 3306, true, &["ssl-mode"], false)
            .await
            .unwrap();
        assert_eq!(ok.addr, Some("8.8.8.8".parse().unwrap()));
        for bad in [
            "mysql://u@127.0.0.1/db",
            "mysql://u@10.0.0.5/db",
            "mysql://u@169.254.169.254/db",
            "mysql://u@localhost/db",
            "mysql://u@8.8.8.8/db?socket=/tmp/x",
        ] {
            assert!(check("mysql", bad, &schemes, 3306, true, &["ssl-mode"], false).await.is_err(), "{bad}");
        }
        // console.allow_private_urls lets private hosts through, still without extra parameters.
        assert!(check("mysql", "mysql://u@127.0.0.1/db", &schemes, 3306, true, &[], true).await.is_ok());
        assert!(check("mysql", "mysql://u@127.0.0.1/db?a=b", &schemes, 3306, true, &[], true).await.is_err());
    }
}
