//! Automation definitions through the action surface (docs/design/EVENTS.md
//! Part C, build step 10): `put_automation` / `delete_automation` in
//! `schema.plan|apply|propose`, `[automation] max_automations` enforced at
//! apply and propose, and the automation lint checks in `schema.lint`.

use serde_json::{Value, json};

use onto_intelli::{Config, Intelli, Principal, Role};

fn put(name: &str) -> Value {
    json!({
        "op": "put_automation",
        "name": name,
        "def": {
            "description": "Resolve a blocked issue.",
            "run_as": { "roles": ["member"], "workspaces": ["acme"] },
            "trigger": { "on": "change", "kinds": ["issue"], "ops": ["entity.update"], "state": { "to": ["blocked"] } },
            "effects": [{ "action": { "type": "resolve", "params": { "issue": "{trigger.id}" } } }]
        }
    })
}

#[tokio::test]
async fn definitions_go_through_schema_actions_with_the_configured_limit() {
    let mut cfg = Config::in_memory();
    cfg.engine.default_workspace = "acme".into();
    cfg.automation.max_automations = 1;
    let i = Intelli::open(cfg).await.unwrap();
    let admin = Principal::admin("ada");
    let agent = Principal::new("assistant", vec![Role::Agent], vec!["*".into()]);

    let plan = i.call(&admin, "schema.plan", json!({ "changes": [put("first")] })).await.unwrap();
    assert_eq!(plan["valid"], true, "{plan}");
    assert_eq!(plan["changes"][0]["op"], "put_automation");
    i.call(&admin, "schema.apply", json!({ "changes": [put("first")] })).await.unwrap();
    let status = i.call(&admin, "schema.status", json!({})).await.unwrap();
    assert_eq!(status["automations"], 1);
    let doc = i.call(&admin, "schema.get", json!({})).await.unwrap();
    assert_eq!(doc["automations"]["first"]["trigger"]["on"], "change", "{doc}");

    // The process limit: refused at plan, apply and propose, with the setting named.
    let plan = i.call(&admin, "schema.plan", json!({ "changes": [put("second")] })).await.unwrap();
    assert_eq!(plan["valid"], false);
    assert!(plan["errors"].to_string().contains("max_automations"), "{plan}");
    let err = i.call(&admin, "schema.apply", json!({ "changes": [put("second")] })).await.unwrap_err();
    assert_eq!(err.code(), "invalid_argument");
    assert!(err.to_string().contains("the limit is 1 ([automation] max_automations)"), "{err}");
    let err =
        i.call(&agent, "schema.propose", json!({ "title": "Another", "changes": [put("second")] })).await.unwrap_err();
    assert!(err.to_string().contains("max_automations"), "{err}");
    // Agents propose definitions; they never apply them.
    let err = i.call(&agent, "schema.apply", json!({ "changes": [{ "op": "delete_automation", "name": "first" }] }));
    assert_eq!(err.await.unwrap_err().code(), "permission_denied");
    let filed = i
        .call(
            &agent,
            "schema.propose",
            json!({ "title": "Retire", "changes": [{ "op": "delete_automation", "name": "first" }] }),
        )
        .await
        .unwrap();
    assert_eq!(filed["status"], "open", "{filed}");

    // Lint lists the automation checks; the closure check reads the stored
    // enablements (none here, so it finds nothing).
    let lint = i.call(&admin, "schema.lint", json!({})).await.unwrap();
    let run: Vec<&str> = lint["checks_run"].as_array().unwrap().iter().filter_map(Value::as_str).collect();
    for check in [
        "automation_reference",
        "automation_privilege",
        "automation_loop",
        "automation_closure_changed",
        "automation_gap_blind",
    ] {
        assert!(run.contains(&check), "{check}: {run:?}");
    }
    let gap: Vec<&Value> =
        lint["findings"].as_array().unwrap().iter().filter(|f| f["check"] == "automation_gap_blind").collect();
    assert_eq!(gap.len(), 1);
    assert_eq!(gap[0]["target"], "automation:first");
}
