//! Many tenants in one process, per-call principals.

use serde_json::json;

use onto_intelli::{Config, Hub, Principal, seed};

#[tokio::test]
async fn tenants_are_separate_ontologies() {
    let hub = Hub::open(Config::in_memory()).await.unwrap();
    let a = hub.tenant("ws_a").await.unwrap();
    let b = hub.tenant("ws_b").await.unwrap();
    seed::load_demo(&a, "acme").await.unwrap();
    let admin = Principal::admin("x");
    assert!(a.call(&admin, "entity.get", json!({ "id": "project:alpha" })).await.is_ok());
    assert!(b.call(&admin, "entity.get", json!({ "id": "project:alpha" })).await.is_err());
    // Schema changes are per tenant too.
    a.call(&admin, "schema.apply", json!({ "changes": [ { "op": "put_object_type", "name": "runbook", "def": {} } ] }))
        .await
        .unwrap();
    assert_eq!(a.call(&admin, "schema.status", json!({})).await.unwrap()["version"], 2);
    assert_eq!(b.call(&admin, "schema.status", json!({})).await.unwrap()["version"], 1);
    // The same handle comes back; ids are validated.
    assert!(std::sync::Arc::ptr_eq(&a, &hub.tenant("ws_a").await.unwrap()));
    assert!(hub.tenant("../etc").await.is_err());
    assert_eq!(hub.open_tenants(), vec!["ws_a", "ws_b"]);
    // Single-tenant hubs refuse other tenants.
    assert!(hub.resolve(Some("ws_c")).await.is_err());
}

#[tokio::test]
async fn per_call_principals_apply_roles_and_acls() {
    let hub = Hub::open(Config::in_memory()).await.unwrap();
    let ws = hub.tenant("ws_1").await.unwrap();
    seed::load_demo(&ws, "acme").await.unwrap();
    let admin = Principal::admin("x");
    ws.call(
        &admin,
        "entity.upsert",
        json!({ "id": "knowledge:bands", "title": "Salary bands",
        "props": { "visibility": "restricted", "acl": ["team:payments"] } }),
    )
    .await
    .unwrap();
    let kenji = Principal::named("user:kenji", &["member"], &["*"]).unwrap().with_entity("person:kenji");
    let eve = Principal::named("user:eve", &["member", "approver"], &["*"]).unwrap().with_entity("person:eve");
    assert!(ws.call(&kenji, "entity.get", json!({ "id": "knowledge:bands" })).await.is_ok());
    assert!(ws.call(&eve, "entity.get", json!({ "id": "knowledge:bands" })).await.is_err());
    let viewer = Principal::named("user:v", &["viewer"], &["*"]).unwrap();
    assert!(ws.call(&viewer, "entity.upsert", json!({ "id": "task:z", "title": "z" })).await.is_err());
}

#[tokio::test]
async fn every_listed_action_is_dispatched() {
    let hub = Hub::open(Config::in_memory()).await.unwrap();
    let i = hub.tenant("t").await.unwrap();
    let admin = Principal::admin("x");
    for a in onto_intelli::actions::ACTIONS {
        if matches!(a.name, "ingest.flush" | "eval.run" | "intelli.ask" | "memory.consolidate") {
            continue;
        }
        if let Err(e) = i.call(&admin, a.name, json!({})).await {
            assert!(!e.to_string().contains("see system.actions"), "{} is listed but not dispatched", a.name);
        }
    }
}

mod hooks {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use async_trait::async_trait;
    use parking_lot::Mutex;
    use serde_json::json;

    use onto_intelli::hub::{TenantHooks, TenantSettings};
    use onto_intelli::limits::{Limits, MonthToDate, UsageMeter};
    use onto_intelli::llm::{LlmProvider, LlmRequest, LlmResponse};
    use onto_intelli::token::TokenUsage;
    use onto_intelli::{Config, Error, Hub, Principal, Result, seed};

    /// A provider that counts its calls.
    #[derive(Default)]
    struct Counting(AtomicUsize);

    #[async_trait]
    impl LlmProvider for Counting {
        fn name(&self) -> &str {
            "counting"
        }
        fn model(&self) -> &str {
            "c-1"
        }
        async fn complete(&self, _req: &LlmRequest) -> Result<LlmResponse> {
            self.0.fetch_add(1, Ordering::SeqCst);
            let usage = TokenUsage { prompt_tokens: 10, completion_tokens: 2, cost_usd: 3.0, ..Default::default() };
            Ok(LlmResponse { text: "counted".into(), usage, model: "c-1".into(), finish_reason: None })
        }
    }

    /// (provider, deterministic, error) of each record.
    type Records = Vec<(String, bool, bool)>;

    #[derive(Default)]
    struct Meter(Mutex<(MonthToDate, Records)>);

    impl UsageMeter for Meter {
        fn month_to_date(&self) -> MonthToDate {
            self.0.lock().0
        }
        fn record(&self, usage: &TokenUsage, provider: &str, _model: &str, deterministic: bool, error: bool) {
            let mut s = self.0.lock();
            s.0.cost_usd += usage.cost_usd;
            s.0.tokens += usage.total();
            s.1.push((provider.to_string(), deterministic, error));
        }
    }

    struct Hooks {
        settings: Mutex<TenantSettings>,
    }

    #[async_trait]
    impl TenantHooks for Hooks {
        async fn settings(&self, _tenant: &str) -> Result<TenantSettings> {
            Ok(self.settings.lock().clone())
        }
    }

    #[tokio::test]
    async fn tenant_hooks_set_provider_limits_and_meter() {
        let hub = Hub::open(Config::in_memory()).await.unwrap();
        let provider = Arc::new(Counting::default());
        let meter = Arc::new(Meter::default());
        let hooks = Arc::new(Hooks { settings: Mutex::new(TenantSettings::default()) });
        hub.set_hooks(hooks.clone());
        let t = hub.tenant("t1").await.unwrap();
        seed::load_demo(&t, "default").await.unwrap();
        let p = Principal::admin("x");
        let q = json!({ "question": "Why is Project Alpha blocked?", "mode": "llm", "no_cache": true });
        assert_eq!(t.call(&p, "intelli.ask", q.clone()).await.unwrap()["provider"], "mock");

        // The project's provider, a budget of $5 and two asks a minute, after a refresh.
        *hooks.settings.lock() = TenantSettings {
            llm: Some(provider.clone()),
            limits: Limits { asks_per_minute: Some(2), monthly_cost_usd: Some(5.0), monthly_tokens: None },
            meter: Some(meter.clone()),
            version: 2,
            traces: onto_intelli::trace::TraceSettings { retention_days: Some(3), capture_content: Some(false) },
        };
        assert_eq!(t.call(&p, "intelli.ask", q.clone()).await.unwrap()["provider"], "mock", "until refreshed");
        assert!((t.trace_retention_days(), t.captures_content()) == (30, true));
        assert!(hub.refresh_tenant("t1").await.unwrap());
        assert_eq!((t.trace_retention_days(), t.captures_content()), (3, false), "the project's trace settings");
        let a = t.call(&p, "intelli.ask", q.clone()).await.unwrap();
        assert_eq!((a["provider"].as_str(), a["answer"].as_str()), (Some("counting"), Some("counted")));
        assert_eq!(provider.0.load(Ordering::SeqCst), 1);
        // $3 of $5 spent: one more call, then the budget holds.
        t.set_limits(Limits { asks_per_minute: None, ..t.limits() });
        t.call(&p, "intelli.ask", q.clone()).await.unwrap();
        let a = t.call(&p, "intelli.ask", q.clone()).await.unwrap();
        assert_eq!(
            (a["mode"].as_str(), a["note"].as_str()),
            (Some("fallback"), Some("monthly LLM budget of $5.00 reached"))
        );
        assert_eq!(provider.0.load(Ordering::SeqCst), 2, "no provider call over budget");
        assert_eq!(meter.0.lock().1.len(), 3);
        // The rate limit: two a minute.
        t.set_limits(Limits { asks_per_minute: Some(2), ..Default::default() });
        t.call(&p, "intelli.ask", q.clone()).await.unwrap();
        t.call(&p, "intelli.ask", q.clone()).await.unwrap();
        let err = t.call(&p, "intelli.ask", q.clone()).await.unwrap_err();
        assert!(matches!(err, Error::Limited(_)), "{err}");
        assert_eq!(err.code(), "resource_exhausted");
        let mut rx = t.ask_stream(p.clone(), serde_json::from_value(q.clone()).unwrap());
        let ev = rx.recv().await.unwrap();
        assert_eq!((ev.kind.as_str(), ev.data["code"].as_str()), ("error", Some("resource_exhausted")));
        // A new tenant opens with the hooks' settings.
        let t2 = hub.tenant("t2").await.unwrap();
        assert_eq!(t2.llm().name(), "counting");
        assert!(!hub.refresh_tenant("never-opened").await.unwrap());
    }
}

#[tokio::test]
async fn agents_neither_write_raw_nor_confirm_their_own_risky_actions() {
    let hub = Hub::open(Config::in_memory()).await.unwrap();
    let ws = hub.tenant("t").await.unwrap();
    seed::load_demo(&ws, "acme").await.unwrap();
    let agent = Principal::named("agent:bot", &["member", "agent"], &["*"]).unwrap();
    let raw = [
        ("entity.upsert", json!({ "id": "task:z", "title": "z" })),
        ("entity.patch", json!({ "id": "project:alpha", "title": "renamed" })),
        ("entity.delete", json!({ "id": "project:alpha" })),
        ("link.create", json!({ "from": "project:alpha", "rel": "owned_by", "to": "team:platform" })),
        ("graph.apply", json!({ "mutations": [{ "op": "delete", "id": "project:alpha" }] })),
        ("ingest.batch", json!({ "records": [{ "kind": "task", "key": "q", "title": "q" }] })),
        ("ingest.sheet", json!({ "csv": "id,title\nz,z\n", "kind": "task", "key": "id" })),
    ];
    for (action, input) in raw {
        let err = ws.call(&agent, action, input).await.unwrap_err();
        assert!(matches!(err, onto_intelli::Error::Denied(ref m) if m.contains("agent")), "{action}: {err}");
    }
    // Write actions not on the agent list are closed to agents too.
    for action in ["execution.init_graph", "connector.remove", "catalog.rerank", "learned.certify"] {
        let err = ws.call(&agent, action, json!({})).await.unwrap_err();
        assert!(
            matches!(err, onto_intelli::Error::Denied(ref m) if m.contains("not open to agents")),
            "{action}: {err}"
        );
    }
    let admin = Principal::admin("x");
    assert!(ws.call(&admin, "entity.get", json!({ "id": "project:alpha" })).await.is_ok());

    // A high-risk action type: an agent's confirm does not count.
    let delete = json!({ "action": "delete", "params": { "target": "project:alpha" }, "confirm": true });
    let r = ws.call(&agent, "action.apply", delete.clone()).await.unwrap();
    assert_eq!(r["status"], "proposed", "{r}");
    assert!(r["reason"].as_str().unwrap().contains("cannot confirm"), "{r}");
    assert!(ws.call(&admin, "entity.get", json!({ "id": "project:alpha" })).await.is_ok(), "not deleted");
    // A person confirms it.
    let person = Principal::named("user:m", &["member"], &["*"]).unwrap();
    assert_eq!(ws.call(&person, "action.apply", delete).await.unwrap()["status"], "applied");
    // Low-risk action types still work for agents.
    let r = ws
        .call(
            &agent,
            "action.apply",
            json!({ "action": "set_state", "params": { "item": "issue:421", "state": "in_progress" } }),
        )
        .await
        .unwrap();
    assert_eq!(r["status"], "applied", "{r}");

    // governance.agent_raw_writes = true opts back in.
    let mut cfg = Config::in_memory();
    cfg.governance.agent_raw_writes = true;
    let hub = Hub::open(cfg).await.unwrap();
    let ws = hub.tenant("t").await.unwrap();
    ws.call(&agent, "entity.upsert", json!({ "id": "task:z", "title": "z" })).await.unwrap();
}

#[test]
fn every_write_action_is_classified_for_agents() {
    use onto_intelli::actions::{ACTIONS, AGENT_WRITES, RAW_WRITES};
    for name in AGENT_WRITES.iter().chain(RAW_WRITES) {
        assert!(ACTIONS.iter().any(|a| a.name == *name && a.write), "{name} is not a write action");
        assert!(!(AGENT_WRITES.contains(name) && RAW_WRITES.contains(name)), "{name} is on both lists");
    }
}
