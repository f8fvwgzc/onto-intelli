//! Entailment preservation of the packed LLM context: every packed `derived`
//! fact is checked for the links it rests on, repaired with `premise` facts
//! when the budget allows, and counted lost otherwise.

use std::sync::Arc;

use serde_json::json;

use onto_core::reason::{Fact as RFact, ReasonOptions, Status};
use onto_core::{ActionContext, EntityInput, Mutation, Role};
use onto_intelli::context::{ContextPack, PRESERVATION_MEANS};
use onto_intelli::{Config, Intelli, Principal, seed};

async fn demo_with(tweak: impl FnOnce(&mut Config)) -> Arc<Intelli> {
    let mut cfg = Config::in_memory();
    cfg.engine.default_workspace = "acme".into();
    tweak(&mut cfg);
    let i = Intelli::open(cfg).await.unwrap();
    seed::load_demo(&i, "acme").await.unwrap();
    i
}

async fn demo() -> Arc<Intelli> {
    demo_with(|_| {}).await
}

fn admin() -> Principal {
    Principal::admin("lead")
}

const TEAM_Q: &str = "Which team handles issue #421?";

/// The `(from, rel, to)` of a `premise: a rel → b` fact.
fn premise_link(text: &str) -> (String, String, String) {
    let rest = text.strip_prefix("premise: ").expect("premise prefix");
    let (left, to) = rest.split_once(" → ").expect("arrow");
    let (from, rel) = left.split_once(' ').expect("relation");
    (from.to_string(), rel.to_string(), to.to_string())
}

/// The `(from, rel, to)` of a `derived: a rel b (via ...)` fact.
fn derived_claim(text: &str) -> (String, String, String) {
    let rest = text.strip_prefix("derived: ").expect("derived prefix");
    let mut parts = rest.split(' ');
    let (from, rel, to) = (parts.next().unwrap(), parts.next().unwrap(), parts.next().unwrap());
    (from.to_string(), rel.to_string(), to.to_string())
}

/// Facts kept by packing come first, unchanged; premise facts only follow
/// them, and the prompt text and token count match the fact list.
fn assert_well_formed(ctx: &ContextPack) {
    let first_premise = ctx.facts.iter().position(|f| f.source == "premise").unwrap_or(ctx.facts.len());
    assert!(
        ctx.facts[first_premise..].iter().all(|f| f.source == "premise"),
        "premise facts must only follow the packed facts: {:#?}",
        ctx.facts
    );
    let lines: Vec<String> = ctx.facts.iter().map(|f| format!("- {}", f.text)).collect();
    assert_eq!(ctx.text, lines.join("\n"));
    assert_eq!(ctx.tokens, ctx.facts.iter().map(|f| f.tokens).sum::<usize>());
    if let Some(pr) = &ctx.preservation {
        assert_eq!(pr.claims, pr.preserved + pr.repaired + pr.lost.len(), "{pr:?}");
        assert_eq!(pr.claims, ctx.facts.iter().filter(|f| f.text.starts_with("derived: ")).count(), "{pr:?}");
        assert_eq!(pr.means, PRESERVATION_MEANS);
    }
}

#[tokio::test]
async fn ample_budget_preserves_every_derived_fact() {
    let i = demo().await;
    let ctx = i.build_context(&admin(), "acme", TEAM_Q, None).unwrap();
    assert_well_formed(&ctx);
    let pr = ctx.preservation.as_ref().expect("derived facts were packed");
    assert!(pr.claims >= 1, "{pr:?}");
    assert_eq!(pr.preserved, pr.claims, "{pr:?}\n{}", ctx.text);
    assert_eq!(pr.repaired, 0);
    assert!(pr.lost.is_empty());
    assert!(ctx.facts.iter().all(|f| f.source != "premise"), "nothing to repair: {}", ctx.text);
    assert!(ctx.text.contains("derived: issue:421 handled_by_team team:payments"), "{}", ctx.text);
    // The premises are spelled by relations lines (owned_by is the inverse of owns).
    assert!(ctx.text.contains("issue:421: affects → service:payment-api; assigned_to → person:kenji"), "{}", ctx.text);
    let json = serde_json::to_value(&ctx).unwrap();
    assert_eq!(json["preservation"]["preserved"], pr.claims, "{json}");
}

#[tokio::test]
async fn tight_budget_loses_claims_without_adding_partial_premises() {
    let i = demo().await;
    let ctx = i.build_context(&admin(), "acme", TEAM_Q, Some(200)).unwrap();
    assert_well_formed(&ctx);
    let pr = ctx.preservation.as_ref().expect("derived facts outrank relations lines and were packed");
    assert!(!pr.lost.is_empty() || pr.repaired > 0, "{pr:?}\n{}", ctx.text);
    assert!(ctx.tokens <= 200, "repairs never exceed the budget: {}", ctx.tokens);
    // Lost claims are spelled "<from> <relation> <to>" and are packed claims.
    for lost in &pr.lost {
        let line = format!("derived: {lost} (via ");
        assert!(ctx.text.contains(&line), "{lost} is not a packed claim: {}", ctx.text);
    }
    if pr.repaired == 0 {
        assert!(ctx.facts.iter().all(|f| f.source != "premise"), "a lost claim adds no premise: {}", ctx.text);
    }
}

#[tokio::test]
async fn hops_outside_the_packed_nodes_are_repaired() {
    // With two nodes in the context, the derivations walk entities whose links
    // no relations line states, while the budget has room for the premises.
    let i = demo_with(|c| c.context.max_nodes = 2).await;
    let ctx = i.build_context(&admin(), "acme", TEAM_Q, None).unwrap();
    assert_well_formed(&ctx);
    let pr = ctx.preservation.as_ref().expect("derived facts were packed");
    assert!(pr.repaired >= 1, "{pr:?}\n{}", ctx.text);
    assert!(pr.lost.is_empty(), "the budget is ample: {pr:?}");
    let premises: Vec<&str> = ctx.facts.iter().filter(|f| f.source == "premise").map(|f| f.text.as_str()).collect();
    assert!(premises.contains(&"premise: person:kenji member_of → team:payments"), "{premises:?}");
    // Canonical direction: owned_by is the inverse name of owns.
    assert!(premises.contains(&"premise: team:payments owns → service:payment-api"), "{premises:?}");
    let unique: std::collections::BTreeSet<&&str> = premises.iter().collect();
    assert_eq!(unique.len(), premises.len(), "a shared premise is added once: {premises:?}");

    // The reasoner agrees: every premise is an asserted link and every claim is derived.
    let closure = i.ontology().reason(&admin(), &ReasonOptions::default()).unwrap();
    for text in &premises {
        let (s, p, o) = premise_link(text);
        let fact = RFact::new(format!("<{s}>"), p, format!("<{o}>"));
        assert_eq!(closure.contains(&fact), Some(Status::Asserted), "{text}");
    }
    for f in ctx.facts.iter().filter(|f| f.text.starts_with("derived: ")) {
        let (s, p, o) = derived_claim(&f.text);
        let fact = RFact::new(format!("<{s}>"), p, format!("<{o}>"));
        assert!(matches!(closure.contains(&fact), Some(Status::Derived { .. })), "{}", f.text);
    }
}

#[tokio::test]
async fn questions_without_derived_facts_keep_their_json() {
    let i = demo().await;
    for q in ["Who owns Project Alpha?", "Why is Project Alpha blocked?"] {
        let ctx = i.build_context(&admin(), "acme", q, None).unwrap();
        assert_well_formed(&ctx);
        assert!(ctx.preservation.is_none(), "{q}: {:?}", ctx.preservation);
        assert!(ctx.facts.iter().all(|f| f.source != "premise"), "{q}");
        let json = serde_json::to_value(&ctx).unwrap();
        assert!(json.get("preservation").is_none(), "{q}: {json}");
        // Stored packs from before the field existed still load.
        let back: ContextPack = serde_json::from_value(json).unwrap();
        assert!(back.preservation.is_none());
    }
}

#[tokio::test]
async fn too_small_a_budget_packs_no_claim() {
    let i = demo().await;
    let ctx = i.build_context(&admin(), "acme", TEAM_Q, Some(64)).unwrap();
    assert_well_formed(&ctx);
    assert!(ctx.preservation.is_none(), "no derived fact fits next to the pinned seed: {}", ctx.text);
}

#[tokio::test]
async fn preservation_is_deterministic() {
    // Memory scores decay with time, so compare what preservation produced.
    let i = demo_with(|c| c.context.max_nodes = 2).await;
    let run = || {
        let ctx = i.build_context(&admin(), "acme", TEAM_Q, None).unwrap();
        let premises: Vec<String> =
            ctx.facts.iter().filter(|f| f.source == "premise").map(|f| f.text.clone()).collect();
        (serde_json::to_string(&ctx.preservation).unwrap(), premises)
    };
    let first = run();
    assert!(!first.1.is_empty());
    for _ in 0..3 {
        assert_eq!(run(), first);
    }
}

#[tokio::test]
async fn a_symmetric_hop_walked_backwards_is_spelled_as_stored() {
    // issue:100 stores `relates_to → issue:421`; the rule walks it from issue:421.
    let dir = std::env::temp_dir().join(format!("onto-preservation-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let layer = dir.join("related.toml");
    std::fs::write(
        &layer,
        r#"
[[rules]]
name = "related_assignee"
description = "Whoever is assigned related work."
chain = ["relates_to", "assigned_to"]
infer = "related_assignee"
"#,
    )
    .unwrap();
    let path = layer.to_string_lossy().to_string();
    let i = demo_with(|c| {
        c.ontology.extend.push(path);
        c.context.max_nodes = 1;
    })
    .await;
    let ctx = ActionContext::new(admin(), "acme");
    i.ontology()
        .apply(
            &ctx,
            vec![Mutation::Upsert(
                EntityInput::new("issue", "100", "Checkout retries")
                    .link("relates_to", "issue:421")
                    .link("assigned_to", "person:aiko"),
            )],
        )
        .unwrap();
    let pack = i.build_context(&admin(), "acme", TEAM_Q, None).unwrap();
    std::fs::remove_dir_all(&dir).ok();
    assert_well_formed(&pack);
    assert!(pack.text.contains("derived: issue:421 related_assignee person:aiko"), "{}", pack.text);
    let premises: Vec<&str> = pack.facts.iter().filter(|f| f.source == "premise").map(|f| f.text.as_str()).collect();
    assert!(premises.contains(&"premise: issue:100 relates_to → issue:421"), "{premises:?}");
    assert!(!premises.contains(&"premise: issue:421 relates_to → issue:100"), "{premises:?}");

    // Every premise, the backwards symmetric one included, is an asserted link.
    let closure = i.ontology().reason(&admin(), &ReasonOptions::default()).unwrap();
    for text in &premises {
        let (s, p, o) = premise_link(text);
        let fact = RFact::new(format!("<{s}>"), p, format!("<{o}>"));
        assert_eq!(closure.contains(&fact), Some(Status::Asserted), "{text}");
    }
}

#[tokio::test]
async fn provenance_lines_name_only_readable_sources() {
    let i = demo().await;
    let ctx = ActionContext::new(admin(), "acme");
    i.ontology()
        .apply(
            &ctx,
            vec![
                Mutation::Upsert(
                    EntityInput::new("knowledge", "topsecret", "Board notes")
                        .prop("visibility", "restricted")
                        .prop("acl", json!(["person:sarah"])),
                ),
                Mutation::Upsert(
                    EntityInput::new("knowledge", "summary", "Quarter summary")
                        .link("derived_from", "knowledge:topsecret")
                        .link("derived_from", "knowledge:runbook-payments"),
                ),
            ],
        )
        .unwrap();
    let q = "Where did knowledge:summary come from? What is it derived from?";
    let lead = i.build_context(&admin(), "acme", q, None).unwrap();
    let provenance =
        |text: &str| text.lines().find(|l| l.starts_with("- provenance of knowledge:summary: ")).map(str::to_string);
    let line = provenance(&lead.text).expect("a provenance line for the lead");
    assert!(line.contains("derived_from knowledge:topsecret"), "{line}");

    let member = Principal::new("aiko", vec![Role::Member], vec!["acme".into()]).with_entity("person:aiko");
    let pack = i.build_context(&member, "acme", q, None).unwrap();
    assert_well_formed(&pack);
    assert!(!pack.text.contains("topsecret"), "an unreadable source leaked: {}", pack.text);
    let line = provenance(&pack.text).expect("a provenance line for the member");
    assert_eq!(line, "- provenance of knowledge:summary: derived_from knowledge:runbook-payments");
}
