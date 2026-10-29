//! Outbound requests a project chooses the target of (its LLM base URL,
//! and the git and PostgreSQL URLs of connectors its credentials register):
//! the SSRF guard of docs/CONTROL_PLANE.md section 14. The client and the
//! checks are [`onto_intelli::net`]'s (docs/design/EVENTS.md B.5).
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

use onto_core::{Error, Result};

/// The guarded client, the URL and header checks and the address rules live
/// in [`onto_intelli::net`] (shared with webhook delivery); they are
/// re-exported here.
pub use onto_intelli::net::{
    AllowRule, CREDENTIAL_HEADERS, CREDENTIAL_WORDS, Cidr, Guarded, PublicOnly, Reach, always_denied, check_headers,
    check_target_url, check_url, client, credential_header, guarded_client, join_route, public_ip,
};

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
    use reqwest::dns::Resolve;

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
