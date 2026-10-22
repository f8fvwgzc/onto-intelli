//! Bring your own LLM (docs/CONTROL_PLANE.md section 8) on [`Control`]:
//! validating and storing a project's provider settings with the key sealed
//! by the [`Vault`](crate::vault::Vault), building the project's provider
//! (on the SSRF-guarded client of [`net`](crate::net)), the test connection
//! and the usage report.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use onto_core::time::{fmt_date, now_ms, parse_ts};
use onto_core::{Error, Result};
use onto_intelli::config::{LlmConfig, Pricing, Secret};
use onto_intelli::limits::{Limits, MonthToDate};
use onto_intelli::llm::{self, ChatMessage, LlmProvider, LlmRequest};

use crate::model::{AuditEntry, LLM_PROVIDERS, Project, ProjectLlm, Sealed, UsageDay};
use crate::{Control, net, vault};

/// LLM tests per project per minute.
pub const LLM_TESTS_PER_MINUTE: usize = 10;
/// Provider error text in a test result is clipped to this many characters.
pub const TEST_ERROR_CHARS: usize = 200;
/// The test completion.
pub const TEST_SYSTEM: &str = "Reply with the single word OK.";
pub const TEST_USER: &str = "ping";
pub const TEST_MAX_TOKENS: u32 = 8;

/// `PUT …/llm` and the draft of `POST …/llm/test`: a project's provider
/// settings. `api_key` is write-only: absent keeps the stored key, `""`
/// removes it.
#[derive(Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LlmUpdate {
    pub provider: String,
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub base_url: Option<String>,
    #[serde(default)]
    pub api_key: Option<String>,
    #[serde(default)]
    pub max_output_tokens: Option<u32>,
    #[serde(default)]
    pub temperature: Option<f32>,
    #[serde(default)]
    pub effort: Option<String>,
    #[serde(default)]
    pub timeout_secs: Option<u32>,
    #[serde(default)]
    pub pricing: Option<Pricing>,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    #[serde(default)]
    pub monthly_budget_usd: Option<f64>,
    #[serde(default)]
    pub monthly_token_budget: Option<i64>,
    #[serde(default)]
    pub asks_per_minute: Option<i32>,
}

impl std::fmt::Debug for LlmUpdate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LlmUpdate")
            .field("provider", &self.provider)
            .field("model", &self.model)
            .field("base_url", &self.base_url)
            .field("api_key", &self.api_key.as_ref().map(|_| "****"))
            .finish_non_exhaustive()
    }
}

impl LlmUpdate {
    /// Parse a request body. A project can never name an environment
    /// variable for its key (it would let a project admin send any server
    /// secret to a URL of their choosing).
    pub fn from_json(v: Value) -> Result<Self> {
        if v.get("api_key_env").is_some() {
            return Err(Error::invalid(
                "api_key_env cannot be set by a project (it would read the server's environment); send api_key",
            ));
        }
        serde_json::from_value(v).map_err(|e| Error::invalid(format!("LLM settings: {e}")))
    }
}

/// The result of a test connection.
#[derive(Clone, Debug, Serialize)]
pub struct LlmTest {
    pub ok: bool,
    pub latency_ms: u64,
    pub provider: String,
    pub model: String,
    pub response_model: Option<String>,
    pub usage: Option<onto_intelli::token::TokenUsage>,
    pub error: Option<String>,
}

/// What `onto console rotate-master-key` did.
#[derive(Clone, Debug, Default, Serialize)]
pub struct RotateReport {
    /// Keys rewrapped under the current master key (rows changed).
    pub rewrapped: usize,
    /// Keys already under it.
    pub current: usize,
    /// Keys that did not decrypt, by project id.
    pub failed: Vec<(String, String)>,
}

fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max).collect();
    out.push('…');
    out
}

/// The last four characters of a key long enough to show them.
fn last4(key: &str) -> Option<String> {
    let n = key.chars().count();
    (n >= 12).then(|| key.chars().skip(n - 4).collect())
}

fn finite_non_negative(what: &str, v: f64) -> Result<()> {
    if !v.is_finite() || v < 0.0 {
        return Err(Error::invalid(format!("{what} must be a number of at least 0")));
    }
    Ok(())
}

/// The limits of a project: `asks_per_minute` from its LLM settings, else
/// from its project settings; the budgets from its LLM settings.
pub fn limits_of(project: &Project, llm: Option<&ProjectLlm>) -> Limits {
    let setting = project
        .settings
        .get("asks_per_minute")
        .and_then(Value::as_u64)
        .filter(|v| *v > 0)
        .map(|v| v.min(u32::MAX as u64) as u32);
    Limits {
        asks_per_minute: llm.and_then(|l| l.asks_per_minute).filter(|v| *v > 0).map(|v| v as u32).or(setting),
        monthly_cost_usd: llm.and_then(|l| l.monthly_budget_usd),
        monthly_tokens: llm.and_then(|l| l.monthly_token_budget).map(|v| v.max(0) as u64),
    }
}

/// The month to date of usage rows (`YYYY-MM-DD` days of the current month).
pub fn month_total(rows: &[UsageDay]) -> MonthToDate {
    let mut m = MonthToDate::default();
    for r in rows {
        m.cost_usd += r.cost_usd;
        m.tokens += (r.input_tokens + r.output_tokens).max(0) as u64;
    }
    m
}

/// `YYYY-MM-01` of the month of `now`.
pub fn month_start(now: i64) -> String {
    format!("{}-01", &fmt_date(now)[..7])
}

impl Control {
    /// Room for one more LLM test in this project this minute.
    pub fn allow_llm_test(&self, project_id: &str) -> bool {
        let now = Instant::now();
        let mut m = self.llm_tests.lock();
        let v = m.entry(project_id.to_string()).or_default();
        v.retain(|t| now.duration_since(*t) < Duration::from_secs(60));
        if v.len() >= LLM_TESTS_PER_MINUTE {
            return false;
        }
        v.push(now);
        true
    }

    /// `ONTO_MASTER_KEY` is set: LLM keys can be stored.
    pub fn can_store_llm_keys(&self) -> bool {
        self.vault.is_some()
    }

    /// Why the project's own provider is not in use (its key does not
    /// decrypt, its settings no longer build), if it is not.
    pub fn llm_error(&self, project_id: &str) -> Option<String> {
        self.llm_errors.lock().get(project_id).cloned()
    }

    pub(crate) fn set_llm_error(&self, project_id: &str, e: Option<String>) {
        let mut m = self.llm_errors.lock();
        match e {
            Some(e) => m.insert(project_id.to_string(), e),
            None => m.remove(project_id),
        };
    }

    /// Check settings: a known provider (`mock` only in dev mode), the base
    /// URL rules of the SSRF guard, no credential headers, sane numbers.
    fn check_llm(&self, u: &LlmUpdate, dev: bool) -> Result<()> {
        let p = u.provider.trim();
        if !LLM_PROVIDERS.contains(&p) {
            return Err(Error::invalid(format!(
                "unknown LLM provider '{p}' (anthropic, openai, ollama, openai-compatible, none)"
            )));
        }
        if p == "mock" && !dev {
            return Err(Error::invalid("the mock provider is only for development mode (onto serve --dev)"));
        }
        let allow = self.config.allow_private_urls;
        match u.base_url.as_deref().map(str::trim).filter(|b| !b.is_empty()) {
            Some(b) => {
                net::check_url(b, allow)?;
            }
            None if matches!(p, "ollama" | "openai-compatible") => {
                return Err(Error::invalid(format!("provider {p} needs a base_url")));
            }
            None => {}
        }
        net::check_headers(&u.headers)?;
        if u.model.chars().count() > 200 || u.model.chars().any(char::is_control) {
            return Err(Error::invalid("model must be at most 200 printable characters"));
        }
        if let Some(k) = &u.api_key
            && (k.len() > 4096 || k.chars().any(char::is_control) || k.trim() != k)
        {
            return Err(Error::invalid("api_key must be at most 4096 characters, without spaces around it"));
        }
        if u.max_output_tokens.is_some_and(|m| !(1..=1_000_000).contains(&m)) {
            return Err(Error::invalid("max_output_tokens must be 1 to 1000000"));
        }
        if u.timeout_secs.is_some_and(|t| !(1..=600).contains(&t)) {
            return Err(Error::invalid("timeout_secs must be 1 to 600"));
        }
        if u.temperature.is_some_and(|t| !(0.0..=2.0).contains(&t)) {
            return Err(Error::invalid("temperature must be 0 to 2"));
        }
        if let Some(e) = &u.effort
            && !["low", "medium", "high", "xhigh", "max"].contains(&e.as_str())
        {
            return Err(Error::invalid("effort must be low, medium, high, xhigh or max"));
        }
        if let Some(pr) = &u.pricing {
            finite_non_negative("pricing.input_per_mtok", pr.input_per_mtok)?;
            finite_non_negative("pricing.output_per_mtok", pr.output_per_mtok)?;
            finite_non_negative("pricing.cached_input_per_mtok", pr.cached_input_per_mtok)?;
        }
        if let Some(b) = u.monthly_budget_usd {
            finite_non_negative("monthly_budget_usd", b)?;
        }
        if u.monthly_token_budget.is_some_and(|t| t < 0) {
            return Err(Error::invalid("monthly_token_budget must be at least 0"));
        }
        if u.asks_per_minute.is_some_and(|a| !(1..=1_000_000).contains(&a)) {
            return Err(Error::invalid("asks_per_minute must be 1 to 1000000"));
        }
        Ok(())
    }

    /// A stored key may follow the settings only to the provider and base
    /// URL it was entered for: otherwise whoever changes the base URL would
    /// receive the key.
    fn same_target(stored: &ProjectLlm, u: &LlmUpdate) -> bool {
        let norm = |b: Option<&str>| b.map(|b| b.trim().trim_end_matches('/').to_string()).filter(|b| !b.is_empty());
        stored.provider == u.provider.trim() && norm(stored.base_url.as_deref()) == norm(u.base_url.as_deref())
    }

    /// The settings as stored from an update (the key sealed).
    fn settings_from(
        &self,
        project: &Project,
        u: &LlmUpdate,
        stored: Option<&ProjectLlm>,
        by: &str,
    ) -> Result<ProjectLlm> {
        let (ct, last4) = match u.api_key.as_deref() {
            Some("") => (None, None),
            Some(k) => {
                let v = self.vault.as_ref().ok_or_else(|| Error::invalid(vault::NO_MASTER_KEY))?;
                (Some(Sealed(v.seal(&project.id, k.as_bytes())?)), last4(k))
            }
            None => match stored.filter(|s| s.has_api_key()) {
                Some(s) if Self::same_target(s, u) => (s.api_key_ct.clone(), s.api_key_last4.clone()),
                Some(_) => {
                    return Err(Error::invalid(
                        "changing the provider or base_url needs the api_key again (send it, or \"\" for none)",
                    ));
                }
                None => (None, None),
            },
        };
        let provider = u.provider.trim().to_string();
        if ct.is_none() && matches!(provider.as_str(), "openai" | "anthropic") {
            return Err(Error::invalid(format!("provider {provider} needs an api_key")));
        }
        Ok(ProjectLlm {
            project_id: project.id.clone(),
            provider,
            model: u.model.trim().to_string(),
            base_url: u
                .base_url
                .as_deref()
                .map(str::trim)
                .filter(|b| !b.is_empty())
                .map(|b| b.trim_end_matches('/').to_string()),
            api_key_ct: ct,
            api_key_last4: last4,
            max_output_tokens: u.max_output_tokens.unwrap_or(4096),
            temperature: u.temperature,
            effort: u.effort.clone(),
            timeout_secs: u.timeout_secs.unwrap_or(120),
            pricing: u.pricing.clone().unwrap_or_default(),
            headers: u.headers.iter().map(|(k, v)| (k.trim().to_ascii_lowercase(), v.clone())).collect(),
            monthly_budget_usd: u.monthly_budget_usd,
            monthly_token_budget: u.monthly_token_budget,
            asks_per_minute: u.asks_per_minute,
            updated_at: now_ms(),
            updated_by: by.to_string(),
        })
    }

    /// `GET …/llm`: the settings (never the key), whether keys can be
    /// stored, and why the project's provider is not in use, if it is not.
    pub async fn llm_view(&self, project: &Project) -> Result<Value> {
        let stored = self.store.llm(&project.id).await?;
        Ok(json!({
            "llm": stored.as_ref().map(llm_json),
            "error": self.llm_error(&project.id),
            "master_key": self.can_store_llm_keys(),
        }))
    }

    /// `PUT …/llm`: replace the project's settings (absent `api_key` keeps
    /// the stored key). Bumps `settings_version`; the caller refreshes the
    /// tenant.
    pub async fn put_llm(&self, project: &Project, u: LlmUpdate, by: &str, dev: bool) -> Result<ProjectLlm> {
        self.check_llm(&u, dev)?;
        let stored = self.store.llm(&project.id).await?;
        let s = self.settings_from(project, &u, stored.as_ref(), by)?;
        // The new settings must build (a bad model name surfaces later, at the first call).
        self.provider_for(project, &s, dev)?;
        self.store.put_llm(s.clone()).await?;
        self.set_llm_error(&project.id, None);
        self.audit(AuditEntry::new(by, "llm.update").project(project).target(&project.r#ref).detail(json!({
            "provider": s.provider, "model": s.model, "base_url": s.base_url,
            "api_key": match u.api_key.as_deref() { Some("") => "removed", Some(_) => "set", None => "kept" },
            "monthly_budget_usd": s.monthly_budget_usd, "monthly_token_budget": s.monthly_token_budget,
            "asks_per_minute": s.asks_per_minute, "headers": s.headers.keys().collect::<Vec<_>>(),
        })))
        .await;
        Ok(s)
    }

    /// `DELETE …/llm`: back to the server's provider. Returns whether there
    /// were settings.
    pub async fn delete_llm(&self, project: &Project, by: &str) -> Result<bool> {
        let had = self.store.delete_llm(&project.id).await?;
        self.set_llm_error(&project.id, None);
        if had {
            self.audit(AuditEntry::new(by, "llm.delete").project(project).target(&project.r#ref)).await;
        }
        Ok(had)
    }

    /// The decrypted key of stored settings.
    fn key_of(&self, project: &Project, s: &ProjectLlm) -> Result<Option<Secret>> {
        let Some(ct) = &s.api_key_ct else { return Ok(None) };
        let v = self.vault.as_ref().ok_or_else(|| {
            Error::Config("the stored LLM API key cannot be decrypted: ONTO_MASTER_KEY is not set".into())
        })?;
        let plain = v.open(&project.id, &ct.0)?;
        let key = String::from_utf8(plain).map_err(|_| Error::Invalid("the stored LLM API key is not UTF-8".into()))?;
        Ok(Some(Secret::new(key)))
    }

    /// The project's provider from stored settings, with its key decrypted,
    /// on the SSRF-guarded client.
    pub fn provider_for(&self, project: &Project, s: &ProjectLlm, dev: bool) -> Result<Arc<dyn LlmProvider>> {
        let key = self.key_of(project, s)?;
        self.build_provider(s, key, dev)
    }

    fn build_provider(&self, s: &ProjectLlm, key: Option<Secret>, dev: bool) -> Result<Arc<dyn LlmProvider>> {
        let provider = s.provider.as_str();
        if !LLM_PROVIDERS.contains(&provider) {
            return Err(Error::invalid(format!("unknown LLM provider '{provider}'")));
        }
        if provider == "mock" && !dev {
            return Err(Error::invalid("the mock provider is only for development mode"));
        }
        let allow = self.config.allow_private_urls;
        let base_url = s.base_url.as_deref().map(|b| net::check_url(b, allow)).transpose()?;
        if base_url.is_none() && matches!(provider, "ollama" | "openai-compatible") {
            return Err(Error::invalid(format!("provider {provider} needs a base_url")));
        }
        net::check_headers(&s.headers)?;
        let cfg = LlmConfig {
            provider: s.provider.clone(),
            model: s.model.clone(),
            base_url,
            // Never the server's environment: no variable at all.
            api_key_env: Some(String::new()),
            api_key: key,
            max_output_tokens: s.max_output_tokens.max(1),
            temperature: s.temperature,
            effort: s.effort.clone(),
            timeout_secs: s.timeout_secs.max(1) as u64,
            pricing: s.pricing.clone(),
            headers: s.headers.clone(),
            ..Default::default()
        };
        match provider {
            "mock" | "none" => llm::build(&cfg),
            _ => llm::build_with_client(&cfg, net::client(allow, Duration::from_secs(cfg.timeout_secs))?),
        }
    }

    /// `POST …/llm/test`: one small completion on the draft settings, or the
    /// stored ones, or (with neither) `server_default`. The stored key is
    /// used when the draft has none and keeps the stored provider and base
    /// URL. Not metered, not counted against the budget.
    pub async fn test_llm(
        &self,
        project: &Project,
        draft: Option<LlmUpdate>,
        server_default: Arc<dyn LlmProvider>,
        dev: bool,
    ) -> Result<LlmTest> {
        let stored = self.store.llm(&project.id).await?;
        let provider = match (draft, &stored) {
            (Some(d), _) => {
                self.check_llm(&d, dev)?;
                let key = match d.api_key.as_deref() {
                    Some("") => None,
                    Some(k) => Some(Secret::new(k)),
                    None => match stored.as_ref().filter(|s| s.has_api_key() && Self::same_target(s, &d)) {
                        Some(s) => self.key_of(project, s)?,
                        None => None,
                    },
                };
                self.build_provider(&draft_llm(project, &d), key, dev)?
            }
            (None, Some(s)) => self.provider_for(project, s, dev)?,
            (None, None) => server_default,
        };
        let req = LlmRequest {
            system: TEST_SYSTEM.into(),
            messages: vec![ChatMessage::user(TEST_USER)],
            max_tokens: TEST_MAX_TOKENS,
        };
        let t0 = Instant::now();
        let result = match provider.unavailable() {
            Some(why) => Err(Error::Unavailable(why)),
            None => provider.complete(&req).await,
        };
        let latency_ms = t0.elapsed().as_millis() as u64;
        let (name, model) = (provider.name().to_string(), provider.model().to_string());
        Ok(match result {
            Ok(r) => LlmTest {
                ok: true,
                latency_ms,
                provider: name,
                model,
                response_model: Some(r.model),
                usage: Some(r.usage),
                error: None,
            },
            Err(e) => LlmTest {
                ok: false,
                latency_ms,
                provider: name,
                model,
                response_model: None,
                usage: None,
                error: Some(clip(&e.to_string(), TEST_ERROR_CHARS)),
            },
        })
    }
}

/// Draft settings as a row, without a key (the test of a draft).
fn draft_llm(project: &Project, d: &LlmUpdate) -> ProjectLlm {
    ProjectLlm {
        project_id: project.id.clone(),
        provider: d.provider.trim().to_string(),
        model: d.model.trim().to_string(),
        base_url: d.base_url.clone().filter(|b| !b.trim().is_empty()),
        api_key_ct: None,
        api_key_last4: None,
        max_output_tokens: d.max_output_tokens.unwrap_or(4096),
        temperature: d.temperature,
        effort: d.effort.clone(),
        timeout_secs: d.timeout_secs.unwrap_or(120),
        pricing: d.pricing.clone().unwrap_or_default(),
        headers: d.headers.clone(),
        monthly_budget_usd: None,
        monthly_token_budget: None,
        asks_per_minute: None,
        updated_at: now_ms(),
        updated_by: "test".into(),
    }
}

impl Control {
    /// `GET …/usage?from=&to=`: the `usage_daily` rows (default: this month),
    /// the month to date (from this replica's meter when the project is
    /// open here, which includes usage not yet written) and the budget.
    pub async fn usage_report(&self, project: &Project, from: Option<&str>, to: Option<&str>) -> Result<Value> {
        let now = now_ms();
        let today = fmt_date(now);
        let first = month_start(now);
        let from = from.map(str::trim).filter(|v| !v.is_empty()).unwrap_or(&first).to_string();
        let to = to.map(str::trim).filter(|v| !v.is_empty()).unwrap_or(&today).to_string();
        let (Some(f), Some(t)) =
            (parse_ts(&from).filter(|_| from.len() == 10), parse_ts(&to).filter(|_| to.len() == 10))
        else {
            return Err(Error::invalid("from and to are days, YYYY-MM-DD"));
        };
        if f > t || t - f > 400 * 86_400_000 {
            return Err(Error::invalid("from must not be after to, and at most 400 days before it"));
        }
        let rows = self.store.usage(&project.id, &from, &to).await?;
        let meter = self.meters.lock().get(&project.id).cloned();
        let mtd = match meter {
            Some(m) => onto_intelli::limits::UsageMeter::month_to_date(m.as_ref()),
            None => month_total(&self.store.usage(&project.id, &first, &today).await?),
        };
        let llm = self.store.llm(&project.id).await?;
        let limits = limits_of(project, llm.as_ref());
        Ok(json!({
            "from": from,
            "to": to,
            "rows": rows,
            "month_to_date": { "month": &first[..7], "cost_usd": mtd.cost_usd, "tokens": mtd.tokens },
            "budget": {
                "monthly_budget_usd": limits.monthly_cost_usd,
                "monthly_token_budget": limits.monthly_tokens,
                "asks_per_minute": limits.asks_per_minute,
                "reached": limits.budget_reached(&mtd),
            },
        }))
    }

    /// Rewrap every stored LLM key under the current `ONTO_MASTER_KEY`
    /// (`onto console rotate-master-key`); the secrets are not re-encrypted.
    pub async fn rotate_master_key(&self) -> Result<RotateReport> {
        let v = self
            .vault
            .as_ref()
            .ok_or_else(|| Error::Config("ONTO_MASTER_KEY is not set: nothing to rotate to".into()))?;
        let mut report = RotateReport::default();
        for l in self.store.llms().await? {
            let Some(ct) = &l.api_key_ct else { continue };
            match v.rewrap(&l.project_id, &ct.0) {
                Ok(None) => report.current += 1,
                Ok(Some(new)) => {
                    if self.store.replace_llm_key(&l.project_id, ct, &Sealed(new)).await? {
                        report.rewrapped += 1;
                    }
                }
                Err(e) => report.failed.push((l.project_id.clone(), e.to_string())),
            }
        }
        Ok(report)
    }
}

/// Settings as the console shows them: never the key.
pub fn llm_json(s: &ProjectLlm) -> Value {
    let mut v = serde_json::to_value(s).unwrap_or(Value::Null);
    if let Value::Object(m) = &mut v {
        m.insert("has_api_key".into(), json!(s.has_api_key()));
        // Headers stored before credential names were refused: names only.
        if let Some(Value::Object(h)) = m.get_mut("headers") {
            for (k, v) in h.iter_mut() {
                if net::credential_header(k) {
                    *v = json!("***");
                }
            }
        }
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clipping_and_last4() {
        assert_eq!(clip("abc", 5), "abc");
        assert_eq!(clip(&"x".repeat(300), 200).chars().count(), 201);
        assert_eq!(last4("sk-0123456789abcd").as_deref(), Some("abcd"));
        assert_eq!(last4("short"), None);
        assert_eq!(month_start(parse_ts("2026-10-22").unwrap()), "2026-10-01");
    }

    #[test]
    fn credential_headers_stored_earlier_are_not_shown() {
        let s = ProjectLlm {
            project_id: "p".into(),
            provider: "openai-compatible".into(),
            model: String::new(),
            base_url: None,
            api_key_ct: None,
            api_key_last4: None,
            max_output_tokens: 4096,
            temperature: None,
            effort: None,
            timeout_secs: 60,
            pricing: Default::default(),
            headers: BTreeMap::from([
                ("x-goog-api-key".to_string(), "AIza-secret".to_string()),
                ("x-team".to_string(), "core".to_string()),
            ]),
            monthly_budget_usd: None,
            monthly_token_budget: None,
            asks_per_minute: None,
            updated_at: 0,
            updated_by: String::new(),
        };
        let v = llm_json(&s);
        assert_eq!(v["headers"], json!({ "x-goog-api-key": "***", "x-team": "core" }));
    }

    #[test]
    fn a_project_cannot_name_an_environment_variable() {
        let err = LlmUpdate::from_json(json!({ "provider": "openai", "api_key_env": "ONTO_MASTER_KEY" })).unwrap_err();
        assert!(err.to_string().contains("api_key_env"), "{err}");
        assert!(LlmUpdate::from_json(json!({ "provider": "openai", "surprise": 1 })).is_err());
        let u = LlmUpdate::from_json(json!({ "provider": "openai", "api_key": "sk-123" })).unwrap();
        assert!(!format!("{u:?}").contains("sk-123"));
    }
}
