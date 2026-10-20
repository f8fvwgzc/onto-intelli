# onto-intelli

**Ontology-based Enterprise Execution Intelligence in one small Rust binary.**

`workspace → team → project → issue → task` + knowledge & artifacts + chat conversations,
modelled as an ontology, with a context graph, dependency reasoning, episodic and long-term
memory, multi-layer caching and token-efficient LLM orchestration — served over **gRPC**
(including **your own `.proto` files, configured at runtime**), JSON/HTTP, a CLI, a Rust
library (sync + async) and a C ABI.

```text
$ onto ask "Who owns Project Alpha?"
Project Alpha (project:alpha) is owned by Payments Team (team:payments).
deterministic · intent owner · 0 tokens · 0.8 ms

$ onto ask "Why is Project Alpha blocked?"
• cause chain: Project Alpha —depends_on→ Payment API —depends_on→ PostgreSQL
  —deployed_in→ Deployment #182 (PostgreSQL 16.4 upgrade) [deployment:182 is failed]
llm · intent blockers · 1894 tokens · 3.5 ms
```

The first answer never touches an LLM. The second sends the model ~1.9k tokens of ranked,
permission-filtered facts instead of the database.

---

## Why it is fast and cheap

The orchestrator retrieves **deterministically before** the model is called; the LLM only
reasons and explains.

```text
question ─► intent (rules, µs) ─► entity resolution (names, #ids, fuzzy) ─┐
                                                                         ▼
               deterministic answer possible? ── yes ──► answer from the graph (0 tokens, <1 ms)
                                                                         │ no
                                                                         ▼
   Question Context Graph: intent-weighted expansion + inference rules + root-cause chains
   + impact + recent domain events + knowledge (BM25) + recalled memory, permission-filtered
                                                                         ▼
          packed into a token budget ─► LLM provider (stable cached prefix) ─► answer
                                                                         ▼
               conversation · episodic memory · audit event · caches
```

| Layer | What it saves |
|---|---|
| Deterministic answers | owner, assignee, status, members, counts, lists, blockers, dependencies, impact, relations, workload, history — no LLM call at all |
| Answer cache | repeated questions; keys include the graph revision, so any write invalidates |
| Resolution cache | entity lookup per (question, revision) |
| LLM completion cache | identical prompts (memory + optional on-disk persistence) |
| Provider prompt cache | the system prompt + schema summary is byte-stable (`cache_control` on Anthropic, automatic prefix caching on OpenAI) |
| Context packing | ranked facts under `context.token_budget`; every response reports `context_tokens` vs `naive_tokens` |
| Conversation folding | old turns become a deterministic summary, so history cost is bounded |

`onto stats` shows the running ledger: deterministic answers, cache hits, tokens and cost
spent, and an estimate of tokens saved.

## Ontology model

Built in (see [`crates/onto-core/schemas/enterprise.toml`](crates/onto-core/schemas/enterprise.toml)),
fully dynamic and extensible from TOML — no recompilation:

* **Things** live in the graph: `workspace, team, person, agent, project, milestone, issue,
  task, service, repository, environment, deployment, knowledge, decision, artifact, folder,
  conversation` — typed properties, state machines, terminal and alert states.
* **Relations** are stored once, canonically, and walked by inverse names:
  `team -owns-> project` is also `project -owned_by-> team`.
* **Events** (who did what, when, with which outcome) go to a separate append-only event
  store and point at things. They never become graph nodes, so the graph stays small while the
  audit trail grows without bound.
* **Provenance** is first class: artifacts record `source`, `derived_from`, `generated_by`,
  `run` — and become `derived_from` / `generated_by` edges, so "why did we generate this
  Terraform file?" is a graph walk.
* **Inference rules** derive relations on demand: `assigned_to > member_of ⇒ handled_by_team`,
  `affects > owned_by > managed_by ⇒ escalate_to`.
* **Analysis** settings say which relations explain blockers (root cause), which carry impact,
  and what is "inside" a team or project.
* **Every write** carries actor + workspace + authorization + target + execution + result and
  is audited — including denials (`authorization.denied`).
* **Permissions**: workspace isolation, roles (`viewer`, `member`, `agent`, `admin`), and
  `visibility = "restricted"` entities with ACLs (people or teams). Answers, context and search
  are filtered per caller.

Extend it: [`examples/schema/custom.toml`](examples/schema/custom.toml) adds an `incident`
type that `extends = "issue"`, a `customer` type, new relations and a rule.

## Capabilities (v0.2)

Grounded in Palantir's Ontology, Databricks Unity Catalog semantics, Glean's
enterprise graph and recent arXiv research — see [docs/RESEARCH.md](docs/RESEARCH.md).

| Capability | Try it |
|---|---|
| **Live schema as data** — create, change and delete object types, properties, link types, interfaces, action types, rules and metrics at runtime; every change set is validated, previewed (entities/links affected, breaking or not), migrated explicitly when it breaks data in use, versioned with diffs and point-in-time reads, audited and reversible ([docs/SCHEMA.md](docs/SCHEMA.md)) | `onto schema plan change.toml` · `onto schema apply change.toml` · `onto schema history` · `onto schema revert 2` |
| **Shared PostgreSQL store** — several processes (replicas embedding the library, several servers) share one consistent store per tenant: optimistic commits on a commit log, in-memory indexes as caches refreshed through LISTEN/NOTIFY, optional read-your-writes ([docs/STORAGE.md](docs/STORAGE.md)) | `[engine] storage = "postgres"` |
| **nexc-execution vocabulary** — `extend = ["builtin:nexc-execution"]`: graphs, node types (executor, default role, stage, produces_artifact, allow_code_exec) and relation types (blocking) as validated, versioned data, read and written in nexc's shape ([docs/NEXC_EXECUTION.md](docs/NEXC_EXECUTION.md)) | `onto call execution.init_graph '{"graph":"g1"}'` |
| **Web UI** at `/ui` — workspace map (filter, search, expand, path, impact, history slider), schema editor with impact preview and apply/propose, proposals inbox, entity browser with history; iframe-embeddable with browser sessions, works on phones ([docs/UI.md](docs/UI.md)) | `onto serve --dev` then open http://127.0.0.1:50051/ui/ |
| **Benchmarked at a million entities** on PostgreSQL: graph reads in microseconds to ~2 ms, hybrid search ~14 ms, writes ~1 ms, replication ~1 ms ([docs/BENCHMARK.md](docs/BENCHMARK.md)) | `cargo run --release -p onto-bench -- --url … --entities 1000000` |
| **Governed action types** — typed parameters, preconditions, conditional effects, roles, risk; every plan derives an inverse, so reversibility is known before running; high-risk or irreversible actions need `confirm`; atomic audit event + undo receipt | `onto do` · `onto do assign item=issue:433 assignee=person:aiko` · `onto undo <receipt>` |
| **Natural-language commands** as gated proposals | `onto ask "Assign issue 433 to Aiko"` → proposal; add `confirm: true` to apply |
| **Interfaces + glossary** — `work_item` = issue/task/milestone; "tickets", "people", "per team" understood | `onto ask "How many open work items per team?"` |
| **Derived properties** (functions over relations) | `onto show project:alpha` · `entity.get {derived: true}` |
| **Governed metrics** with dimensions and certification, plus process KPIs from history and events: lead time (p50/p90), throughput, failure rate, volume | `onto metric` · `onto metric open_work --by team` · `onto ask "What is the deployment failure rate?"` |
| **Time**: every write versioned; history, as-of reads, change feeds | `onto history issue:421` · `onto changes --days 7` · `onto ask "What changed in Project Alpha this week?"` |
| **What-if scenarios** on an isolated fork | `onto whatif task:31 done` · `scenario.run` |
| **Entity alignment** — duplicate detection and merge with permanent id redirects | `onto dupes` · `onto merge person:kenji person:kenji.t` |
| **Data quality** — required links, uniqueness, staleness, orphans, dangling provenance, duplicates, low-confidence facts, scored per kind | `onto quality` · `onto ask "Show me stale issues"` |
| **Hybrid personalised search** — BM25 + hashed vectors (typo/morphology tolerant), boosted by your graph neighbourhood and recency | `onto call entity.search '{"query":"paymnt timeout"}'` |
| **Expert finding** with reasons | `onto experts postgres` · `onto ask "Who knows about PostgreSQL?"` |
| **Clarifying questions** instead of guessing on ambiguous names | `onto ask "What is #182?"` |
| **Graph profile** grounds prompts in patterns that exist; **drift suggestions** for data the schema doesn't cover | `onto schema profile` · `onto schema suggest` |
| **MCP server** for agents (same permissions, writes only via action types) | `onto mcp` |
| **Evaluation** with competency questions and evidence | `onto eval` (15/15 on the demo, 80% without the LLM) |
| **Export** — Turtle (OWL + PROV-O provenance) and Mermaid | `onto export --format mermaid --start project:alpha` |

### Use from an MCP client (Claude Code, IDEs)

```json
{ "mcpServers": { "onto-intelli": { "command": "onto", "args": ["--config", "/path/to/onto-intelli.toml", "mcp"] } } }
```

Tools: `ask`, `search`, `get_entity`, `neighbors`, `find_entities`, `explain_blockers`,
`impact`, `dependencies`, `critical_path`, `metric`, `history`, `changes`, `experts`,
`data_quality`, `what_if`, `list_actions`, `plan_action` (+ `apply_action`, `undo_action`
when `[mcp] allow_writes = true`).

## Install

| Option | Command |
|---|---|
| Release binary | `curl -fsSL https://raw.githubusercontent.com/f8fvwgzc/onto-intelli/main/packaging/install.sh \| sh` |
| From source | `cargo install --path crates/onto-cli` (or `make install`) |
| Docker | `docker build -t onto-intelli . && docker run -p 50051:50051 -e ONTO_TOKEN=$(openssl rand -hex 24) -v onto:/data onto-intelli` |
| Homebrew | formula template in [`packaging/homebrew/onto-intelli.rb`](packaging/homebrew/onto-intelli.rb) |

One static-ish binary (`onto`, ~13 MB release). SQLite is compiled in and protos are compiled
with [protox](https://github.com/andrewhickman/protox) — no `protoc`, no database server, no
system libraries.

## Quick start

```bash
mkdir my-org && cd my-org
onto init                       # onto-intelli.toml + protos/acme/tickets/v1/tickets.proto
onto seed demo                  # demo workspace "acme"
onto ask "How many open issues does the Payments Team have?"
onto ask "Why is Project Alpha blocked?" --context      # show the facts sent to the model
onto show issue:421             # entity card, neighbours, derived relations
onto deps project:alpha --cause # root-cause chains
onto deps project:alpha --plan  # execution order + critical path
onto chat                       # multi-turn, remembers what "it" refers to
export ONTO_TOKEN=$(openssl rand -hex 24)
onto serve                      # gRPC + HTTP on 127.0.0.1:50051 (token auth)
onto serve --dev                # local development: no auth, loopback only
```

Bring data in with JSONL (mutations, bare entities or GitHub `issues` webhook payloads):

```bash
onto ingest examples/ingest/sample.jsonl --async
```

## LLM providers

Default is `mock`: offline and deterministic, so everything works without a key.

```toml
[llm]
provider = "anthropic"          # default model: claude-opus-5-5
api_key_env = "ANTHROPIC_API_KEY"
effort = "medium"               # low | medium | high | xhigh | max

# provider = "openai"           # api_key_env = "OPENAI_API_KEY", model = "..."
# provider = "ollama"           # local, base_url = "http://localhost:11434/v1"
# provider = "openai-compatible" # vLLM, LM Studio, OpenRouter, Groq, Together...
# provider = "none"             # deterministic answers + fact summaries only
```

Keys are read from the environment only. If a key is missing the server still starts and
answers deterministically; LLM questions return the most relevant facts with a note.
The Anthropic provider uses the native Messages API with prompt caching on the stable system
block and server-side refusal fallback (`fallbacks: "default"`, disable with
`llm.fallbacks = false`). Implement `onto_intelli::llm::LlmProvider` to plug in anything else.

## gRPC

`onto serve` exposes on one port:

* the built-in API [`onto_intelli.v1`](crates/onto-grpc/proto/onto_intelli/v1/onto_intelli.proto):
  `OntologyService`, `IntelliService` (incl. server-streaming `AskStream`),
  `ActionService` (plan/apply/undo governed actions), `AnalyticsService` (metrics,
  history, changes, quality, duplicates/merge, scenarios, experts, profile, export, eval),
  `MemoryService`, `EventService` (incl. client-streaming `Ingest`), `AdminService`;
* `SchemaService`: the live schema and proposals ([docs/SCHEMA.md](docs/SCHEMA.md));
* gRPC reflection and `grpc.health.v1`;
* JSON over HTTP/1.1 on the same port.

```bash
grpcurl -plaintext -H "authorization: Bearer $ONTO_TOKEN" localhost:50051 list
grpcurl -plaintext -H "authorization: Bearer $ONTO_TOKEN" -d '{"question":"Who owns Project Alpha?"}' \
  localhost:50051 onto_intelli.v1.IntelliService/Ask
curl -s localhost:50051/v1/call/graph.root_cause -H "authorization: Bearer $ONTO_TOKEN" \
  -H 'content-type: application/json' -d '{"id":"project:alpha"}'
curl -sN localhost:50051/v1/ask/stream -H "authorization: Bearer $ONTO_TOKEN" \
  -H 'content-type: application/json' -d '{"question":"Why is Project Alpha blocked?"}'
```

### Your own proto files, no code generation

Point the server at your protos and map each RPC onto an engine action with a template.
The server compiles them at startup and transcodes requests and responses dynamically.

```toml
[grpc]
proto_dirs = ["protos"]
proto_files = ["acme/tickets/v1/tickets.proto"]

[[grpc.routes]]
method = "acme.tickets.v1.TicketService/CreateTicket"
action = "entity.upsert"
[grpc.routes.request]
kind = "issue"
key = "{{ticket_id}}"
title = "{{title}}"
state = "open"
props = { priority = "{{priority}}" }
links = [ { rel = "belongs_to", to = "project:{{project}}" },
          { rel = "assigned_to", to = "person:{{assignee}}" } ]
[grpc.routes.response]
id = "{{id}}"
title = "{{title}}"
priority = "{{props.priority}}"
```

* `"{{x}}"` alone keeps the JSON type (arrays stay arrays); inside text it interpolates.
* Missing fields are dropped; an array element that loses a key is dropped (so an absent
  assignee does not create a broken `person:` link).
* Without a template, the request JSON goes straight to the action and the result is mapped
  field-by-field onto the response message.
* Unary, server-streaming (`intelli.ask_stream`) and client-streaming RPCs are supported.
* `onto protos` compiles your files and prints the route table, so bad config fails before deploy.

`onto actions` lists every action (`entity.*`, `link.*`, `graph.*`, `context.build`,
`intelli.ask`, `memory.*`, `event.*`, `ingest.*`, `system.*`).

### Auth

Authentication is on by default. A server with no token configured refuses to start;
`onto serve --dev` (no authentication, every caller is an admin) only listens on loopback.

```toml
[auth]
mode = "token"
[[auth.tokens]]
token_env = "ONTO_TOKEN_OPS"
principal = { id = "ops-bot", roles = ["agent"], workspaces = ["acme"], entity = "agent:ops-bot" }

# A backend acting for its own users: per-call principals, per-user roles and ACLs.
[[auth.tokens]]
token_env = "NEXC_ONTO_TOKEN"
principal = { id = "nexc-backend", roles = ["member"], workspaces = ["*"] }
impersonate = true
impersonate_roles = ["viewer", "member", "agent", "approver"]
tenants = ["*"]
```

Every gRPC and HTTP call carries `authorization: Bearer <token>`. Optional headers (or gRPC
metadata) refine it:

| header | does |
|---|---|
| `x-onto-workspace` | selects the workspace |
| `x-onto-tenant` | selects the tenant |
| `x-onto-act-as`, `x-onto-act-roles`, `x-onto-act-workspaces`, `x-onto-act-entity` | an impersonating token names the user the call is for ([docs/EMBEDDING.md](docs/EMBEDDING.md)) |

## Embedding

**Rust, async** ([example](crates/onto-intelli/examples/embed_async.rs)):

```rust
let intelli = Intelli::open(Config::load("onto-intelli.toml")?).await?;
let ctx = ActionContext::new(Principal::admin("app"), "acme");
intelli.ontology().upsert(&ctx, EntityInput::new("issue", "9", "Login fails").state("open"))?;
let answer = intelli.ask(&Principal::admin("app"), AskRequest::new("Who owns issue #9?")).await?;
```

**Many tenants in one process** (`Hub`): one ontology per tenant (a PostgreSQL schema, a
SQLite file or memory), opened on first use; `Hub::with_postgres(cfg, PgShared::from_pool(app_pool)?)`
shares the application's sqlx pool. See [docs/EMBEDDING.md](docs/EMBEDDING.md).

**Rust, sync** ([example](crates/onto-intelli/examples/embed_sync.rs)): `BlockingIntelli`
owns a small runtime; the graph API (`Ontology`) is synchronous anyway.

Feature flags: `onto-intelli = { default-features = false }` drops SQLite and HTTP providers
for the smallest footprint; `onto-core` alone is the ontology engine with no async runtime.

**Any language** via the C ABI ([header](crates/onto-ffi/include/onto_intelli.h)) —
`cargo build --release -p onto-intelli-ffi` produces `libonto_intelli.{so,dylib,dll,a}`:

```python
onto = Onto()                                   # examples/ffi/python_demo.py (ctypes)
onto.call("graph.apply", {"mutations": [...]})
onto.ask("Who owns Project Ranker?")["answer"]
```

C: [`examples/ffi/demo.c`](examples/ffi/demo.c).

## Configuration

`onto init` writes an annotated [`onto-intelli.toml`](crates/onto-intelli/templates/onto-intelli.toml).
Sections: `[engine]` storage and workspace, `[ontology]` schema layers, `[llm]`, `[context]`
token budget and expansion, `[cache]`, `[memory]`, `[reasoning]` deterministic intents,
`[grpc]` listener and routes, `[auth]`. Lookup order: `--config`, `$ONTO_CONFIG`,
`./onto-intelli.toml`, `~/.config/onto-intelli/config.toml`.

## Memory

* **Episodic**: every answered question (asker, entities, answer), expires after
  `episodic_ttl_days`.
* **Long-term**: `memory.remember` facts and preferences (shared or private), plus summaries
  promoted by consolidation when the same entities keep coming up.
* Recall blends BM25, entity overlap, recency and salience. Earlier answers to the *same*
  question are never fed back as evidence; long-term facts rank with graph facts, episodes
  rank below them.

## Project layout

```text
crates/onto-core      ontology: schema, graph, store (SQLite/memory), events, policy, rules, deps, BM25
crates/onto-intelli   orchestrator: intent, resolution, context graph, memory, LLM providers, caches, actions
crates/onto-grpc      gRPC/HTTP server: built-in proto, dynamic proto routes, reflection, health, SSE
crates/onto-cli       the `onto` binary
crates/onto-ffi       C ABI (cdylib + staticlib) and header
examples/             FFI demos, JSONL ingest sample, schema extension
packaging/            install script, Homebrew formula; Dockerfile and CI at the root
docs/ARCHITECTURE.md  design notes
docs/RESEARCH.md      what v0.2 took from Palantir, Databricks, Glean and arXiv
```

## Development

```bash
make test        # cargo test --workspace (unit + end-to-end server tests)
make lint        # fmt check + clippy
make release     # optimized binaries in target/release
make dist        # tarball for this platform in dist/
```

Tagging `vX.Y.Z` runs [`.github/workflows/release.yml`](.github/workflows/release.yml):
binaries for Linux (x86_64, arm64), macOS (arm64, x86_64) and Windows, plus the FFI library,
attached to a GitHub release with checksums.

## License

MIT
