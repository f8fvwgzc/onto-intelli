//! The control plane's [`TenantHooks`] (docs/CONTROL_PLANE.md sections 8.3
//! and 8.5): each project's own provider, its limits and its usage meter.
//!
//! * The provider is built from `project_llm` with the decrypted key; a
//!   project without settings uses the server's `[llm]`. When the key does
//!   not decrypt (or the settings no longer build) the project uses the
//!   server's provider too, the error is logged and `GET …/llm` shows it.
//! * The [`Meter`] keeps a project's month-to-date totals in memory, loaded
//!   from `usage_daily` when the project opens and re-read every 60 s;
//!   `record` adds to them and queues a delta, written with `add_usage`
//!   every 10 s.
//! * Every 30 s the projects' `settings_version`s are read and open tenants
//!   whose version moved are refreshed, so other replicas pick up a change
//!   within 30 s (the replica that made it refreshes at once).

use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Weak};
use std::time::Duration;

use async_trait::async_trait;
use parking_lot::Mutex;

use onto_core::Result;
use onto_core::time::{fmt_date, now_ms};
use onto_intelli::Hub;
use onto_intelli::hub::{Secret, SecretBox, TenantHooks, TenantSettings};
use onto_intelli::limits::{MonthToDate, UsageMeter};
use onto_intelli::token::TokenUsage;
use onto_intelli::trace::TraceSettings;
use serde_json::{Map, Value};

use crate::llm::{limits_of, month_start, month_total};
use crate::model::{Project, UsageDay};
use crate::{Control, ControlStore, Vault};

/// How often queued usage is written.
pub const FLUSH_EVERY: Duration = Duration::from_secs(10);
/// Every this many flushes the projects' `settings_version`s are polled (30 s).
pub const POLL_TICKS: u64 = 3;
/// Every this many flushes the meters re-read `usage_daily` (60 s).
pub const RELOAD_TICKS: u64 = 6;

type DayKey = (String, String, String);

struct MeterState {
    /// `YYYY-MM-01` of the month `base` covers.
    month: String,
    /// Month to date as written in `usage_daily` (plus what this meter
    /// wrote since it last read it).
    base: MonthToDate,
    /// Recorded, not yet written: by (day, provider, model).
    pending: HashMap<DayKey, UsageDay>,
}

/// One project's usage meter.
pub struct Meter {
    project_id: String,
    state: Mutex<MeterState>,
}

impl Meter {
    fn new(project_id: &str) -> Self {
        Meter {
            project_id: project_id.to_string(),
            state: Mutex::new(MeterState {
                month: month_start(now_ms()),
                base: MonthToDate::default(),
                pending: HashMap::new(),
            }),
        }
    }

    /// Read this month's totals from the store.
    pub async fn load(&self, store: &dyn ControlStore) -> Result<()> {
        let now = now_ms();
        let first = month_start(now);
        let rows = store.usage(&self.project_id, &first, &fmt_date(now)).await?;
        let mut s = self.state.lock();
        s.month = first;
        s.base = month_total(&rows);
        Ok(())
    }

    /// Write the queued deltas; on failure they stay queued.
    pub async fn flush(&self, store: &dyn ControlStore) -> Result<usize> {
        let rows: Vec<UsageDay> = self.state.lock().pending.drain().map(|(_, v)| v).collect();
        if rows.is_empty() {
            return Ok(0);
        }
        match store.add_usage(&rows).await {
            Ok(()) => {
                let mut s = self.state.lock();
                let month = s.month.clone();
                let written: Vec<UsageDay> = rows.iter().filter(|r| r.day[..7] == month[..7]).cloned().collect();
                let add = month_total(&written);
                s.base.cost_usd += add.cost_usd;
                s.base.tokens += add.tokens;
                Ok(rows.len())
            }
            Err(e) => {
                let mut s = self.state.lock();
                for r in rows {
                    let key = (r.day.clone(), r.provider.clone(), r.model.clone());
                    match s.pending.get_mut(&key) {
                        Some(p) => p.add(&r),
                        None => {
                            s.pending.insert(key, r);
                        }
                    }
                }
                Err(e)
            }
        }
    }
}

impl UsageMeter for Meter {
    fn month_to_date(&self) -> MonthToDate {
        let month = month_start(now_ms());
        let s = self.state.lock();
        let mut m = if s.month == month { s.base } else { MonthToDate::default() };
        for d in s.pending.values().filter(|d| d.day[..7] == month[..7]) {
            m.cost_usd += d.cost_usd;
            m.tokens += (d.input_tokens + d.output_tokens).max(0) as u64;
        }
        m
    }

    fn record(&self, usage: &TokenUsage, provider: &str, model: &str, deterministic: bool, error: bool) {
        let day = fmt_date(now_ms());
        let key = (day.clone(), provider.to_string(), model.to_string());
        let mut s = self.state.lock();
        let row = s.pending.entry(key).or_insert_with(|| UsageDay {
            project_id: self.project_id.clone(),
            day,
            provider: provider.to_string(),
            model: model.to_string(),
            ..Default::default()
        });
        let called = usage.total() > 0 || error;
        row.add(&UsageDay {
            asks: 1,
            llm_calls: called as i64,
            deterministic: deterministic as i64,
            input_tokens: usage.prompt_tokens as i64,
            output_tokens: usage.completion_tokens as i64,
            cached_tokens: usage.cached_tokens as i64,
            cost_usd: usage.cost_usd,
            errors: error as i64,
            ..Default::default()
        });
    }
}

/// [`TenantHooks`] on a [`Control`].
pub struct ControlHooks {
    control: Arc<Control>,
    dev: bool,
}

impl ControlHooks {
    pub fn new(control: Arc<Control>, dev: bool) -> Self {
        ControlHooks { control, dev }
    }
}

#[async_trait]
impl TenantHooks for ControlHooks {
    /// A tenant no project has (a config tenant) gets the defaults; the
    /// tenant of a deleted project is refused, so a replica that has not
    /// heard of the deletion does not open it again (and recreate its
    /// dropped schema).
    async fn settings(&self, tenant: &str) -> Result<TenantSettings> {
        let c = &self.control;
        let project = match c.store().project_by_ref(tenant).await? {
            None => return Ok(TenantSettings::default()),
            Some(p) if !p.active() => {
                c.applied.lock().remove(tenant);
                return Err(onto_core::Error::not_found(format!("project {tenant} (deleted)")));
            }
            Some(p) => p,
        };
        let row = c.store().llm(&project.id).await?;
        let mut llm = None;
        if let Some(s) = &row {
            match c.provider_for(&project, s, self.dev) {
                Ok(p) => {
                    llm = Some(p);
                    c.set_llm_error(&project.id, None);
                }
                Err(e) => {
                    tracing::error!(project = %project.r#ref, "the project's LLM provider: {e}; using the server's");
                    c.set_llm_error(&project.id, Some(e.to_string()));
                }
            }
        } else {
            c.set_llm_error(&project.id, None);
        }
        let meter = c.meter(&project.id).await?;
        c.applied.lock().insert(tenant.to_string(), project.settings_version);
        Ok(TenantSettings {
            llm,
            limits: limits_of(&project, row.as_ref()),
            meter: Some(meter),
            version: project.settings_version.max(0) as u64,
            traces: trace_settings_of(&project),
            secrets: c
                .vault
                .clone()
                .map(|vault| Arc::new(VaultSecrets { vault, project_id: project.id.clone() }) as Arc<dyn SecretBox>),
        })
    }
}

/// A project's outbound secrets (webhook signing secrets, …), sealed with
/// the vault ([`Vault::seal_for`]) under AAD `onto/<purpose>/v1/<project>/<name>`.
pub struct VaultSecrets {
    vault: Arc<Vault>,
    project_id: String,
}

impl VaultSecrets {
    pub fn new(vault: Arc<Vault>, project_id: impl Into<String>) -> Self {
        Self { vault, project_id: project_id.into() }
    }
}

impl SecretBox for VaultSecrets {
    fn seal(&self, purpose: &str, name: &str, plain: &[u8]) -> Result<String> {
        use base64::Engine as _;
        let blob = self.vault.seal_for(purpose, &self.project_id, name, plain)?;
        Ok(base64::engine::general_purpose::STANDARD.encode(blob))
    }

    fn open(&self, purpose: &str, name: &str, sealed: &str) -> Result<Secret> {
        use base64::Engine as _;
        let blob = base64::engine::general_purpose::STANDARD
            .decode(sealed.trim())
            .map_err(|_| onto_core::Error::invalid("a sealed secret is base64"))?;
        Ok(Secret::new(self.vault.open_for(purpose, &self.project_id, name, &blob)?))
    }

    fn rewrap(&self, sealed: &str) -> Result<Option<String>> {
        use base64::Engine as _;
        let b64 = base64::engine::general_purpose::STANDARD;
        let blob = b64.decode(sealed.trim()).map_err(|_| onto_core::Error::invalid("a sealed secret is base64"))?;
        Ok(self.vault.rewrap(&self.project_id, &blob)?.map(|new| b64.encode(new)))
    }
}

/// A project's trace settings: `trace_retention_days` (1 to 400) and
/// `capture_content` from its settings; absent or invalid ones use `[traces]`.
pub fn trace_settings_of(project: &Project) -> TraceSettings {
    trace_settings_from(&project.settings)
}

fn trace_settings_from(s: &Value) -> TraceSettings {
    TraceSettings {
        retention_days: s
            .get("trace_retention_days")
            .and_then(Value::as_u64)
            .filter(|d| (1..=400).contains(d))
            .map(|d| d as u32),
        capture_content: s.get("capture_content").and_then(Value::as_bool),
    }
}

/// Refuse project settings the traces cannot use (`null` removes a key).
pub fn check_trace_settings(m: &Map<String, Value>) -> Result<()> {
    match m.get("trace_retention_days") {
        None | Some(Value::Null) => {}
        Some(v) if v.as_u64().is_some_and(|d| (1..=400).contains(&d)) => {}
        Some(_) => {
            return Err(onto_core::Error::invalid("settings.trace_retention_days is a whole number of days, 1 to 400"));
        }
    }
    match m.get("capture_content") {
        None | Some(Value::Null) | Some(Value::Bool(_)) => Ok(()),
        Some(_) => Err(onto_core::Error::invalid("settings.capture_content is true or false")),
    }
}

impl Control {
    /// A project's meter, created and loaded from `usage_daily` on first use.
    pub async fn meter(&self, project_id: &str) -> Result<Arc<dyn UsageMeter>> {
        if let Some(m) = self.meters.lock().get(project_id).cloned() {
            return Ok(m);
        }
        let m = Arc::new(Meter::new(project_id));
        m.load(self.store.as_ref()).await?;
        let mut meters = self.meters.lock();
        // Another open may have made one meanwhile: keep the first.
        let m: Arc<Meter> = meters.entry(project_id.to_string()).or_insert(m).clone();
        Ok(m)
    }

    /// Write every meter's queued usage now; returns the rows written.
    pub async fn flush_usage(&self) -> Result<usize> {
        let meters: Vec<Arc<Meter>> = self.meters.lock().values().cloned().collect();
        let mut n = 0;
        for m in meters {
            n += m.flush(self.store.as_ref()).await?;
        }
        Ok(n)
    }

    /// Re-read every meter's month to date from `usage_daily` (after a flush).
    pub async fn reload_usage(&self) -> Result<()> {
        let meters: Vec<Arc<Meter>> = self.meters.lock().values().cloned().collect();
        for m in meters {
            m.flush(self.store.as_ref()).await?;
            m.load(self.store.as_ref()).await?;
        }
        Ok(())
    }

    /// Refresh the open tenants whose project's `settings_version` moved
    /// since their settings were read; returns the refreshed refs. Open
    /// tenants of projects that are no longer active (deleted through
    /// another replica) are evicted.
    pub async fn poll_settings(&self, hub: &Hub) -> Result<Vec<String>> {
        let versions: HashMap<String, i64> = self.store.settings_versions().await?.into_iter().collect();
        let gone: Vec<String> = {
            let mut applied = self.applied.lock();
            let gone: Vec<String> = applied.keys().filter(|t| !versions.contains_key(*t)).cloned().collect();
            for t in &gone {
                applied.remove(t);
            }
            gone
        };
        for t in gone {
            if hub.evict(&t) {
                tracing::info!(tenant = %t, "the project is no longer active: tenant closed");
            }
        }
        let stale: Vec<String> = {
            let applied = self.applied.lock();
            hub.open_tenants()
                .into_iter()
                .filter(|t| versions.get(t).is_some_and(|v| applied.get(t) != Some(v)))
                .collect()
        };
        let mut done = Vec::new();
        for t in stale {
            match hub.refresh_tenant(&t).await {
                Ok(true) => done.push(t),
                Ok(false) => {}
                Err(e) => tracing::warn!(tenant = %t, "refreshing tenant settings: {e}"),
            }
        }
        Ok(done)
    }
}

/// Give `hub` the control plane's tenant settings: open tenants are
/// refreshed now, and (once per control plane) a task writes usage every
/// 10 s, polls `settings_version` every 30 s and re-reads the meters every
/// 60 s, for as long as both live.
pub async fn install(control: &Arc<Control>, hub: &Arc<Hub>, dev: bool) {
    hub.set_hooks(Arc::new(ControlHooks::new(control.clone(), dev)));
    for t in hub.open_tenants() {
        if let Err(e) = hub.refresh_tenant(&t).await {
            tracing::warn!(tenant = %t, "applying tenant settings: {e}");
        }
    }
    if control.hooks_started.swap(true, Ordering::SeqCst) {
        return;
    }
    let (wc, wh): (Weak<Control>, Weak<Hub>) = (Arc::downgrade(control), Arc::downgrade(hub));
    tokio::spawn(async move {
        let mut tick: u64 = 0;
        loop {
            tokio::time::sleep(FLUSH_EVERY).await;
            let (Some(c), Some(hub)) = (wc.upgrade(), wh.upgrade()) else { break };
            tick += 1;
            if tick.is_multiple_of(RELOAD_TICKS) {
                if let Err(e) = c.reload_usage().await {
                    tracing::warn!("re-reading LLM usage: {e}");
                }
            } else if let Err(e) = c.flush_usage().await {
                tracing::warn!("writing LLM usage: {e}");
            }
            if tick.is_multiple_of(POLL_TICKS)
                && let Err(e) = c.poll_settings(&hub).await
            {
                tracing::warn!("polling project settings: {e}");
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MemoryControl;

    #[test]
    fn trace_settings_come_from_project_settings() {
        let s = serde_json::json!({ "trace_retention_days": 7, "capture_content": false });
        assert_eq!(trace_settings_from(&s), TraceSettings { retention_days: Some(7), capture_content: Some(false) });
        let s = serde_json::json!({ "trace_retention_days": 0, "capture_content": "no" });
        assert_eq!(trace_settings_from(&s), TraceSettings::default());
        let ok = serde_json::json!({ "trace_retention_days": 400, "capture_content": null });
        assert!(check_trace_settings(ok.as_object().unwrap()).is_ok());
        for bad in [serde_json::json!({ "trace_retention_days": 401 }), serde_json::json!({ "capture_content": 1 })] {
            assert!(check_trace_settings(bad.as_object().unwrap()).is_err(), "{bad}");
        }
    }

    #[tokio::test]
    async fn a_deleted_project_is_closed_and_not_reopened() {
        let c = Arc::new(Control::memory(Default::default(), Default::default()).unwrap());
        let mut cfg = onto_intelli::Config::in_memory();
        cfg.engine.tenancy = "multi".into();
        let hub = Hub::open(cfg).await.unwrap();
        install(&c, &hub, false).await;
        let u = c
            .store()
            .upsert_login(crate::LoginUpsert { provider: "t".into(), subject: "u".into(), ..Default::default() })
            .await
            .unwrap();
        let org = c.store().create_org("Acme", "acme", &u.id).await.unwrap();
        let p = c.store().create_project(&org.id, "p", &u.id).await.unwrap();
        hub.tenant(&p.r#ref).await.unwrap();
        hub.tenant("config-tenant").await.expect("a tenant no project has opens with the defaults");
        assert!(c.poll_settings(&hub).await.unwrap().is_empty());
        // Deleted through another replica: this one closes it on the next poll…
        c.store().delete_project(&p.r#ref, &u.id).await.unwrap();
        c.poll_settings(&hub).await.unwrap();
        assert_eq!(hub.open_tenants(), vec!["config-tenant".to_string()]);
        // …and refuses to open it again.
        let e = hub.tenant(&p.r#ref).await.err().expect("a deleted project's tenant opened");
        assert!(matches!(e, onto_core::Error::NotFound(_)), "{e}");
        assert!(!hub.open_tenants().contains(&p.r#ref));
    }

    #[tokio::test]
    async fn the_meter_counts_flushes_and_reloads() {
        let store = MemoryControl::new();
        let u = store
            .upsert_login(crate::LoginUpsert { provider: "t".into(), subject: "u".into(), ..Default::default() })
            .await
            .unwrap();
        let org = store.create_org("Acme", "acme", &u.id).await.unwrap();
        let p = store.create_project(&org.id, "p", &u.id).await.unwrap();
        let m = Meter::new(&p.id);
        m.load(&store).await.unwrap();
        let usage = TokenUsage { prompt_tokens: 100, completion_tokens: 20, cost_usd: 0.5, ..Default::default() };
        m.record(&usage, "openai", "gpt", false, false);
        m.record(&TokenUsage::default(), "openai", "gpt", true, false);
        m.record(&TokenUsage::default(), "openai", "gpt", false, true);
        assert_eq!(m.month_to_date(), MonthToDate { cost_usd: 0.5, tokens: 120 });
        assert_eq!(m.flush(&store).await.unwrap(), 1);
        assert_eq!(m.month_to_date(), MonthToDate { cost_usd: 0.5, tokens: 120 }, "written, still counted once");
        let today = fmt_date(now_ms());
        let rows = store.usage(&p.id, &today, &today).await.unwrap();
        assert_eq!(rows.len(), 1);
        let r = &rows[0];
        assert_eq!((r.asks, r.llm_calls, r.deterministic, r.errors), (3, 2, 1, 1));
        assert_eq!((r.input_tokens, r.output_tokens), (100, 20));
        // Another replica's usage arrives on reload.
        store.add_usage(&[UsageDay { cost_usd: 1.0, input_tokens: 5, output_tokens: 0, ..r.clone() }]).await.unwrap();
        m.load(&store).await.unwrap();
        assert_eq!(m.month_to_date(), MonthToDate { cost_usd: 1.5, tokens: 125 });
        assert_eq!(m.flush(&store).await.unwrap(), 0);
    }

    #[test]
    fn sealed_outbound_secrets_move_to_the_new_master_key() {
        use crate::vault::Vault;
        let old = Arc::new(Vault::new(vec![1; 32], None).unwrap());
        let sealed = VaultSecrets::new(old, "p1").seal("webhook-secret", "crm", b"whsec_x").unwrap();
        // Rotating: the new key current, the old one previous.
        let rotating = VaultSecrets::new(Arc::new(Vault::new(vec![2; 32], Some(vec![1; 32])).unwrap()), "p1");
        let moved = rotating.rewrap(&sealed).unwrap().expect("the old key wrapped it");
        assert!(rotating.rewrap(&moved).unwrap().is_none(), "already current");
        // Once the previous key is gone only the rewrapped secret opens.
        let after = VaultSecrets::new(Arc::new(Vault::new(vec![2; 32], None).unwrap()), "p1");
        assert_eq!(after.open("webhook-secret", "crm", &moved).unwrap().expose(), b"whsec_x");
        assert!(after.open("webhook-secret", "crm", &sealed).is_err());
    }
}
