# onto-intelli

**Ontology-based Enterprise Execution Intelligence: one Rust binary + PostgreSQL.**

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

## Plan a change, see every consequence, hand the reviewer a proof

A schema change can touch no entity and no link and still change what the
ontology says. Making `depends_on` transitive is one such change. On the demo
workspace (with the manufacturing pack installed) the plan shows its blast
radius, zero, and its consequences: the facts it adds (the new schema fact
itself, asserted, and three derived facts, each with the rule and premises
behind it), all stated in names you already use, so the change is not
conservative:

```text
$ onto schema plan transitive.toml
non-breaking on schema v2 · 0 entities · 0 links in scope
  · change link type project|task|service|issue|milestone|package -depends_on-> project|task|service|issue|milestone|repository|environment|package (now transitive) (0 entities, 0 links)
  risk medium: not conservative: 4 added facts are stated purely in the current vocabulary, e.g. <project:alpha> depends_on <service:auth-service>
  consequences: +4 added (1 asserted, 3 derived), -0 removed · not conservative (4 new over existing names)
      asserted 1 · builtin/transitive 3
      + <project:alpha> depends_on <service:auth-service>  by builtin/transitive from depends_on a transitive, <project:alpha> depends_on <service:payment-api>, <service:payment-api> depends_on <service:auth-service>
      + <project:alpha> depends_on <service:postgres>  by builtin/transitive from depends_on a transitive, <project:alpha> depends_on <service:payment-api>, <service:payment-api> depends_on <service:postgres>
      + <task:34> depends_on <task:31>  by builtin/transitive from depends_on a transitive, <task:34> depends_on <task:32>, <task:32> depends_on <task:31>
      + depends_on a transitive
  shapes: 0 new results, 0 resolved · violates → violates
```

Derived facts are never written to the graph. They carry a certificate: four
plain files in which every derived fact is one line citing a rule and earlier
lines. `onto-cert`, a checker that shares no code with the engine, re-derives
each line by pattern alone. The verdict says whether the conclusions rest on
the schema's built-in semantics only (`entailed`) or also on supplied rules,
such as an industry pack's (`entailed_under_supplied_rules`):

```text
$ onto reason certify --out cert
entailed_under_supplied_rules 345 asserted · 62 derivations · rules cited: 2 built-in, 10 supplied · onto-cert 0.2.0
means: every derived fact follows from asserted.tsv under the cited rules, re-derived line by line by pattern without search; the supplied (non-built-in) rules are assumptions the certificate carries, not facts it establishes; this does not show that asserted.tsv is the graph the engine read
wrote cert (check again with `onto cert check cert` or `onto-cert check cert`)
```

Change one conclusion in `derivations.tsv` (line 15: issue 421 handled by
Platform instead of Payments), even with the manifest digest recomputed to
match, and the checker refuses it by line and rule (exit 1):

```text
$ onto cert check forged
{
  "asserted": 345,
  "conclusion": "<issue:421> handled_by_team <team:platform>",
  "derivations": 62,
  "first_rejected": 15,
  "ok": false,
  "premises": [
    "<issue:421> assigned_to <person:kenji>",
    "<person:kenji> member_of <team:payments>"
  ],
  "reason": "conclusion_mismatch",
  "rule": "schema/responsible_team"
}
```

Shapes measure the data (`conforms`, `violates` or `undetermined`, never
"valid"), lint reviews the schema, alignment scores another schema against
yours and exports SSSOM, and a CSV sheet becomes an object type, links and a
shape with the evidence for each line. Each report says whether it is a proof,
a measurement or an opinion. Ten industry packs (manufacturing, supply chain,
retail, logistics, healthcare, finance, insurance, energy, telecom, SaaS)
install as governed schema changes. See [docs/ASSURANCE.md](docs/ASSURANCE.md)
and [docs/PACKS.md](docs/PACKS.md); the ideas come from
[open-ontologies](https://github.com/fabio-rovai/open-ontologies) by Fabio
Rovai / Tesseract Semantics, reimplemented here for a typed property graph.

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
| **Ontology builder** — connect a database and draft object types backed by its tables (typed properties, keys, titles, states), link types from foreign keys and join tables, and action types; refine it with the LLM, apply or propose it, sync rows into objects and browse them in the Object Explorer ([docs/ONTOLOGY_BUILDER.md](docs/ONTOLOGY_BUILDER.md)) | `onto model draft shop` · `onto model apply shop` · `onto explore order --filter state=paid` |
| **Live schema as data** — create, change and delete object types, properties, link types, interfaces, action types, rules, metrics and shapes at runtime; every change set is validated, previewed (entities/links affected, breaking or not), migrated explicitly when it breaks data in use, versioned with diffs and point-in-time reads, audited and reversible ([docs/SCHEMA.md](docs/SCHEMA.md)) | `onto schema plan change.toml` · `onto schema apply change.toml` · `onto schema history` · `onto schema revert 2` |
| **Reasoner with certificates** — derived facts kept apart from stored ones, each with its rule and premises; a certificate per run that the independent checker `onto-cert` accepts or refuses line by line, offline; justifications, provenance, claim verification and slice preservation ([docs/ASSURANCE.md](docs/ASSURANCE.md)) | `onto reason run` · `onto reason certify --out cert` · `onto cert check cert` · `onto reason explain issue:421 escalate_to person:sarah` |
| **Semantic schema plans** — every plan also lists the derived facts a change adds and removes, whether it is conservative, a risk word with reasons, lock violations, deletes that look like renames and the shape results it changes | `onto schema plan change.toml` · `onto lock object_types issue --reason "…"` · `onto do assign item=issue:433 assignee=person:maria --consequences` |
| **Shapes** (SHACL-style) — `conforms`, `violates` or `undetermined`, never a silent pass; a structural check for authors and agents; shapes induced from data with the counts behind each constraint | `onto shapes validate` · `onto shapes check shapes.toml` · `onto shapes induce part` |
| **Schema lint** — 19 checks with fixes and stable finding ids; dismiss or accept with a note, never hidden silently | `onto lint` · `onto lint --dismiss <id> --note "…"` |
| **Industry packs** — manufacturing, supply chain, retail, logistics, healthcare, finance, insurance, energy, telecom and SaaS, each with rules, shapes, metrics, action types and a sample, installed as a governed schema change ([docs/PACKS.md](docs/PACKS.md)) | `onto pack list` · `onto pack plan retail` · `onto pack install manufacturing --sample` |
| **Schema alignment** — scored matches between a pack or a TOML schema and yours, SSSOM export, certification of claimed mappings against both hierarchies | `onto align --pack supply-chain --sssom out.tsv` · `onto align other.toml --certify mappings.json` |
| **One sheet in, one ontology out** — a CSV becomes an object type, link types and a shape, with an evidence sentence per line; ingest with a lexical guard that never coerces | `onto induce vendors.csv --change-set c.json` · `onto induce vendors.csv --ingest` · `onto induce other.csv --ingest --mapping m.json` |
| **PostgreSQL store** (16+, with pgvector) — the durable store: versioned migrations embedded in the binary, one schema per tenant, JSONB documents, an append-only event log and history, vectors in an HNSW index. Any number of processes (replicas embedding the library, several servers) share it consistently: optimistic commits on a commit log, in-memory graph indexes refreshed through LISTEN/NOTIFY, optional read-your-writes ([docs/STORAGE.md](docs/STORAGE.md)) | `make db-up` · `onto --database-url postgres://… status` |
| **nexc-execution vocabulary** — `extend = ["builtin:nexc-execution"]`: graphs, node types (executor, default role, stage, produces_artifact, allow_code_exec) and relation types (blocking) as validated, versioned data, read and written in nexc's shape ([docs/NEXC_EXECUTION.md](docs/NEXC_EXECUTION.md)) | `onto call execution.init_graph '{"graph":"g1"}'` |
| **Web UI** at `/ui` — workspace map (filter, search, expand, path, impact, history slider), Object Explorer, ontology Builder, schema editor with impact preview and apply/propose, proposals inbox, entity browser with history; iframe-embeddable with browser sessions, works on phones ([docs/UI.md](docs/UI.md)) | `onto serve --dev` then open http://127.0.0.1:50051/ui/ |
| **Benchmarked at a million entities** on PostgreSQL: graph reads in microseconds to a few ms, vector search on pgvector HNSW ~18 ms, writes ~3 ms, replication ~3 ms, 1.4 GB per replica ([docs/BENCHMARK.md](docs/BENCHMARK.md)) | `cargo run --release -p onto-bench -- --url … --entities 1000000` |
| **Governed action types** — typed parameters, preconditions, conditional effects, roles, risk; every plan derives an inverse, so reversibility is known before running; high-risk or irreversible actions need `confirm`; atomic audit event + undo receipt | `onto do` · `onto do assign item=issue:433 assignee=person:aiko` · `onto undo <receipt>` |
| **Natural-language commands** as gated proposals | `onto ask "Assign issue 433 to Aiko"` → proposal; add `confirm: true` to apply |
| **Interfaces + glossary** — `work_item` = issue/task/milestone; "tickets", "people", "per team" understood | `onto ask "How many open work items per team?"` |
| **Derived properties** (functions over relations) | `onto show project:alpha` · `entity.get {derived: true}` |
| **Governed metrics** with dimensions and certification, plus process KPIs from history and events: lead time (p50/p90), throughput, failure rate, volume | `onto metric` · `onto metric open_work --by team` · `onto ask "What is the deployment failure rate?"` |
| **Time**: every write versioned; history, as-of reads, change feeds | `onto history issue:421` · `onto changes --days 7` · `onto ask "What changed in Project Alpha this week?"` |
| **Change feed** — every commit's changes as records with a resumable cursor, filtered per reader (hidden entities become `entity.hidden`); a commit from a build that did not record changes appears as a gap; poll, or follow over server-sent events ([docs/AUTOMATE.md](docs/AUTOMATE.md)) | `onto watch` · `onto watch --once --after 13.0` · `GET /v1/changes/stream` |
| **Webhooks** — CloudEvents 1.0 signed with Standard Webhooks, from an outbox: each change is enqueued once per endpoint and delivered at least once (dedupe on the event id); retries, a breaker, replay; the receiver sees only what its principal may; SSRF-guarded, metadata addresses never reachable; every request carries a W3C `traceparent` continuing the trace of the call that caused it, and attempts are traces of kind `delivery`; verify helpers in both SDKs | `onto webhook add ops https://… --secret env:HOOK_SECRET --kind issue` · `verify_webhook(secret, headers, body)` |
| **Automate** — change-triggered automations defined in the schema (agents propose, people approve and enable), running one governed action and any number of webhooks as `automation:<name>`; a run's edits, receipt, outbox rows and run row commit together; bound to everything it reaches, expiring after 30 days; depth, rate and breaker guards; each run is a trace of kind `automation`; change lag, gaps, outbox states and runs per state in `system.stats` and `GET /v1/admin/tenants` ([docs/AUTOMATE.md](docs/AUTOMATE.md)) | `onto schema propose escalate.toml` · `onto automation enable escalate_blocked` · `onto automation runs` |
| **HTTP writeback** — an action type's `writeback` effect posts to a registered target after the action commits, through the outbox with `Idempotency-Key: <receipt>:<effect>`; writeback is not reversible, so plans with it need confirmation; off by default | `writeback.add` · `onto do close_ticket issue=issue:1 --confirm` |
| **What-if scenarios** on an isolated fork | `onto whatif task:31 done` · `scenario.run` |
| **Entity alignment** — duplicate detection and merge with permanent id redirects | `onto dupes` · `onto merge person:kenji person:kenji.t` |
| **Data quality** — required links, uniqueness, staleness, orphans, dangling provenance, duplicates, low-confidence facts, scored per kind | `onto quality` · `onto ask "Show me stale issues"` |
| **Knowledge** — documents (text, Markdown, parsed blocks, or passages imported with their embeddings) split into cited passages and searched by BM25 fused with vectors; sensitivity labels with an agent ceiling, document ACLs; not graph nodes ([docs/KNOWLEDGE.md](docs/KNOWLEDGE.md)) | `onto call knowledge.ingest '{"name":"Runbook.md","text":"# Runbook …"}'` · `onto call knowledge.search '{"query":"failover"}'` |
| **External signals** — RSS and Atom feeds fetched SSRF-safely by admins; items become `signal.item` events and knowledge, never entities; the recommender suggests link types and chain rules from what the items name together, filed only as schema proposals ([docs/design/SIGNALS.md](docs/design/SIGNALS.md)) | `onto call signals.feed_add '{"name":"wire","url":"https://…/rss"}'` · `onto call signals.recommend '{}'` |
| **Hybrid personalised search** — BM25 in memory + hashed vectors (typo/morphology tolerant) in pgvector, fused and boosted by your graph neighbourhood and recency | `onto call entity.search '{"query":"paymnt timeout"}'` |
| **Expert finding** with reasons | `onto experts postgres` · `onto ask "Who knows about PostgreSQL?"` |
| **Clarifying questions** instead of guessing on ambiguous names | `onto ask "What is #182?"` |
| **Graph profile** grounds prompts in patterns that exist; **drift suggestions** for data the schema doesn't cover | `onto schema profile` · `onto schema suggest` |
| **MCP server** for agents (same permissions, writes only via action types), with tool profiles per job | `onto mcp` · `onto mcp --profile reasoning` |
| **Evaluation** with competency questions and evidence | `onto eval` (22/22 on the demo, 86% answered without the LLM) |
| **Export** — Turtle (OWL + PROV-O provenance) and Mermaid | `onto export --format mermaid --start project:alpha` |

## A data catalog that knows what to trust

Connect a database and onto-intelli maps the data estate from its metadata,
never its rows (unless you opt a table into [row sync](docs/ONTOLOGY_BUILDER.md#row-sync)),
the way Databricks Genie's knowledge layer does
([docs/CATALOG.md](docs/CATALOG.md)):

- **Connectors** crawl schemas, tables, views, columns, comments, foreign
  keys, grants and query usage (`pg_stat_statements`) in the background,
  incrementally, through the audited write path with provenance.
- **OntoRank** scores every asset for authority: PageRank over who reads
  what, usage, freshness, ownership, documentation and certification. Each
  component is stored, so `onto rank <id>` explains the score. Search and
  context packing put authoritative assets first.
- **A learned layer** infers join paths, metric definitions and synonyms
  from constraints, the query log and names, each with a confidence and its
  evidence. People certify or reject them; the learner never overrides them.
- **Questions answered from the graph:** "Which table holds revenue?", "How
  do orders join to customers?", "Which revenue metric is the trusted one?"
  are deterministic: 0 tokens, with evidence.

What Genie does not offer:

| | |
|---|---|
| **Runs anywhere** | One binary + PostgreSQL, next to any database, cloud or laptop; no Databricks workspace, Unity Catalog or warehouse to buy into. |
| **Governed actions with undo** | Changes go through typed, role-checked action types with preconditions, risk levels and an inverse plan; every apply leaves an audit event and an undo receipt. Certifying a fact is one of them. |
| **Evidence paths and scores** | Every answer returns the facts and graph paths it used; every authority score keeps its components and their inputs. |
| **Time travel and what-if** | Every write is versioned: history, as-of reads, a history slider on the map, and what-if scenarios on an isolated fork. |
| **Zero-token graph answers** | Ownership, blockers, impact, counts, joins, data locations and trusted metrics come from the graph without calling a model; the LLM only gets ranked, permission-filtered facts when reasoning is needed. |

### Use from an MCP client (Claude Code, IDEs)

```json
{ "mcpServers": { "onto-intelli": { "command": "onto", "args": ["--config", "/path/to/onto-intelli.toml", "mcp"] } } }
```

Tools: `ask`, `search`, `get_entity`, `neighbors`, `find_data`, `join_path`, `trusted_metric`,
`authority`, `learned_facts`, `find_entities`, `explain_blockers`, `impact`, `dependencies`,
`critical_path`, `metric`, `history`, `changes`, `experts`, `data_quality`, `what_if`,
`list_actions`, `plan_action`, `get_schema`, `plan_schema_change`, `propose_schema_change`,
`list_proposals`, `get_proposal`, `draft_ontology`, `refine_ontology`, `advise_ontology`,
`propose_ontology`, `object_types`, `explore_objects`, `query_inferred`, `explain_fact`,
`verify_claims`, `check_slice`, `certify_reasoning`, `validate_shapes`, `induce_shape`,
`check_shapes`, `lint_schema`, `list_packs`, `show_pack`, `plan_pack`, `install_pack`,
`align_schema`, `induce_sheet` (+ `apply_action`, `undo_action`, `crawl` when
`[mcp] allow_writes = true`). `propose_ontology`, `propose_schema_change` and
`install_pack` only ever file a proposal.

A profile narrows the list to one job: `full` (default), `catalog`, `authoring`,
`reasoning`, `governance`, `retrieval` or `evaluation`, each with `ask`, `search`,
`get_entity` and `get_schema`. Set it with `onto mcp --profile <name>`,
`ONTO_MCP_PROFILE` or `[mcp] profile`, in that order of precedence; an unknown name
refuses to start. The profile tables are in [docs/ASSURANCE.md](docs/ASSURANCE.md#mcp-tools-and-profiles).

## Build an ontology from your database

Like Palantir's ontology builder: a crawled source becomes object types
backed by its tables, link types from its foreign keys and join tables, and
action types to create, edit, transition and relink objects. The draft is
deterministic and explains every decision; the LLM can refine it and answer
modeling questions. People with an apply role apply it, everyone else and
every agent proposes it. Tables you opt in (`backing.sync`) become objects
on every crawl, incrementally, and the Object Explorer filters, facets and
summarizes them ([docs/ONTOLOGY_BUILDER.md](docs/ONTOLOGY_BUILDER.md)).

```bash
onto connect postgres env:SHOP_URL --name shop
onto model draft shop                       # object, link and action types, each with its reason
onto model refine shop                      # suggestions from the LLM, kept only if the schema stays valid
onto model advise "Which link types should orders have?"
onto model apply shop                       # a new schema version, then a crawl syncs the rows (or --propose)
onto explore order --filter 'amount>=100' --sort=-amount
```

In the web UI: **Builder** (`#/build`) and **Explorer** (`#/explore`).

## Install

| Option | Command |
|---|---|
| Release binary | `curl -fsSL https://raw.githubusercontent.com/f8fvwgzc/onto-intelli/main/packaging/install.sh \| sh` |
| From source | `cargo install --path crates/onto-cli` (or `make install`) |
| Docker | `ONTO_TOKEN=$(openssl rand -hex 24) docker compose --profile server up --build` (onto + PostgreSQL) |
| Homebrew | formula template in [`packaging/homebrew/onto-intelli.rb`](packaging/homebrew/onto-intelli.rb) |
| Image | `ghcr.io/f8fvwgzc/onto-intelli:<version>` (linux/amd64 and arm64, from the first tagged release) |
| Kubernetes | `helm install onto oci://ghcr.io/f8fvwgzc/charts/onto-intelli --version <version> --set existingSecret=onto` ([chart](deploy/helm/onto-intelli/values.yaml); the Secret holds `ONTO_DATABASE_URL` and `ONTO_TOKEN`) |

One binary (`onto`, ~13 MB release) plus PostgreSQL 16 or newer with the
[pgvector](https://github.com/pgvector/pgvector) extension: the `pgvector/pgvector` images,
or any managed PostgreSQL that offers it. The binary checks the server version, creates the
extension and applies its migrations on first connect. Protos are compiled with
[protox](https://github.com/andrewhickman/protox), so there is no `protoc` and no other system
library to install.

## Quick start

```bash
make db-up                      # PostgreSQL 16 + pgvector on 127.0.0.1:5441 (docker compose)
export ONTO_DATABASE_URL=postgres://onto:onto@127.0.0.1:5441/onto
mkdir my-org && cd my-org
onto init                       # onto-intelli.toml + protos/acme/tickets/v1/tickets.proto
onto seed demo                  # demo workspace "acme"
onto ask "How many open issues does the Payments Team have?"
onto ask "Why is Project Alpha blocked?" --context      # show the facts sent to the model
onto show issue:421             # entity card, neighbours, derived relations
onto deps project:alpha --cause # root-cause chains
onto deps project:alpha --plan  # execution order + critical path
onto ask "Which table holds revenue?"  # the demo warehouse catalog; see docs/CATALOG.md
onto rank table:warehouse.sales.orders # OntoRank and why
onto chat                       # multi-turn, remembers what "it" refers to
onto pack install manufacturing --sample  # an industry pack and its sample data
onto reason run                 # derived facts per rule, and what the reasoner did not evaluate
onto reason certify --out cert  # a certificate, checked by onto-cert
onto cert check cert            # check it again offline: exit 0 accepted, 1 rejected, 2 malformed
onto shapes validate            # conforms, violates or undetermined
onto lint                       # schema findings with fixes
export ONTO_TOKEN=$(openssl rand -hex 24)
onto serve                      # gRPC + HTTP on 127.0.0.1:50051 (token auth)
onto serve --dev                # local development: no auth, loopback only
```

`onto serve` checks its whole configuration (the port, auth tokens, the console)
before it prints its banner, so a bad config never looks like a running server.
On SIGTERM or Ctrl-C it shuts down gracefully within `[grpc] shutdown_grace_secs`
(20): change streams get a final `end` frame to resume from, and the leases and
claimed outbox rows it holds are released so another replica takes over at once.
In the docs run (docs/AUTOMATE.md, "A restart") the server was gone 0.03 s after
SIGTERM, and the restarted one delivered a change made while it was down 0.28 s
after it started. A killed process still leaves its leases to expire (up to 60 s).

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

On a platform, the shared **LLM connection contract v1** configures the same model access for
every core: `LLM_PROVIDER` (`anthropic`, `openai`, `openai-compatible`), `LLM_BASE_URL`,
`LLM_API_KEY_FILE` or `LLM_API_KEY`, `LLM_MODEL`, `LLM_MODEL_SMALL`, `LLM_TIMEOUT_SECS`,
`LLM_MAX_RETRIES`, and `LLM_GATEWAY_URL` + `LLM_GATEWAY_TOKEN_FILE` (gateway mode: no provider key
in onto). `[llm]` keys in the file win; see [docs/EMBEDDING.md](docs/EMBEDDING.md#llm-connection-contract).

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
  `MemoryService`, `KnowledgeService` ([docs/KNOWLEDGE.md](docs/KNOWLEDGE.md)), `SignalsService`
  ([docs/design/SIGNALS.md](docs/design/SIGNALS.md)),
  `EventService` (incl. client-streaming `Ingest`), `AdminService`;
* `SchemaService`: the live schema and proposals ([docs/SCHEMA.md](docs/SCHEMA.md));
* `AssuranceService`: the reasoner and certificates, shapes, lint, locks, alignment,
  packs and sheet induction ([docs/ASSURANCE.md](docs/ASSURANCE.md));
* `OntologyBuilderService` and `ObjectService`: the ontology builder and the Object Explorer
  ([docs/ONTOLOGY_BUILDER.md](docs/ONTOLOGY_BUILDER.md));
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

## Control plane

`onto serve` can also serve a hosted console, the way Clerk, Supabase and
Langfuse do, from the same binary and PostgreSQL database:

* **Organizations and sign-in**: members with roles (owner, admin,
  developer, viewer), invitations, and sign-in through any OpenID Connect
  provider (Clerk, Auth0, Google, Keycloak, Supabase Auth), a bootstrap
  admin token, or `--dev`.
* **Projects and API keys**: a project is a tenant (its own PostgreSQL
  schema); `onto_sk_…` keys with scopes `read`, `write`, `agent` and
  `admin`, shown once, rotated and revoked individually; a **Connect** page
  with curl, TypeScript, Python, MCP and monorepo snippets.
* **Bring your own LLM**: each project sets its own provider, model and key
  (encrypted with AES-256-GCM under `ONTO_MASTER_KEY`), monthly budgets and
  a rate limit, with daily usage and cost.
* **Monitoring**: every question, governed action and model call becomes a
  trace with spans (intent, resolution, retrieval, generation, tools),
  token counts, cost and scores; sessions replay a conversation; metrics
  show requests, errors, latency percentiles and cost per model.
* **For services**: SDKs for [TypeScript](sdk/typescript/README.md) and
  [Python](sdk/python/README.md), MCP over HTTP at `/mcp`, and
  `onto connect repo` to map a monorepo's packages, dependencies and
  CODEOWNERS into the ontology.

A minimal console with a bootstrap admin token:

```toml
# onto-intelli.toml
[engine]
tenancy = "multi"

[console]
enabled = true
public_url = "http://127.0.0.1:50051"   # https:// unless loopback
```

```sh
export ONTO_DATABASE_URL=postgres://onto:onto@127.0.0.1:5441/onto
export ONTO_SESSION_SECRET=$(openssl rand -hex 32) ONTO_MASTER_KEY=$(openssl rand -base64 32)
export ONTO_ADMIN_TOKEN=$(openssl rand -hex 32)
onto serve        # open http://127.0.0.1:50051/ui/ and choose "Sign in with admin token"
```

`onto serve --dev` (loopback only) signs in automatically. Then, from a
service, with a key from **API keys**:

```ts
import { Onto } from "onto-intelli";
const onto = new Onto({ url: "http://127.0.0.1:50051", apiKey: process.env.ONTO_API_KEY });
console.log((await onto.ask("Who owns Project Alpha?", { sessionId: "chat-42" })).answer);
```

```sh
onto --remote http://127.0.0.1:50051 --token "$ONTO_API_KEY" \
  connect repo https://github.com/acme/platform --every 1h   # needs an admin-scope key
```

[docs/CONTROL_PLANE.md](docs/CONTROL_PLANE.md) is the full reference: data
model, roles, keys, sign-in with an IdP (section 15 walks through Clerk),
BYO LLM, traces, routes, the repository connector and security.
[docs/UI.md](docs/UI.md) describes the console pages.

## Embedding

**Rust, async** ([example](crates/onto-intelli/examples/embed_async.rs)):

```rust
let intelli = Intelli::open(Config::load("onto-intelli.toml")?).await?;
let ctx = ActionContext::new(Principal::admin("app"), "acme");
intelli.ontology().upsert(&ctx, EntityInput::new("issue", "9", "Login fails").state("open"))?;
let answer = intelli.ask(&Principal::admin("app"), AskRequest::new("Who owns issue #9?")).await?;
```

**Many tenants in one process** (`Hub`): one ontology per tenant (a PostgreSQL schema, or
memory), opened on first use; `Hub::with_postgres(cfg, PgShared::from_pool(app_pool)?)`
shares the application's sqlx pool. See [docs/EMBEDDING.md](docs/EMBEDDING.md).

**Rust, sync** ([example](crates/onto-intelli/examples/embed_sync.rs)): `BlockingIntelli`
owns a small runtime; the graph API (`Ontology`) is synchronous anyway.

Feature flags: `onto-intelli = { default-features = false }` drops PostgreSQL (in-memory
only: tests and demos) and the HTTP providers; `onto-core` with `default-features = false` is
the ontology engine with no async runtime.

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
Sections: `[engine]` storage, database and workspace, `[ontology]` schema layers, `[connectors]`
background crawls, `[llm]`, `[context]`
token budget and expansion, `[cache]`, `[memory]`, `[reasoning]` deterministic intents,
`[grpc]` listener and routes, `[mcp]` agent principal, writes and tool profile, `[auth]`, `[console]` the control plane, `[traces]` what is
recorded and for how long, `[changes]` change-feed retention and stream limits, `[webhooks]` delivery,
retries and operator allow rules, `[automation]` limits, guards and the enablement TTL, `[writeback]`
(off by default). Lookup order: `--config`, `$ONTO_CONFIG`,
`./onto-intelli.toml`, `~/.config/onto-intelli/config.toml`.

## Memory

* **Episodic**: every answered question (asker, entities, answer), expires after
  `episodic_ttl_days`. Asking the same question again updates its episode in place.
* **Long-term**: `memory.remember` facts and preferences (shared or private), plus summaries
  promoted by consolidation when the same entities keep coming up.
* **Writes are deduplicated without an LLM.** A repeat of a fact merges into it (and
  reinforces it). A small change to a fact about the same entities, such as a different
  name, number or negation, **supersedes** it: the old item gets `valid_to` and
  `superseded_by` and is kept for `superseded_ttl_days`, not deleted. `supersedes: <id>`
  replaces a memory explicitly, `supersede: false` turns the automatic rule off, and
  `valid_from` dates a fact. Forgetting the replacement reopens the old fact.
* **Recall** returns current memories by default. `as_of` (ms) recalls what was true then,
  and `include_superseded` returns history too. The score blends saturating BM25, entity
  overlap, ACT-R activation (how often and how recently a memory was used) and salience.
  Near-duplicates are dropped (MMR), so a summary and the episodes it came from are not
  packed together, and the ask path stops at `recall_budget_tokens` (default 600).
  Earlier answers to the *same* question are never fed back as evidence; long-term facts
  rank with graph facts, episodes rank below them.
* **Consolidation** (`memory.consolidate`, and every `consolidate_every` asks in the
  background) persists access statistics, expires old items, writes one summary per
  recurring topic under a deterministic id (replicas upsert the same row), prunes
  consolidated episodes nobody recalled once they fade (about 14 days), and caps episodes
  and summaries per subject. A pass that changes nothing visible leaves the answer cache
  alone, and the cache revision is per workspace.
* `memory.stats` (also under `memory` in `system.stats`) reports item counts and this
  process's counters: merged, superseded, pruned, evicted, reinforced, recalls and recall
  tokens. Design notes and sources: [docs/MEMORY_RESEARCH.md](docs/MEMORY_RESEARCH.md).

## Project layout

```text
crates/onto-core      ontology: schema, graph, store (PostgreSQL/memory), events, policy, rules, deps, BM25
crates/onto-intelli   orchestrator: intent, resolution, context graph, memory, LLM providers, caches, actions
crates/onto-grpc      gRPC/HTTP server: built-in proto, dynamic proto routes, reflection, health, SSE, console API, MCP over HTTP
crates/onto-control   control plane: organizations, members, projects, API keys, OIDC sign-in, BYO LLM vault, usage
crates/onto-cli       the `onto` binary
crates/onto-cert      the independent certificate checker (`onto-cert`; std, sha2 and serde_json only)
crates/onto-ffi       C ABI (cdylib + staticlib) and header
examples/             FFI demos, JSONL ingest sample, schema extension
packaging/            install script, Homebrew formula; Dockerfile and CI at the root
docs/ARCHITECTURE.md  design notes
docs/ASSURANCE.md     reasoner and certificates, semantic plans, shapes, lint, alignment, sheet induction
docs/PACKS.md         the ten industry packs
docs/CATALOG.md       connectors, OntoRank, learned facts
docs/ONTOLOGY_BUILDER.md  object, link and action types from a database; row sync; Object Explorer
docs/CONTROL_PLANE.md the console: sign-in, projects, API keys, BYO LLM, traces, SDKs, repository connector
docs/AUTOMATE.md      change feed, webhooks, automations and writeback, with a real run and benchmarks
docs/STORAGE.md       PostgreSQL: migrations, tables, consistency across processes, rollbacks
docs/UI.md            the web UI and console pages
docs/RESEARCH.md      what v0.2 took from Palantir, Databricks, Glean, open-ontologies and arXiv
sdk/typescript        TypeScript SDK (zero dependencies)
sdk/python            Python SDK (standard library only)
ui/                   the web UI (React); built into crates/onto-grpc/ui-dist
```

## Development

```bash
make db-up       # PostgreSQL 16 + pgvector on 127.0.0.1:5441; db-down stops it, db-reset wipes it
make test        # cargo test --workspace (PostgreSQL tests skip without DATABASE_URL)
make test-db     # the same with DATABASE_URL pointing at the db-up database
make lint        # fmt check + clippy
make release     # optimized binaries in target/release
make dist        # tarball for this platform in dist/
```

Tagging `vX.Y.Z` runs [`.github/workflows/release.yml`](.github/workflows/release.yml):
binaries for Linux (x86_64, arm64), macOS (arm64, x86_64) and Windows, plus the FFI library,
attached to a GitHub release with checksums.

## License

MIT
