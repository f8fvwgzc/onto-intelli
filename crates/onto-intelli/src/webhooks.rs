//! Webhook endpoints, fan-out and delivery from the outbox (docs/design/EVENTS.md Part B).
//!
//! * **Envelope**: a CloudEvents 1.0 structured JSON event ([`envelope`]).
//!   Its `id` is the outbox row id, kept when a row is replayed.
//! * **Signature**: Standard Webhooks. The signed content is
//!   `"<webhook-id>.<webhook-timestamp>.<body>"`, signed with HMAC-SHA256 and
//!   sent base64-encoded with a `v1,` prefix in `webhook-signature`; during a
//!   rotation both secrets sign and the header holds both values, separated
//!   by a space. A `whsec_` secret is base64 after its prefix ([`signing_key`]).
//! * **Delivery** ([`spawn_delivery`]): one worker per open tenant claims due
//!   rows with claim tokens and a lease by the store's clock, sends them with
//!   the guarded client (no redirects, no proxy, the SSRF rules of
//!   [`crate::net`]), and records each attempt. 2xx is delivered; 408, 409,
//!   425, 429, 5xx, timeouts and connection errors are retried with backoff
//!   (`Retry-After` honoured up to the maximum); 410 disables the endpoint;
//!   other answers are dead. Each change is enqueued once per endpoint and
//!   delivered at least once; receivers dedupe on the event id.
//! * **Breaker**: `pause_after_failures` consecutive failures pause an
//!   endpoint for `pause_secs`; then one probe is sent (half-open). Success
//!   closes the breaker, failure doubles the pause (up to an hour).
//! * **Kill switches**: `[webhooks] enabled = false` (the process sends
//!   nothing) and the tenant's `webhook.control/kill`, checked every pass and
//!   before every send; claimed rows are released unsent.
//!
//! Delivery concurrency limits are per process: the effective limit is the
//! configured one times the replicas that hold the tenant open.
//!
//! Endpoint configuration is tenant KV (`webhook.endpoint`, one value per
//! endpoint); operational state (kill switch, breakers, backlog pauses) is
//! `webhook.control`. Both are written with their audit event through
//! [`onto_core::Ontology::commit_config`]. The actions that manage endpoints
//! ([`CALLS`]) need admins who dominate the endpoint's principal.
//!
//! * **Fan-out** ([`spawn_fanout`]): one process per tenant at a time (the
//!   `webhook:fanout` lease, writes fenced by its epoch) turns change records
//!   into rows for each endpoint whose filter wants them, as the endpoint's
//!   principal sees them. Each endpoint keeps its own cursor; rows hold only
//!   the cursor and are rendered when sent. An endpoint with `pending_cap`
//!   undelivered rows is paused (`backlogged`) with its cursor kept, and
//!   resumed when its backlog drains below half the cap. An endpoint whose
//!   cursor expired gets one `onto.changes.gap` delivery and resumes at the
//!   floor.

use std::path::PathBuf;

use base64::Engine as _;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::{Value, json};

use onto_core::changes::ChangeFilter;
use onto_core::outbox::{ConfigEvent, OutboxKind, OutboxRow, SOURCE_TEST};
use onto_core::policy::ScopePrincipal;
use onto_core::time::fmt_ts;
use onto_core::{ActionContext, Error, Ontology, Principal, Result};

use crate::hub::{Secret, SecretBox};

/// KV namespace of endpoints (key: the endpoint name).
pub const NS_ENDPOINT: &str = "webhook.endpoint";
/// KV namespace of the tenant's kill switch and the endpoints' breakers.
pub const NS_CONTROL: &str = "webhook.control";
/// `webhook.control` key of the tenant kill switch (`{ "on": true }`).
pub const KILL_KEY: &str = "kill";
/// `webhook.control` keys of breakers: `breaker:<endpoint>`.
pub const BREAKER_PREFIX: &str = "breaker:";
/// `webhook.control` keys of endpoints fan-out paused for their backlog:
/// `backlog:<endpoint>` (`{since, pending}`). Kept apart from the endpoint's
/// configuration, so a pause never moves its `version`, and deliveries
/// continue (the backlog drains) while fan-out holds the endpoint's cursor.
pub const BACKLOG_PREFIX: &str = "backlog:";
/// The CloudEvent type of the delivery an endpoint gets when records it
/// subscribed to expired before fan-out reached them (`{from, to}`).
pub const GAP_EVENT: &str = "onto.changes.gap";
/// The sealing purpose of endpoint signing secrets.
pub const SECRET_PURPOSE: &str = "webhook-secret";
/// The CloudEvent type of `webhook.test` deliveries.
pub const TEST_EVENT: &str = "onto.webhook.test";
/// Prefix of Standard Webhooks secrets (base64 after it).
pub const SECRET_PREFIX: &str = "whsec_";
/// Receivers refuse timestamps further than this from their clock (the
/// Standard Webhooks default), which bounds replays of a captured request.
pub const TOLERANCE_SECS: i64 = 300;
/// Rows claimed per pass.
pub const CLAIM_LIMIT: usize = 32;
/// The lease of a claimed row (store clock).
pub const LEASE_MS: i64 = 60_000;
/// A breaker never pauses an endpoint longer than this.
pub const MAX_PAUSE_MS: i64 = 3_600_000;
/// Words for outputs about deliveries (docs/design/EVENTS.md section 13).
pub const DELIVERY_NOTE: &str =
    "Each change is enqueued once per endpoint and delivered at least once; dedupe on the event id.";

// ── secrets ────────────────────────────────────────────────────────────

/// Where an endpoint's signing secret comes from. `env:` and `file:`
/// references are the operator's (config tokens and the CLI); sealed
/// values are encrypted by the control plane's vault.
#[derive(Clone, PartialEq, Eq)]
pub enum SecretRef {
    /// `env:VAR`: the server's environment variable.
    Env(String),
    /// `file:/path`: a file on the server.
    File(PathBuf),
    /// `sealed:v1:<base64>`: sealed by the tenant's [`SecretBox`].
    Sealed(String),
}

impl std::fmt::Debug for SecretRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.describe())
    }
}

/// Is `raw` an `env:` or `file:` reference (any case, surrounding space ignored)?
pub fn is_reference(raw: &str) -> bool {
    let r = raw.trim().to_ascii_lowercase();
    r.starts_with("env:") || r.starts_with("file:")
}

impl SecretRef {
    /// `env:VAR`, `file:/path` or `sealed:v1:<base64>`.
    pub fn parse(raw: &str) -> Result<Self> {
        let raw = raw.trim();
        let lower = raw.to_ascii_lowercase();
        if lower.starts_with("env:") {
            let var = raw[4..].trim();
            let ok = !var.is_empty() && var.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
            if !ok {
                return Err(Error::invalid("an env: reference names a variable ([A-Za-z0-9_])"));
            }
            return Ok(SecretRef::Env(var.to_string()));
        }
        if lower.starts_with("file:") {
            let path = PathBuf::from(raw[5..].trim());
            if !path.is_absolute() {
                return Err(Error::invalid("a file: reference names an absolute path"));
            }
            return Ok(SecretRef::File(path));
        }
        if let Some(v) = raw.strip_prefix("sealed:v1:") {
            if v.is_empty() || base64::engine::general_purpose::STANDARD.decode(v).is_err() {
                return Err(Error::invalid("a sealed secret is sealed:v1:<base64>"));
            }
            return Ok(SecretRef::Sealed(v.to_string()));
        }
        Err(Error::invalid("a stored secret is env:VAR, file:/path or sealed:v1:<base64>"))
    }

    /// An operator reference (`env:` or `file:`).
    pub fn is_reference(&self) -> bool {
        matches!(self, SecretRef::Env(_) | SecretRef::File(_))
    }

    /// The stored form.
    pub fn stored(&self) -> String {
        match self {
            SecretRef::Env(v) => format!("env:{v}"),
            SecretRef::File(p) => format!("file:{}", p.display()),
            SecretRef::Sealed(v) => format!("sealed:v1:{v}"),
        }
    }

    /// The form outputs show: `env:VAR`, `file:/path` or `sealed (kek <id>)`,
    /// never a secret.
    pub fn describe(&self) -> String {
        match self {
            SecretRef::Sealed(v) => {
                let blob = base64::engine::general_purpose::STANDARD.decode(v).unwrap_or_default();
                match blob.get(1..9) {
                    Some(id) => format!("sealed (kek {})", id.iter().map(|b| format!("{b:02x}")).collect::<String>()),
                    None => "sealed".into(),
                }
            }
            other => other.stored(),
        }
    }

    /// Seal `plain` for endpoint `name` with the tenant's vault. Without one
    /// (no `ONTO_MASTER_KEY`) plaintext secrets are refused: the operator
    /// registers an `env:` or `file:` reference instead.
    pub fn seal(sealer: Option<&dyn SecretBox>, name: &str, plain: &[u8]) -> Result<Self> {
        Self::seal_for(sealer, SECRET_PURPOSE, name, plain)
    }

    /// [`seal`](Self::seal) for another sealing purpose (writeback targets
    /// seal theirs as `writeback-secret`).
    pub fn seal_for(sealer: Option<&dyn SecretBox>, purpose: &str, name: &str, plain: &[u8]) -> Result<Self> {
        let Some(sealer) = sealer else {
            return Err(Error::Unavailable(
                "failed_precondition: no master key (ONTO_MASTER_KEY) to seal secrets; register env:VAR or \
                 file:/path instead"
                    .into(),
            ));
        };
        Ok(SecretRef::Sealed(sealer.seal(purpose, name, plain)?))
    }

    /// The secret's text. Errors name the reference, never a value.
    pub fn resolve(&self, sealer: Option<&dyn SecretBox>, name: &str) -> Result<Secret> {
        self.resolve_for(sealer, SECRET_PURPOSE, &format!("endpoint {name}"), name)
    }

    /// [`resolve`](Self::resolve) for another sealing purpose; `owner` names
    /// what the secret belongs to in errors (`endpoint ops`, `writeback
    /// target helpdesk`).
    pub fn resolve_for(
        &self,
        sealer: Option<&dyn SecretBox>,
        purpose: &str,
        owner: &str,
        name: &str,
    ) -> Result<Secret> {
        match self {
            SecretRef::Env(var) => std::env::var(var)
                .ok()
                .filter(|v| !v.trim().is_empty())
                .map(|v| Secret::new(v.trim().as_bytes().to_vec()))
                .ok_or_else(|| Error::Unavailable(format!("the secret in env:{var} is not set"))),
            SecretRef::File(p) => std::fs::read_to_string(p)
                .map(|v| Secret::new(v.trim().as_bytes().to_vec()))
                .map_err(|_| Error::Unavailable(format!("the secret in file:{} cannot be read", p.display()))),
            SecretRef::Sealed(v) => match sealer {
                Some(s) => s.open(purpose, name, v).map_err(|_| {
                    Error::Unavailable(format!("the sealed secret of {owner} does not open with this master key"))
                }),
                None => Err(Error::Unavailable(format!(
                    "the secret of {owner} is sealed and this server has no master key (ONTO_MASTER_KEY)"
                ))),
            },
        }
    }
}

impl Serialize for SecretRef {
    fn serialize<S: Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        s.serialize_str(&self.stored())
    }
}

impl<'de> Deserialize<'de> for SecretRef {
    fn deserialize<D: Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        SecretRef::parse(&s).map_err(serde::de::Error::custom)
    }
}

/// A text that may be an operator reference (`env:VAR`, `file:/path`),
/// resolved; `what` names it in errors, which never contain the value.
pub fn resolve_text(raw: &str, what: &str) -> Result<String> {
    if !is_reference(raw) {
        return Ok(raw.trim().to_string());
    }
    let r = SecretRef::parse(raw).map_err(|e| Error::invalid(format!("{what}: {e}")))?;
    let v = r.resolve(None, what).map_err(|_| Error::Unavailable(format!("{what} {} is not set", r.stored())))?;
    String::from_utf8(v.expose().to_vec()).map_err(|_| Error::invalid(format!("{what} {} is not text", r.stored())))
}

/// Check an endpoint's delivery URL (`url`, resolved from `raw`, which may be
/// an operator reference) against the outbound rules. When it came from a
/// reference, the error names the reference only: the refused host or
/// address is part of the value.
pub fn check_endpoint_url(raw: &str, url: &str, reach: &crate::net::Reach) -> Result<url::Url> {
    check_endpoint_url_as(raw, url, "url", reach)
}

/// [`check_endpoint_url`], naming a literal URL by `field`: the input field
/// the caller gave it in (a writeback target's `base_url`, say).
pub fn check_endpoint_url_as(raw: &str, url: &str, field: &str, reach: &crate::net::Reach) -> Result<url::Url> {
    if !is_reference(raw) {
        return crate::net::check_target_url(url, field, reach);
    }
    let what = format!("the URL in {}", raw.trim());
    crate::net::check_target_url(url, &what, reach).map_err(|_| {
        Error::invalid(format!(
            "{what} is refused: it must be an https URL without userinfo or a fragment, to a public address or a \
             host [[webhooks.allow]] lists"
        ))
    })
}

/// A new Standard Webhooks secret: `whsec_` and 32 random bytes in base64.
pub fn generate_secret() -> Result<String> {
    let mut key = [0u8; 32];
    getrandom::fill(&mut key).map_err(|e| Error::Unavailable(format!("random secret: {e}")))?;
    Ok(format!("{SECRET_PREFIX}{}", base64::engine::general_purpose::STANDARD.encode(key)))
}

/// The HMAC key of a secret's text: the bytes after `whsec_`, base64-decoded
/// (Standard Webhooks); any other text is used as its bytes.
pub fn signing_key(secret: &[u8]) -> Result<Vec<u8>> {
    match secret.strip_prefix(SECRET_PREFIX.as_bytes()) {
        Some(b64) => base64::engine::general_purpose::STANDARD
            .decode(b64)
            .map_err(|_| Error::invalid("a whsec_ secret is base64 after its prefix")),
        None if secret.is_empty() => Err(Error::invalid("an empty signing secret")),
        None => Ok(secret.to_vec()),
    }
}

/// The shortest HMAC key a signing secret may give (Standard Webhooks keys
/// are 24 to 64 bytes).
pub const MIN_KEY_BYTES: usize = 24;
/// The longest key a `whsec_` secret may hold.
pub const MAX_WHSEC_KEY_BYTES: usize = 64;

/// Refuse a signing secret that signs with a weak or broken key: a `whsec_`
/// secret must be base64 of 24 to 64 bytes, any other text at least 24
/// bytes. `what` names it in the error (never its value).
pub fn check_secret(secret: &[u8], what: &str) -> Result<()> {
    let key = signing_key(secret).map_err(|e| Error::invalid(format!("{what}: {e}")))?;
    let whsec = secret.starts_with(SECRET_PREFIX.as_bytes());
    if key.len() < MIN_KEY_BYTES || (whsec && key.len() > MAX_WHSEC_KEY_BYTES) {
        return Err(Error::invalid(format!(
            "{what}: a signing secret needs a key of {MIN_KEY_BYTES} to {MAX_WHSEC_KEY_BYTES} bytes (a whsec_ secret \
             is base64 of the key, other text is used as its bytes); leave it out to generate one"
        )));
    }
    Ok(())
}

/// One Standard Webhooks signature: `v1,` and base64 of
/// HMAC-SHA256(key, `"<id>.<timestamp>.<body>"`).
#[cfg(feature = "outbound")]
pub fn sign(key: &[u8], id: &str, timestamp: i64, body: &[u8]) -> String {
    use hmac::{KeyInit, Mac};
    let mut mac = hmac::Hmac::<sha2::Sha256>::new_from_slice(key).expect("HMAC takes any key length");
    mac.update(id.as_bytes());
    mac.update(b".");
    mac.update(timestamp.to_string().as_bytes());
    mac.update(b".");
    mac.update(body);
    format!("v1,{}", base64::engine::general_purpose::STANDARD.encode(mac.finalize().into_bytes()))
}

/// The `webhook-signature` header: one signature per key, space-separated
/// (the current secret first; the previous one during a rotation).
#[cfg(feature = "outbound")]
pub fn signature_header(keys: &[Vec<u8>], id: &str, timestamp: i64, body: &[u8]) -> String {
    keys.iter().map(|k| sign(k, id, timestamp, body)).collect::<Vec<_>>().join(" ")
}

/// What a receiver does (the SDK helpers do the same): check the timestamp
/// against `now_secs` within `tolerance_secs`, accept when any `v1,`
/// signature in the header matches any of `secrets` (compared in constant
/// time), and return the parsed body (the CloudEvent), whose `id`, when it
/// has one, must be the `webhook-id`. Receivers then dedupe on that id.
#[cfg(feature = "outbound")]
pub fn verify(
    secrets: &[&str],
    id: &str,
    timestamp: &str,
    signature: &str,
    body: &[u8],
    now_secs: i64,
    tolerance_secs: i64,
) -> Result<Value> {
    let ts: i64 = timestamp.trim().parse().map_err(|_| Error::invalid("webhook-timestamp is not a number"))?;
    if (now_secs - ts).abs() > tolerance_secs {
        return Err(Error::invalid("webhook-timestamp is outside the tolerance"));
    }
    let given: Vec<&[u8]> = signature.split(' ').filter_map(|s| s.strip_prefix("v1,")).map(str::as_bytes).collect();
    let mut ok = false;
    for secret in secrets {
        let expected = sign(&signing_key(secret.as_bytes())?, id, ts, body);
        let expected = expected.as_bytes()[3..].to_vec();
        for g in &given {
            ok |= constant_time_eq(&expected, g);
        }
    }
    if !ok {
        return Err(Error::invalid("no webhook-signature matches"));
    }
    let event: Value = serde_json::from_slice(body).map_err(|_| Error::invalid("the body is not JSON"))?;
    if let Some(eid) = event.get("id")
        && eid.as_str() != Some(id)
    {
        return Err(Error::invalid("the event id is not the webhook-id"));
    }
    Ok(event)
}

#[cfg(feature = "outbound")]
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

// ── endpoints ──────────────────────────────────────────────────────────

/// Why an endpoint is paused (`backlogged` by fan-out, or by a person).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Paused {
    pub reason: String,
    pub since: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub until: Option<i64>,
}

/// A webhook endpoint (KV `webhook.endpoint`, key = name). Its principal
/// id is `webhook:<name>`; it sees what its `principal` scope may read.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Endpoint {
    /// `[a-z0-9_-]`, at most 40 characters.
    pub name: String,
    /// `https://…` (or an operator `env:`/`file:` reference to one); checked
    /// when written and before every delivery.
    pub url: String,
    pub secret: SecretRef,
    /// The previous secret during a rotation: it signs too until `previous_until`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous_secret: Option<SecretRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous_until: Option<i64>,
    /// The change subscription; `None`: effect deliveries only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filter: Option<ChangeFilter>,
    /// What the receiver may see.
    pub principal: ScopePrincipal,
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub paused: Option<Paused>,
    /// Registered by a project credential: no operator allow rules apply.
    #[serde(default)]
    pub restricted: bool,
    /// Bumped on every configuration change.
    #[serde(default)]
    pub version: u64,
    #[serde(default)]
    pub created_by: String,
    #[serde(default)]
    pub created_at: i64,
    /// A random id drawn at registration: with `created_at`, part of
    /// automation closure hashes, so an endpoint removed and added again
    /// never hashes like the one before (whatever its version).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub incarnation: String,
}

/// The change-feed consumer (cursor) of endpoint `name`: `webhook:<name>`.
pub fn consumer_name(name: &str) -> String {
    format!("{CONSUMER_PREFIX}{name}")
}

/// The prefix of an endpoint's change consumer ([`consumer_name`]).
pub const CONSUMER_PREFIX: &str = "webhook:";

/// `[a-z0-9_-]`, 1 to 40 characters.
pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 40
        && name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
}

impl Endpoint {
    /// Is `row` this endpoint's: a webhook row of its name created since it
    /// was registered? Rows are keyed by name only, so an endpoint removed
    /// and added again (perhaps by a narrower admin, with another URL) never
    /// lists, replays or receives the rows of the one before.
    pub fn owns(&self, row: &onto_core::outbox::OutboxRow) -> bool {
        row.kind == onto_core::outbox::OutboxKind::Webhook
            && row.target == self.name
            && row.created_at >= self.created_at
    }

    /// An endpoint with only a name and a principal (dominance checks of
    /// what a call would configure).
    pub fn bare(name: &str, principal: ScopePrincipal) -> Self {
        Endpoint {
            name: name.to_string(),
            url: String::new(),
            secret: SecretRef::Env(String::new()),
            previous_secret: None,
            previous_until: None,
            filter: None,
            principal,
            enabled: false,
            paused: None,
            restricted: false,
            version: 0,
            created_by: String::new(),
            created_at: 0,
            incarnation: String::new(),
        }
    }

    /// `webhook:<name>`.
    pub fn principal_id(&self) -> String {
        format!("webhook:{}", self.name)
    }

    /// The principal deliveries are rendered as.
    pub fn principal(&self) -> Principal {
        self.principal.principal(self.principal_id())
    }

    /// The endpoint `name`, if it exists.
    pub fn load(onto: &Ontology, name: &str) -> Result<Option<Endpoint>> {
        match onto.store().kv_get(NS_ENDPOINT, name)? {
            None | Some(Value::Null) => Ok(None),
            Some(v) => serde_json::from_value(v)
                .map(Some)
                .map_err(|e| Error::Storage(format!("corrupt webhook endpoint {name}: {e}"))),
        }
    }

    /// Every endpoint, by name.
    pub fn list(onto: &Ontology) -> Result<Vec<Endpoint>> {
        let mut out = Vec::new();
        for (name, v) in onto.store().kv_scan(NS_ENDPOINT, "")? {
            if v.is_null() {
                continue;
            }
            out.push(
                serde_json::from_value(v)
                    .map_err(|e| Error::Storage(format!("corrupt webhook endpoint {name}: {e}")))?,
            );
        }
        Ok(out)
    }

    /// The HMAC keys deliveries are signed with at `now`: the current
    /// secret's, then the previous one's while it is still valid.
    pub fn signing_keys(&self, now: i64, sealer: Option<&dyn SecretBox>) -> Result<Vec<Vec<u8>>> {
        let mut keys = vec![signing_key(self.secret.resolve(sealer, &self.name)?.expose())?];
        if let (Some(prev), Some(until)) = (&self.previous_secret, self.previous_until)
            && until > now
        {
            keys.push(signing_key(prev.resolve(sealer, &self.name)?.expose())?);
        }
        Ok(keys)
    }
}

/// When an endpoint or writeback target `name` registered now starts (the
/// store's clock): strictly after every row left from an earlier one of that
/// name (waiting out the millisecond if one was made in it), so
/// [`Endpoint::owns`] and `Target::owns` tell its rows from theirs.
pub(crate) fn incarnation_start(onto: &Ontology, name: &str) -> Result<i64> {
    let store = onto.store();
    let mut now = store.clock_ms()?;
    if !store.keeps_outbox() {
        return Ok(now);
    }
    let q = onto_core::outbox::OutboxQuery { target: Some(name.to_string()), limit: Some(1), ..Default::default() };
    let Some(newest) = store.outbox_list(&q)?.first().map(|r| r.created_at) else { return Ok(now) };
    for _ in 0..1000 {
        if now > newest {
            return Ok(now);
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
        now = store.clock_ms()?;
    }
    Err(Error::Unavailable(format!("{name}: the store clock is behind rows made for an earlier {name}")))
}

/// A new random incarnation id (128 bits, hex) for an endpoint or writeback
/// target being registered.
pub(crate) fn new_incarnation() -> Result<String> {
    let mut b = [0u8; 16];
    getrandom::fill(&mut b).map_err(|e| Error::Unavailable(format!("random incarnation id: {e}")))?;
    Ok(b.iter().map(|x| format!("{x:02x}")).collect())
}

/// Is the tenant's kill switch on?
pub fn killed(onto: &Ontology) -> Result<bool> {
    Ok(onto.store().kv_get(NS_CONTROL, KILL_KEY)?.is_some_and(|v| v.get("on").and_then(Value::as_bool) == Some(true)))
}

// ── envelope ───────────────────────────────────────────────────────────

/// The CloudEvents 1.0 structured envelope of `row` with `data`.
pub fn envelope(row: &OutboxRow, tenant: &str, subject: Option<&str>, data: Value) -> Value {
    let mut ev = json!({
        "specversion": "1.0",
        "id": row.id,
        "source": format!("onto-intelli://{tenant}"),
        "type": row.event_type,
        "time": fmt_ts(row.created_at),
        "datacontenttype": "application/json",
        "tenant": tenant,
        "workspace": row.workspace,
    });
    let m = ev.as_object_mut().expect("an object");
    if let Some(s) = subject {
        m.insert("subject".into(), json!(s));
    }
    if let Some(c) = row.cursor {
        m.insert("cursor".into(), json!(c.to_string()));
    }
    if let Some(c) = &row.cause {
        // A cascade a trusted service started names no automation here.
        if !c.automation.is_empty() {
            m.insert("causeautomation".into(), json!(c.automation));
        }
        if !c.run.is_empty() {
            m.insert("causerun".into(), json!(c.run));
        }
        m.insert("causedepth".into(), json!(c.depth));
    }
    m.insert("data".into(), data);
    ev
}

// ── retries ────────────────────────────────────────────────────────────

/// What a receiver's answer means for the row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Class {
    /// 2xx.
    Delivered,
    /// 408, 409, 425, 429, 5xx: try again later.
    Retry,
    /// 410: the endpoint is gone; it is disabled and the row is dead.
    Gone,
    /// Any other answer (other 4xx, 1xx, 3xx since redirects are not followed).
    Dead,
}

pub fn classify(status: u16) -> Class {
    match status {
        200..=299 => Class::Delivered,
        408 | 409 | 425 | 429 | 500..=599 => Class::Retry,
        410 => Class::Gone,
        _ => Class::Dead,
    }
}

/// The delay before attempt `attempt + 1`: `min(base * 2^(attempt-1), max)`
/// times `jitter` (0.8 to 1.2), never above `max`.
pub fn backoff_ms(attempt: u32, base_ms: i64, max_ms: i64, jitter: f64) -> i64 {
    let exp = attempt.saturating_sub(1).min(40);
    let raw = base_ms.saturating_mul(1i64 << exp.min(62)).min(max_ms);
    ((raw as f64) * jitter.clamp(0.8, 1.2)).round().min(max_ms as f64).max(0.0) as i64
}

/// A random jitter factor in 0.8..1.2.
pub fn jitter() -> f64 {
    0.8 + fastrand_f64() * 0.4
}

fn fastrand_f64() -> f64 {
    let mut b = [0u8; 8];
    if getrandom::fill(&mut b).is_err() {
        return 0.5;
    }
    (u64::from_le_bytes(b) >> 11) as f64 / (1u64 << 53) as f64
}

/// `Retry-After` in seconds (an HTTP date is not honoured) as milliseconds,
/// at most `max_ms`.
pub fn retry_after_ms(header: Option<&str>, max_ms: i64) -> Option<i64> {
    let secs: i64 = header?.trim().parse().ok()?;
    Some(secs.max(0).saturating_mul(1000).min(max_ms))
}

/// The pause after the breaker opens again: double the last (from
/// `base_ms` the first time), at most an hour.
pub fn next_pause(last_ms: Option<i64>, base_ms: i64) -> i64 {
    last_ms.map_or(base_ms, |l| l.saturating_mul(2)).clamp(1, MAX_PAUSE_MS.max(base_ms))
}

/// A breaker as stored in `webhook.control` (`breaker:<endpoint>`).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BreakerState {
    /// `open` (paused until `until`, then half-open) or `closed`.
    pub state: String,
    #[serde(default)]
    pub until: i64,
    #[serde(default)]
    pub pause_ms: i64,
    #[serde(default)]
    pub failures: u32,
}

impl BreakerState {
    pub fn is_open(&self) -> bool {
        self.state == "open"
    }
}

// ── test deliveries ────────────────────────────────────────────────────

/// A test delivery row for `endpoint`.
pub fn test_row(endpoint: &str, workspace: &str, trace_id: Option<String>) -> OutboxRow {
    let mut r = OutboxRow::new(OutboxKind::Webhook, endpoint, "", SOURCE_TEST, workspace, TEST_EVENT);
    r.dedupe = format!("test:{}", r.id);
    r.payload = json!({ "endpoint": endpoint, "message": "a test delivery from onto-intelli" });
    r.otel_trace_id = trace_id;
    r
}

/// Enqueue a test delivery to an existing endpoint, audited as
/// `webhook.test`. Callers check who may (admins dominating the endpoint
/// principal) first.
pub fn enqueue_test(onto: &Ontology, ctx: &ActionContext, endpoint: &str) -> Result<Value> {
    let Some(ep) = Endpoint::load(onto, endpoint)? else {
        return Err(Error::not_found(format!("webhook endpoint {endpoint}")));
    };
    // The caller's trace continues in the delivery (`traceparent`).
    let row = test_row(&ep.name, &ctx.workspace, ctx.trace_id.clone());
    let id = row.id.clone();
    onto.enqueue_outbox(ctx, vec![row], Some(ConfigEvent::new("webhook.test", Some(&ep.name), json!({ "id": id }))))?;
    Ok(json!({ "id": id, "endpoint": ep.name, "state": "pending", "note": DELIVERY_NOTE }))
}

/// A system context for the worker's own writes (events, breakers).
#[cfg_attr(not(feature = "outbound"), allow(dead_code))]
pub(crate) fn system_ctx(onto: &Ontology, workspace: &str) -> ActionContext {
    let ws = if workspace.is_empty() { onto.options().default_workspace.clone() } else { workspace.to_string() };
    ActionContext::new(Principal::system(), ws).with_intent("webhook.delivery")
}

/// What [`rewrap_secrets`] did.
#[derive(Clone, Debug, Default, Serialize)]
pub struct RewrapReport {
    /// Records written again with their secrets under the current master key.
    pub rewrapped: usize,
    /// Records whose sealed secrets already were.
    pub current: usize,
    /// Records left as they were, by `<namespace>/<name>`, and why.
    pub failed: Vec<(String, String)>,
}

/// Rewrap the sealed signing secrets of webhook endpoints and writeback
/// targets (`secret`, `previous_secret`) under the current master key, for
/// `onto console rotate-master-key`: once the previous key is removed, a
/// secret only it wrapped would never open again. A record is written
/// again, with its audit event, only when one of its secrets changed; the
/// secrets themselves are not re-encrypted and versions do not change, so
/// enabled automations stay bound. A record changed while this runs is left
/// for the next run.
pub fn rewrap_secrets(onto: &Ontology, sealer: &dyn SecretBox) -> Result<RewrapReport> {
    /// Each `sealed:v1:` string of `v`, rewrapped; whether one changed.
    fn walk(v: &mut Value, sealer: &dyn SecretBox) -> Result<bool> {
        Ok(match v {
            Value::String(s) => match s.strip_prefix("sealed:v1:").map(|b| sealer.rewrap(b)).transpose()? {
                Some(Some(new)) => {
                    *s = format!("sealed:v1:{new}");
                    true
                }
                _ => false,
            },
            Value::Array(a) => a.iter_mut().try_fold(false, |any, v| Ok::<_, Error>(walk(v, sealer)? || any))?,
            Value::Object(m) => m.values_mut().try_fold(false, |any, v| Ok::<_, Error>(walk(v, sealer)? || any))?,
            _ => false,
        })
    }
    let ctx = ActionContext::new(Principal::system(), onto.options().default_workspace.clone())
        .with_intent("rotate-master-key");
    let mut report = RewrapReport::default();
    for (ns, kind) in [(NS_ENDPOINT, "webhook"), (crate::writeback::NS_TARGET, "writeback")] {
        for (name, old) in onto.store().kv_scan(ns, "")? {
            let mut new = old.clone();
            match walk(&mut new, sealer) {
                Ok(false) => report.current += 1,
                Ok(true) => {
                    if onto.store().kv_get(ns, &name)?.as_ref() != Some(&old) {
                        report.failed.push((format!("{ns}/{name}"), "changed while rewrapping: run again".into()));
                        continue;
                    }
                    let event = ConfigEvent::new(format!("{kind}.secret_rewrap"), Some(&name), json!({ "name": name }));
                    onto.commit_config(&ctx, vec![(ns.to_string(), name.clone(), new)], event)?;
                    report.rewrapped += 1;
                }
                Err(e) => report.failed.push((format!("{ns}/{name}"), e.to_string())),
            }
        }
    }
    Ok(report)
}

#[cfg(feature = "outbound")]
pub use deliver::spawn_delivery;
pub use fanout::{FANOUT_LEASE, spawn_fanout};
pub use registry::{CALLS, url_origin};

#[cfg(feature = "outbound")]
mod deliver;
mod fanout;
mod registry;

#[cfg(test)]
mod tests {
    use super::*;

    /// Wraps under "new" what "old" wrapped.
    struct Rotating;

    impl SecretBox for Rotating {
        fn seal(&self, _: &str, _: &str, _: &[u8]) -> Result<String> {
            Ok("bmV3".into())
        }
        fn open(&self, _: &str, _: &str, _: &str) -> Result<Secret> {
            Ok(Secret::new(b"s".to_vec()))
        }
        fn rewrap(&self, sealed: &str) -> Result<Option<String>> {
            match sealed {
                "b2xk" => Ok(Some("bmV3".into())),
                "bmV3" => Ok(None),
                _ => Err(Error::invalid("not a version 1 envelope")),
            }
        }
    }

    #[test]
    fn master_key_rotation_rewraps_endpoint_and_target_secrets() {
        let onto = Ontology::in_memory(onto_core::Schema::enterprise());
        let ctx = ActionContext::new(Principal::system(), "default");
        let kv = vec![
            (
                NS_ENDPOINT.to_string(),
                "crm".to_string(),
                json!({ "name": "crm", "version": 3, "secret": "sealed:v1:b2xk", "previous_secret": "sealed:v1:bmV3" }),
            ),
            (NS_ENDPOINT.to_string(), "ops".to_string(), json!({ "name": "ops", "secret": "env:OPS_SECRET" })),
            (crate::writeback::NS_TARGET.to_string(), "desk".to_string(), json!({ "secret": "sealed:v1:b2xk" })),
            (crate::writeback::NS_TARGET.to_string(), "bad".to_string(), json!({ "secret": "sealed:v1:eA==" })),
        ];
        onto.commit_config(&ctx, kv, ConfigEvent::new("test.setup", None, Value::Null)).unwrap();
        let report = rewrap_secrets(&onto, &Rotating).unwrap();
        assert_eq!((report.rewrapped, report.current), (2, 1), "{report:?}");
        assert_eq!(report.failed.len(), 1);
        assert!(report.failed[0].0.ends_with("/bad"), "{report:?}");
        let crm = onto.store().kv_get(NS_ENDPOINT, "crm").unwrap().unwrap();
        assert_eq!(
            crm,
            json!({ "name": "crm", "version": 3, "secret": "sealed:v1:bmV3", "previous_secret": "sealed:v1:bmV3" })
        );
        let desk = onto.store().kv_get(crate::writeback::NS_TARGET, "desk").unwrap().unwrap();
        assert_eq!(desk["secret"], "sealed:v1:bmV3");
        let audited = onto
            .events(
                &Principal::admin("a"),
                &onto_core::EventFilter { action: Some("webhook.secret_rewrap".into()), ..Default::default() },
            )
            .unwrap();
        assert_eq!(audited.len(), 1);
        // Nothing left to rewrap.
        let again = rewrap_secrets(&onto, &Rotating).unwrap();
        assert_eq!((again.rewrapped, again.current), (0, 3));
    }

    #[cfg(feature = "outbound")]
    #[test]
    fn standard_webhooks_reference_vector() {
        // The test vector of the Standard Webhooks reference libraries
        // (standard-webhooks/standard-webhooks, the libraries' test suites),
        // recomputed independently with Python 3:
        //   python3 -c 'import hmac,hashlib,base64; k=base64.b64decode("MfKQ9r8GKYqrTwjUPD8ILPZIo2LaLaSw");
        //   print(base64.b64encode(hmac.new(k, b"msg_p5jXN8AQM9LWM0D4loKWxJek.1614265330.{\"test\": 2432232314}",
        //   hashlib.sha256).digest()).decode())'
        //   -> g0hM9SsE+OTPJTGt/tmIKtSyZlE3uFJELVlNIOLJ1OE=
        let secret = "whsec_MfKQ9r8GKYqrTwjUPD8ILPZIo2LaLaSw";
        let (id, ts, body) = ("msg_p5jXN8AQM9LWM0D4loKWxJek", 1_614_265_330, br#"{"test": 2432232314}"#);
        let key = signing_key(secret.as_bytes()).unwrap();
        assert_eq!(sign(&key, id, ts, body), "v1,g0hM9SsE+OTPJTGt/tmIKtSyZlE3uFJELVlNIOLJ1OE=");
        // Rotation: both secrets sign, space-separated, the current one first.
        let other = signing_key(b"whsec_dGVzdC1zZWNyZXQtdHdvLXR3by10d28tdHdvLXR3bw==").unwrap();
        let header = signature_header(&[key.clone(), other.clone()], id, ts, body);
        let parts: Vec<&str> = header.split(' ').collect();
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0], "v1,g0hM9SsE+OTPJTGt/tmIKtSyZlE3uFJELVlNIOLJ1OE=");
        assert_eq!(parts[1], sign(&other, id, ts, body));
        // The receivers' check accepts it (the SDK helpers run the same vector).
        let v = verify(&[secret], id, "1614265330", &header, body, ts, TOLERANCE_SECS).unwrap();
        assert_eq!(v, json!({ "test": 2432232314u64 }));
    }

    #[cfg(feature = "outbound")]
    #[test]
    fn verification_matches_the_sdks() {
        let secret = "whsec_MfKQ9r8GKYqrTwjUPD8ILPZIo2LaLaSw";
        let body = br#"{"specversion":"1.0","id":"obx_1","type":"onto.webhook.test","data":{}}"#;
        let key = signing_key(secret.as_bytes()).unwrap();
        let sig = sign(&key, "obx_1", 1_000, body);
        let ev = verify(&[secret], "obx_1", "1000", &format!("v1,bogus {sig}"), body, 1_100, 300).unwrap();
        assert_eq!(ev["type"], "onto.webhook.test");
        assert!(verify(&[secret], "obx_1", "1000", &sig, body, 1_400, 300).is_err(), "outside the tolerance");
        assert!(verify(&[secret], "obx_2", "1000", &sig, body, 1_000, 300).is_err(), "the id is signed");
        assert!(verify(&["whsec_b3RoZXI="], "obx_1", "1000", &sig, body, 1_000, 300).is_err(), "another secret");
        assert!(verify(&[secret], "obx_1", "1000", &sig, br#"{"id":"obx_1"}"#, 1_000, 300).is_err(), "the body");
        // A raw (non-whsec_) secret is its bytes.
        assert_eq!(signing_key(b"plain").unwrap(), b"plain");
        assert!(signing_key(b"whsec_!!!").is_err());
        assert!(generate_secret().unwrap().starts_with(SECRET_PREFIX));
        assert_eq!(signing_key(generate_secret().unwrap().as_bytes()).unwrap().len(), 32);
    }

    #[test]
    fn cloudevent_shape() {
        let mut row = test_row("ops", "acme", None);
        row.created_at = 1_793_091_600_000;
        row.cursor = Some(onto_core::Cursor::new(1234, 0));
        row.cause = Some(onto_core::RunCause { automation: "escalate".into(), run: "run_1".into(), depth: 1 });
        let ev = envelope(&row, "acme", Some("issue:1"), json!({ "op": "entity.update" }));
        assert_eq!(ev["specversion"], "1.0");
        assert_eq!(ev["id"], row.id);
        assert_eq!(ev["source"], "onto-intelli://acme");
        assert_eq!(ev["type"], TEST_EVENT);
        assert_eq!(ev["time"], "2026-10-27T09:00:00Z");
        assert_eq!((ev["subject"].as_str(), ev["cursor"].as_str()), (Some("issue:1"), Some("1234.0")));
        assert_eq!((ev["causeautomation"].as_str(), ev["causedepth"].as_u64()), (Some("escalate"), Some(1)));
        assert_eq!(ev["datacontenttype"], "application/json");
        assert_eq!(ev["data"]["op"], "entity.update");
        let plain = envelope(&test_row("ops", "acme", None), "acme", None, json!({}));
        assert!(plain.get("subject").is_none() && plain.get("cursor").is_none() && plain.get("causerun").is_none());
    }

    #[test]
    fn backoff_and_jitter_bounds() {
        let (base, max) = (10_000, 3_600_000);
        assert_eq!(backoff_ms(1, base, max, 1.0), 10_000);
        assert_eq!(backoff_ms(2, base, max, 1.0), 20_000);
        assert_eq!(backoff_ms(4, base, max, 1.0), 80_000);
        assert_eq!(backoff_ms(30, base, max, 1.0), max, "capped");
        assert_eq!(backoff_ms(u32::MAX, base, max, 1.2), max, "never above the max, even with jitter");
        for _ in 0..1000 {
            let j = jitter();
            assert!((0.8..=1.2).contains(&j), "{j}");
            let d = backoff_ms(3, base, max, j);
            assert!((32_000..=48_000).contains(&d), "{d}");
        }
        assert_eq!(retry_after_ms(Some("120"), max), Some(120_000));
        assert_eq!(retry_after_ms(Some("999999"), max), Some(max));
        assert_eq!(retry_after_ms(Some("Wed, 21 Oct 2015 07:28:00 GMT"), max), None);
        assert_eq!(retry_after_ms(None, max), None);
    }

    #[test]
    fn status_classification() {
        for s in [200, 201, 204, 299] {
            assert_eq!(classify(s), Class::Delivered, "{s}");
        }
        for s in [408, 409, 425, 429, 500, 502, 503, 599] {
            assert_eq!(classify(s), Class::Retry, "{s}");
        }
        assert_eq!(classify(410), Class::Gone);
        for s in [100, 301, 302, 307, 400, 401, 403, 404, 413, 422] {
            assert_eq!(classify(s), Class::Dead, "{s}");
        }
    }

    #[test]
    fn breaker_pauses_double_up_to_an_hour() {
        assert_eq!(next_pause(None, 300_000), 300_000);
        assert_eq!(next_pause(Some(300_000), 300_000), 600_000);
        assert_eq!(next_pause(Some(2_400_000), 300_000), MAX_PAUSE_MS);
        assert!(BreakerState { state: "open".into(), ..Default::default() }.is_open());
        assert!(!BreakerState::default().is_open());
    }

    #[test]
    fn secret_refs() {
        assert_eq!(SecretRef::parse("env:HOOK_SECRET").unwrap(), SecretRef::Env("HOOK_SECRET".into()));
        assert_eq!(SecretRef::parse(" ENV:X ").unwrap().stored(), "env:X");
        assert_eq!(SecretRef::parse("file:/run/secrets/hook").unwrap().stored(), "file:/run/secrets/hook");
        assert!(SecretRef::parse("file:relative").is_err());
        assert!(SecretRef::parse("env:").is_err());
        assert!(SecretRef::parse("whsec_abc").is_err(), "plaintext is never stored");
        let sealed = SecretRef::parse(&format!(
            "sealed:v1:{}",
            base64::engine::general_purpose::STANDARD.encode([1u8, 0xab, 0xcd, 1, 2, 3, 4, 5, 6, 7, 8])
        ))
        .unwrap();
        assert_eq!(sealed.describe(), "sealed (kek abcd010203040506)");
        assert!(format!("{sealed:?}").starts_with("sealed (kek"));
        let e = SecretRef::Env("ONTO_TEST_SURELY_UNSET_VAR".into()).resolve(None, "ops").unwrap_err().to_string();
        assert!(e.contains("env:ONTO_TEST_SURELY_UNSET_VAR"), "{e}");
        let e = sealed.resolve(None, "ops").unwrap_err().to_string();
        assert!(e.contains("no master key"), "{e}");
        assert!(SecretRef::seal(None, "ops", b"x").is_err());
        let v = serde_json::to_value(SecretRef::Env("A".into())).unwrap();
        assert_eq!(v, json!("env:A"));
        assert!(valid_name("ops-alerts_1") && !valid_name("Ops") && !valid_name(&"x".repeat(41)));
    }
}
