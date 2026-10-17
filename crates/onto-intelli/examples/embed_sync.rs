//! Embed onto-intelli in plain synchronous code (no async runtime needed).
//!
//! cargo run -p onto-intelli --example embed_sync

use onto_intelli::blocking::BlockingIntelli;
use onto_intelli::{ActionContext, EntityInput, Principal, Result};

fn main() -> Result<()> {
    let engine = BlockingIntelli::in_memory()?;
    let me = Principal::admin("script");
    let ctx = ActionContext::new(me.clone(), "default");

    // Direct synchronous graph API.
    let onto = engine.ontology();
    onto.upsert(&ctx, EntityInput::new("team", "data", "Data Team"))?;
    onto.upsert(
        &ctx,
        EntityInput::new("issue", "7", "Nightly ETL fails").state("open").link("assigned_to", "person:ana"),
    )
    .expect_err("links must point at existing entities");
    onto.upsert(&ctx, EntityInput::new("person", "ana", "Ana Silva").link("member_of", "team:data"))?;
    onto.upsert(
        &ctx,
        EntityInput::new("issue", "7", "Nightly ETL fails").state("open").link("assigned_to", "person:ana"),
    )?;

    let r = engine.ask(&me, "Who is assigned to issue #7?")?;
    println!("{}", r.answer);
    println!(
        "{}",
        serde_json::to_string_pretty(&engine.call(&me, "system.stats", serde_json::json!({}))?["ontology"]).unwrap()
    );
    Ok(())
}
