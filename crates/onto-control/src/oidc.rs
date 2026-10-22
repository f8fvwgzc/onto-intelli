//! OpenID Connect for console sign-in (docs/CONTROL_PLANE.md, sections 7.1
//! to 7.3): discovery, JWKS, PKCE, the token exchange and ID token
//! verification with `aws-lc-rs`.
//!
//! The server is the OIDC client: the browser only follows redirects to the
//! IdP's authorization endpoint and back; the code is exchanged here, with
//! the PKCE verifier that never leaves the server.
//!
//! * Discovery (`{issuer}/.well-known/openid-configuration`) and the JWKS are
//!   cached for an hour; on an unknown `kid` the JWKS is fetched again, at
//!   most once a minute.
//! * HTTP: 10-second timeout, 1 MB response limit, no redirects followed for
//!   the token and userinfo endpoints (at most 3 for discovery and JWKS).
//! * ID tokens: compact JWS signed with RS256/384/512, PS256/384/512,
//!   ES256, ES384 or EdDSA (Ed25519). `none`, `HS*` and any `crit` header
//!   are refused.

use std::sync::Arc;
use std::time::{Duration, Instant};

use aws_lc_rs::signature::{self, RsaParameters, RsaPublicKeyComponents, UnparsedPublicKey};
use base64::Engine;
use parking_lot::Mutex;
use serde::Deserialize;
use serde_json::{Value, json};
use url::Url;

use onto_core::{Error, Result};
use onto_intelli::config::OidcProviderConfig;

use crate::keys::{b64, sha256};

/// How long a discovery document is trusted.
pub const DISCOVERY_TTL: Duration = Duration::from_secs(3600);
/// How long a JWKS is trusted.
pub const JWKS_TTL: Duration = Duration::from_secs(3600);
/// An unknown `kid` refetches the JWKS at most this often.
pub const JWKS_REFETCH_EVERY: Duration = Duration::from_secs(60);
/// Every request to an IdP.
pub const HTTP_TIMEOUT: Duration = Duration::from_secs(10);
/// Largest IdP response read.
pub const MAX_BODY: usize = 1024 * 1024;
/// Allowed clock difference with the IdP for `exp` and `iat`.
pub const CLOCK_SKEW_SECS: i64 = 60;
/// Path of the redirect URI under `console.public_url`.
pub const CALLBACK_PATH: &str = "/v1/console/auth/oidc/callback";

// ── URLs ──────────────────────────────────────────────────────────────────

/// `localhost`, `127.0.0.0/8` or `[::1]`.
pub fn is_loopback(url: &Url) -> bool {
    match url.host() {
        Some(url::Host::Domain(d)) => d.eq_ignore_ascii_case("localhost"),
        Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
        Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
        None => false,
    }
}

/// `https://`, or `http://` with a loopback host.
pub fn https_or_loopback(url: &Url) -> bool {
    url.host().is_some() && (url.scheme() == "https" || (url.scheme() == "http" && is_loopback(url)))
}

fn trim_one_slash(s: &str) -> &str {
    s.strip_suffix('/').unwrap_or(s)
}

fn endpoint(what: &str, v: &str) -> Result<Url> {
    let u = Url::parse(v).map_err(|e| Error::Config(format!("{what} '{v}' is not a URL: {e}")))?;
    if !https_or_loopback(&u) {
        return Err(Error::Config(format!("{what} '{v}' must be https:// (http:// only for a loopback host)")));
    }
    Ok(u)
}

/// The startup rules of one `[[console.oidc]]` entry: `id` of `[a-z0-9-]`
/// (1 to 40 characters), a name, an `https://` issuer (or loopback `http://`)
/// without query or fragment, a client id, and `openid` among the scopes.
pub fn validate_provider(p: &OidcProviderConfig) -> Result<()> {
    let id = &p.id;
    if id.is_empty() || id.len() > 40 || !id.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    {
        return Err(Error::Config(format!("console.oidc id '{id}' must be 1 to 40 characters of [a-z0-9-]")));
    }
    if p.name.trim().is_empty() {
        return Err(Error::Config(format!("console.oidc '{id}' needs a name (shown on the sign-in button)")));
    }
    let issuer = endpoint(&format!("console.oidc '{id}' issuer"), &p.issuer)?;
    if issuer.query().is_some() || issuer.fragment().is_some() {
        return Err(Error::Config(format!("console.oidc '{id}' issuer must not have a query or fragment")));
    }
    if p.client_id.trim().is_empty() {
        return Err(Error::Config(format!("console.oidc '{id}' needs a client_id")));
    }
    if !p.scopes.iter().any(|s| s == "openid") {
        return Err(Error::Config(format!("console.oidc '{id}' scopes must include \"openid\"")));
    }
    Ok(())
}

// ── discovery ─────────────────────────────────────────────────────────────

/// The parts of an OpenID Provider configuration document the console uses.
#[derive(Clone, Debug, Deserialize)]
pub struct Discovery {
    pub issuer: String,
    pub authorization_endpoint: String,
    pub token_endpoint: String,
    pub jwks_uri: String,
    #[serde(default)]
    pub userinfo_endpoint: Option<String>,
    #[serde(default)]
    pub code_challenge_methods_supported: Option<Vec<String>>,
    #[serde(default)]
    pub token_endpoint_auth_methods_supported: Option<Vec<String>>,
}

impl Discovery {
    /// The rules of section 7.1: the same issuer (one trailing slash aside),
    /// `https` endpoints unless loopback, and `S256` when methods are listed.
    pub fn check(&self, configured_issuer: &str) -> Result<()> {
        if trim_one_slash(&self.issuer) != trim_one_slash(configured_issuer) {
            return Err(Error::Config(format!(
                "the discovery document's issuer '{}' is not the configured issuer '{configured_issuer}'",
                self.issuer
            )));
        }
        endpoint("authorization_endpoint", &self.authorization_endpoint)?;
        endpoint("token_endpoint", &self.token_endpoint)?;
        endpoint("jwks_uri", &self.jwks_uri)?;
        if let Some(u) = &self.userinfo_endpoint {
            endpoint("userinfo_endpoint", u)?;
        }
        if let Some(m) = &self.code_challenge_methods_supported
            && !m.iter().any(|m| m == "S256")
        {
            return Err(Error::Config(
                "the IdP does not support PKCE with S256 (code_challenge_methods_supported)".into(),
            ));
        }
        Ok(())
    }

    /// How the client authenticates at the token endpoint when it has a
    /// secret: `client_secret_basic` (the OIDC default) or `client_secret_post`.
    pub fn secret_method(&self) -> Result<SecretMethod> {
        let methods =
            self.token_endpoint_auth_methods_supported.clone().unwrap_or_else(|| vec!["client_secret_basic".into()]);
        if methods.iter().any(|m| m == "client_secret_basic") {
            Ok(SecretMethod::Basic)
        } else if methods.iter().any(|m| m == "client_secret_post") {
            Ok(SecretMethod::Post)
        } else {
            Err(Error::Config(format!(
                "the IdP supports neither client_secret_basic nor client_secret_post ({})",
                methods.join(", ")
            )))
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SecretMethod {
    Basic,
    Post,
}

// ── JWKS and JWS ──────────────────────────────────────────────────────────

/// One JSON Web Key; the fields the supported algorithms read.
#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
pub struct Jwk {
    pub kty: String,
    #[serde(default)]
    pub kid: Option<String>,
    #[serde(default, rename = "use")]
    pub use_: Option<String>,
    #[serde(default)]
    pub alg: Option<String>,
    #[serde(default)]
    pub crv: Option<String>,
    #[serde(default)]
    pub n: Option<String>,
    #[serde(default)]
    pub e: Option<String>,
    #[serde(default)]
    pub x: Option<String>,
    #[serde(default)]
    pub y: Option<String>,
}

/// The keys of a JWKS document; entries that are not keys are skipped.
pub fn parse_jwks(body: &[u8]) -> Result<Vec<Jwk>> {
    #[derive(Deserialize)]
    struct Set {
        keys: Vec<Value>,
    }
    let set: Set =
        serde_json::from_slice(body).map_err(|e| Error::Unavailable(format!("the JWKS is not JSON: {e}")))?;
    Ok(set.keys.into_iter().filter_map(|k| serde_json::from_value(k).ok()).collect())
}

/// The signature algorithms accepted for ID tokens.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Alg {
    RS256,
    RS384,
    RS512,
    PS256,
    PS384,
    PS512,
    ES256,
    ES384,
    EdDSA,
}

impl Alg {
    pub fn parse(s: &str) -> Option<Alg> {
        Some(match s {
            "RS256" => Alg::RS256,
            "RS384" => Alg::RS384,
            "RS512" => Alg::RS512,
            "PS256" => Alg::PS256,
            "PS384" => Alg::PS384,
            "PS512" => Alg::PS512,
            "ES256" => Alg::ES256,
            "ES384" => Alg::ES384,
            "EdDSA" => Alg::EdDSA,
            _ => return None,
        })
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Alg::RS256 => "RS256",
            Alg::RS384 => "RS384",
            Alg::RS512 => "RS512",
            Alg::PS256 => "PS256",
            Alg::PS384 => "PS384",
            Alg::PS512 => "PS512",
            Alg::ES256 => "ES256",
            Alg::ES384 => "ES384",
            Alg::EdDSA => "EdDSA",
        }
    }

    /// The JWK `kty` of this algorithm's keys.
    pub fn kty(self) -> &'static str {
        match self {
            Alg::RS256 | Alg::RS384 | Alg::RS512 | Alg::PS256 | Alg::PS384 | Alg::PS512 => "RSA",
            Alg::ES256 | Alg::ES384 => "EC",
            Alg::EdDSA => "OKP",
        }
    }

    fn rsa_params(self) -> Option<&'static RsaParameters> {
        Some(match self {
            Alg::RS256 => &signature::RSA_PKCS1_2048_8192_SHA256,
            Alg::RS384 => &signature::RSA_PKCS1_2048_8192_SHA384,
            Alg::RS512 => &signature::RSA_PKCS1_2048_8192_SHA512,
            Alg::PS256 => &signature::RSA_PSS_2048_8192_SHA256,
            Alg::PS384 => &signature::RSA_PSS_2048_8192_SHA384,
            Alg::PS512 => &signature::RSA_PSS_2048_8192_SHA512,
            _ => return None,
        })
    }
}

/// Why an ID token was refused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TokenError {
    /// No key of the JWKS has this `kid` (the IdP may have rotated its keys).
    UnknownKey(String),
    Rejected(String),
}

impl std::fmt::Display for TokenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TokenError::UnknownKey(kid) => write!(f, "no key '{kid}' in the IdP's JWKS"),
            TokenError::Rejected(m) => f.write_str(m),
        }
    }
}

fn reject(m: impl Into<String>) -> TokenError {
    TokenError::Rejected(m.into())
}

fn unb64(part: &str) -> Option<Vec<u8>> {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(part).ok()
}

/// A compact JWS split and decoded, its signature not yet checked.
#[derive(Clone, Debug)]
pub struct Jws {
    pub alg: Alg,
    pub kid: Option<String>,
    signing_input: String,
    payload: Vec<u8>,
    signature: Vec<u8>,
}

impl Jws {
    /// Steps 1 and 2 of section 7.3: three base64url parts, a JSON header
    /// with a supported `alg` (never `none` or `HS*`) and no `crit`.
    pub fn decode(token: &str) -> std::result::Result<Jws, TokenError> {
        let parts: Vec<&str> = token.split('.').collect();
        if parts.len() != 3 || parts[0].is_empty() || parts[1].is_empty() {
            return Err(reject("the ID token is not a compact JWS (three base64url parts)"));
        }
        let header = unb64(parts[0]).ok_or_else(|| reject("the ID token header is not base64url"))?;
        let header: Value =
            serde_json::from_slice(&header).map_err(|_| reject("the ID token header is not a JSON object"))?;
        let Value::Object(header) = header else { return Err(reject("the ID token header is not a JSON object")) };
        let alg = header.get("alg").and_then(Value::as_str).ok_or_else(|| reject("the ID token header has no alg"))?;
        if alg.eq_ignore_ascii_case("none") {
            return Err(reject("unsigned ID token (alg none) refused"));
        }
        if alg.starts_with("HS") {
            return Err(reject(format!("symmetric ID token signature ({alg}) refused")));
        }
        let alg = Alg::parse(alg).ok_or_else(|| reject(format!("unsupported ID token alg '{alg}'")))?;
        if header.contains_key("crit") {
            return Err(reject("ID token header with crit refused"));
        }
        let kid = match header.get("kid") {
            None => None,
            Some(Value::String(k)) => Some(k.clone()),
            Some(_) => return Err(reject("the ID token kid is not a string")),
        };
        let payload = unb64(parts[1]).ok_or_else(|| reject("the ID token payload is not base64url"))?;
        let signature = unb64(parts[2])
            .filter(|s| !s.is_empty())
            .ok_or_else(|| reject("the ID token signature is missing or not base64url"))?;
        Ok(Jws { alg, kid, signing_input: format!("{}.{}", parts[0], parts[1]), payload, signature })
    }

    /// Step 3: the JWK with the header's `kid` (without one, the only key of
    /// the algorithm's `kty`), whose `use` and `alg`, when given, fit.
    pub fn key<'k>(&self, keys: &'k [Jwk]) -> std::result::Result<&'k Jwk, TokenError> {
        let kty = self.alg.kty();
        let key = match &self.kid {
            Some(kid) => keys
                .iter()
                .find(|k| k.kid.as_deref() == Some(kid))
                .ok_or_else(|| TokenError::UnknownKey(kid.clone()))?,
            None => {
                let mut of_kty = keys.iter().filter(|k| k.kty == kty);
                match (of_kty.next(), of_kty.next()) {
                    (Some(k), None) => k,
                    (None, _) => return Err(TokenError::UnknownKey(format!("(no kid, no {kty} key)"))),
                    (Some(_), Some(_)) => {
                        return Err(reject(format!("the ID token has no kid and the JWKS has several {kty} keys")));
                    }
                }
            }
        };
        if key.kty != kty {
            return Err(reject(format!(
                "key '{}' is {}, not {kty} as {} needs",
                key.kid.as_deref().unwrap_or(""),
                key.kty,
                self.alg.as_str()
            )));
        }
        if key.use_.as_deref().is_some_and(|u| u != "sig") {
            return Err(reject("the JWK is not a signing key (use is not sig)"));
        }
        if key.alg.as_deref().is_some_and(|a| a != self.alg.as_str()) {
            return Err(reject(format!("the JWK's alg is not the ID token's {}", self.alg.as_str())));
        }
        Ok(key)
    }

    /// Steps 3 and 4: find the key and check the signature; returns the
    /// payload as a JSON object.
    pub fn verify(&self, keys: &[Jwk]) -> std::result::Result<Value, TokenError> {
        let key = self.key(keys)?;
        verify_signature(self.alg, key, self.signing_input.as_bytes(), &self.signature)?;
        let claims: Value =
            serde_json::from_slice(&self.payload).map_err(|_| reject("the ID token payload is not JSON"))?;
        if !claims.is_object() {
            return Err(reject("the ID token payload is not a JSON object"));
        }
        Ok(claims)
    }
}

fn component(key: &Jwk, name: &str, v: &Option<String>) -> std::result::Result<Vec<u8>, TokenError> {
    v.as_deref()
        .and_then(unb64)
        .filter(|b| !b.is_empty())
        .ok_or_else(|| reject(format!("the {} key has no valid '{name}'", key.kty)))
}

/// Check `sig` over `msg` with the JWK, by algorithm.
pub fn verify_signature(alg: Alg, key: &Jwk, msg: &[u8], sig: &[u8]) -> std::result::Result<(), TokenError> {
    let bad = || reject("the ID token signature does not verify");
    if let Some(params) = alg.rsa_params() {
        let n = component(key, "n", &key.n)?;
        let e = component(key, "e", &key.e)?;
        return RsaPublicKeyComponents { n: &n, e: &e }.verify(params, msg, sig).map_err(|_| bad());
    }
    match alg {
        Alg::ES256 | Alg::ES384 => {
            let (crv, len, verifier) = if alg == Alg::ES256 {
                ("P-256", 32, &signature::ECDSA_P256_SHA256_FIXED)
            } else {
                ("P-384", 48, &signature::ECDSA_P384_SHA384_FIXED)
            };
            if key.crv.as_deref() != Some(crv) {
                return Err(reject(format!("{} needs a {crv} key", alg.as_str())));
            }
            let x = component(key, "x", &key.x)?;
            let y = component(key, "y", &key.y)?;
            if x.len() != len || y.len() != len {
                return Err(reject(format!("the {crv} key's x and y must be {len} bytes each")));
            }
            let mut point = Vec::with_capacity(1 + 2 * len);
            point.push(0x04);
            point.extend_from_slice(&x);
            point.extend_from_slice(&y);
            UnparsedPublicKey::new(verifier, &point).verify(msg, sig).map_err(|_| bad())
        }
        Alg::EdDSA => {
            if key.crv.as_deref() != Some("Ed25519") {
                return Err(reject("EdDSA is accepted with Ed25519 keys only"));
            }
            let x = component(key, "x", &key.x)?;
            if x.len() != 32 {
                return Err(reject("the Ed25519 key's x must be 32 bytes"));
            }
            UnparsedPublicKey::new(&signature::ED25519, &x).verify(msg, sig).map_err(|_| bad())
        }
        _ => Err(reject("unsupported algorithm")),
    }
}

// ── claims ────────────────────────────────────────────────────────────────

/// What the claims of an ID token must match.
#[derive(Clone, Debug)]
pub struct Expect<'a> {
    /// The discovered issuer, compared exactly.
    pub issuer: &'a str,
    pub client_id: &'a str,
    /// Extra accepted `aud` values.
    pub audiences: &'a [String],
    pub nonce: &'a str,
    /// Unix seconds.
    pub now: i64,
}

/// The verified identity and profile of an ID token (or of userinfo).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct IdClaims {
    pub sub: String,
    pub email: Option<String>,
    /// `email` may be trusted (invitations, domain rules) only when true.
    pub email_verified: bool,
    pub name: Option<String>,
    pub picture: Option<String>,
    pub preferred_username: Option<String>,
}

impl IdClaims {
    /// The profile claims kept with an identity (never the token).
    pub fn profile(&self) -> Value {
        json!({ "email": self.email, "email_verified": self.email_verified, "name": self.name,
                "picture": self.picture, "preferred_username": self.preferred_username })
    }

    /// Take `email`, `email_verified` and missing profile fields from
    /// another set of claims (userinfo).
    pub fn merge_profile(&mut self, other: &IdClaims) {
        if self.email.is_none() {
            self.email = other.email.clone();
            self.email_verified = other.email_verified;
        }
        if self.name.is_none() {
            self.name = other.name.clone();
        }
        if self.picture.is_none() {
            self.picture = other.picture.clone();
        }
        if self.preferred_username.is_none() {
            self.preferred_username = other.preferred_username.clone();
        }
    }
}

fn text(claims: &Value, name: &str) -> Option<String> {
    claims
        .get(name)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.chars().take(320).collect())
}

fn seconds(claims: &Value, name: &str) -> std::result::Result<i64, TokenError> {
    let v = claims.get(name).ok_or_else(|| reject(format!("the ID token has no {name}")))?;
    v.as_i64()
        .or_else(|| v.as_f64().map(|f| f as i64))
        .ok_or_else(|| reject(format!("the ID token's {name} is not a number")))
}

/// The profile claims of an ID token or userinfo response. `email_verified`
/// is true only for `true` (or the string `"true"`, which some IdPs send).
pub fn profile_claims(claims: &Value) -> IdClaims {
    let email_verified = match claims.get("email_verified") {
        Some(Value::Bool(b)) => *b,
        Some(Value::String(s)) => s.eq_ignore_ascii_case("true"),
        _ => false,
    };
    let name = text(claims, "name").or_else(|| {
        let parts: Vec<String> = ["given_name", "family_name"].iter().filter_map(|n| text(claims, n)).collect();
        (!parts.is_empty()).then(|| parts.join(" "))
    });
    IdClaims {
        sub: text(claims, "sub").unwrap_or_default(),
        email: text(claims, "email"),
        email_verified,
        name,
        picture: text(claims, "picture"),
        preferred_username: text(claims, "preferred_username"),
    }
}

/// Step 5 of section 7.3.
pub fn check_claims(claims: &Value, e: &Expect<'_>) -> std::result::Result<IdClaims, TokenError> {
    if claims.get("iss").and_then(Value::as_str) != Some(e.issuer) {
        return Err(reject(format!("the ID token's iss is not the issuer {}", e.issuer)));
    }
    let aud: Vec<&str> = match claims.get("aud") {
        Some(Value::String(a)) => vec![a.as_str()],
        Some(Value::Array(v)) => v.iter().filter_map(Value::as_str).collect(),
        _ => return Err(reject("the ID token has no aud")),
    };
    if !aud.iter().any(|a| *a == e.client_id || e.audiences.iter().any(|x| x == a)) {
        return Err(reject("the ID token's aud does not name this client"));
    }
    let azp = claims.get("azp").and_then(Value::as_str);
    if aud.len() > 1 && azp.is_none() {
        return Err(reject("the ID token has several aud values and no azp"));
    }
    if azp.is_some_and(|a| a != e.client_id) {
        return Err(reject("the ID token's azp is not this client"));
    }
    if seconds(claims, "exp")? <= e.now - CLOCK_SKEW_SECS {
        return Err(reject("the ID token has expired (exp)"));
    }
    if seconds(claims, "iat")? >= e.now + CLOCK_SKEW_SECS {
        return Err(reject("the ID token was issued in the future (iat)"));
    }
    let nonce = claims.get("nonce").and_then(Value::as_str).unwrap_or_default();
    if !crate::keys::constant_time_eq(nonce.as_bytes(), e.nonce.as_bytes()) {
        return Err(reject("the ID token's nonce is not this sign-in's"));
    }
    let out = profile_claims(claims);
    if out.sub.is_empty() {
        return Err(reject("the ID token has no sub"));
    }
    Ok(out)
}

// ── PKCE and the authorization request ────────────────────────────────────

/// `BASE64URL(SHA-256(verifier))`, the S256 code challenge.
pub fn code_challenge(verifier: &str) -> String {
    b64(&sha256(verifier.as_bytes()))
}

fn form_encode(s: &str) -> String {
    url::form_urlencoded::byte_serialize(s.as_bytes()).collect()
}

// ── the provider ──────────────────────────────────────────────────────────

/// The token endpoint's answer.
#[derive(Clone, Deserialize)]
pub struct TokenResponse {
    pub id_token: String,
    #[serde(default)]
    pub access_token: Option<String>,
}

impl std::fmt::Debug for TokenResponse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("TokenResponse(****)")
    }
}

#[derive(Default)]
struct JwksState {
    keys: Arc<Vec<Jwk>>,
    fetched_at: Option<Instant>,
    /// The last refetch caused by an unknown `kid`.
    refetched_at: Option<Instant>,
}

/// One configured IdP with its cached discovery document and JWKS.
pub struct Provider {
    cfg: OidcProviderConfig,
    secret: Option<String>,
    /// Discovery and JWKS (follows at most 3 redirects).
    fetch: reqwest::Client,
    /// Token and userinfo endpoints (follows no redirect).
    direct: reqwest::Client,
    discovery: Mutex<Option<(Arc<Discovery>, Instant)>>,
    jwks: Mutex<JwksState>,
}

impl std::fmt::Debug for Provider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Provider")
            .field("id", &self.cfg.id)
            .field("issuer", &self.cfg.issuer)
            .field("client_id", &self.cfg.client_id)
            .field("secret", &self.secret.as_ref().map(|_| "****"))
            .finish_non_exhaustive()
    }
}

fn client(redirects: reqwest::redirect::Policy) -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .timeout(HTTP_TIMEOUT)
        .redirect(redirects)
        .user_agent(concat!("onto-intelli/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|e| Error::Config(format!("OIDC HTTP client: {e}")))
}

/// Read a response body, at most [`MAX_BODY`] bytes.
async fn read_body(mut resp: reqwest::Response, what: &str) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    while let Some(chunk) = resp.chunk().await.map_err(|e| Error::Unavailable(format!("{what}: {e}")))? {
        if out.len() + chunk.len() > MAX_BODY {
            return Err(Error::Unavailable(format!("{what}: the response is larger than 1 MB")));
        }
        out.extend_from_slice(&chunk);
    }
    Ok(out)
}

fn clip(s: &str, n: usize) -> String {
    let s: String = s.chars().filter(|c| !c.is_control()).collect();
    if s.chars().count() <= n { s } else { format!("{}…", s.chars().take(n).collect::<String>()) }
}

async fn get_json(http: &reqwest::Client, url: &str, bearer: Option<&str>, what: &str) -> Result<Value> {
    let mut req = http.get(url).header(reqwest::header::ACCEPT, "application/json");
    if let Some(t) = bearer {
        req = req.bearer_auth(t);
    }
    let resp = req.send().await.map_err(|e| Error::Unavailable(format!("{what} ({url}): {e}")))?;
    let status = resp.status();
    let body = read_body(resp, what).await?;
    if !status.is_success() {
        return Err(Error::Unavailable(format!("{what} ({url}) answered {status}")));
    }
    serde_json::from_slice(&body).map_err(|e| Error::Unavailable(format!("{what} ({url}) is not JSON: {e}")))
}

impl Provider {
    /// A provider from its configuration (checked with [`validate_provider`])
    /// and its client secret, when it has one.
    pub fn new(cfg: OidcProviderConfig, secret: Option<String>) -> Result<Self> {
        validate_provider(&cfg)?;
        Ok(Self {
            cfg,
            secret: secret.filter(|s| !s.is_empty()),
            fetch: client(reqwest::redirect::Policy::limited(3))?,
            direct: client(reqwest::redirect::Policy::none())?,
            discovery: Mutex::new(None),
            jwks: Mutex::new(JwksState::default()),
        })
    }

    pub fn id(&self) -> &str {
        &self.cfg.id
    }

    pub fn name(&self) -> &str {
        &self.cfg.name
    }

    pub fn config(&self) -> &OidcProviderConfig {
        &self.cfg
    }

    /// `{issuer}/.well-known/openid-configuration`, cached for an hour.
    pub async fn discovery(&self) -> Result<Arc<Discovery>> {
        if let Some((d, at)) = self.discovery.lock().as_ref()
            && at.elapsed() < DISCOVERY_TTL
        {
            return Ok(d.clone());
        }
        let url = format!("{}/.well-known/openid-configuration", trim_one_slash(&self.cfg.issuer));
        let v = get_json(&self.fetch, &url, None, "OIDC discovery").await?;
        let d: Discovery =
            serde_json::from_value(v).map_err(|e| Error::Unavailable(format!("OIDC discovery ({url}): {e}")))?;
        d.check(&self.cfg.issuer).map_err(|e| Error::Unavailable(format!("OIDC discovery ({url}): {e}")))?;
        let d = Arc::new(d);
        *self.discovery.lock() = Some((d.clone(), Instant::now()));
        Ok(d)
    }

    async fn fetch_keys(&self, d: &Discovery) -> Result<Arc<Vec<Jwk>>> {
        let resp = self
            .fetch
            .get(&d.jwks_uri)
            .send()
            .await
            .map_err(|e| Error::Unavailable(format!("OIDC JWKS ({}): {e}", d.jwks_uri)))?;
        let status = resp.status();
        let body = read_body(resp, "OIDC JWKS").await?;
        if !status.is_success() {
            return Err(Error::Unavailable(format!("OIDC JWKS ({}) answered {status}", d.jwks_uri)));
        }
        let keys = Arc::new(parse_jwks(&body)?);
        let mut s = self.jwks.lock();
        s.keys = keys.clone();
        s.fetched_at = Some(Instant::now());
        Ok(keys)
    }

    /// The JWKS, cached for an hour.
    pub async fn keys(&self, d: &Discovery) -> Result<Arc<Vec<Jwk>>> {
        {
            let s = self.jwks.lock();
            if s.fetched_at.is_some_and(|at| at.elapsed() < JWKS_TTL) {
                return Ok(s.keys.clone());
            }
        }
        self.fetch_keys(d).await
    }

    /// The JWKS fetched again for an unknown `kid`, unless that happened
    /// less than a minute ago (`None` then).
    async fn refetch_keys(&self, d: &Discovery) -> Result<Option<Arc<Vec<Jwk>>>> {
        {
            let mut s = self.jwks.lock();
            if s.refetched_at.is_some_and(|at| at.elapsed() < JWKS_REFETCH_EVERY) {
                return Ok(None);
            }
            s.refetched_at = Some(Instant::now());
        }
        self.fetch_keys(d).await.map(Some)
    }

    /// `{public_url}/v1/console/auth/oidc/callback`.
    pub fn redirect_uri(public_url: &str) -> String {
        format!("{}{CALLBACK_PATH}", trim_one_slash(public_url))
    }

    /// The authorization request: code flow, PKCE S256, state and nonce.
    pub fn authorization_url(
        &self,
        d: &Discovery,
        public_url: &str,
        state: &str,
        nonce: &str,
        verifier: &str,
    ) -> Result<String> {
        let mut u = endpoint("authorization_endpoint", &d.authorization_endpoint)?;
        u.query_pairs_mut()
            .append_pair("response_type", "code")
            .append_pair("client_id", &self.cfg.client_id)
            .append_pair("redirect_uri", &Self::redirect_uri(public_url))
            .append_pair("scope", &self.cfg.scopes.join(" "))
            .append_pair("state", state)
            .append_pair("nonce", nonce)
            .append_pair("code_challenge", &code_challenge(verifier))
            .append_pair("code_challenge_method", "S256");
        Ok(u.to_string())
    }

    /// Exchange the code at the token endpoint (no redirects followed).
    /// Network failures are `Unavailable`; a refusal by the IdP is `Denied`.
    pub async fn exchange_code(
        &self,
        d: &Discovery,
        public_url: &str,
        code: &str,
        verifier: &str,
    ) -> Result<TokenResponse> {
        let redirect_uri = Self::redirect_uri(public_url);
        let method = match &self.secret {
            Some(_) => Some(d.secret_method()?),
            None => None,
        };
        let body = {
            let mut form = url::form_urlencoded::Serializer::new(String::new());
            form.append_pair("grant_type", "authorization_code")
                .append_pair("code", code)
                .append_pair("redirect_uri", &redirect_uri)
                .append_pair("code_verifier", verifier)
                .append_pair("client_id", &self.cfg.client_id);
            if let (Some(secret), Some(SecretMethod::Post)) = (&self.secret, method) {
                form.append_pair("client_secret", secret);
            }
            form.finish()
        };
        let mut req = self
            .direct
            .post(&d.token_endpoint)
            .header(reqwest::header::ACCEPT, "application/json")
            .header(reqwest::header::CONTENT_TYPE, "application/x-www-form-urlencoded");
        if let (Some(secret), Some(SecretMethod::Basic)) = (&self.secret, method) {
            // RFC 6749 section 2.3.1: both parts form-encoded first.
            let pair = format!("{}:{}", form_encode(&self.cfg.client_id), form_encode(secret));
            let basic = base64::engine::general_purpose::STANDARD.encode(pair);
            req = req.header(reqwest::header::AUTHORIZATION, format!("Basic {basic}"));
        }
        let resp = req.body(body).send().await.map_err(|e| Error::Unavailable(format!("OIDC token endpoint: {e}")))?;
        let status = resp.status();
        if status.is_redirection() {
            return Err(Error::Unavailable(format!("the OIDC token endpoint answered a redirect ({status})")));
        }
        let body = read_body(resp, "OIDC token endpoint").await?;
        let v: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
        if !status.is_success() {
            let err = v.get("error").and_then(Value::as_str).unwrap_or("error");
            let desc = v.get("error_description").and_then(Value::as_str).unwrap_or_default();
            let msg = clip(&format!("the IdP refused the code ({status}): {err} {desc}"), 300);
            return Err(if status.is_server_error() { Error::Unavailable(msg) } else { Error::Denied(msg) });
        }
        serde_json::from_value(v).map_err(|_| Error::Denied("the token endpoint's answer has no id_token".into()))
    }

    /// Verify an ID token (section 7.3) against this provider's discovered
    /// issuer and JWKS. An unknown `kid` refetches the JWKS once (at most
    /// once a minute). Fetch failures are `Err(Error)`; refusals `Ok(Err)`.
    pub async fn verify_id_token(
        &self,
        d: &Discovery,
        token: &str,
        nonce: &str,
        now: i64,
    ) -> Result<std::result::Result<IdClaims, TokenError>> {
        let jws = match Jws::decode(token) {
            Ok(j) => j,
            Err(e) => return Ok(Err(e)),
        };
        let keys = self.keys(d).await?;
        let claims = match jws.verify(&keys) {
            Err(TokenError::UnknownKey(kid)) => match self.refetch_keys(d).await? {
                Some(keys) => jws.verify(&keys),
                None => Err(TokenError::UnknownKey(kid)),
            },
            other => other,
        };
        let claims = match claims {
            Ok(c) => c,
            Err(e) => return Ok(Err(e)),
        };
        let expect =
            Expect { issuer: &d.issuer, client_id: &self.cfg.client_id, audiences: &self.cfg.audiences, nonce, now };
        Ok(check_claims(&claims, &expect))
    }

    /// The userinfo endpoint (no redirects followed). Its `sub` must equal
    /// the ID token's; `Ok(None)` when the IdP has no userinfo endpoint.
    pub async fn userinfo(&self, d: &Discovery, access_token: &str, sub: &str) -> Result<Option<IdClaims>> {
        let Some(url) = &d.userinfo_endpoint else { return Ok(None) };
        let v = get_json(&self.direct, url, Some(access_token), "OIDC userinfo").await?;
        let claims = profile_claims(&v);
        if claims.sub != sub {
            return Err(Error::Denied("the userinfo sub is not the ID token's sub".into()));
        }
        Ok(Some(claims))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aws_lc_rs::rand::SystemRandom;
    use aws_lc_rs::signature::{ECDSA_P256_SHA256_FIXED_SIGNING, EcdsaKeyPair, Ed25519KeyPair, KeyPair};

    // RFC 7515 appendix A.2: an RS256 JWS and the public part of its key.
    const RFC_N: &str = "ofgWCuLjybRlzo0tZWJjNiuSfb4p4fAkd_wWJcyQoTbji9k0l8W26mPddxHmfHQp-Vaw-4qPCJrcS2mJPMEzP1Pt0Bm4d4QlL-yRT-SFd2lZS-pCgNMsD1W_YpRPEwOWvG6b32690r2jZ47soMZo9wGzjb_7OMg0LOL-bSf63kpaSHSXndS5z5rexMdbBYUsLA9e-KXBdQOS-UTo7WTBEMa2R2CapHg665xsmtdVMTBQY4uDZlxvb3qCo5ZwKh9kG4LT6_I5IhlJH7aGhyxXFvUK-DWNmoudF8NAco9_h9iaGNj8q2ethFkMLs91kzk2PAcDTW9gb54h4FRWyuXpoQ";
    const RFC_JWS: &str = "eyJhbGciOiJSUzI1NiJ9.eyJpc3MiOiJqb2UiLA0KICJleHAiOjEzMDA4MTkzODAsDQogImh0dHA6Ly9leGFtcGxlLmNvbS9pc19yb290Ijp0cnVlfQ.cC4hiUPoj9Eetdgtv3hF80EGrhuB__dzERat0XF9g2VtQgr9PJbu3XOiZj5RZmh7AAuHIm4Bh-0Qc_lF5YKt_O8W2Fp5jujGbds9uJdbF9CUAr7t1dnZcAcQjbKBYNX4BAynRFdiuB--f_nZLgrnbyTyWzO75vRK5h6xBArLIARNPvkSjtQBMHlb1L07Qe7K0GarZRmB_eSN9383LcOLn6_dO--xi12jzDwusC-eOkHWEsqtFZESc6BfI7noOPqvhJ1phCnvWh6IeYI2w9QOYEUipUTI8np6LbgGY9Fs98rqVt5AXLIhWkWywlVmtVrBp0igcN_IoypGlUPQGe77Rw";

    fn rfc_key() -> Jwk {
        Jwk { kty: "RSA".into(), n: Some(RFC_N.into()), e: Some("AQAB".into()), ..Default::default() }
    }

    fn enc(v: &Value) -> String {
        b64(&serde_json::to_vec(v).unwrap())
    }

    fn sign_es(kp: &EcdsaKeyPair, header: &Value, claims: &Value) -> String {
        let input = format!("{}.{}", enc(header), enc(claims));
        let sig = kp.sign(&SystemRandom::new(), input.as_bytes()).unwrap();
        format!("{input}.{}", b64(sig.as_ref()))
    }

    fn es_jwk(kp: &EcdsaKeyPair, kid: &str) -> Jwk {
        let p = kp.public_key().as_ref();
        Jwk {
            kty: "EC".into(),
            kid: Some(kid.into()),
            crv: Some("P-256".into()),
            x: Some(b64(&p[1..33])),
            y: Some(b64(&p[33..65])),
            ..Default::default()
        }
    }

    #[test]
    fn rfc7515_rs256_vector_verifies() {
        let jws = Jws::decode(RFC_JWS).unwrap();
        assert_eq!((jws.alg, jws.kid.as_deref()), (Alg::RS256, None));
        let claims = jws.verify(&[rfc_key()]).unwrap();
        assert_eq!(claims["iss"], "joe");
        assert_eq!(claims["exp"], 1300819380);
        // A changed payload no longer verifies.
        let parts: Vec<&str> = RFC_JWS.split('.').collect();
        let tampered = format!("{}.{}.{}", parts[0], enc(&json!({"iss": "joe", "exp": 1300819380})), parts[2]);
        assert_eq!(
            Jws::decode(&tampered).unwrap().verify(&[rfc_key()]),
            Err(reject("the ID token signature does not verify"))
        );
        // The RSA key must not claim another algorithm or use.
        let mut k = rfc_key();
        k.alg = Some("RS512".into());
        assert!(jws.verify(&[k]).is_err());
        let mut k = rfc_key();
        k.use_ = Some("enc".into());
        assert!(jws.verify(&[k]).is_err());
        // PS256 over the same key does not accept a PKCS#1 signature.
        let ps = format!("{}.{}.{}", enc(&json!({"alg": "PS256"})), parts[1], parts[2]);
        assert!(Jws::decode(&ps).unwrap().verify(&[rfc_key()]).is_err());
    }

    #[test]
    fn headers_that_are_refused() {
        let body = enc(&json!({"sub": "x"}));
        for (header, why) in [
            (json!({"alg": "none"}), "alg none"),
            (json!({"alg": "HS256"}), "HS256"),
            (json!({"alg": "ES256", "crit": ["exp"]}), "crit"),
            (json!({"alg": "RS1"}), "unsupported"),
            (json!({"typ": "JWT"}), "no alg"),
        ] {
            let t = format!("{}.{body}.c2ln", enc(&header));
            let err = Jws::decode(&t).unwrap_err().to_string();
            assert!(err.contains(why), "{why}: {err}");
        }
        assert!(
            Jws::decode(&format!("{}.{body}.", enc(&json!({"alg": "none"}))))
                .unwrap_err()
                .to_string()
                .contains("alg none")
        );
        assert!(Jws::decode("a.b").is_err());
        assert!(Jws::decode("a.b.c.d").is_err());
        assert!(Jws::decode(&format!("{}.{body}=.c2ln", enc(&json!({"alg": "ES256"})))).is_err(), "padding");
    }

    #[test]
    fn ecdsa_and_ed25519_keys() {
        let es = EcdsaKeyPair::generate(&ECDSA_P256_SHA256_FIXED_SIGNING).unwrap();
        let ed = Ed25519KeyPair::generate().unwrap();
        let ed_jwk = Jwk {
            kty: "OKP".into(),
            kid: Some("ed".into()),
            crv: Some("Ed25519".into()),
            x: Some(b64(ed.public_key().as_ref())),
            ..Default::default()
        };
        let keys = vec![es_jwk(&es, "es"), ed_jwk.clone()];
        let claims = json!({"sub": "u1"});
        let t = sign_es(&es, &json!({"alg": "ES256", "kid": "es"}), &claims);
        assert_eq!(Jws::decode(&t).unwrap().verify(&keys).unwrap(), claims);
        // Without a kid: the only EC key.
        let t = sign_es(&es, &json!({"alg": "ES256"}), &claims);
        assert!(Jws::decode(&t).unwrap().verify(&keys).is_ok());
        let two = vec![es_jwk(&es, "a"), es_jwk(&es, "b")];
        assert!(Jws::decode(&t).unwrap().verify(&two).unwrap_err().to_string().contains("several"));
        // An unknown kid is told apart (the caller refetches the JWKS).
        let t = sign_es(&es, &json!({"alg": "ES256", "kid": "gone"}), &claims);
        assert_eq!(Jws::decode(&t).unwrap().verify(&keys), Err(TokenError::UnknownKey("gone".into())));
        // A kid naming a key of another type is refused.
        let t = sign_es(&es, &json!({"alg": "ES256", "kid": "ed"}), &claims);
        assert!(Jws::decode(&t).unwrap().verify(&keys).unwrap_err().to_string().contains("OKP"));
        // Ed25519.
        let input = format!("{}.{}", enc(&json!({"alg": "EdDSA", "kid": "ed"})), enc(&claims));
        let t = format!("{input}.{}", b64(ed.sign(input.as_bytes()).as_ref()));
        assert_eq!(Jws::decode(&t).unwrap().verify(&keys).unwrap(), claims);
        let mut wrong_crv = ed_jwk;
        wrong_crv.crv = Some("X25519".into());
        assert!(Jws::decode(&t).unwrap().verify(&[wrong_crv]).is_err());
    }

    #[test]
    fn claims_rules() {
        let aud = vec!["extra".to_string()];
        let e = Expect { issuer: "https://idp", client_id: "cid", audiences: &aud, nonce: "n1", now: 1_000_000 };
        let base = json!({"iss": "https://idp", "aud": "cid", "sub": "s", "exp": 1_000_300, "iat": 1_000_000,
                          "nonce": "n1", "email": "a@x.org", "email_verified": true, "name": "Ann"});
        let with = |patch: Value| {
            let mut c = base.clone();
            for (k, v) in patch.as_object().unwrap() {
                if v.is_null() {
                    c.as_object_mut().unwrap().remove(k);
                } else {
                    c[k] = v.clone();
                }
            }
            check_claims(&c, &e)
        };
        let ok = with(json!({})).unwrap();
        assert_eq!((ok.sub.as_str(), ok.email.as_deref(), ok.email_verified), ("s", Some("a@x.org"), true));
        assert!(with(json!({"aud": "extra"})).is_ok(), "a configured audience");
        assert!(with(json!({"aud": ["cid", "other"], "azp": "cid"})).is_ok());
        assert!(with(json!({"exp": 1_000_000 - 30})).is_ok(), "within the skew");
        for (patch, why) in [
            (json!({"iss": "https://idp/"}), "iss"),
            (json!({"aud": "other"}), "aud"),
            (json!({"aud": ["cid", "other"]}), "azp"),
            (json!({"aud": ["cid", "other"], "azp": "other"}), "azp"),
            (json!({"exp": 1_000_000 - 61}), "exp"),
            (json!({"exp": null}), "exp"),
            (json!({"iat": 1_000_000 + 61}), "iat"),
            (json!({"nonce": "n2"}), "nonce"),
            (json!({"nonce": null}), "nonce"),
            (json!({"sub": ""}), "sub"),
        ] {
            let err = with(patch.clone()).unwrap_err().to_string();
            assert!(err.contains(why), "{patch}: {err}");
        }
        assert!(!with(json!({"email_verified": null})).unwrap().email_verified);
        assert!(with(json!({"email_verified": "true"})).unwrap().email_verified);
        assert!(!with(json!({"email_verified": "yes"})).unwrap().email_verified);
    }

    #[test]
    fn discovery_and_config_rules() {
        let d = |issuer: &str, token: &str, methods: Option<Vec<&str>>| Discovery {
            issuer: issuer.into(),
            authorization_endpoint: "https://idp/authorize".into(),
            token_endpoint: token.into(),
            jwks_uri: "https://idp/jwks".into(),
            userinfo_endpoint: None,
            code_challenge_methods_supported: methods.map(|m| m.into_iter().map(String::from).collect()),
            token_endpoint_auth_methods_supported: None,
        };
        assert!(d("https://idp/", "https://idp/token", None).check("https://idp").is_ok(), "one trailing slash");
        assert!(d("https://idp//", "https://idp/token", None).check("https://idp").is_err());
        assert!(d("https://other", "https://idp/token", None).check("https://idp").is_err());
        assert!(d("https://idp", "http://idp/token", None).check("https://idp").is_err(), "http endpoint");
        assert!(d("http://127.0.0.1:9", "http://127.0.0.1:9/token", None).check("http://127.0.0.1:9").is_ok());
        assert!(d("https://idp", "https://idp/token", Some(vec!["plain"])).check("https://idp").is_err());
        assert!(d("https://idp", "https://idp/token", Some(vec!["plain", "S256"])).check("https://idp").is_ok());
        let mut m = d("https://idp", "https://idp/token", None);
        assert_eq!(m.secret_method().unwrap(), SecretMethod::Basic);
        m.token_endpoint_auth_methods_supported = Some(vec!["client_secret_post".into()]);
        assert_eq!(m.secret_method().unwrap(), SecretMethod::Post);
        m.token_endpoint_auth_methods_supported = Some(vec!["private_key_jwt".into()]);
        assert!(m.secret_method().is_err());

        let p = |id: &str, issuer: &str| OidcProviderConfig {
            id: id.into(),
            name: "IdP".into(),
            issuer: issuer.into(),
            client_id: "cid".into(),
            client_secret_env: None,
            scopes: vec!["openid".into()],
            audiences: vec![],
        };
        assert!(validate_provider(&p("clerk", "https://x.clerk.accounts.dev")).is_ok());
        assert!(validate_provider(&p("local-1", "http://localhost:8080/realms/r")).is_ok());
        assert!(validate_provider(&p("Clerk", "https://x")).is_err(), "id");
        assert!(validate_provider(&p("a/b", "https://x")).is_err(), "id");
        assert!(validate_provider(&p("idp", "http://idp.example.com")).is_err(), "http issuer");
        assert!(validate_provider(&p("idp", "https://idp.example.com?x=1")).is_err(), "query");
        let mut no_openid = p("idp", "https://idp");
        no_openid.scopes = vec!["email".into()];
        assert!(validate_provider(&no_openid).is_err());
        assert_eq!(
            code_challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }
}
