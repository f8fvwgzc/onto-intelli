//! Outbound requests a project chooses the target of (its LLM base URL,
//! and the git and PostgreSQL URLs of connectors its credentials register):
//! the SSRF guard of docs/CONTROL_PLANE.md section 14.
//!
//! Unless `console.allow_private_urls` is set, a URL must be `https` and its
//! host must resolve only to public addresses. The check runs on the
//! resolved addresses, in the HTTP client's own DNS resolver, right before
//! connecting (so a name that changes its answer after a check cannot slip
//! through), and on IP literals before the request is built (the client
//! does not resolve those). Loopback, private, link-local, CGNAT,
//! multicast, unspecified, broadcast, documentation and reserved ranges are
//! refused, IPv4 and IPv6 (with IPv4-mapped and NAT64 addresses judged by
//! the IPv4 address inside). Redirects are never followed and no proxy is
//! used.

use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

use reqwest::dns::{Addrs, Name, Resolve, Resolving};

use onto_core::{Error, Result};

/// Header names a project may not set: they carry credentials. So does a
/// name with a part (between `-`, `_` or `.`) that is one of
/// [`CREDENTIAL_WORDS`], starts with `auth`, or ends with `token`, `secret`,
/// `password` or `apikey` (`x-goog-api-key`, `x-portkey-api-key`,
/// `helicone-auth`, `x-auth-token`, `x-accesstoken`, …).
pub const CREDENTIAL_HEADERS: [&str; 5] = ["authorization", "proxy-authorization", "x-api-key", "api-key", "cookie"];

/// Parts of header names that mark a credential.
pub const CREDENTIAL_WORDS: [&str; 9] =
    ["key", "token", "secret", "password", "passwd", "credential", "credentials", "cookie", "signature"];

/// Does the header `name` (any case) carry credentials? Extra headers are
/// stored in plaintext and shown to every project member, so such headers
/// are refused; the provider's key goes in `api_key`, sealed.
pub fn credential_header(name: &str) -> bool {
    let n = name.trim().to_ascii_lowercase();
    CREDENTIAL_HEADERS.contains(&n.as_str())
        || n.split(['-', '_', '.']).any(|part| {
            CREDENTIAL_WORDS.contains(&part)
                || part.starts_with("auth")
                || ["token", "secret", "password", "apikey"].iter().any(|w| part.ends_with(w))
        })
}

pub use onto_intelli::net::public_ip;

/// A DNS resolver that answers only with public addresses: a name with any
/// non-public address is refused.
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

/// Check a project-chosen base URL: `https` (or `http` with
/// `allow_private`), a host, no credentials, query or fragment, and an IP
/// literal host only when public (or `allow_private`). Returns it without a
/// trailing `/`.
pub fn check_url(raw: &str, allow_private: bool) -> Result<String> {
    let raw = raw.trim();
    let url = url::Url::parse(raw).map_err(|e| Error::invalid(format!("base_url '{raw}': {e}")))?;
    match url.scheme() {
        "https" => {}
        "http" if allow_private => {}
        "http" => {
            return Err(Error::invalid(format!(
                "base_url '{raw}' must be https:// (http:// needs console.allow_private_urls)"
            )));
        }
        other => return Err(Error::invalid(format!("base_url '{raw}': scheme {other} is not http(s)"))),
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(Error::invalid("base_url may not carry credentials; use the api_key field"));
    }
    if url.query().is_some() || url.fragment().is_some() {
        return Err(Error::invalid("base_url may not have a query or a fragment"));
    }
    let ip = match url.host() {
        None => return Err(Error::invalid(format!("base_url '{raw}' has no host"))),
        Some(url::Host::Ipv4(v4)) => Some(IpAddr::V4(v4)),
        Some(url::Host::Ipv6(v6)) => Some(IpAddr::V6(v6)),
        Some(url::Host::Domain(_)) => None,
    };
    if let Some(ip) = ip
        && !allow_private
        && !public_ip(ip)
    {
        return Err(Error::invalid(format!(
            "base_url '{raw}': {ip} is not a public address (console.allow_private_urls is off)"
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

/// The HTTP client of project providers: no redirects, no proxy, the given
/// timeout, and (unless `allow_private`) the [`PublicOnly`] resolver.
pub fn client(allow_private: bool, timeout: Duration) -> Result<reqwest::Client> {
    let mut b = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .timeout(timeout)
        .connect_timeout(timeout.min(Duration::from_secs(10)));
    if !allow_private {
        b = b.dns_resolver(PublicOnly);
    }
    b.build().map_err(|e| Error::Config(format!("http client: {e}")))
}

/// Check a git URL a project credential registers (a repository
/// connector) with [`onto_intelli::connectors::repo::restricted_reach`]:
/// unless `allow_private`, it must be `https://` and its host must resolve
/// only to public addresses (an IP literal is judged as is). With
/// `allow_private`, `ssh://` and `user@host:path` URLs and private hosts
/// are accepted too. The same check runs again before every crawl, and git
/// is then pinned to the address checked, with redirects off.
pub async fn check_git_url(raw: &str, allow_private: bool) -> Result<()> {
    let url = onto_intelli::connectors::repo::parse_git_url(raw)?;
    tokio::task::spawn_blocking(move || onto_intelli::connectors::repo::restricted_reach(&url, allow_private))
        .await
        .map_err(|e| Error::Other(format!("checking a git URL: {e}")))??;
    Ok(())
}

/// Check a PostgreSQL connector URL a project credential registers with
/// [`onto_intelli::connectors::postgres::check_url`]: a TCP host and a user,
/// no Unix socket, no `host`, `hostaddr`, `port`, `options` or SSL file
/// parameters, and (unless `allow_private`) a host that resolves only to
/// public addresses. The check runs again whenever the connector connects.
pub async fn check_pg_url(raw: &str, allow_private: bool) -> Result<()> {
    onto_intelli::connectors::postgres::check_url(raw, allow_private).await.map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn git_urls() {
        for ok in ["https://8.8.8.8/acme/platform.git", "https://[2606:4700:4700::1111]/r"] {
            assert!(check_git_url(ok, false).await.is_ok(), "{ok}");
        }
        for bad in [
            "https://127.0.0.1/acme/platform",
            "https://10.1.2.3/r",
            "https://169.254.169.254/latest",
            "https://[::1]/r",
            "https://localhost/r",
            "git@github.com:acme/platform.git",
            "ssh://git@github.com/acme/platform",
            "ext::sh -c id",
            "file:///srv/repo",
        ] {
            assert!(check_git_url(bad, false).await.is_err(), "{bad}");
        }
        for ok in ["https://127.0.0.1/r", "git@git.internal:acme/platform.git", "ssh://git@10.0.0.5/r"] {
            assert!(check_git_url(ok, true).await.is_ok(), "{ok} with allow_private");
        }
        assert!(check_git_url("ext::sh -c id", true).await.is_err());
    }

    #[test]
    fn urls() {
        assert_eq!(check_url("https://api.example.com/v1/", false).unwrap(), "https://api.example.com/v1");
        for bad in [
            "http://api.example.com",
            "https://127.0.0.1:8080",
            "https://[::1]/v1",
            "https://169.254.169.254/latest",
            "https://user:pw@api.example.com",
            "https://api.example.com/?x=1",
            "file:///etc/passwd",
            "not a url",
        ] {
            assert!(check_url(bad, false).is_err(), "{bad}");
        }
        assert!(check_url("http://127.0.0.1:11434/v1", true).is_ok());
        assert!(check_url("ftp://127.0.0.1", true).is_err());
    }

    #[test]
    fn headers() {
        let h = |k: &str| std::collections::BTreeMap::from([(k.to_string(), "v".to_string())]);
        assert!(check_headers(&h("x-team")).is_ok());
        for bad in [
            "Authorization",
            "x-api-key",
            "API-Key",
            "cookie",
            "bad header",
            "proxy-authorization",
            "x-goog-api-key",
            "x-portkey-api-key",
            "helicone-auth",
            "x-auth-token",
            "x-client-secret",
            "x-access-token",
            "x-accesstoken",
            "x-apikey",
            "openai_api_key",
        ] {
            assert!(check_headers(&h(bad)).is_err(), "{bad}");
        }
        for ok in ["anthropic-beta", "openai-project", "x-portkey-provider", "helicone-property-team"] {
            assert!(check_headers(&h(ok)).is_ok(), "{ok}");
        }
    }

    #[tokio::test]
    async fn the_resolver_refuses_private_answers() {
        let err = match PublicOnly.resolve("localhost".parse().unwrap()).await {
            Ok(_) => panic!("localhost resolved"),
            Err(e) => e.to_string(),
        };
        assert!(err.contains("not a public address"), "{err}");
    }
}
