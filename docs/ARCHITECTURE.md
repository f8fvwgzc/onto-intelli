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
   → durable write (entities, vectors, links, events, history, change records and
     outbox rows in one PostgreSQL transaction)
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
| Outbound (webhooks, writeback) | async, outbox | commits never wait on receivers |
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
index over a `Store`. PostgreSQL 16+ with pgvector is the durable store, shared by any number
of processes: JSONB documents, an append-only event log, history, a commit log and the vector
index. `MemoryStore` is for tests and demos. Specialized stores (a graph DB, another vector
index) can implement the same trait when a workload needs them — not before.

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

## 9. The assurance layer: inference is not assertion

The reasoner ([ASSURANCE.md](ASSURANCE.md)) derives facts from the graph and
the schema's rules, and the design keeps three things apart.

**Stored facts and derived facts.** The reasoner reads the visible graph as
triples, computes a closure in memory and never writes it back. A derived
fact is labelled `derived` and carries the rule and premises that produced
it. The graph stays what people and connectors wrote; what follows from it is
recomputed per caller (permissions apply, as for any read) and cached by graph
revision and schema version, like the answer caches.

```text
   graph (stored) ──► asserted facts ──► reason::closure ──► derived facts (rule + premises)
   schema ─────────► rule table ────┘          │
                                              ├──► queries, explanations, plans, shapes (inferred)
                                              └──► certificate files ──► onto-cert ──► verdict
```

**Built-in semantics and supplied rules.** Kind inheritance, interfaces and
transitive and symmetric link types are a fixed table compiled into both the
engine and the checker. Schema `[[rules]]`, an organization's or an industry
pack's, are supplied rules: assumptions that a certificate carries, and that
change its verdict word.

**Engine and checker.** onto-core computes the closure and writes the
certificate; it only ever says `derived`. `crates/onto-cert` checks it: a
separate crate that depends on the standard library, `sha2` and `serde_json`
only (a test fails if it ever depends on an `onto-` crate), reimplements the
format from its specification, and re-derives each line by pattern, without
search. Its report type has a private constructor and no `Deserialize`, so a
verdict cannot be produced anywhere else, not even by parsing one. A checker
that shared code with the engine would share its bugs.

**Plans that reason.** A schema plan runs the reasoner twice over the same
current graph, under the current and the planned schema, and reports the
difference: the facts that appear and disappear, conservativity over the
existing vocabulary, a risk word, lock violations and the shape delta. Blast
radius alone misses changes like a link type made transitive, which touch no
entity and still change what the ontology says.

**Measurements are not proofs.** Shapes, lint, alignment and sheet induction
are separate modules that measure or score; each report says which in its
`means` field. Only a checked certificate is a proof, and only under its rule
table.

**Bounded, and honest about it.** A closure is bounded by derived facts,
rounds and the candidate facts its rule joins try; a query by its atoms,
bindings and join work; shape validation by path steps and path work. The
facts are collected under the graph's read guard and the closure runs after
it is released. A run cut by a bound says so: a closure that stopped is a
lower bound (its certificate still checks, and says so), a cut shape run
never `conforms`, and a plan whose closures stopped or refused terms is
`undetermined` rather than conservative.

## 10. Events out: change feed, outbox, Automate

Phase 1 of [design/EVENTS.md](design/EVENTS.md); [AUTOMATE.md](AUTOMATE.md)
shows it running.

```text
 commit (Ontology::apply / transact)
   ├─ graph edits, events, history
   ├─ change records (changes)  ──► changes.poll · SSE stream      (filtered per reader)
   │                           ──► fan-out ──► outbox rows (cursor only)
   │                           ──► automation consumer ──► run: one transact
   │                                   graph edits + receipt + outbox rows + run row + event
   └─ outbox rows of actions (writeback)
                         outbox ──► delivery worker ──► receivers
                                    (CloudEvents + Standard Webhooks, Idempotency-Key)
```

**Changes are computed where the write is.** `commit_locked` sees the graph
before the commit and the staged batch, so it is the one place that knows
old and new values for every write path. Records are written in the commit
itself. A state-changing commit that did not record changes (an older build,
a bulk import) becomes a `gap` record, made by a deferred trigger in
PostgreSQL. A reader learns that it missed something; it is never told
nothing happened.

**Outside effects go through one outbox.** Rows from actions and automation
runs commit with the change that caused them, so a change and its delivery
are never split by a crash. Change-sourced rows hold only the cursor and are
rendered, and checked for visibility, when sent. Each row is enqueued once
and delivered at least once: receivers dedupe on the signed event id, and
writeback receivers on `Idempotency-Key`. External writes are not reversible, so a
plan with one needs confirmation and `action.undo` says what it did not undo.

**Automations are schema; running them is a person's decision.**
Definitions go through `schema.apply` or proposals (agents propose only).
Enabling binds to a hash of everything the automation reaches and expires,
because onto cannot tell when the person who enabled it loses the right to.
A run is claimed by `(automation, trigger_key)`, so a duplicate is settled by
the claim. Its effects commit in one fenced transaction, so a crash leaves a
`running` row that recovery executes again.

**One worker per job per tenant, by the database clock.** Fan-out and the
automation consumer hold leases whose epochs fence their writes; leases,
claims and timers use `now_ms()` in SQL, since process clocks skew. Change
streams are pulled per subscriber from a per-tenant hub, and filtering runs
off the async runtime in slices, so a slow client or a large page never
holds the graph lock for long. Delivery concurrency limits are per process.

## Performance notes

* Reads take a single `RwLock` read guard; context building does all graph work under one
  guard and calls nothing re-entrant.
* Writes validate against a copy-on-write overlay and touch the graph only after the durable
  write succeeds.
* Revision-keyed caches make invalidation O(1); episodic memory does not bump the memory
  revision, so ordinary traffic keeps answer caches warm.
* Ids are time-ordered with a process-wide sequence, so events sort correctly within a
  millisecond.
