//! Embed onto-intelli in an async (Tokio) application.
//!
//! cargo run -p onto-intelli --example embed_async

use onto_intelli::{ActionContext, AskRequest, EntityInput, Intelli, LinkInput, Mutation, Principal, Result};

#[tokio::main]
async fn main() -> Result<()> {
    // Volatile engine with the offline mock LLM; use Intelli::open(Config::load("onto-intelli.toml")?) for real use.
    let intelli = Intelli::in_memory().await?;
    let ctx = ActionContext::new(Principal::admin("app"), "default").with_source("example");

    // Writes are synchronous, atomic and audited.
    intelli.ontology().apply(
        &ctx,
        vec![
            Mutation::Upsert(EntityInput::new("team", "search", "Search Team")),
            Mutation::Upsert(EntityInput::new("person", "lin", "Lin Wei").link("member_of", "team:search")),
            Mutation::Upsert(
                EntityInput::new("service", "indexer", "Indexer").state("down").link("owned_by", "team:search"),
            ),
            Mutation::Upsert(
                EntityInput::new("project", "relevance", "Project Relevance")
                    .state("blocked")
                    .link("owned_by", "team:search"),
            ),
            Mutation::Link(LinkInput::new("project:relevance", "depends_on", "service:indexer")),
        ],
    )?;

    let me = Principal::admin("app");
    for q in
        ["Who owns Project Relevance?", "Why is Project Relevance blocked?", "What breaks if the Indexer goes down?"]
    {
        let r = intelli.ask(&me, AskRequest::new(q)).await?;
        println!(
            "Q: {q}\nA: {}\n   [{:?}, {} tokens, {:.1} ms]\n",
            r.answer,
            r.mode,
            r.usage.total(),
            r.timings.total_ms
        );
    }

    // Everything is also reachable by action name (the same surface gRPC/HTTP/FFI use).
    let deps = intelli.call(&me, "graph.root_cause", serde_json::json!({ "id": "project:relevance" })).await?;
    println!("root cause: {}", deps["chains"][0]["text"]);
    Ok(())
}
