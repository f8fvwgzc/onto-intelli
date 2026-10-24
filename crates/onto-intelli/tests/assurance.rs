//! The assurance actions on the demo data: the reasoner and its certificates,
//! explanations, shapes, lint, locks, packs, alignment and sheet induction,
//! with the role matrix of the writing ones.

use std::sync::Arc;

use serde_json::{Value, json};

use onto_intelli::{Config, Intelli, Principal, Role, seed};

async fn demo() -> Arc<Intelli> {
    let mut cfg = Config::in_memory();
    cfg.engine.default_workspace = "acme".into();
    let intelli = Intelli::open(cfg).await.unwrap();
    seed::load_demo(&intelli, "acme").await.unwrap();
    intelli
}

fn admin() -> Principal {
    Principal::admin("lead")
}

fn viewer() -> Principal {
    Principal::named("user:v", &["viewer"], &["*"]).unwrap()
}

fn agent() -> Principal {
    Principal::new("bot", vec![Role::Agent], vec!["*".into()])
}

fn member() -> Principal {
    Principal::named("user:m", &["member"], &["*"]).unwrap()
}

async fn ok(i: &Intelli, p: &Principal, action: &str, input: Value) -> Value {
    match i.call(p, action, input).await {
        Ok(v) => v,
        Err(e) => panic!("{action}: {e}"),
    }
}

async fn err(i: &Intelli, p: &Principal, action: &str, input: Value) -> onto_intelli::Error {
    match i.call(p, action, input).await {
        Ok(v) => panic!("{action} should fail: {v}"),
        Err(e) => e,
    }
}

#[tokio::test]
async fn reasoner_runs_queries_and_explains() {
    let i = demo().await;
    let run = ok(&i, &admin(), "reason.run", json!({ "samples": 3 })).await;
    assert!(run["asserted"].as_u64().unwrap() > 0, "{run}");
    assert!(run["derived"].as_u64().unwrap() > 0, "{run}");
    assert_eq!(run["fixpoint_reached"], true);
    assert!(run["by_rule"]["schema/responsible_team"].as_u64().unwrap() > 0, "{run}");
    assert!(!run["not_evaluated"].as_array().unwrap().is_empty(), "silence is never read as valid");
    let samples = run["samples"].as_array().unwrap();
    assert_eq!(samples.len(), 3);
    assert!(samples[0]["rule"].is_string() && samples[0]["premises"].is_array(), "{run}");
    assert!(run["means"].as_str().unwrap().contains("not a consistency result"));

    let q = ok(
        &i,
        &admin(),
        "reason.query",
        json!({ "pattern": [["?i", "handled_by_team", "team:payments"]], "limit": 100 }),
    )
    .await;
    let found: Vec<&str> = q["bindings"].as_array().unwrap().iter().filter_map(|b| b["i"].as_str()).collect();
    assert!(found.contains(&"<issue:421>"), "{q}");
    assert_eq!(q["fixpoint_reached"], true);
    assert!(err(&i, &admin(), "reason.query", json!({ "pattern": [] })).await.to_string().contains("atom"));

    let ex =
        ok(&i, &admin(), "reason.explain", json!({ "fact": ["issue:421", "handled_by_team", "team:payments"] })).await;
    assert_eq!(ex["status"], "derived", "{ex}");
    let j = &ex["justifications"][0];
    assert!(j["rules"].as_array().unwrap().iter().any(|r| r == "schema/responsible_team"), "{ex}");
    assert_eq!(j["sufficient"], true);
    let as_claim = ok(
        &i,
        &admin(),
        "reason.explain",
        json!({ "fact": { "subject": "issue:421", "relation": "handled_by_team", "object": "team:payments" }, "max": 1 }),
    )
    .await;
    assert_eq!(as_claim["justifications"].as_array().unwrap().len(), 1);
    assert!(err(&i, &admin(), "reason.explain", json!({ "fact": ["a", "b"] })).await.to_string().contains("3 terms"));

    let prov = ok(
        &i,
        &admin(),
        "reason.provenance",
        json!({ "fact": ["issue:421", "handled_by_team", "team:payments"], "semiring": "count" }),
    )
    .await;
    assert_eq!(prov["semiring"], "count");
    assert!(prov["value"].as_u64().unwrap() >= 1, "{prov}");

    let v = ok(
        &i,
        &admin(),
        "reason.verify",
        json!({ "claims": [
            { "subject": "issue:421", "relation": "handled_by_team", "object": "team:payments" },
            { "subject": "issue:421", "relation": "handled_by_team", "object": "team:nobody" }
        ] }),
    )
    .await;
    let verdicts = v["verdicts"].as_array().unwrap();
    assert_eq!(verdicts[0]["verdict"], "derived", "{v}");
    assert_ne!(verdicts[1]["verdict"], "derived", "{v}");

    let goal = json!([{ "subject": "issue:421", "relation": "handled_by_team", "object": "team:payments" }]);
    let slice = ok(&i, &admin(), "reason.slice_check", json!({ "entities": ["issue:421"], "goals": goal })).await;
    assert_eq!(slice["slice"], 1);
    assert_eq!(slice["goals"][0]["status"], "lost", "the issue alone does not carry its team: {slice}");
    assert!(!slice["goals"][0]["missing_entities"].as_array().unwrap().is_empty(), "{slice}");
    assert!(slice["means"].as_str().unwrap().contains("preservation"));
}

#[tokio::test]
async fn certificates_are_checked_by_the_independent_checker() {
    let i = demo().await;
    let r = ok(&i, &admin(), "reason.certify", json!({})).await;
    let cert = &r["certificate"];
    for f in ["rules_tsv", "asserted_tsv", "derivations_tsv", "manifest_json"] {
        assert!(cert[f].as_str().is_some_and(|t| !t.is_empty()), "{f} missing");
    }
    let check = &r["check"];
    assert_eq!(check["ok"], true, "{check}");
    // The enterprise schema derives through supplied chain rules.
    assert_eq!(check["verdict"], "entailed_under_supplied_rules", "{check}");
    assert!(check["checker"].as_str().unwrap().starts_with("onto-cert"));
    // Only the checker's report carries checker words.
    let engine_side = serde_json::to_string(cert).unwrap();
    assert!(!engine_side.contains("entailed"), "the engine never says entailed");

    let files = json!({
        "rules_tsv": cert["rules_tsv"],
        "asserted_tsv": cert["asserted_tsv"],
        "derivations_tsv": cert["derivations_tsv"],
        "manifest_json": cert["manifest_json"],
    });
    let again = ok(&i, &viewer(), "reason.check_certificate", files.clone()).await;
    assert_eq!(again, *check, "the same files, the same report");

    // Forge the first derivation's conclusion: the checker refuses it, naming the line.
    let derivations = cert["derivations_tsv"].as_str().unwrap();
    let mut lines: Vec<String> = derivations.lines().map(str::to_string).collect();
    let mut cols: Vec<String> = lines[0].split('\t').map(str::to_string).collect();
    cols[3] = "<team:forged>".into();
    lines[0] = cols.join("\t");
    let forged = lines.join("\n") + "\n";
    let mut bad = files.clone();
    bad["derivations_tsv"] = json!(forged);
    bad["manifest_json"] = json!("");
    let refused = ok(&i, &admin(), "reason.check_certificate", bad).await;
    assert_eq!(refused["ok"], false, "{refused}");
    assert_eq!(refused["first_rejected"], 1, "{refused}");
    assert!(refused["reason"].is_string(), "{refused}");
    // With the manifest the digest no longer matches.
    let mut bad = files.clone();
    bad["derivations_tsv"] = json!(forged);
    let digest = ok(&i, &admin(), "reason.check_certificate", bad).await;
    assert_eq!(digest["ok"], false, "{digest}");

    let binding = ok(
        &i,
        &admin(),
        "reason.binding",
        json!({ "asserted_tsv": cert["asserted_tsv"], "manifest_json": cert["manifest_json"] }),
    )
    .await;
    assert_eq!(binding["verdict"], "same_graph", "{binding}");
    assert_eq!(binding["manifest_digest_matches"], true, "{binding}");
    ok(&i, &admin(), "entity.upsert", json!({ "id": "task:cert1", "title": "New work" })).await;
    let moved = ok(&i, &admin(), "reason.binding", json!({ "asserted_tsv": cert["asserted_tsv"] })).await;
    assert_eq!(moved["verdict"], "different_graph", "{moved}");
    assert_eq!(moved["only_in_store"], 1, "{moved}");
    assert!(moved["means"].as_str().unwrap().contains("bytes"), "{moved}");
}

#[tokio::test]
async fn shapes_lint_and_packs_report_on_the_demo() {
    let i = demo().await;
    let report = ok(&i, &admin(), "shapes.validate", json!({})).await;
    assert!(["conforms", "violates", "undetermined"].contains(&report["verdict"].as_str().unwrap()), "{report}");
    assert!(report["means"].as_str().unwrap().contains("measurement"), "{report}");
    let unknown = err(&i, &admin(), "shapes.validate", json!({ "shapes": ["nope"] })).await;
    assert!(unknown.to_string().contains("nope"));

    let induced = ok(&i, &admin(), "shapes.induce", json!({ "kind": "issue" })).await;
    assert_eq!(induced["kind"], "issue");
    assert_eq!(induced["change_set"]["changes"][0]["op"], "put_shape", "{induced}");
    let shapes = json!({ induced["name"].as_str().unwrap(): induced["shape"] });
    let check = ok(&i, &admin(), "shapes.check", json!({ "shapes": shapes })).await;
    assert_eq!(check["ok"], true, "an induced shape passes the structural check: {check}");
    let broken = ok(
        &i,
        &admin(),
        "shapes.check",
        json!({ "shapes": { "s": { "target": "no_such_kind", "properties": [{ "path": "title", "min_count": 2, "max_count": 1 }] } } }),
    )
    .await;
    assert_eq!(broken["ok"], false, "{broken}");

    let lint = ok(&i, &admin(), "schema.lint", json!({})).await;
    assert!(lint["means"].as_str().unwrap().contains("measurement"));
    assert!(!lint["checks_run"].as_array().unwrap().is_empty());

    let packs = ok(&i, &viewer(), "pack.list", json!({})).await;
    let items = packs["items"].as_array().unwrap();
    assert!(items.iter().any(|p| p["id"] == "manufacturing"), "{packs}");
    assert!(items.iter().all(|p| p["installed"] == false));
    let shown = ok(&i, &viewer(), "pack.show", json!({ "id": "manufacturing" })).await;
    assert!(!shown["object_types"].as_array().unwrap().is_empty());
    assert!(shown["lint"]["counts"].is_object(), "{}", shown["lint"]);
    let plan = ok(&i, &viewer(), "pack.plan", json!({ "id": "manufacturing" })).await;
    assert_eq!(plan["valid"], true, "{plan}");
    assert!(plan["consequences"].is_object() && plan["risk"].is_string(), "{plan}");
    assert!(plan["shapes"]["means"].is_string(), "the plan carries a shape delta: {plan}");
    assert!(
        err(&i, &viewer(), "pack.show", json!({ "id": "no-such-pack" })).await.to_string().contains("no-such-pack")
    );

    let align = ok(&i, &admin(), "schema.align", json!({ "pack": "manufacturing", "threshold": 0.4 })).await;
    assert!(align["sssom_tsv"].as_str().unwrap().contains("subject_id"), "{align}");
    assert!(align["change_set"].is_object());
    assert_eq!(align["thresholds"]["min_score"], 0.4, "{}", align["thresholds"]);
    assert!(err(&i, &admin(), "schema.align", json!({})).await.to_string().contains("pack"));
    let certified = ok(
        &i,
        &admin(),
        "schema.align_certify",
        json!({ "pack": "manufacturing", "mappings": [{ "source": "work_order", "target": "task", "predicate": "close_match" }] }),
    )
    .await;
    assert_eq!(certified["items"].as_array().unwrap().len(), 1, "{certified}");
    assert!(certified["items"][0]["certification"].is_string());
}

#[tokio::test]
async fn sheets_induce_and_ingest() {
    let i = demo().await;
    let csv = "sku,name,weight\nA1,Bolt,1.5\nA2,Nut,0.5\n";
    let start = i.ontology().revision();
    let induced = ok(&i, &admin(), "model.induce", json!({ "csv": csv, "kind": "widget", "key": "sku" })).await;
    assert_eq!(induced["kind"], "widget");
    assert_eq!(induced["rows"], 2);
    assert!(induced["means"].is_string());
    assert_eq!(i.ontology().revision(), start, "induction writes nothing");
    // Without a mapping the sheet is induced, and a new type must be applied first.
    let new_kind = err(&i, &admin(), "ingest.sheet", json!({ "csv": csv, "kind": "widget", "key": "sku" })).await;
    let msg = new_kind.to_string();
    assert!(msg.contains("change_set") && msg.contains("widget,"), "{msg}");
    assert!(!msg.contains("widget_2"), "the base kind is named, not a suffixed one: {msg}");

    ok(&i, &admin(), "schema.apply", induced["change_set"].clone()).await;
    let before = i.ontology().revision();
    let mapping = induced["mapping"].clone();
    let dry = ok(&i, &admin(), "ingest.sheet", json!({ "csv": csv, "mapping": mapping, "dry_run": true })).await;
    assert_eq!(dry["dry_run"], true, "{dry}");
    assert_eq!(dry["mapping_induced"], false);
    assert_eq!(dry["upserts"], 2, "{dry}");
    assert_eq!(i.ontology().revision(), before, "a dry run writes nothing");
    let denied = err(&i, &viewer(), "ingest.sheet", json!({ "csv": csv, "mapping": mapping })).await;
    assert_eq!(denied.code(), "permission_denied");
    let done = ok(&i, &member(), "ingest.sheet", json!({ "csv": csv, "mapping": mapping })).await;
    assert_eq!(done["upserts"], 2, "{done}");
    assert!(i.ontology().revision() > before);

    // Once the type exists, a mapping-less ingest goes into it (induction
    // alone would name widget_2), column by column.
    let more = "sku,name,weight\nA3,Washer,0.2\n";
    let into = ok(&i, &member(), "ingest.sheet", json!({ "csv": more, "kind": "widget", "key": "sku" })).await;
    assert_eq!(into["mapping_induced"], true, "{into}");
    assert_eq!(into["mapping"]["kind"], "widget", "{into}");
    assert_eq!(into["mapping"]["key"], "sku", "{into}");
    assert_eq!(into["upserts"], 1, "{into}");
    assert!(!i.ontology().schema().object_types.contains_key("widget_2"), "no duplicate type");
    for id in ["widget:A1", "widget:A3"] {
        let e = i.ontology().get(&admin(), id).unwrap_or_else(|e| panic!("{id}: {e}"));
        assert_eq!(e.kind, "widget");
    }
    // The same for a seeded type (induction alone would name task_2).
    let tasks = json!({ "csv": "id,title\nzz1,Hello\n", "kind": "task", "dry_run": true });
    let task = ok(&i, &admin(), "ingest.sheet", tasks).await;
    assert_eq!(task["mapping"]["kind"], "task", "{task}");
    assert_eq!(task["dry_run"], true, "{task}");
    // An existing type that lacks a column is refused, naming the column.
    let extra = "sku,name,colour\nA4,Pin,red\n";
    let refused = err(&i, &admin(), "ingest.sheet", json!({ "csv": extra, "kind": "widget", "key": "sku" })).await;
    assert!(refused.to_string().contains("colour"), "{refused}");
}

#[tokio::test]
async fn locks_and_lint_feedback_follow_the_role_matrix() {
    let i = demo().await;
    let lock = json!({ "section": "object_types", "name": "issue", "reason": "core type" });
    assert_eq!(err(&i, &viewer(), "schema.lock", lock.clone()).await.code(), "permission_denied");
    assert_eq!(err(&i, &agent(), "schema.lock", lock.clone()).await.code(), "permission_denied");
    assert_eq!(err(&i, &member(), "schema.lock", lock.clone()).await.code(), "permission_denied");
    let l = ok(&i, &admin(), "schema.lock", lock).await;
    assert_eq!(l["locked_by"], "lead");
    let locks = ok(&i, &viewer(), "schema.locks", json!({})).await;
    assert_eq!(locks["items"].as_array().unwrap().len(), 1, "{locks}");
    let plan =
        ok(&i, &admin(), "schema.plan", json!({ "changes": [{ "op": "delete_object_type", "name": "issue" }] })).await;
    let v = &plan["locked_violations"][0];
    assert_eq!(v["name"], "issue", "{plan}");
    assert_eq!(v["overridden"], false);
    assert_eq!(plan["risk"], "high");
    let unlock = json!({ "section": "object_types", "name": "issue" });
    assert_eq!(err(&i, &agent(), "schema.unlock", unlock.clone()).await.code(), "permission_denied");
    assert_eq!(ok(&i, &admin(), "schema.unlock", unlock.clone()).await["removed"], true);
    assert_eq!(ok(&i, &admin(), "schema.unlock", unlock).await["removed"], false, "nothing held: a no-op");

    let lint = ok(&i, &admin(), "schema.lint", json!({})).await;
    if let Some(f) = lint["findings"].as_array().and_then(|f| f.first()) {
        let id = f["id"].as_str().unwrap();
        let fb = json!({ "id": id, "decision": "dismiss", "note": "by design" });
        assert_eq!(err(&i, &viewer(), "schema.lint_feedback", fb.clone()).await.code(), "permission_denied");
        let stored = ok(&i, &member(), "schema.lint_feedback", fb).await;
        assert_eq!(stored["decision"], "dismiss");
        let after = ok(&i, &admin(), "schema.lint", json!({})).await;
        assert_eq!(after["suppressed"], 1, "{after}");
        let all = ok(&i, &admin(), "schema.lint", json!({ "include_dismissed": true })).await;
        assert!(all["findings"].as_array().unwrap().iter().any(|f| f["id"] == id && f["dismissed"] == true));
    }
}

#[tokio::test]
async fn packs_install_by_role() {
    let i = demo().await;
    let denied = err(&i, &viewer(), "pack.install", json!({ "id": "manufacturing" })).await;
    assert_eq!(denied.code(), "permission_denied", "{denied}");
    let proposed = ok(&i, &agent(), "pack.install", json!({ "id": "manufacturing", "mode": "apply" })).await;
    assert_eq!(proposed["mode"], "propose", "agents propose: {proposed}");
    assert!(proposed["proposal"]["id"].is_string(), "{proposed}");
    assert!(proposed["note"].as_str().unwrap().contains("proposal"));
    let packs = ok(&i, &admin(), "pack.list", json!({})).await;
    let m = packs["items"].as_array().unwrap().iter().find(|p| p["id"] == "manufacturing").unwrap().clone();
    assert_eq!(m["installed"], false, "a proposal changes nothing");

    let done = ok(&i, &admin(), "pack.install", json!({ "id": "manufacturing", "sample": true })).await;
    assert_eq!(done["mode"], "apply", "{done}");
    assert!(done["version"].is_object(), "{done}");
    assert!(done["sample_entities"].as_u64().unwrap() > 0, "{done}");
    let packs = ok(&i, &admin(), "pack.list", json!({})).await;
    let m = packs["items"].as_array().unwrap().iter().find(|p| p["id"] == "manufacturing").unwrap().clone();
    assert_eq!(m["installed"], true);
    let again = ok(&i, &admin(), "pack.install", json!({ "id": "manufacturing" })).await;
    assert_eq!(again["already_installed"], true);
    // The pack's supplied rules and sample data still certify.
    let r = ok(&i, &admin(), "reason.certify", json!({})).await;
    assert_eq!(r["check"]["ok"], true, "{}", r["check"]);
}

/// `put_link_type depends_on` with its current definition, made transitive.
async fn transitive_depends_on(i: &Intelli) -> Value {
    let schema = ok(i, &admin(), "schema.get", json!({})).await;
    let mut def = schema["link_types"]["depends_on"].clone();
    assert!(def.is_object(), "depends_on in the demo schema");
    def["transitive"] = json!(true);
    json!({ "changes": [{ "op": "put_link_type", "name": "depends_on", "def": def }] })
}

#[tokio::test]
async fn a_transitive_flip_plans_consequences_and_risk() {
    let i = demo().await;
    // A chain a -> b -> c so the flip derives something.
    for (id, title) in [("task:a1", "A"), ("task:b1", "B"), ("task:c1", "C")] {
        ok(&i, &admin(), "entity.upsert", json!({ "id": id, "title": title })).await;
    }
    ok(&i, &admin(), "link.create", json!({ "from": "task:a1", "rel": "depends_on", "to": "task:b1" })).await;
    ok(&i, &admin(), "link.create", json!({ "from": "task:b1", "rel": "depends_on", "to": "task:c1" })).await;
    let plan = ok(&i, &admin(), "schema.plan", transitive_depends_on(&i).await).await;
    assert_eq!(plan["valid"], true, "{plan}");
    assert_eq!(plan["entities_affected"], 0, "a flag flip touches no entity");
    let c = &plan["consequences"];
    assert!(c["skipped"].is_null(), "{c}");
    assert!(c["added"].as_u64().unwrap() >= 2, "{c}");
    assert_eq!(c["conservative"], false, "{c}");
    assert!(c["by_rule"]["builtin/transitive"].as_u64().unwrap() >= 1, "{c}");
    assert_eq!(plan["risk"], "medium", "{}", plan["risk_reasons"]);
    assert!(plan["shapes"].is_object(), "{plan}");
}

#[tokio::test]
async fn action_plans_report_consequences_on_request() {
    let i = demo().await;
    let actions = ok(&i, &admin(), "action.list", json!({})).await;
    let assign = actions["action_types"].as_array().unwrap().iter().any(|a| a["name"] == "assign");
    assert!(assign, "the demo schema has an assign action");
    let input = json!({ "action": "assign", "params": { "target": "issue:421", "assignee": "person:ada" } });
    let plain = ok(&i, &admin(), "action.plan", input.clone()).await;
    assert!(plain["consequences"].is_null(), "opt-in: {plain}");
    let mut with = input;
    with["consequences"] = json!(true);
    let planned = ok(&i, &admin(), "action.plan", with).await;
    assert!(planned["consequences"]["means"].is_string(), "{planned}");
}

#[tokio::test]
async fn scenarios_reason_over_forks() {
    let i = demo().await;
    let r = ok(
        &i,
        &admin(),
        "scenario.run",
        json!({
            "mutations": [{ "op": "unlink", "from": "issue:421", "rel": "assigned_to", "to": "person:kenji" }],
            "queries": [{ "action": "reason.query", "input": { "pattern": [["issue:421", "handled_by_team", "?t"]] } }]
        }),
    )
    .await;
    assert_eq!(r["results"][0]["action"], "reason.query", "{r}");
    assert!(r["results"][0]["before"]["bindings"].is_array(), "{r}");
}

/// `depends_on` made transitive and a chain of `n` tasks: n(n-1)/2 derived
/// facts from n-1 links.
async fn chain(i: &Intelli, n: usize) {
    use onto_core::{ActionContext, EntityInput, Mutation};
    let flip = transitive_depends_on(i).await;
    ok(i, &admin(), "schema.apply", flip).await;
    let ctx = ActionContext::new(admin(), "acme");
    let muts = (0..n)
        .map(|k| {
            let e = EntityInput::new("task", format!("c{k}"), format!("Chain {k}"));
            Mutation::Upsert(if k > 0 { e.link("depends_on", format!("task:c{}", k - 1)) } else { e })
        })
        .collect();
    i.ontology().apply(&ctx, muts).unwrap();
}

#[tokio::test]
async fn caller_knobs_are_clamped() {
    let i = demo().await;
    chain(&i, 60).await;
    let v = viewer();
    // A caller may lower the derivation bound, never raise it; samples and
    // limits have ceilings.
    let run = ok(&i, &v, "reason.run", json!({ "samples": 1_000_000_000u64, "max_derived": u64::MAX })).await;
    assert!(run["derived"].as_u64().unwrap() > 1000, "{}", run["derived"]);
    assert_eq!(run["samples"].as_array().unwrap().len(), 1000);
    assert_eq!(run["fixpoint_reached"], true);
    let low = ok(&i, &v, "reason.run", json!({ "max_derived": 5 })).await;
    assert_eq!((low["derived"].as_u64(), low["fixpoint_reached"].as_bool()), (Some(5), Some(false)));
    let q = ok(&i, &v, "reason.query", json!({ "pattern": [["?x", "depends_on", "?y"]], "limit": 1_000_000 })).await;
    assert_eq!(q["bindings"].as_array().unwrap().len(), 1000);
    let atoms: Vec<Value> = (0..9).map(|k| json!([format!("?x{k}"), "a", "task"])).collect();
    let e = err(&i, &v, "reason.query", json!({ "pattern": atoms })).await;
    assert!(e.to_string().contains("at most 8 atoms"), "{e}");
    let evens: Vec<String> = (0..60).step_by(2).map(|k| format!("task:c{k}")).collect();
    let slice = ok(&i, &v, "reason.slice_check", json!({ "entities": evens, "limit": 1_000_000_000u64 })).await;
    let listed =
        slice["lost_in_slice_vocabulary"].as_array().map_or(0, Vec::len) + slice["lost"].as_array().map_or(0, Vec::len);
    assert!(listed <= 1000, "{listed}");
}

#[tokio::test]
async fn a_certificate_carries_the_closures_fixpoint_flag() {
    let i = demo().await;
    let done = ok(&i, &admin(), "reason.certify", json!({})).await;
    assert_eq!(done["fixpoint_reached"], true, "{}", done["fixpoint_reached"]);
    assert!(done["rounds"].as_u64().unwrap() > 0);
    assert!(done["means"].is_null() && done["truncated"].is_null());
    // A chain longer than max_rounds: the closure stops, the certificate
    // still checks, and the report says it is a lower bound.
    use onto_core::{ActionContext, EntityInput, Mutation};
    let rules = json!({ "message": "reach", "changes": [
        { "op": "put_rule", "def": { "name": "reach_seed", "description": "reached from the seed",
          "when": [["task:r0", "depends_on", "?y"]], "then": ["?y", "a", "reached"] } },
        { "op": "put_rule", "def": { "name": "reach_step", "description": "reach spreads",
          "when": [["?x", "a", "reached"], ["?x", "depends_on", "?y"]], "then": ["?y", "a", "reached"] } }
    ] });
    ok(&i, &admin(), "schema.apply", rules).await;
    let ctx = ActionContext::new(admin(), "acme");
    let n = 10_010;
    // Last first, so every link's target exists.
    let muts = (0..n)
        .rev()
        .map(|k| {
            let e = EntityInput::new("task", format!("r{k}"), format!("Reach {k}"));
            Mutation::Upsert(if k + 1 < n { e.link("depends_on", format!("task:r{}", k + 1)) } else { e })
        })
        .collect();
    i.ontology().apply(&ctx, muts).unwrap();
    let cut = ok(&i, &admin(), "reason.certify", json!({})).await;
    assert_eq!(cut["check"]["ok"], true, "{}", cut["check"]);
    assert_eq!(cut["fixpoint_reached"], false);
    assert!(cut["truncated"].as_str().unwrap().contains("max_rounds"), "{}", cut["truncated"]);
    assert!(cut["means"].as_str().unwrap().contains("lower bound"));
}

#[tokio::test]
async fn pack_lint_counts_are_the_packs_own() {
    let i = demo().await;
    let before = ok(&i, &admin(), "pack.show", json!({ "id": "retail" })).await;
    ok(&i, &admin(), "pack.install", json!({ "id": "manufacturing", "mode": "apply" })).await;
    ok(&i, &admin(), "pack.install", json!({ "id": "retail", "mode": "apply" })).await;
    let after = ok(&i, &admin(), "pack.show", json!({ "id": "retail" })).await;
    assert_eq!(before["lint"]["counts"], after["lint"]["counts"], "installed or not, the pack's own findings");
    assert_eq!(after["lint"]["new_on_current"], 0, "{}", after["lint"]);
    let other = ok(&i, &admin(), "pack.show", json!({ "id": "manufacturing" })).await;
    let n = |v: &Value| v["lint"]["new_findings"].as_u64().unwrap();
    assert_eq!(n(&after), after["lint"]["findings"].as_array().unwrap().len() as u64);
    assert!(other["lint"]["counts"].is_object());
}
