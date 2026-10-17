# Architecture

onto-intelli turns an organization's execution data into an ontology an orchestrator can
query in microseconds, so the LLM only has to reason. These are the decisions behind it.

## 1. Things vs Events

```text
                 ONTOLOGY (graph, in memory + store)          EVENT STORE (append-only)
   Workspace ─ Team ─ Project ─ Issue ─ Task ─ ...      ◄──   who · did what · to which thing
   Knowledge · Decision · Artifact · Conversation              when · outcome · request id
```

A file is a Thing; uploading it is an Event. The graph holds stable entities, their
relations, state and provenance. Everything that *happened* goes to the event store and
references things by id. 10,000 users × 100 actions/day is 365M events/year — none of them
become graph nodes, so traversal cost stays proportional to the organization, not its history.

## 2. Request ≠ action ≠ execution ≠ result

Every write goes through `Ontology::apply(&ActionContext, mutations)`:

```text
ActionContext (actor, workspace, request_id, intent, source)
   → authorization (workspace isolation, role, entity ACL)    — denials are audited too
   → validation against the schema (kinds, states, property types, relation endpoints)
   → staged copy-on-write overlay (batch is all-or-nothing)
   → durable write (entities, links, events in one SQLite transaction)
   → graph + index update, revision bump, change listeners
```

So no caller can mutate the ontology without leaving actor + context + authorization +
target + execution + result behind.

## 3. Provenance is first class

Artifacts and knowledge carry `source` (upload, github, ai_generation, ...), `derived_from`,
`generated_by` and `run`. These also become `derived_from` / `generated_by` edges, so
"why does this Terraform file exist?" walks: artifact → issue → service → agent → run.

## 4. Two speeds

| Component | Mode | Why |
|---|---|---|
| Ontology queries | sync, in-process | answered before the LLM call, microseconds |
| Agent retrieval / context graph | sync | small, deterministic, permission-filtered |
| LLM | async, streaming | the only slow step |
| Ingestion (webhooks, bulk, connectors) | async queue | queries never wait on writes |
| Audit | inline, cheap | one row per action in the same transaction |
| Caches | sync | keyed by graph revision — invalidation is free |

`IngestQueue` applies mutations in order on a background worker; `ingest.flush` waits for it.

## 5. The orchestrator owns retrieval

```text
User → Orchestrator → deterministic ontology retrieval → small context → LLM → answer
```

not `User → LLM → "figure out the company" → database`. The pipeline:

1. **Intent** — rules classify owner / assignee / status / members / count / list /
   blockers / dependencies / impact / relation / workload / history / lookup / summarize /
   reasoning, plus kind and state filters, "why", and "my".
2. **Entity resolution** — explicit ids, `#421`, longest-n-gram name and alias matches
   (preferring kinds named in the question), trigram fuzzy matching, pronoun carry-over from
   the conversation, "me" → the asker's entity.
3. **Deterministic answer** when the intent allows and resolution is confident.
4. Otherwise the **Question Context Graph**: breadth-first expansion weighted by relation
   weight × intent boost × hop decay; inference rules; root-cause chains (deepest culprit
   first) for blocker and "why" questions; impact sets; recent *domain* events (not Q&A or
   CRUD bookkeeping); BM25 knowledge hits; recalled memory. Each becomes a fact line with a
   score and a token estimate, then greedy budget packing keeps the best.
5. **Prompt** = stable system prefix (instructions + schema summary, cache-friendly) +
   folded conversation history + facts + question.
6. **Remember**: conversation turn (and `conversation -about-> topic` edges), episodic memory,
   `intelli.ask` audit event, caches.

The split of responsibilities:

* Ontology — what exists and how it relates.
* Orchestrator — what is relevant to this question, for this caller, right now.
* LLM — what it means and how to explain it.
* Event store — what happened.

## 6. Ontology as a semantic layer, storage kept simple

The schema (types, relations with inverses and weights, rules, analysis settings) is data,
loaded from TOML and layered: built-in → organization extensions. The graph is an in-memory
index over a pluggable `Store`; SQLite (WAL, bundled) is the default, `MemoryStore` for tests
and embedding. Specialized stores (Postgres, a graph DB, a vector index) can implement the
same trait when a workload needs them — not before.

## 7. One action surface

`Intelli::call(principal, action, json)` is the single dispatch table. The built-in gRPC API,
user protos, HTTP JSON, the CLI and the C ABI are all thin transports over it. The built-in
proto is itself served dynamically (descriptor + route table), which is why user protos
behave exactly like first-party ones.

## 8. v0.2: governed change, time, and evaluation

* **Action types** are the only edit path offered to agents: a plan validates
  parameters and preconditions, renders effects against the current graph and
  derives an inverse per effect. Risk and reversibility are known before
  anything runs; high-risk or irreversible plans need `confirm`. Applying commits
  the edits, an `action.<name>` audit event and the undo receipt in one
  transaction. Undo refuses when touched entities changed since (revision check)
  unless forced.
* **Versions**: every committed entity state (and deletion) is written to
  `entity_versions` in the same transaction as the change. History, as-of reads,
  change feeds and lead-time/throughput metrics read it; the hot graph stays a
  current-state index.
* **Forks** clone the in-memory indexes onto a `MemoryStore`, so scenarios run
  every read algorithm unchanged and can never leak writes.
* **Merges** move links, fill properties and aliases, delete the duplicate and
  record a redirect in the same transaction; reads and writes follow redirects.
* **Prompts** use the populated pattern profile (count-free, sorted) instead of
  the full schema; it changes only when a new pattern appears, keeping provider
  prompt caches warm.
* **Evaluation** is part of the product: competency questions with expected
  substrings, modes, intents and entities, reported with their evidence.

See [RESEARCH.md](RESEARCH.md) for the sources of each decision.

## Performance notes

* Reads take a single `RwLock` read guard; context building does all graph work under one
  guard and calls nothing re-entrant.
* Writes validate against a copy-on-write overlay and touch the graph only after the durable
  write succeeds.
* Revision-keyed caches make invalidation O(1); episodic memory does not bump the memory
  revision, so ordinary traffic keeps answer caches warm.
* Ids are time-ordered with a process-wide sequence, so events sort correctly within a
  millisecond.
