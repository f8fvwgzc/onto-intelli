# onto-intelli phase 1: change subscriptions, Automate, webhooks and writeback

Status: implementation design, revision 2. Written against the worktree `onto-intelli-wt/verify-345`
(main plus PRs #3 audit blockers, #4 LLM connection contract, #5 MySQL/SQL Server/MongoDB
connectors). Revision 2 folds in an independent critique of revision 1; every point was checked
against the code before it was accepted, adjusted or rejected (section 17 records each decision).
Every file, type and function named as existing was read in that tree; line numbers are from it.
Nothing here is built, and nothing was compiled or run (Docker is down; no cargo was run).

## 0. Scope

Phase 1 ships in this order, each slice usable on its own:

1. **Change feed**: capture, `changes.poll`, `changes.head`, HTTP SSE stream.
2. **Webhooks**: async delivery through an outbox; change-sourced subscriptions; signed envelopes.
3. **Automate**: automations with `change` triggers, at most one `action` effect and any number of
   `webhook` effects.
4. **Writeback, HTTP only, async only**: an action-type effect that posts to a registered HTTP
   target through the outbox with an `Idempotency-Key`.

Deferred out of phase 1 (each adds a failure class the critique found; section 15 keeps the design
notes so the next phase starts from them): metric and shape monitors, cron schedules, sync writeback,
SQL writeback (PostgreSQL, MySQL, SQL Server), compensation and `outbox.compensate`, `emit` effects,
gRPC server streaming and the UI live tail, `changes.snapshot`, MCP and CLI breadth beyond the list in
section 7.

## 1. Decisions in one place

| # | Decision | Why (short) |
|---|---|---|
| D1 | A new append-only `changes` table is the change feed. Cursor = `(commit seq, n)`. A change row holds metadata and the old values of changed fields; the new entity document is joined from `entity_versions` by a new `change_seq` column, not copied. | The commit log lists ids only and is pruned at 100,000 seqs (postgres.rs:50). Joining avoids a second copy of every entity document. |
| D2 | Change records are computed in `commit_locked` (engine.rs:1191) from the pre-commit graph and the staged batch, and the batch is marked `captured`. Any state-changing commit that is not marked gets a `gap` record from the store (MemoryStore in `write`, PostgreSQL by a deferred trigger on `commits`). | `commit_locked` is the one place that sees old and new entities for every write path. The gap rule makes uncaptured writers (old replicas mid-deploy, `Store::bulk_import`, a future path that bypasses the engine) visible instead of silent. |
| D3 | Capture is a tenant property stored in the database (`changes_meta.capture`, default on), not process config. | A per-process switch lets two replicas disagree about one tenant. |
| D4 | A cursor is expired iff `cursor.seq < floor`, where `floor` is written by retention in the same transaction that deletes rows. The in-memory ring serves a cursor only when `cursor >= covered_from`. | Seq gaps are normal, so "older than the oldest kept row" cannot tell pruned from never-existed. |
| D5 | Streams are pull-based per subscriber (bounded channel, `send_timeout`), fed by one per-tenant `ChangeHub`; producers hold `Weak<Intelli>`; subscribers with the same `(scope_key, filter)` share one evaluation per page, off the async runtime. | Memory per subscriber stays O(page); a stalled client is cut; filtering never runs on a runtime worker while holding the graph lock for long. |
| D6 | Transport in phase 1: HTTP SSE only (`GET` and `POST /v1/changes/stream`). Poll and head are unary on every surface. | The SDK SSE parsers exist (they parse `id:`; `retry:` is added). gRPC server streaming and WebSocket wait. |
| D7 | One `outbox` table. Rows from actions and automations are inserted in the same commit as the graph change. Change-sourced webhook rows hold only the change cursor; the body is rendered at send time with a visibility re-check as the endpoint principal. | Small rows, no stale payloads after a permission change, one place for retries, replay and dead letters. Each change is enqueued once per endpoint and delivered at least once. |
| D8 | Fan-out keeps one cursor per endpoint. | A paused or backlogged endpoint resumes from its own cursor instead of losing what passed while it was paused. |
| D9 | Automation definitions are a schema section (`[automations.<name>]`); enablement, run history and cursors are operational state. Enablement binds to a closure hash: the automation plus every action type, endpoint and writeback target it reaches. | Definitions get proposals, lint, diff and revert. A change anywhere in the closure stops the automation until a person enables it again. |
| D10 | Runs are claimed first (`INSERT … ON CONFLICT (automation, trigger_key) DO NOTHING RETURNING id`) in their own statement, then executed in one `transact` that re-plans against the locked graph and commits the graph edits, receipt, outbox rows and the finished run row together, fenced by the consumer lease epoch. | A duplicate trigger is skipped before any work, never retried; a crash leaves a `running` row that recovery re-executes safely, because the second commit is atomic. |
| D11 | No sync external effects in phase 1. Writeback is async through the outbox, HTTP only, with `Idempotency-Key: <receipt>:<effect>`. A writeback effect makes the plan not reversible, so it needs a person's confirmation unless the action type declares `unattended` for agents or automations (a schema-governed, approved declaration). | Without 2PC, sync external writes can run twice or run with no record. Compensation is not an inverse, so "reversible" would be a false word. |
| D12 | Leases, outbox claims and timers use the database clock (`now_ms()` in SQL), and every lease has an epoch that commits check (fencing). | Process clocks skew; a paused process can wake up believing it still holds a lease. |
| D13 | Webhook envelope: CloudEvents 1.0 structured JSON; signature: Standard Webhooks (`webhook-id`, `webhook-timestamp`, `webhook-signature`). Decided by the owner on 2026-10-28 as the platform-wide format for all four cores (section 16 item 1). | Published standards with verifier libraries in most languages, instead of an onto-only format. |
| D14 | Mutating outbound configuration (endpoints, targets, enablement) requires the caller to dominate the principal the configuration acts as (section 6.1). | Admin roles are scoped to workspaces (policy.rs:23-24, 177-179); `is_privileged()` alone lets a narrower admin redirect a broader endpoint. |
| D15 | Service principal ids are reserved: `automation:`, `webhook:`, `connector:`, `system`. No credential may act as them. | `act_for` (auth.rs:211-258) accepts any `x-onto-act-as` id today, so a service token could read and undo every receipt an automation wrote. |
| D16 | `env:` and `file:` references are allowed only to the operator credential itself (config token or dev, no `act-as`), checked by an allowlist of actions that accept references. Errors never echo resolved values. | `guard_project_call` acts only when `credential.is_project()` (auth.rs:716); an impersonating config token is not a project credential. |
| D17 | The SSRF guard and HTTP client move from onto-control to `onto_intelli::net`; allow-list entries are `(host pattern, CIDRs)` set by the operator, never apply to project-registered endpoints or targets, and never open link-local or metadata ranges. URLs are built with `Url::join` and the host is asserted unchanged. | A per-server suffix allow-list is reachable by every tenant; a name can resolve to 169.254.169.254; string concatenation can change the host. |

Philosophy carried over: derived vs asserted and measured vs proved stay separate; refuse rather
than lie (an uncaptured commit becomes a `gap` record, an expired cursor gets `reset`, a missing
template value refuses the render, a hidden entity becomes `entity.hidden`, never a silent skip);
every write goes through `Ontology::apply`/`transact` with an `ActionContext`; agents propose and
never enable, approve or apply schema; output is sorted and deterministic. Words used in outputs:
"enqueued once, delivered at least once" (never "exactly once"); "not reversible" for external
effects (never "reversible" or "compensated").

## 2. What exists today (ground truth)

- **Commit path.** `Ontology::transact` (engine_sync.rs:43-65) holds the graph write lock, calls
  `build(self, &g)` (so a build closure sees the locked graph), then `commit_locked`
  (engine.rs:1191), which stages mutations, appends events, entity and link versions (engine.rs:1219-1259;
  every put entity gets a `VersionRecord` with the full doc, a delete gets one with `doc: None`), KV,
  sets `expect_seq`, calls `Store::write`, then patches indexes and calls listeners with the write
  lock held (engine.rs:1362). `Stale` triggers catch-up and retry; exhaustion becomes
  `Error::Conflict("write kept losing …")`.
- **apply_inner** (engine.rs:1163) passes a closure that ignores the graph and re-stages the same
  mutations; `apply_action` (action_types.rs:993) plans outside the lock and never re-checks
  preconditions at commit.
- **Store.** `trait Store` (store/mod.rs:204); `WriteBatch` (113); `CommitRecord` lists entity ids,
  link keys and KV keys plus a `reload` flag (55-69). The default `Store::bulk_import`
  (store/mod.rs:247-256) calls `self.write` directly, bypassing `commit_locked`; PostgreSQL
  overrides it with `copy_import` (postgres.rs:634, 1377); only onto-bench calls it today.
- **PostgreSQL write** (postgres.rs:854): locks `head` `FOR UPDATE` only for state-changing batches,
  compares `expect_seq`, writes rows, inserts the commit record, prunes commits every 1000 seqs,
  NOTIFYs inside the transaction (1131). Event-only batches take no lock and get no seq.
- **Errors.** `impl From<sqlx::Error> for Error` maps every error, including unique violations
  (SQLSTATE 23505) and pool timeouts, to `Error::Storage` (postgres.rs:326-329). `Error` has
  `Conflict`, `Unavailable`, `Limited` and `Stale` variants (error.rs:5-32).
- **Migrations.** A binary refuses to open a tenant whose schema is at a newer migration than it knows
  (postgres.rs:521), but a replica that already has the tenant open keeps writing.
- **Reads.** `Ontology::read` holds the graph read lock for the closure (engine.rs:510-514).
  `fork()` builds a `MemoryStore` copy (engine.rs:518-540) with the parent's options.
- **Diff.** `diff_fields` (engine.rs:200-221) compares title, state, props, aliases, provenance; not
  `workspace` or `kind`.
- **Action types.** `Effect` (action_types.rs:161-179): patch, link, unlink, create, delete, event.
  `ActionType.roles`/`risk` (182-200). Template `Env::lookup` reads any entity's fields through any
  string parameter without a visibility check (399-413); only `Entity`-typed parameters are checked
  (619). `render_value` drops object keys and array items whose placeholder has no value
  (`filter_map`, 439-458). `needs_confirmation = risk >= Medium || !reversible` (942). Agents cannot
  confirm (apply_action, the `is_agent` branch). `action_receipt` lets the receipt's actor read it
  (1077-1083) and `undo_action` builds on it (1087).
- **Principals.** Admin is "everything inside its workspaces" (policy.rs:23-24); `is_privileged()` is
  admin or system (166); `in_workspace` (177); `allows` (181); `scope_key` (193).
- **Auth.** `act_for` (onto-grpc auth.rs:211-258) builds any `x-onto-act-as` id for config tokens
  and API keys with `impersonate`; config tokens' own ids are only checked for reserved secret
  prefixes (296-300); API-key principals are `key:<prefix>` (661). `guard_project_call`
  (auth.rs:710-760) acts only for `credential.is_project()` and only on `connector.add`.
- **Surfaces.** `dispatch` (actions.rs:774) runs `agent_may_write` and read checks before every
  action; `run` (http.rs:189) adds `guard_project_call`; `ask_stream` (http.rs:618) is
  `POST`-with-JSON, calls `caller_full` (173) itself and bypasses `dispatch`.
- **SDK SSE.** `sdk/typescript/src/sse.ts` and `sdk/python/onto_intelli/sse.py` parse `event`,
  `data`, `id`; neither parses `retry:`.
- **Tenancy.** `Hub::enforce_limits` and `close_if_unused_since` (hub.rs:292-345) only remove the
  slot; anything still holding an `Arc<Intelli>` keeps it alive, and the next request opens a second
  `Intelli`. `estimated_bytes` (engine.rs:998) counts the graph only. `spawn_scheduler`
  (connectors.rs:980) holds a `Weak`.
- **Verified unchanged from revision 1:** `on_change` (engine.rs:400) has no callers; `try_lease`
  (engine.rs:415) commits KV through the commit log with a NOTIFY; the head-row lock gives
  seq-ordered visibility; `commit_locked` still has the pre-commit graph at capture time;
  `add_connector` writes KV and its event in two writes (connectors.rs:842-851).
- **Records.** docs/design/RECORDS.md is a proposal; no Records-stored type exists in code.

## 3. Ownership and boundaries

- onto-intelli owns: the ontology change feed; ontology-native automations (change triggers on its
  own data, effects that are its own governed action types); outbound webhooks of its own changes and
  events; HTTP writeback effects of its own action types.
- Not owned here: identity and service-principal issuance (Tutela; phase 1 declares `run_as` locally
  with an enablement TTL, section 16 item 2); approvals and the evidence ledger (attache); LLM or
  agent steps, schedules and general orchestration (texc-symphony; section 16 item 3); human
  notification channels (products); SQL writeback into customer databases (pending, section 16
  item 4).
- An automation that needs an LLM, a wait or a schedule hands off through a webhook; the consumer
  writes back through `action.apply` or a schema proposal like any agent, and may pass the cause
  depth it received (C.6).

---

## Part A. Change capture and subscriptions

### A.1 Types (new module `crates/onto-core/src/changes.rs`)

```rust
/// `"<seq>.<n>"`; totally ordered per tenant.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Cursor { pub seq: u64, pub n: u32 }               // Display/FromStr "seq.n"; serde as string

pub enum ChangeOp {                                           // serde names reuse the audit vocabulary
    EntityCreate, EntityUpdate, EntityDelete,                 // "entity.create" | "entity.update" | "entity.delete"
    EntityHidden,                                             // "entity.hidden": per subscriber, never stored
    LinkCreate, LinkUpdate, LinkDelete,                       // "link.*"; link.delete includes cascades
    SchemaChange,                                             // "schema.change"
    Gap,                                                      // "gap": state changed here without records
}

pub struct ChangeRecord {
    pub cursor: Cursor, pub ts: i64, pub op: ChangeOp,
    pub workspace: String,              // after the change (before, for deletes and hidden)
    pub kind: String,                   // entity kind, canonical rel, "schema" or ""
    pub id: String,                     // entity id, "from|rel|to", schema version or ""
    pub revision: Option<u64>,
    pub changed: Vec<String>,           // capture_fields order: title, state, props…, aliases, provenance, workspace, kind
    pub state: Option<StateMove>,       // {from, to}
    pub moved: Option<Moved>,           // {workspace?: {from, to}, kind?: {from, to}}
    /// Old values of the changed fields. Served only when the reader may see the entity both
    /// before and after the change.
    pub before: Map<String, Value>,
    /// The entity as the change left it (create, update). Joined from `entity_versions`; `None`
    /// for deletes, for `fields = "ids"`, and when the version row is gone (`entity_missing`).
    pub entity: Option<Entity>,
    pub entity_missing: bool,
    pub became_visible: bool,           // the reader could not see it before this change
    pub link: Option<LinkChange>,       // {from, rel, to, props, ends: [VisStub; 2]}
    pub schema: Option<SchemaMove>,     // {version, renames: [{what: kind|rel, from, to}]}
    pub gap: Option<Gap>,               // {reason: uncaptured_writer|bulk_import|capture_enabled, origin, entities, links,
                                        //  hidden, truncated, total}: at most 1000 ids and links listed (MAX_GAP_IDS);
                                        //  past that `truncated` and `total` (a bulk import is one gap of any size)
    pub cause: Cause,
}

pub struct Cause {                      // copied from the ActionContext
    pub actor: String, pub request_id: String, pub source: Option<String>, pub intent: Option<String>,
    pub automation: Option<String>, pub run: Option<String>, pub depth: u8,
    pub migration: bool,                // intent == "schema.migration" (live_schema.rs migration_ctx)
}

/// What `policy::visible` needs: id, kind, workspace, `visibility`, `acl`.
pub struct VisStub { pub id: Id, pub kind: String, pub workspace: String,
                     pub visibility: Option<String>, pub acl: Vec<String> }

pub struct ChangeFilter {               // streams, poll, webhook endpoints, automations
    pub kinds: Vec<String>, pub ops: Vec<ChangeOp>, pub workspaces: Vec<String>,
    pub ids: Vec<Id>,                   // ≤ 1000
    pub rels: Vec<String>,              // canonical, inverse or ^rel
    pub props: Vec<String>,             // any of these in `changed`
    pub sources: Vec<String>,           // glob on cause.source; "!x" excludes
    pub include_migrations: bool,       // default true (streams, poll, webhooks); automations default false
    pub fields: FieldMode,              // full | ids
}   // check(): ≤ 100 kinds, workspaces, rels and props each, ≤ 16 ops, ≤ 20 sources, names ≤ 256 bytes
    // (filtering runs per record for any reader); filters apply to the record the reader receives
    // (after `view`), so they cannot probe fields `entity.hidden` or `became_visible` leave out

pub struct ChangePage { pub changes: Vec<ChangeRecord>, pub next: Cursor, pub head: u64,
                        pub floor: u64, pub expired: bool }
```

What is stored per row (`StoredChange`, the `doc` column) is the record minus `entity`, plus
`before_vis: Option<VisStub>` (the visibility-relevant fields of the entity before the change) and
`rev_after` for the join. `entity.hidden` and `became_visible` are computed per reader and never
stored.

Pure functions:

- `pub(crate) fn capture(g: &Graph, batch: &WriteBatch, ctx: &ActionContext, schema: Option<(&Schema, u64, &[Rename])>,
  now: i64) -> Vec<StoredChange>`: entities in `put_entities` order (old = `g.get(id)`: none →
  create; `capture_fields(old, new)` empty → no record; else update with `before`, `before_vis`,
  `state`, `moved`), then `del_entities` (`before_vis` from `g`), then `put_links` (sorted; new or
  props changed), then `del_links` (sorted; end stubs from `g`), then one `schema.change` when the
  batch carries a schema (with the renames of the migration, so mirrors can rewrite). `n` counts from
  0 in that order. `capture_fields` = `diff_fields` (engine.rs:200) plus `workspace` and `kind`.
  `Schema::captures(kind) -> bool` (true for every graph kind today) is the hook that will exclude
  Records-stored types until Tutela field masks exist.
- `pub fn matches(f: &ChangeFilter, schema: &Schema, r: &ChangeRecord) -> bool` (kinds through
  `Schema::kind_is`, schema.rs:979).
- `pub fn view(p: &Principal, r: &ChangeRecord, before_vis: Option<&VisStub>, g: &Graph) -> Option<ChangeRecord>`:
  the record this reader gets, or none.
  - Entity create or update: `a = visible(p, after)`, `b = before_vis.map(visible)` (false for
    creates). `a && b` → the record with `before`. `a && !b` → the record without `before`,
    `became_visible = true` (mirrors treat it as an upsert). `!a && b` → `entity.hidden {id, kind,
    workspace}` with the before values of kind and workspace and nothing else (mirrors treat it as a
    delete). Neither → none.
  - Entity delete: visible iff `visible(p, before_vis)`; carries id, kind, workspace only.
  - Link changes: both ends visible, judged on the current graph entity when it exists, else on the
    stub. Phase 1 does not re-issue link records when an end becomes hidden; the docs tell mirrors to
    drop links whose ends they dropped.
  - `schema.change` and `gap`: anyone with `Access::Read` in a workspace of the tenant; a gap lists
    only the ids the reader can see in the current graph, plus a count of the rest.
  - Team membership in `acl` is judged as of delivery (current graph); stated in the docs.
- `pub fn redact(r: &mut ChangeRecord, mode: FieldMode)`.

`impl Ontology` (same file): `changes_page(&self, p, after, f, limit) -> Result<ChangePage>` (reads
`Store::changes_since`, evaluates `matches` and `view` under one `read(..)` per page, returns `next`
= the last cursor scanned so a client advances past filtered-out traffic, `expired = after.seq <
floor`); `change_head(&self) -> Result<Cursor>`.

### A.2 Capture and storage

- `engine.rs`: `commit_locked`, in the `tracks_commits` block before `expect_seq` is set: when the
  tenant captures (`Inner.capture: AtomicBool`, loaded from the store at open and refreshed by the
  `changes.capture` action and by followers on a `changes_meta` notice), set
  `staged.batch.changes = changes::capture(..)` and `staged.batch.captured = true`. A stale cached
  flag is harmless: captured-while-off writes extra rows; uncaptured-while-on gets a `gap` from the
  store.
- `fork()` (engine.rs:518) sets capture off for the fork (what-if edits never produce records).
- `ActionContext` (engine.rs:66) gains `#[serde(default)] pub cause: Option<RunCause>`
  (`{automation, run, depth}`); `ActionContext::new` leaves it `None`; `capture` copies it.
- `ChangeSet` (engine.rs:102) gains `pub seq: u64` (from the store's return and from `report.seq`
  in `catch_up_locked`), so listeners know the head without touching the lock.
- `store/mod.rs`:
  - `WriteBatch` gains `changes: Vec<StoredChange>`, `captured: bool`, `outbox: Vec<OutboxRow>`,
    `runs: Vec<RunRow>`, `fence: Option<Fence>` (Parts B and C). `CommitRecord` gains
    `#[serde(default)] captured: bool`.
  - `trait Store` gains defaulted methods: `keeps_changes() -> bool { false }`,
    `changes_since(after, limit) -> Result<ChangePage>` (default `Error::Unavailable("this store
    keeps no change log")`), `changes_meta() -> Result<ChangesMeta>` (`{capture, floor, head}`),
    `set_capture(on) -> Result<()>`. A store without a change log makes every subscription refuse
    with `unavailable`; it never returns an empty stream that looks healthy.
  - `MemoryStore`: a bounded `VecDeque<(StoredChange, Option<Entity>)>` (full docs, memory only);
    `write` stamps `seq`; a state-changing batch with `!captured` pushes one `gap` record (reason
    `bulk_import` when it came from `bulk_import`, else `uncaptured_writer`), so the default
    `Store::bulk_import` is covered. Evicting from the deque raises `floor`.
- `store/postgres.rs` `write` (854):
  - The head select becomes `SELECT seq, min_writer FROM head … FOR UPDATE`; a `min_writer` above
    this build's `WRITER_VERSION` refuses with `Error::Unavailable("tenant requires a newer
    onto-intelli")`. This only stops builds from this one onward; older builds do not read the
    column, which is why the gap trigger exists.
  - The new seq (`head + 1`) is known before the inserts, so entity version rows get
    `change_seq = seq`, change rows get `seq`, and the commit doc gets `captured`.
  - One multi-row insert into `changes` (UNNEST, chunks of `CHUNK`).
  - `copy_import` writes its commit with `captured = true` and inserts one `gap` row
    (`bulk_import`) itself.
  - `changes_since`: `SELECT c.seq, c.n, c.doc::text, v.doc::text FROM changes c LEFT JOIN
    entity_versions v ON v.id = c.target AND v.change_seq = c.seq WHERE (c.seq, c.n) > ($1, $2)
    ORDER BY c.seq, c.n LIMIT $3`, plus `changes_meta` and `head` in the same round trip.
- Migration `0009_changes.sql` (section 9) adds the deferred constraint trigger on `commits` that
  inserts a `gap` row when a state-changing commit (entities, links, the schema KV key, or `reload`)
  arrives without `captured` while `changes_meta.capture` is on.

Write amplification: one small row per changed entity or link plus one index entry on
`entity_versions`; no second copy of the document. Step 6 of the build order measures commit latency
with capture on (onto-bench) before the stream ships.

### A.3 Retention and expiry

`PgStore::vacuum` (postgres.rs:578) deletes `changes` rows with `seq < F` (F from `[changes]
retain_days`, default 7, and `retain_max_rows`) inside the `onto.retention` transaction (0008
pattern), deleting whole seqs only, and sets `changes_meta.floor = F` in the same transaction. A
periodic `spawn_retention` (intelli.rs, hourly like `spawn_trace_retention`) calls it with outbox and
run retention.

A cursor is expired iff `cursor.seq < floor`. A client fully caught up on seq `F - 1` is also
reported expired: that is a false `reset` at the exact retention edge, never a silent gap.
`changes.capture {on: true}` after a period off sets `floor` to the new head, so every older cursor
is expired (nothing was recorded while capture was off), and appends a `gap` (reason
`capture_enabled`) after every cursor at the head, because a reader that kept polling or streaming
while capture was off was moved to the head over the unrecorded commits. A cursor ahead of the head
(from before a restart, a re-provisioned tenant or a restore) is expired too.

Consistent resync is not offered in phase 1: there is no snapshot pinned to a cursor, and
`entity.find` paging is not consistent with any cursor. `reset` and `gap` say so in their `message`
("re-read with entity.find; the result is not pinned to a cursor"). `changes.snapshot` is planned
for phase 1.1.

### A.4 ChangeHub (per tenant, per process)

New module `crates/onto-intelli/src/changes.rs`:

```rust
pub struct ChangeHub {
    head: tokio::sync::watch::Sender<u64>,
    ring: parking_lot::RwLock<Option<Ring>>,       // allocated only while subscribers > 0
    backfill: tokio::sync::Semaphore,              // concurrent store reads for lagging subscribers (4)
    groups: Mutex<HashMap<GroupKey, Weak<Group>>>, // (scope_key, filter hash) -> shared evaluation
    subscribers: AtomicUsize, per_principal: Mutex<HashMap<String, usize>>,
}
struct Ring { covered_from: Cursor, records: VecDeque<Arc<(StoredChange, Option<Entity>)>>, bytes: usize }
impl ChangeHub {
    pub(crate) fn install(intelli: &Arc<Intelli>) -> Arc<Self>;
    pub fn subscribe(&self, intelli: Weak<Intelli>, p: Principal, f: ChangeFilter, start: Start)
        -> Result<Subscription>;                   // Subscription { rx: mpsc::Receiver<Frame>, cursor }
    pub fn bytes(&self) -> usize;                  // ring plus cached group pages, for Intelli::estimated_bytes
}
pub enum Start { Now, Earliest, After(Cursor) }
pub enum Frame { Change(Box<ChangeRecord>), Heartbeat { cursor: Cursor, head: u64 },
                 Reset { code: &'static str, floor: u64, head: u64 },
                 End { code: &'static str, cursor: Cursor, message: String } }
```

- `install` registers `onto.on_change(move |c| head.send_replace(c.seq))`; the listener runs under
  the graph write lock (engine.rs:1362) and does nothing else. A tail task (only while the ring
  exists) wakes on `head.changed()`, reads `changes_since(ring_last)` on `spawn_blocking`, appends,
  and on eviction sets `covered_from` to the evicted cursor. When the ring is first allocated,
  `covered_from` = the cursor the tail starts from.
- Ring rule: a cursor `c` is served from the ring iff `c >= covered_from`; otherwise from the store
  under the `backfill` semaphore.
- `Intelli` gains `changes: Arc<ChangeHub>`, `closing: tokio::sync::watch::Sender<bool>` and
  `pub fn estimated_bytes(&self) -> u64` (ontology plus ring). `Hub` sums `Intelli::estimated_bytes`
  and, when it removes a slot (`close_if_unused_since`), sends `closing = true`.

### A.5 Producers, backpressure and limits

One producer task per subscription, one bounded `mpsc::channel(64)`:

1. Wait for `head.changed()`, the heartbeat tick, or `closing`. On `closing`, or when the `Weak`
   no longer upgrades, end with `End { code: "tenant_closed" }`; the client reconnects and the
   request opens the tenant again. A producer never keeps a closed tenant's `Intelli` alive.
2. If `cursor.seq < floor`: `Reset { code: "cursor_expired" }` and end.
3. If `head - cursor.seq > max_backfill_seqs` (one unit: seqs): `End { code: "too_far_behind" }`.
   SDKs then page with `changes.poll` until within the limit and switch back to the stream, so no
   reconnect loop.
4. Read the next page (ring or store). Evaluation runs on `spawn_blocking` with a per-page budget
   (`eval_budget_ms`, 50): the group evaluates `matches` and `view` once per page for every
   subscriber of the same `(scope_key, filter)` and shares the result through `Arc`; a page that
   runs over budget is split. `read(..)` is taken per slice, not per page, so a writer waiting on
   the write lock waits at most one slice.
5. `tx.send_timeout(frame, stall_timeout)`; on timeout end with `slow_consumer`.
6. Every `heartbeat_secs` (15): `Heartbeat { cursor = last scanned }`.
7. Every `reauth_secs` (60): re-authenticate the original headers and re-derive the principal. A
   revoked credential ends with `unauthenticated`; a different `scope_key()` ends with
   `principal_changed`; the client reconnects with its new scope. A check that cannot be made (the
   control store is unreachable) is retried at the next period; two in a row end the stream with
   `unavailable`, so an unchecked stream runs at most two periods.

Limits: `max_subscribers` per tenant per process (200), `max_subscribers_per_principal` (20) and
`max_subscribers_process` (2000); over a limit, `subscribe` fails with `Error::Limited` (HTTP 429).

Tenant eviction: an open stream records `stream_touch` on its tenant slot on each heartbeat. The idle
check uses `max(last_used, stream_touch)`; eviction for `max_open_tenants` or the memory budget uses
`last_used` only, so streams cannot pin tenants past the limits; such an eviction ends the stream
with `tenant_closed`.

Ordering and delivery: frames are in cursor order per tenant; delivery is at least once across
reconnects. Exactly once is not offered.

### A.6 Transports

**HTTP SSE** (onto-grpc http.rs): `GET /v1/changes/stream` (filter as query: `kinds`, `ops`,
`workspaces`, `ids`, `rels`, `props`, `sources`, `fields`, `include_migrations`, `from=now|earliest`,
`after`) and `POST /v1/changes/stream` (JSON body, same keys). `Last-Event-ID` wins over `after`.
A GET SSE route with query filters is a new pattern here (`/v1/ask/stream` is POST with a JSON body,
http.rs:87, 618), so the handler `changes_stream` does what `run` (http.rs:189) and `dispatch` do
before an action: `caller_full`, the new `guard_call` (section 6.3), and `Intelli::admit(p,
"changes.stream")`, a new function factored out of `dispatch`'s pre-checks (actions.rs:774-781) and
called by both. Frames:

```
retry: 2000

event: change
id: 1234.0
data: {"cursor":"1234.0","op":"entity.update","kind":"issue","id":"issue:1","changed":["state"],...}

event: heartbeat
id: 1240.0
data: {"cursor":"1240.0","head":1240}

event: reset
data: {"code":"cursor_expired","floor":1100,"head":1300,"message":"..."}

event: end
data: {"code":"slow_consumer","cursor":"1260.3","message":"..."}
```

`gap` and `entity.hidden` arrive as `change` events with their `op`. The UI and SDKs use fetch
streaming (bearer token), as for `/v1/ask/stream`.

**Unary** on every surface: `changes.poll {after?, from?, limit?, <filter keys>}` → `{changes, next,
head, floor, expired}`; `changes.head {}` → `{cursor, head, floor, capture}`; `changes.capture {on}`
(admin over `*`). gRPC: `ChangeService/Poll`, `/Head`, `/Capture` through `BUILTIN_ROUTES`
(routes.rs:21). gRPC `Subscribe` is deferred.

### A.7 Correctness across replicas

- Order: `seq` is assigned under the head-row lock; change rows commit with their commit, so a change
  is visible iff its commit is, in seq order.
- Any replica serves any cursor; wake-ups use the existing follower (LISTEN plus
  `follow_interval_ms` poll), so a lost notice delays, never drops.
- Writers that do not capture (a replica still on the old build during a rolling deploy, the default
  `bulk_import`, any future path around `commit_locked`) produce `gap` records through the trigger or
  `MemoryStore::write`. Subscribers see the gap; nothing is skipped silently. An old-build replica
  writes only to tenants it already has open: once a tenant is migrated it cannot open it again (after
  idle or limit eviction), so drain old replicas or hold their tenants open for the roll
  (docs/STORAGE.md, upgrades and rollbacks).
- Gaps in `seq` are normal (KV-only commits); the cursor is an order, not a count.

### A.8 SDK helpers

- `sse.ts` and `sse.py` gain `retry:` parsing (exposed on the parsed message).
- TypeScript `sdk/typescript/src/changes.ts`: `onto.changes.subscribe(filter, { after?, from?,
  signal?, onReset?, onGap? })` → `AsyncGenerator<Change>`; reconnect with exponential backoff (0.5 s
  to 30 s, honouring `retry:`), `Last-Event-ID`, cursor tracking from `change` and `heartbeat`;
  `slow_consumer`, `tenant_closed` and `principal_changed` resume transparently; `too_far_behind`
  switches to `poll` paging; `reset` goes to `onReset` (default: throw `OntoError("cursor_expired")`).
  `onto.changes.poll`, `onto.changes.head`. Types in types.ts.
- Python `sdk/python/onto_intelli/changes.py`: the same over `_open(stream=True)` and `parse_sse`.
- Webhook verification (`webhooks.ts`, `webhooks.py`): Standard Webhooks verification with a
  constant-time compare and a tolerance (default 300 s); returns the parsed CloudEvent. Receivers
  dedupe on the envelope `id` (which is also the signed `webhook-id`).

---

## Part B. Outbox and webhooks

### B.1 Outbox rows (`crates/onto-core/src/outbox.rs`)

```rust
pub struct OutboxRow {
    pub id: String,                 // new_id("obx"); the CloudEvent id and webhook-id
    pub created_at: i64,
    pub kind: OutboxKind,           // webhook | writeback
    pub target: String,             // endpoint or writeback target name
    pub dedupe: String,             // unique per target: "<cursor>" | "<receipt>:<effect>" | "<run>:<effect>"
    pub source: String,             // change | action | automation | test
    pub workspace: String,
    pub receipt: Option<String>, pub run: Option<String>,
    pub cursor: Option<Cursor>,     // change-sourced: the only content; rendered at send time
    pub event_type: String,
    pub payload: Value,             // effect-sourced only: the strictly rendered body (≤ max_payload_bytes)
    pub cause: Option<RunCause>,
    pub state: OutboxState,         // pending | inflight | retry | delivered | dead | cancelled
    pub attempts: u32, pub next_at: i64,
    pub claim: Option<String>, pub lease_until: Option<i64>,
    pub last_status: Option<u16>, pub last_error: Option<String>, pub delivered_at: Option<i64>,
    pub otel_trace_id: Option<String>,
}
```

Store additions (defaulted; MemoryStore implements all; times are the database clock `now_ms()`):

- `outbox_enqueue(&[OutboxRow], advance: Option<(consumer, Cursor)>, fence) -> Result<usize>`:
  `ON CONFLICT (target, dedupe) DO NOTHING`, and the consumer cursor advance in the same
  transaction.
- `outbox_claim(limit, lease_ms, holder) -> Result<Vec<OutboxRow>>`: `UPDATE … SET state =
  'inflight', claim = gen_random_uuid(), lease_until = now_ms() + $lease, attempts = attempts + 1
  WHERE id IN (SELECT id … WHERE (state IN ('pending','retry') AND next_at <= now_ms()) OR
  (state = 'inflight' AND lease_until < now_ms()) ORDER BY next_at LIMIT $n FOR UPDATE SKIP LOCKED)
  RETURNING …`.
- `outbox_finish(id, claim, &Attempt, next_state, delay_ms)`: `WHERE id = $1 AND claim = $2`, so a
  worker whose lease expired cannot overwrite a newer claim; appends a `delivery_attempts` row.
- `outbox_list(&OutboxQuery)`, `outbox_reset(target, ids | since, state)` (replay).

Commit-time rows go through `WriteBatch.outbox`: `Extra` (engine.rs:174) gains `outbox` and `runs`,
`Extra::is_empty` includes them, `commit_locked` moves them into the batch, and PostgreSQL inserts
them in the commit transaction (commit-time dedupe keys are unique by construction).

### B.2 Endpoints (`crates/onto-intelli/src/webhooks.rs`)

```rust
pub struct Endpoint {
    pub name: String,                       // [a-z0-9_-], ≤ 40; principal id "webhook:<name>"
    pub url: String,                        // https; checked at write and at every delivery (B.5)
    pub secret: SecretRef,                  // current
    pub previous_secret: Option<SecretRef>, pub previous_until: Option<i64>,
    pub filter: Option<ChangeFilter>,       // change subscription; None = effect deliveries only
    pub principal: ScopePrincipal,          // {roles, workspaces}: what the receiver may see
    pub enabled: bool, pub paused: Option<Paused>,
    pub restricted: bool,                   // registered by a project credential (server sets it)
    pub version: u64,                       // bumped on every change; part of closure hashes
    pub created_by: String, pub created_at: i64,
}
pub enum SecretRef { Env(String), File(PathBuf), Sealed(String) } // "env:X" | "file:/p" | "sealed:v1:<b64>"
```

- Stored in tenant KV `webhook.endpoint`, written with audit atomically through a new
  `Ontology::commit_config(ctx, kv, event)` (`transact` with `Extra { kv, events }`) that accepts only
  `CONFIG_NAMESPACES = ["webhook.endpoint", "webhook.control", "writeback.target",
  "automation.state", "automation.control"]`.
- Who: admin (`is_privileged()`), never agents, and the caller must dominate the endpoint principal
  (section 6.1) on add, update, rotate, replay, remove, test, resume and deliveries. The endpoint
  principal may not hold `system` or `agent`; its workspaces must be a subset of the creator's.
- Secrets: `SecretRef::resolve(sealer)`; `env:`/`file:` only for the operator credential (D16). A
  plaintext `secret` input is sealed on the way in by `TenantSettings.secrets` (new
  `Option<Arc<dyn SecretBox>>`, hub.rs:45). An empty input generates a 32-byte `whsec_` secret,
  sealed and returned once. Without a sealer (self-hosted without `ONTO_MASTER_KEY`), plaintext and
  generated secrets are refused with `failed_precondition`; the operator registers `env:`/`file:`
  instead.
- `trait SecretBox: Send + Sync { fn seal(&self, purpose, name, plain) -> Result<String>; fn open(&self,
  purpose, name, sealed) -> Result<Secret>; }` (hub.rs). onto-control implements it in hooks.rs on
  `Vault::seal_for/open_for` (vault.rs) with AAD `onto/<purpose>/v1/<project>/<name>`; existing
  `seal`/`open` stay as the `llm-key` purpose.
- Schema renames: the `schema.apply` commit that renames a kind or relation also rewrites
  `filter.kinds`/`filter.rels` of every `webhook.endpoint` value in the same `Extra.kv`
  (live_schema.rs `rewrite_kind` 1229 / `rewrite_rel` 1320 gain a KV pass), bumping `version`.

### B.3 Envelope and signature

CloudEvents 1.0, structured JSON (the platform-wide format, decided by the owner on 2026-10-28; section 16 item 1):

```json
{ "specversion": "1.0", "id": "obx_…", "source": "onto-intelli://acme", "type": "onto.entity.update",
  "time": "2026-10-27T09:00:00Z", "subject": "issue:1", "datacontenttype": "application/json",
  "tenant": "acme", "workspace": "acme", "cursor": "1234.0",
  "causeautomation": "escalate_blocked_p1", "causerun": "run_…", "causedepth": 1,
  "data": { … the change record as the endpoint principal sees it, or the effect payload … } }
```

Headers (Standard Webhooks): `webhook-id: <id>`, `webhook-timestamp: <unix secs>`,
`webhook-signature: v1,<base64 HMAC-SHA256(secret, "<id>.<timestamp>.<body>")>` (a second
space-separated `v1,` value during rotation), plus `content-type`, `user-agent:
onto-intelli/<version>`, `traceparent`. The id is signed, and receivers dedupe on the envelope `id`.
Bodies over `max_payload_bytes` (256 KB) are not sent: `dead: payload_too_large`.

Change-sourced `data` is rendered at send time (B.4) from the change record, whose entity document
is the version that change wrote, so retries carry the same document unless visibility removed it.

### B.4 Delivery worker

`webhooks::spawn_delivery(weak intelli)` (started from `Intelli::open_with_store` when workers are
on; exits on `closing`):

- Wake: a local `Notify` on enqueue, the tenant's commit NOTIFY (outbox rows commit inside commits),
  and a fallback poll every `poll_secs` (20). `outbox_claim` up to 32 rows with a 60 s lease.
- Render: effect rows send their stored payload. Change rows read the change (`changes_get(cursor)`)
  and apply `view` as the endpoint principal on the current graph: not visible now → `cancelled:
  not_visible`; the change expired (`cursor.seq < floor`) → `dead: expired`.
- Request: POST, `timeout_secs` (10), no redirects, no proxy, the guarded client (B.5); response
  body read up to 4 KB for diagnostics; the endpoint is re-read so a URL change applies to rows not
  yet sent.
- Outcome: 2xx → `delivered`. 408, 409, 425, 429, 5xx, timeouts, connection errors → `retry` at
  `min(base * 2^(attempt-1), max) * jitter(0.8..1.2)` (10 s .. 1 h), honouring `Retry-After` up to
  the max. 410 → endpoint disabled, row `dead`. Other 4xx → `dead`. After `max_attempts` (15) →
  `dead`. Every `dead` writes a `webhook.dead` event.
- Concurrency: `max_in_flight_per_endpoint` (4) and `max_in_flight_per_tenant` (8) are per process,
  so the effective limit is that times the replicas holding the tenant; documented as such. A
  process-wide semaphore (64).
- Breaker: `pause_after_failures` (20) consecutive failures pause the endpoint for `pause_secs`
  (300). Then half-open: one probe row is sent; success closes the breaker and releases the backlog
  at the normal concurrency, failure doubles the pause (up to 1 h).
- Kill: `[webhooks] enabled = false` (process) and KV `webhook.control/kill` (tenant), checked
  every tick; claimed rows are released.

### B.5 SSRF protection

- Move `PublicOnly`, `client`, `check_url`, `check_headers` from onto-control net.rs into
  `onto_intelli::net` behind a feature `outbound = ["dep:reqwest", "dep:hmac"]` (default on;
  `http-llm` implies it); onto-control re-exports them.
- Operator config `[[webhooks.allow]] host = "hooks.internal.example" | "*.suffix", cidrs =
  ["10.20.0.0/16"]`: an allow-listed name may resolve only into its listed CIDRs. Allow rules never
  apply to `restricted` endpoints or targets (project-registered): those reach public addresses only.
  Phase 1 has no per-project allowlists.
- Always denied, allow-listed or not: link-local and metadata ranges (169.254.0.0/16, fe80::/10,
  100.100.100.200/32, fd00:ec2::254/128), 0.0.0.0/8, multicast, reserved, and their IPv4-mapped and
  NAT64 forms. Loopback and private ranges are denied unless allow-listed (or
  `console.allow_private_urls` in dev).
- The check runs in the client's resolver right before connecting (no rebinding window); IP
  literals are checked before the request is built.
- URLs: `https://` unless allow-listed; no userinfo, no fragment. Target routes are relative paths
  without `//`, `@`, `\` or `..`; the request URL is `base.join(route)` and the worker asserts its
  host and port equal the base's.

### B.6 Subscription fan-out

`webhooks::spawn_fanout(weak intelli)` under lease `webhook:fanout` (D12):

- One cursor per endpoint (`consumers` row `webhook:<name>`). Each round reads one page from the
  lowest cursor among endpoints that are enabled and not backlogged; each endpoint only considers
  records after its own cursor.
- Endpoints are indexed by `(kind, op)` so a change is matched only against endpoints that could
  match it. Matching uses `matches` plus a cheap pre-check of `view` as the endpoint principal (the
  authoritative check is at send time).
- Rows hold only the cursor. `outbox_enqueue` and the endpoint's cursor advance commit together,
  fenced by the lease epoch.
- Backlog: an endpoint over `pending_cap` undelivered rows is `paused: backlogged` with an event; its
  cursor stays, and fan-out resumes it when the backlog drains below half the cap. If its cursor
  expired meanwhile, it gets one `onto.changes.gap` delivery (`{from, to}`), a
  `webhook.endpoint.gap` event, and its cursor moves to the floor.

### B.7 Traces

Rows carry `otel_trace_id` from the causing call's `TraceCtx`; the worker sends `traceparent:
00-<otel_trace_id>-<new span id>-01`. Failed deliveries become traces of kind `delivery` (`[traces]
deliveries = "failures" | "all" | "off"`).

---

## Part C. Automate

### C.1 Storage

- Definition: `Schema.automations: BTreeMap<String, Automation>` (`serde(default, skip_serializing_if
  = "BTreeMap::is_empty")`, next to `shapes`), `SchemaFile`, `extend_toml`, validated in `compile`
  via `Automation::validate(&Schema)` (structure and references only). live_schema.rs:
  `SchemaChange::PutAutomation { name, def }` / `DeleteAutomation { name }` with arms in `op`,
  `target`, `apply_one`, `sections`, `changes_between`, `summarize`, and `rewrite_kind`/`rewrite_rel`.
  Packs may ship automations; installing a pack never enables one.
- Limits (`max_automations`) are enforced at `schema.apply` and `schema.propose`, never in `compile`
  or load, so lowering a limit cannot stop a stored schema from opening.
- Visibility: automation definitions are readable by every schema reader, as action types already
  are (their effects, roles and risk are in `schema.get`). The docs say so. Definitions name
  endpoints and targets only by name; validation refuses any `env:`, `file:` or `sealed:` string in a
  definition. URLs and secrets stay in KV (D7 of revision 1 was about those, not names).
- Operational state: KV `automation.state/<name>` (`{enabled, closure_hash, closure, schema_version,
  enabled_by, enabled_at, expires_at, paused?}`), KV `automation.control/kill`, table
  `automation_runs`, tables `consumers`, `timers`, `leases` (section 9).

### C.2 Definition (`crates/onto-core/src/automate.rs`)

```toml
[automations.escalate_blocked_p1]
description = "Escalate a high-priority issue when it becomes blocked."
mode = "live"                    # live | shadow (records would_run, writes nothing else)
run_as = { roles = ["member", "triager"], workspaces = ["acme"] }   # id is "automation:<name>"
max_depth = 2                    # ≤ [automation] max_depth
rate = { per_minute = 30, per_subject_per_hour = 10 }
allow_self = false

[automations.escalate_blocked_p1.trigger]
on = "change"                    # phase 1: change only
kinds = ["issue"]
ops = ["entity.update"]
state = { from = ["triaged", "in_progress"], to = ["blocked"] }
sources = ["!connector:*"]
workspaces = ["acme"]

[[automations.escalate_blocked_p1.conditions]]
check = "prop_eq"
param = "subject"
prop = "priority"
value = "high"

[[automations.escalate_blocked_p1.effects]]      # at most one action effect
action = { type = "escalate_issue", params = { issue = "{trigger.id}", reason = "blocked from {trigger.state.from}" } }

[[automations.escalate_blocked_p1.effects]]      # any number of webhook effects
webhook = { endpoint = "ops_pager", event = "issue.escalated", payload = { id = "{trigger.id}", title = "{subject.title}" } }
```

Rust: `Automation { description, mode, run_as: ScopePrincipal, max_depth, rate, allow_self, trigger:
Trigger, conditions: Vec<Check>, effects: Vec<AutoEffect> }`; `enum Trigger { Change(ChangeTrigger) }`
(`#[serde(tag = "on")]`, so later trigger kinds add variants); `AutoEffect { action?, webhook? }`
(exactly one).

Validation: kinds and rels resolve; action types exist; at most one `action` effect; at least one
effect; templates name only `trigger.*`, `subject.*`, `now`, `automation`, `run`; `run_as.roles`
contain neither `system` nor `agent`; no secret references.

Closure hash (`automate::closure(schema, def, endpoints, targets) -> Closure`): SHA-256 over the
canonical JSON of the automation, every action type its effects name (including their effects,
`roles`, `risk`, `unattended`, preconditions), and the `version` and principal of every endpoint and
writeback target those reach. Endpoint and target records live in onto-intelli KV, so `closure` takes
them as `(name, version, principal)` triples.

### C.3 Templates and conditions

- One `pub(crate) Templates` helper replaces the private renderer (action_types.rs:203, 383-458),
  parameterized by a lookup closure, with two rules for every caller:
  - Every entity-field lookup checks `policy::visible` as the acting principal; an invisible entity
    has no fields. This also closes today's gap for string parameters in action types (399-413).
  - `strict` mode (automations, webhook payloads, writeback bodies): a placeholder without a value
    refuses the whole render (`invalid_template`); no key or item is dropped. Existing action-type
    rendering stays non-strict, so current action types behave as before apart from the visibility
    rule.
- `automation.test` renders and plans with both the run principal and the caller: a lookup succeeds
  only when both may see the entity.
- Conditions reuse `eval_check` (action_types.rs:469) through `pub(crate) fn eval_checks(schema, g,
  checks, params, principal)` with params `{subject: <id>}`. All must pass.
- `automation.get` says in `means`: trigger matching uses the change as committed; conditions,
  `subject.*` and plans are evaluated against the graph at the run's commit.

### C.4 Effects

- `action`: planned as the run principal against the locked graph inside the run's `transact`
  (C.7). Refused, final, when `!plan.valid` or the plan needs confirmation. Today that is `risk >=
  Medium || !reversible` (action_types.rs:942), so automations run only low-risk, reversible action
  types, or action types whose external effects are declared `unattended` for automations (D.1).
- `webhook`: one outbox row `{kind: webhook, target: endpoint, dedupe: "<run>:<i>", source:
  "automation", payload: strictly rendered}` in the run's commit. At enable time and at every run,
  the endpoint principal must dominate `run_as` (the receiver may see whatever the run could render).

### C.5 Who may do what

| Step | Who | Enforced in |
|---|---|---|
| Define or change | `schema.apply` (`can_apply`) or `schema.propose` then approval; agents propose only | live_schema.rs, proposals.rs |
| Enable | `can_approve(p)`, not an agent, `p` dominates `run_as` (6.1); every endpoint named dominates `run_as` | `automation_enable` |
| Disable, kill | any principal that may enable it, plus any admin (stopping is always allowed) | `automation_disable`, `automation_kill` |
| Rerun a skipped or failed run | as Enable | `automation_rerun` |
| Dry run | any reader; renders with run principal ∩ caller (C.3) | `automation_test` |
| Run | service principal `Principal { id: "automation:<name>", roles, workspaces, via: Some("automation:<name>") }` | automate.rs |

Binding: enablement stores the closure hash and the per-item hashes. Each evaluation recomputes it
(cached by schema version and the endpoint/target KV versions); a mismatch makes the automation
inactive (`stale_definition`, naming the item that changed) with one `automation.stale` event. Lint
rule `automation_closure_changed` shows, in `schema.plan` and on proposals, which enabled automations
a change would stop.

Enablement TTL: `expires_at = enabled_at + [automation] enable_ttl_days` (30). onto cannot tell when
the enabler loses the approver role (that is Tutela's knowledge), so enablement expires and a person
re-enables. Expiry makes it inactive with `enablement_expired` and an event.

### C.6 Loop, cascade and flood guards

- Depth: a run's `ActionContext.cause = {automation, run, depth}`; its changes carry
  `cause.depth`; a change at depth `d` triggers runs at depth `d + 1`; over `min(max_depth,
  [automation] max_depth)` (3) the run is recorded `refused: depth`.
- Leaving onto: the CloudEvent carries `causeautomation`, `causerun`, `causedepth`. Inbound requests
  may send `x-onto-cause-depth` (and `x-onto-cause-run`); onto-grpc accepts them only from config
  tokens (trusted services such as texc-symphony), and only to raise depth (the request's depth is
  `max(0, header)`). Others are ignored.
- Fire budget backstop: `rate.per_subject_per_hour` (10) per `(automation, subject)`, kept in the
  consumer's memory (approximate across lease handoffs; documented).
- Self-trigger: `allow_self = false` skips changes whose `cause.automation` is this automation.
- Migrations: skipped unless the trigger sets `include_migrations`. `gap` records never trigger runs;
  lint rule `automation_gap_blind` warns that change-triggered automations do not see bulk imports or
  uncaptured writes.
- Rate: token buckets per automation (`rate.per_minute`, 60) and per tenant (`max_runs_per_minute`,
  600). Over a limit, or over the subject budget, the trigger becomes a `skipped` run row with its
  trigger key and reason (batched insert, `ON CONFLICT DO NOTHING`), so it is listed and can be rerun.
- Breaker: `pause_after_failures` (5) consecutive failed runs pause the automation.
- Kill switches: `[automation] enabled = false` (process), KV `automation.control/kill` (tenant),
  `automation.disable` (one). A kill cancels pending retries.

### C.7 Execution

`crates/onto-intelli/src/automate.rs`, `spawn_automation_workers(weak intelli)` (when `[automation]
workers = true`; onto-cli's `open_local` sets it false outside `serve`):

- **Consumer** under lease `automation:consumer` (TTL 60 s, renewed every 20 s, epoch fenced). Reads
  change pages after its cursor; active automations are pre-indexed by `(kind, op)`; for each match
  it applies the guards, then:
  1. **Claim**: `run_claim(RunRow { state: running, epoch, … })` = `INSERT … ON CONFLICT (automation,
     trigger_key) DO NOTHING RETURNING id`, its own statement. No row returned → a duplicate:
     counted, final, never retried. `trigger_key` = the change cursor.
  2. **Commit**: `Ontology::commit_run(ctx, run, opts)` runs one `transact` whose build closure, on
     the locked graph: re-evaluates conditions, plans the action effect (`plan_on(g, …)`, the graph
     form of `plan_action`, preconditions included), renders webhook payloads strictly, and returns
     the mutations plus `Extra { kv: receipt, outbox, runs: [run → succeeded], events:
     [automation.run] }` with `fence = (automation:consumer, holder, epoch)`. A failed precondition
     or condition returns a typed error; the run is updated `failed: precondition_changed` (final).
     Shadow mode stops after planning and records `would_run` with the plan.
  3. **Advance** the consumer cursor after the page (fenced).
- **Recovery**: on acquiring the lease, the consumer lists runs in `running` with an older epoch and
  executes step 2 again for each. Step 2 is atomic, so a `running` row means nothing of it
  committed.
- **Errors**: `impl From<sqlx::Error>` maps SQLSTATE 23505 to `Error::Conflict`, pool timeouts and
  I/O errors to `Error::Unavailable`, everything else to `Storage` as today. Retried (timers row
  `retry:<run>`, `base * 2^attempt`, 10 s .. 1 h, `max_attempts` 8): `Unavailable` and `Conflict`
  from contention (duplicates never reach this path; they are settled by the claim). Final: `Denied`,
  `Invalid`, `Storage`, `invalid_template`, `precondition_changed`, refused plans. A lost fence stops
  the worker; the next holder's recovery picks the run up.
- `apply_action` (action_types.rs:993) is refactored into plan + `pub(crate) fn apply_planned(ctx,
  receipt, extra)`, and it also re-checks preconditions inside its `transact` closure, so both paths
  write identical receipts and neither commits a plan the locked graph no longer satisfies.
- The run's `ActionContext`: `request_id = run.id`, `intent = "automation:<name>"`, `source =
  "automation:<name>"`, `cause = {automation, run, depth}`.

### C.8 Run history and evidence

`RunRow { id, automation, trigger_key, closure_hash, schema_version, mode, depth, state (running |
succeeded | failed | retry | skipped | refused | would_run | cancelled), reason?, trigger: {kind,
cursor}, principal, effects: [{index, kind, ok, receipt?, outbox?, error?, plan?}], attempts, epoch,
started_at, finished_at, trace_id, means }`. The audit trail is the append-only `automation.run`
event in the run's commit (and a standalone event for runs that write nothing else).
`automation.runs {name?, state?, since?, limit?}` lists newest first, ties by id. Evidence: trigger
cursor, receipts, outbox ids, event ids, and a trace of kind `automation`.

---

## Part D. Writeback (HTTP, async)

### D.1 Effect and plan rules

`Effect` (action_types.rs:161) gains `writeback: Option<WritebackEffect>` = `{target, route, body}`;
`ActionType` gains `#[serde(default)] unattended: Vec<String>` (values `"agent"`, `"automation"`).
`ActionType::validate` counts the new effect in the one-of check; `body` placeholders are checked like
other effects.

- `plan_action` renders writebacks strictly into `ActionPlan.external: Vec<PlannedExternal>`
  (`serde(default)`) with a preview line (`writeback helpdesk.close_ticket {…} [async]`).
- A plan with any external effect has `reversible = false` and an effective risk of at least
  `Medium`, so it needs confirmation. The exception is `unattended`: an action type declaring
  `"automation"` (or `"agent"`) skips the confirmation that the external effect added, for that
  caller kind only. Its declared `risk` still applies. `unattended` is schema, so it is approved
  through governance and is part of every automation closure that reaches it.
- `ActionReceipt` gains `external: [{target, route, outbox, state}]`. The receipt and docs say
  "not reversible: the external write is not undone by action.undo". `undo_action` undoes the graph
  part only when `force = true`, and says so.

### D.2 Targets (`crates/onto-intelli/src/writeback.rs`)

```rust
pub struct Target {
    pub name: String,                      // [a-z0-9_-], ≤ 40
    pub base_url: SecretRef,               // env:/file: (operator) or a plain https URL
    pub secret: Option<SecretRef>,         // signing secret (B.3 format)
    pub routes: BTreeMap<String, String>,  // name -> relative path (B.5 rules)
    pub workspaces: Vec<String>,           // actions in these workspaces may use it
    pub restricted: bool, pub version: u64, pub created_by: String, pub created_at: i64,
}
```

KV `writeback.target`, through `commit_config`; `[writeback] enabled = false` by default. Add and
remove require an admin over every workspace of the target (dominance over `{admin, workspaces}`).

### D.3 Delivery

Writeback rows (`kind = writeback`, `dedupe = "<receipt>:<effect>"`) commit with the action. The
delivery worker posts the stored body to `base.join(route)` with `Idempotency-Key:
<receipt>:<effect>` and the Standard Webhooks signature when `secret` is set; outcomes as B.4. Dead
rows write `writeback.failed` events; `action.receipt` joins outbox rows by receipt id.
Idempotency: the receiver must honour `Idempotency-Key`; the target docs state it.

---

## 6. Security rules shared by all parts

### 6.1 Principal dominance (`policy.rs`)

`pub fn dominates(a: &Principal, b: &ScopePrincipal) -> bool`:

- `a` is not an agent; `b` holds neither `system` nor `agent`.
- Workspaces: every workspace of `b` is in `a` (`a` holding `*` covers all; `b` holding `*` requires
  `a` to hold `*`).
- Roles: `a` admin (in those workspaces) covers every role in `b`. Otherwise every built-in role of
  `b` is covered by `a.allows(..)` at the same access (viewer: Read, member: Write) and `a` holds
  every custom role of `b`; `admin` in `b` requires `a` admin.

Required on every mutating outbound operation: `webhook.add|update|rotate_secret|replay|remove|test|
resume`, `webhook.deliveries` (it shows errors and receiver responses), `writeback.add|remove`,
`automation.enable|rerun`. Stopping (`disable`, `kill`) needs only admin or the enabling rights.

### 6.2 Reserved principal ids

`policy::RESERVED_ID_PREFIXES = ["automation:", "webhook:", "connector:", "system"]` and
`is_reserved_id(id)`. Refused in: `act_for` (auth.rs:211, before building the principal; covers
config tokens and API keys with `impersonate`), `[[auth.tokens]]` validation (auth.rs ~296, a config
error), and the console and OIDC user paths (ids are `user:<id>` and `key:<prefix>` today, so this is
a guard, not a change). Tutela must refuse to mint them too (cross-core contract, recorded in
CONTROL_PLANE.md).

### 6.3 Operator-only references and errors

`guard_project_call` (auth.rs:710) becomes `guard_call(caller, action, input)`:

- An allowlist `REF_ACTIONS` of actions whose inputs may hold references (`connector.add`,
  `webhook.add`, `webhook.update`, `webhook.rotate_secret`, `writeback.add`) is the only place
  references are accepted; any `env:`/`file:` string in other actions' inputs is plain data.
- In `REF_ACTIONS`, `env:`/`file:` need `credential` = config or dev and `principal.via == None`
  (no act-as). Everyone else: refused; project credentials additionally get `restricted = true` and
  URL checks.
- Resolution errors and validation errors after resolution name the reference, never the value
  ("the URL in env:HOOK_URL is not an https URL").

---

## 7. Surfaces (phase 1)

All new write actions are `write: true`, gated in their arms, absent from `AGENT_WRITES`
(actions.rs:57). Agents reach automations only through `schema.propose`. Feature modules mirror
assurance: `changes::CALLS`, `webhooks::CALLS`, `automate::CALLS`, `writeback::CALLS`, dispatched at the
top of `dispatch` like `assurance::CALLS` (actions.rs:780). Every action called with `{}` fails with
`invalid_argument` or is a harmless read, with no network I/O (tests/hub.rs:53).

| Action | w | gRPC (unary) | HTTP | MCP | CLI |
|---|---|---|---|---|---|
| `changes.poll` | | ChangeService/Poll | `GET /v1/changes` | `poll_changes` | `onto watch --once` |
| `changes.head` | | ChangeService/Head | `GET /v1/changes/head` | | |
| `changes.capture` | w | ChangeService/Capture | `POST /v1/changes/capture` | | |
| (stream) | | deferred | `GET\|POST /v1/changes/stream` | | `onto watch` |
| `webhook.add\|update\|remove\|list\|test\|rotate_secret\|deliveries\|replay\|resume\|kill` | mixed | WebhookService/* | `/v1/webhooks…` | | `onto webhook list\|add\|rm` |
| `outbox.list` | | WebhookService/Outbox | `GET /v1/outbox` | | |
| `automation.list\|get\|runs` | | AutomateService/* | `GET /v1/automations…` | `list_automations`, `automation_runs` | `onto automation list\|runs` |
| `automation.test` | | AutomateService/Test | `POST /v1/automations/{name}/test` | `test_automation` | |
| `automation.enable\|disable\|rerun\|kill` | w | AutomateService/* | `POST /v1/automations/{name}/…` | | `onto automation enable\|disable` |
| `writeback.add\|list\|remove\|test` | mixed | WritebackService/* | `/v1/writeback/targets…` | | |

Plus the schema ops `put_automation`/`delete_automation` in `schema.plan|apply|propose` and the MCP
schema-change tool descriptions. Outputs are objects, sorted (names ascending; runs and deliveries
newest first, ties by id). `SecretRef` serializes as `env:VAR`, `file:…` or `sealed (kek <id>)`.
MCP tools are `writes = false`, in `PROFILE_TOOLS` (governance: all four; catalog and retrieval:
`poll_changes`; authoring: `test_automation`). UI: `pages/automate/AutomatePage.tsx` with
`AutomationsTab` and `WebhooksTab`; `NAV` gains Automate; whoami capabilities gain `automate` and
`configure_outbound`; rebuild `crates/onto-grpc/ui-dist`. Docs: `docs/AUTOMATE.md`, README,
STORAGE.md, CONTROL_PLANE.md (routes, SSRF, reserved ids), ARCHITECTURE.md.

## 8. Configuration (onto-intelli config.rs, `templates/onto-intelli.toml`)

```toml
[changes]                         # capture itself is a tenant property (changes.capture)
retain_days = 7
retain_max_rows = 5000000
ring = 4096                       # per tenant per process, only while subscribed; counted in memory budget
page = 500
eval_budget_ms = 50
heartbeat_secs = 15
stall_timeout_secs = 60
max_backfill_seqs = 100000        # seqs behind head before too_far_behind
reauth_secs = 60
max_subscribers = 200             # per tenant per process
max_subscribers_per_principal = 20
max_subscribers_process = 2000

[webhooks]
enabled = true
max_endpoints = 50                # enforced at webhook.add
timeout_secs = 10
poll_secs = 20
max_attempts = 15
backoff_base_secs = 10
backoff_max_secs = 3600
max_payload_bytes = 262144
max_in_flight_per_endpoint = 4    # per process
max_in_flight_per_tenant = 8      # per process
max_in_flight_process = 64
pause_after_failures = 20
pause_secs = 300
pending_cap = 100000
retain_days = 14
# [[webhooks.allow]] host = "hooks.internal.example", cidrs = ["10.20.0.0/16"]

[automation]
enabled = true
workers = true
max_automations = 100             # enforced at schema.apply/propose, never in compile
max_depth = 3
max_runs_per_minute = 600
default_rate_per_minute = 60
default_per_subject_per_hour = 10
pause_after_failures = 5
enable_ttl_days = 30
lease_secs = 60
retry_base_secs = 10
retry_max_secs = 3600
retry_max_attempts = 8

[writeback]
enabled = false
max_targets = 20
timeout_secs = 15

[engine]
wake_poll_secs = 20               # wake scanner fallback poll (section 10)
```

Each struct is `#[serde(default)]` with `check()` called from `open_with_store`; the annotated
template must keep parsing.

## 9. Migrations

Tenant migrations, appended to `MIGRATIONS` (postgres.rs:58); 0001-0008 are not edited.

`0009_changes.sql`

```sql
CREATE TABLE IF NOT EXISTS {schema}.changes (
    seq BIGINT NOT NULL, n INT NOT NULL, ts BIGINT NOT NULL,
    workspace TEXT NOT NULL, kind TEXT NOT NULL, op TEXT NOT NULL, target TEXT NOT NULL,
    doc JSONB NOT NULL,
    PRIMARY KEY (seq, n)
);
CREATE INDEX IF NOT EXISTS changes_ts ON {schema}.changes (ts);
-- append-only: refuse_update / refuse_delete triggers as 0003 and 0008

CREATE TABLE IF NOT EXISTS {schema}.changes_meta (
    one BOOLEAN PRIMARY KEY DEFAULT true CHECK (one),
    capture BOOLEAN NOT NULL DEFAULT true,
    floor BIGINT NOT NULL DEFAULT 0
);
INSERT INTO {schema}.changes_meta (one) VALUES (true) ON CONFLICT DO NOTHING;

ALTER TABLE {schema}.head ADD COLUMN IF NOT EXISTS min_writer INT NOT NULL DEFAULT 0;
ALTER TABLE {schema}.entity_versions ADD COLUMN IF NOT EXISTS change_seq BIGINT;
CREATE INDEX IF NOT EXISTS entity_versions_change ON {schema}.entity_versions (id, change_seq)
    WHERE change_seq IS NOT NULL;

-- A state-changing commit without captured change rows becomes a gap record (old writers,
-- bulk imports, anything around commit_locked). Deferred, so it runs at COMMIT.
CREATE OR REPLACE FUNCTION {schema}.changes_gap() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF coalesce((NEW.doc->>'captured')::boolean, false) THEN RETURN NULL; END IF;
    IF NOT (SELECT capture FROM {schema}.changes_meta) THEN RETURN NULL; END IF;
    IF jsonb_array_length(coalesce(NEW.doc->'entities', '[]')) = 0
       AND jsonb_array_length(coalesce(NEW.doc->'links', '[]')) = 0
       AND NOT coalesce((NEW.doc->>'reload')::boolean, false)
       AND NOT {schema}.commit_touches_schema(NEW.doc) THEN
        RETURN NULL;                                  -- KV-only commit: nothing for the feed
    END IF;
    INSERT INTO {schema}.changes (seq, n, ts, workspace, kind, op, target, doc)
    VALUES (NEW.seq, 0, NEW.ts, '', '', 'gap', '', jsonb_build_object(
        'reason', CASE WHEN coalesce((NEW.doc->>'reload')::boolean, false) THEN 'bulk_import' ELSE 'uncaptured_writer' END,
        'origin', NEW.origin, 'entities', NEW.doc->'entities', 'links', NEW.doc->'links'))
    ON CONFLICT DO NOTHING;
    RETURN NULL;
END $$;
DROP TRIGGER IF EXISTS commits_capture ON {schema}.commits;
CREATE CONSTRAINT TRIGGER commits_capture AFTER INSERT ON {schema}.commits
    DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION {schema}.changes_gap();
```

(`commit_touches_schema` checks the commit's `kv` list for the live-schema KV keys; written in the
same migration.)

(As built, the migrations are numbered after the branch's own: `0010_leases.sql` holds `now_ms`,
`leases`, `consumers` and `timers` (build step 7), `0011_outbox.sql` holds `outbox` and
`delivery_attempts` (build step 8), and the Automate tables come in the next free number (build step
10). Released migrations are never edited.)

`0010_outbox.sql`: `now_ms()` (`SELECT (extract(epoch FROM clock_timestamp()) * 1000)::bigint`);
`outbox` (columns of B.1, `claim TEXT`, `UNIQUE (target, dedupe)`, partial index on `next_at` for
`pending|retry|inflight`, indexes on `(target, created_at)` and `receipt`); `delivery_attempts`
(append-only); `consumers (name PK, seq, n, updated_at)`; `leases (name PK, holder TEXT, epoch BIGINT,
until BIGINT)`.

`0011_automate.sql`: `automation_runs (id PK, automation, trigger_key, closure_hash, state, depth,
epoch, started_at, finished_at, doc JSONB, UNIQUE (automation, trigger_key))` with
`(automation, started_at DESC)` and a partial index on `state = 'running'`; `timers (name PK, due_at
BIGINT, doc JSONB)`.

Store methods (defaulted, MemoryStore-implemented, none through the commit log or NOTIFY):

- `lease(name, holder, ttl_ms) -> Result<Option<u64>>` (the epoch):

  ```sql
  INSERT INTO leases (name, holder, epoch, until) VALUES ($1, $2, 1, now_ms() + $3)
  ON CONFLICT (name) DO UPDATE SET
      epoch = CASE WHEN leases.holder = EXCLUDED.holder THEN leases.epoch ELSE leases.epoch + 1 END,
      holder = EXCLUDED.holder, until = EXCLUDED.until
  WHERE leases.until < now_ms() OR leases.holder = EXCLUDED.holder
  RETURNING epoch
  ```

  `holder` = the store origin plus a random suffix per process start.
- `release`, `consumer_get/consumer_set` (fenced), `timers_set/timers_due/timers_delete` (database
  clock), `run_claim`, `run_update`, `runs_list`, `runs_running(older_epoch)`.
- Fencing: `WriteBatch.fence = Some(Fence { name, holder, epoch })` makes the PostgreSQL write run
  `SELECT 1 FROM leases WHERE name = $1 AND holder = $2 AND epoch = $3 AND until >= now_ms() FOR
  SHARE` inside the transaction; no row → `Error::Unavailable("lease lost")`, nothing written.

Shared table, outside tenant schemas: `PgShared::ensure_sys(prefix)` creates schema `{prefix}_sys`
with `wake (tenant TEXT PRIMARY KEY, due_at BIGINT, bumped BIGINT NOT NULL DEFAULT 0)`. It holds
tenant ids and times only. `tenant_schema` refuses tenant ids that map to `{prefix}_sys`.
`list_tenants` already ignores schemas without a `head` table.

## 10. Multi-tenant behaviour

- Isolation: every table and KV namespace above except `wake` is in the tenant's schema; workers hold
  one tenant's `Intelli` through a `Weak`; secrets are sealed with AAD naming the project.
- Wake (answers revision 1's open question 8): closed tenants are not scanned. A PostgreSQL write
  that enqueues outbox rows, schedules a retry, or commits changes while the tenant has enabled
  endpoints or automations upserts `wake.due_at = LEAST(due_at, $due), bumped = bumped + 1`. One
  scanner per deployment, elected with `pg_try_advisory_lock(hashtext('<prefix>_sys.wake'))`,
  wakes on `LISTEN "<prefix>-wake"` (NOTIFY sent with the upsert; the `-` keeps the channel apart from tenant
  notice channels, which are schema names) or every `wake_poll_secs`, selects
  `due_at <= now_ms()`, and opens those tenants on its replica (affinity: the scanner replica does
  background work for otherwise-closed tenants). When a tenant's workers drain, they set the next due
  time with `UPDATE … WHERE bumped = $seen` (compare-and-set, so a concurrent upsert is not lost).
- Tenants on older schemas never write `wake`, so a mixed-version deployment needs no special scan.
- Monitors are not in phase 1, so nothing keeps a tenant open on a timer.
- Fairness: per-tenant caps (subscribers, endpoints, automations, runs per minute, in-flight
  deliveries), the process-wide delivery semaphore and the process-wide subscriber cap.
- Project deletion (`project_by_ref` inactive, hooks.rs:178) refuses tenant settings; workers end
  on `closing` or when the `Weak` no longer upgrades.

## 11. Observability

- Traces: kind `automation` per run (root `automation.run:<name>`, spans per effect); kind `delivery`
  for failed or all attempts; `call` traces for new write actions via `[traces] calls = "writes"`.
- `tracing` spans: `changes.subscribe`, `automation.run`, `webhook.deliver`; no secrets; URLs through
  `redact_url` (connectors.rs:235).
- Counters in `system.stats` and `GET /v1/admin/tenants`: subscribers (tenant and process), ring
  bytes, change lag per consumer and endpoint, outbox pending/retry/dead per endpoint, runs per state
  over 1 h, gap records over 24 h, paused automations and endpoints.
- `system.health` reports kill switches, paused counts, `capture` and the latest gap.

## 12. Test plan

Unit (in-file):

- `changes.rs`: capture for create, update (`before`, `before_vis`, `state`), no-op upsert
  (nothing), workspace move and kind change (`moved`, `changed`), delete with cascaded link deletes,
  link create/update/delete, merge, schema change with renames; `n` order; cursor parse/format;
  filters (kind and interface, ops, ids, rels incl. inverse, props, sources globs and exclusions,
  migrations default on); `view`: visible→visible with `before`, hidden→visible (`became_visible`, no
  `before`), visible→hidden (`entity.hidden`, id only), a `visibility` flip from restricted to public
  with a `salary` edit never shows the old salary to a reader who could not see it before; link with
  a deleted end.
- `automate.rs`: validation; closure hash stable across field order and changed by any closure item
  (action type effect, roles, risk, `unattended`, endpoint version or principal); strict templates
  refuse missing values in nested objects and arrays; visibility-checked lookups; guards (depth, self,
  migrations, rate, subject budget); dominance cases (scoped admin vs `*`, custom roles, agent).
- `policy.rs`: `dominates`, `is_reserved_id`.
- `webhooks.rs`/`net.rs`: Standard Webhooks test vector (fixed secret, id, ts, body); CloudEvent
  shape; backoff and jitter bounds; status classification; breaker half-open; `Guarded` resolver
  (exact and suffix rules, CIDR limits, metadata always denied, IPv4-mapped, NAT64); `Url::join` with
  `@evil`, `//evil`, `..` routes.
- Error mapping: 23505 → `Conflict`, pool timeout → `Unavailable` (onto-core, behind `DATABASE_URL`).

onto-core integration (`crates/onto-core/tests/`, PostgreSQL variants with `common::TestTenant`):

- `changes.rs`: change rows only with their commit (a denied mutation in the same batch leaves
  none); permission filtering; **model-based property test**: random batches from two `Ontology`
  instances on one tenant (two processes' worth of writers, 8 threads), then for random cursors A < B,
  replaying the visible feed from A onto the snapshot at A equals the snapshot at B; **rolling-deploy
  gap**: a commit inserted the way a pre-0009 build writes it (no `captured`, no change rows) yields
  one `gap` with its ids; `MemoryStore` and the default `bulk_import` yield `gap`; **expired cursor at
  the retention edge** with seq gaps on both sides of the floor; capture off then on expires old
  cursors; **pre-0009 populated tenant** migrated to 0011 keeps reading and writing, and its first
  commit afterwards captures.
- `automate.rs`: a run commits action, receipt, outbox row and run row in one commit; a duplicate
  trigger is skipped by the claim and never retried; **crash between claim and commit** (FlakyStore
  fails the commit) leaves `running`, and recovery completes it exactly once; precondition changed
  between trigger and commit → `failed: precondition_changed`; shadow mode; depth stops an A→B→A
  cascade; closure change → `stale_definition`; enablement expiry; medium-risk and not-reversible
  plans `refused`; rate limit writes `skipped` rows that `automation.rerun` can run.
- `leases.rs`: two holders with process clocks skewed by ±5 min (injected) never both hold a lease
  (database clock); a write fenced with an old epoch is refused.
- `outbox.rs`: `SKIP LOCKED` disjoint claims; expired lease re-claimed; a late `outbox_finish` with an
  old claim token changes nothing; replay; unique `(target, dedupe)` absorbs a repeated fan-out page.

onto-intelli integration (`#[tokio::test]`, `demo()` pattern):

- `webhooks.rs` with a local receiver (axum on 127.0.0.1, `[[webhooks.allow]] host = "127.0.0.1",
  cidrs = ["127.0.0.1/32"]`): signature verifies with the SDK algorithm; 500 then 200 with backoff
  (time paused); 400 dead; 429 with `Retry-After`; 410 disables; no redirects; metadata, `localhost`,
  `[::ffff:127.0.0.1]` and a name resolving to 10.0.0.1 refused; an allow-listed name resolving to
  169.254.169.254 refused; a restricted endpoint ignores the allow list; rotation signs with both
  secrets; replay keeps the id; a change that became invisible to the endpoint principal before
  sending is `cancelled: not_visible`; an expired change is `dead: expired`; a backlogged endpoint
  resumes from its own cursor and loses nothing; **kind rename** rewrites endpoint filters in the
  schema-apply commit; **narrower admin** (workspace `ws1`) cannot update, replay or read deliveries
  of an endpoint whose principal is `{admin, ["*"]}`.
- `automate.rs`: enable matrix (admin over the scope yes; scoped admin for a `*` run_as no; viewer,
  member without the role, agent no; `authorization.denied` events); propose by an agent, approve,
  enable by an approver; kill switch; breaker; `{}` inputs.
- `hub.rs`: every listed action dispatched; **tenant eviction with an open stream**: the stream ends
  `tenant_closed` and exactly one `Intelli` exists for the tenant afterwards; streams do not keep a
  tenant past `max_open_tenants`; `estimated_bytes` includes the ring.
- `mcp_profiles.rs`: new tools in the right profiles, none writes.

onto-grpc (`crates/onto-grpc/tests/changes.rs`, `start()`): SSE in order; heartbeats advance through
filtered traffic; `Last-Event-ID` resume; `reset` on an expired cursor; `too_far_behind` past
`max_backfill_seqs`; **role downgrade during a live stream** ends it with `principal_changed` within
`reauth_secs`; revoked key ends it `unauthenticated`; 429 over the limits; **cross-tenant**: a key for
tenant A with `x-onto-tenant: B` is refused on poll and stream; **`x-onto-act-as: automation:x`**
refused for a config token and for an impersonating API key; a config token with `impersonate` and
act-as cannot register `env:`/`file:` refs; errors never contain a resolved value;
`x-onto-cause-depth` honoured only from config tokens.

Benchmarks (onto-bench, recorded in docs/AUTOMATE.md before build step 6 ships): commit latency p50/p99
with capture off and on; write latency with 0, 50 and 200 subscribers on one tenant; fan-out
throughput with 50 endpoints during a 100k-entity crawl.

SDK tests: `changes.test.ts`, `test_changes.py` against a scripted SSE server (reconnect, `retry:`,
resume, reset, `too_far_behind` fallback to poll); webhook verification vectors shared with Rust.

CI: the test-postgres job runs PostgreSQL variants. Docker is down locally; nothing here was run.

## 13. Words the docs must use

- "Each change is enqueued once per endpoint and delivered at least once; dedupe on the event id."
- "Change records are computed by the build that wrote them; a commit from a build that did not
  record changes appears as a gap."
- "Consistent resync is not offered in this version."
- "Writeback is not reversible; action.undo does not undo the external write."
- "Automation definitions are visible to everyone who can read the schema."
- "Delivery concurrency limits are per process."

## 14. Out of scope for phase 1

WebSocket, gRPC streaming and the UI live tail; triggers other than `change`; LLM or agent steps,
waits, branches, schedules (texc-symphony); notification channels; delegated approval of risky runs
through attache (refused instead); exactly-once delivery and per-endpoint ordering; sync external
effects; SQL and MongoDB writeback; compensation; field masks on change payloads and Records-stored
types (the `Schema::captures` hook is in place); `changes.snapshot`; per-project allow lists;
cross-tenant subscriptions.

## 15. Deferred designs (notes for the next phase)

- **Metric and shape monitors.** Key runs on the timer slot (`due_at`), not the evaluation time.
  Shape result keys carry an occurrence epoch that resets when the result resolves, so a violation
  that returns fires again; results cut by `max_fires` stay out of the stored set. Evaluate only
  open tenants, or SQL-only metrics on closed ones, so a 60 s monitor does not pin a tenant open.
  `undetermined` or `truncated` reports fire nothing and record why.
- **Cron.** Recommended to live in texc-symphony calling `action.apply` (section 16 item 3). If onto
  keeps it: in-house 5-field UTC parser, `trigger_key` = the fire slot.
- **Sync writeback.** Only with a write-ahead intent: commit the run row (`running`) and one
  `writeback` row per external effect (`executing`, with the idempotency key) first; call the external
  system; then commit graph edits, receipt and finished rows. Rows still `executing` on recovery go to
  `orphaned` or `compensate`.
- **SQL writeback.** Registered statements only (single statement, first keyword
  INSERT/UPDATE/DELETE/MERGE, native placeholders counted outside literals), bound parameters only,
  restricted targets through the connectors' URL checks. Ownership pending (section 16 item 4).
- **Compensation.** Declared `compensable` plus `undo`, and receipts that say "compensable", never
  "reversible".
- **`changes.snapshot {at}`.** Pages at a pinned seq inside one REPEATABLE READ transaction, or a
  COPY export; resolves `reset` and `gap` for mirrors.

## 16. Decisions that need the owner

Items 1 and 2 were decided by the owner on 2026-10-28 (the design's defaults); items 3 to 5 stay open.

1. **Platform-wide webhook format.** *Decided 2026-10-28:* CloudEvents 1.0 structured JSON with
   Standard Webhooks signatures (D13) is the shared format for Tutela, attache, onto-intelli and
   texc-symphony. (The question was: confirm it, or name another before build step 8 ships.)
2. **Service principals.** *Decided 2026-10-28:* phase 1 declares `automation:<name>` and
   `webhook:<name>` locally, with a 30-day enablement TTL (`[automation] enable_ttl_days = 30`) and
   the reserved id prefixes of section 6.2, until Tutela issues them. (The question was: accept that
   for phase 1, or require Tutela-minted principals before build step 11.)
3. **Schedules.** Should cron schedules live in texc-symphony (recommended: onto keeps only
   ontology-native triggers) or in onto Automate?
4. **SQL writeback ownership.** Writing into customer databases is integration work. Should it live
   in onto (as action-type effects), in texc-symphony, or in a product? Phase 1 ships HTTP-only async
   writeback either way.
5. **Risky automation runs.** Phase 1 refuses runs whose plans need confirmation. Should a later
   phase route them to attache approvals?

Engineering choices settled here rather than escalated: the shared `wake` table, the 7-day change
retention default (configurable), ids-or-full payloads per filter, and separate writeback targets
from connector credentials (crawl roles stay read-only).

## 17. Build order

One engineer, sequential, one build at a time (each Rust `target/` is large). Every step ends green on
`cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D warnings`, and the tests
named in the step (`cargo test -p <crate> --test <name>`, PostgreSQL variants with
`DATABASE_URL=postgres://onto:onto@127.0.0.1:5441/onto` once Docker is up); `make test` before each
commit.

1. **Principals and errors** (onto-core policy.rs, store/postgres.rs; onto-grpc auth.rs):
   `dominates`, `ScopePrincipal`, `RESERVED_ID_PREFIXES` and their use in `act_for` and token
   validation; `From<sqlx::Error>` maps 23505 → `Conflict`, pool and I/O → `Unavailable` (check
   existing tests that assert `storage`). Tests: policy unit tests, onto-grpc act-as refusal.
2. **Templates and plans** (action_types.rs): `Templates` helper with visibility-checked lookup and
   strict mode; `plan_on(g)`; `apply_planned`; `apply_action` re-checks preconditions inside
   `transact`. Tests: existing action-type tests unchanged, new visibility and strict tests, a
   precondition race test.
3. **Capture core, memory store** (onto-core changes.rs, engine.rs, engine_sync.rs, store/mod.rs):
   types, `capture`, `matches`, `view`, `redact`; `WriteBatch.changes/captured`, `CommitRecord.captured`;
   `ActionContext.cause`; `ChangeSet.seq`; fork capture off; MemoryStore change log with gaps and
   floor. Tests: unit tests of A, memory integration tests, the model-based property test on
   `MemoryStore`.
4. **Capture on PostgreSQL** (store/postgres.rs, `0009_changes.sql`): write path (`change_seq`,
   `captured`, `min_writer`), `copy_import` gap, `changes_since` join, `changes_meta`,
   `set_capture`, vacuum with floor, the gap trigger. Tests: PostgreSQL variants of step 3 including
   the property test across two instances, rolling-deploy gap, retention edge, pre-0009 tenant
   migration.
5. **Poll surfaces** (onto-intelli changes actions, `Intelli::admit`, actions.rs; onto-grpc routes,
   proto `ChangeService` unary, http.rs; mcp.rs `poll_changes`; onto-cli `watch --once`): Tests:
   hub dispatch, cross-tenant poll, MCP profiles.
6. **Stream** (onto-intelli changes.rs ChangeHub, intelli.rs `closing`/`estimated_bytes`, hub.rs
   `stream_touch` and closing signal; onto-grpc `changes_stream`; SDK `sse` retry, `changes.ts`,
   `changes.py`; CLI `watch`; onto-bench capture and subscriber scenarios). Tests: onto-grpc
   changes.rs, hub eviction with an open stream, SDK tests; record bench numbers.
7. **Leases, consumers, timers, wake** (`0010_outbox.sql` lease/consumer tables and `now_ms`, Store
   methods, `WriteBatch.fence`, `PgShared::ensure_sys`, wake scanner in hub.rs). Tests: lease skew and
   fencing, wake CAS, scanner election with two hubs.
8. **Outbox and delivery** (onto-core outbox.rs, `Extra.outbox`, `commit_config`; onto-intelli net.rs
   move with `Guarded` and CIDR rules, webhooks.rs signing, CloudEvent, delivery worker with claim
   tokens, breaker, kill; onto-control vault `seal_for/open_for`, hooks.rs `SecretBox`, net.rs
   re-exports; onto-grpc `guard_call`; SDK verify helpers). Tests: outbox store tests, net unit tests,
   delivery tests with a local receiver using `webhook.test` rows.
9. **Endpoints and fan-out** (webhooks.rs registry with dominance, per-endpoint cursors, render at
   send, backlog and gap handling, rename rewrite in live_schema.rs; routes, proto `WebhookService`,
   HTTP; CLI `webhook`; UI `WebhooksTab`). Tests: the webhooks integration list in section 12.
10. **Automation definitions** (onto-core automate.rs types, validation, closure; schema.rs section;
    live_schema.rs ops and rename propagation; limits at apply and propose; lint.rs rules
    `automation_reference`, `automation_privilege`, `automation_loop`, `automation_closure_changed`,
    `automation_gap_blind`; `0011_automate.sql`). Tests: unit tests, schema propose/approve flow,
    lint output.
11. **Automation runtime** (onto-core `commit_run`, run Store methods; onto-intelli automate.rs
    consumer, claim, recovery, guards, skipped rows, retries, enable with TTL, actions; proto
    `AutomateService`, HTTP; MCP tools; CLI `automation`; UI `AutomationsTab`). Tests: onto-core and
    onto-intelli automate integration tests, including the crash and duplicate cases.
12. **HTTP writeback** (action_types.rs `writeback` effect, `unattended`, plan and receipt rules;
    onto-intelli writeback.rs targets and delivery through the outbox; proto `WritebackService`,
    HTTP). Tests: plan and confirmation rules, `Refuse` without `[writeback] enabled`, delivery with
    `Idempotency-Key` to the local receiver, `restricted` targets ignore the allow list.
13. **Docs** (`docs/AUTOMATE.md` with section 13's wording and the bench numbers, README capability
    table, STORAGE.md, CONTROL_PLANE.md routes/SSRF/reserved ids, ARCHITECTURE.md).

## 18. Remaining open questions (not owner decisions; settle during the build)

- Is 50 ms the right per-page evaluation budget? Set from the step 6 benchmark.
- Should `entity.hidden` also be sent for link records whose end becomes hidden (phase 1: no)?
- `changes_meta.floor` reports a false `reset` at the exact retention edge; acceptable, or add an
  end-of-seq cursor form?

---

## Review decisions

Each critique point was checked against the code before deciding.

1. Sync writeback double-run and unrecorded write — **Accept** (the simpler option): no sync
   externals in phase 1; HTTP writeback is async through the outbox with `Idempotency-Key`; the
   write-ahead intent design is kept in section 15 for a later sync mode.
2. Duplicate run retried as a storage error — **Accept**: confirmed at postgres.rs:326-329; runs are
   claimed with `ON CONFLICT DO NOTHING RETURNING` and duplicates are final; 23505 maps to `Conflict`
   and pool/I-O errors to `Unavailable`; only `Unavailable` and contention `Conflict` are retried.
3. Silent gaps from non-capturing writers — **Adjust**: capture is a tenant property and a deferred
   trigger plus `MemoryStore::write` emit `gap` records keyed on a `captured` flag in the commit doc
   (not on row counts, since no-op upserts write none); `min_writer` is added but only stops builds
   from this one onward, because released builds never read it.
4. Expiry by oldest row is ambiguous — **Accept**: retention writes `floor` in the same transaction;
   expired iff `cursor.seq < floor` (a false reset exactly at the edge is accepted and documented);
   the ring serves only `cursor >= covered_from`.
5. Enablement hashes only the automation — **Accept**: closure hash over the automation, reached
   action types (effects, roles, risk, `unattended`), endpoints and targets (version and principal);
   lint rule `automation_closure_changed`. Metrics and shapes enter the closure when monitors do.
6. Narrower admin can redirect a broader endpoint — **Accept**: confirmed (`is_privileged` plus
   workspace-scoped admin); `dominates` defined in policy.rs and required on every mutating outbound
   operation, including `writeback.add/remove` and `automation.enable`.
7. Automation principal ids can be impersonated — **Accept**: confirmed `act_for` takes any id and
   `action_receipt` trusts `r.actor`; reserved prefixes refused in `act_for` and token validation;
   control-plane keys are `key:<prefix>` and users `user:<id>`, so issuance needs only a guard; Tutela
   gets it as a contract item.
8. Change records leak or miss on visibility and workspace changes — **Accept**: `before` only when
   visible before and after; `workspace` and `kind` added to the capture diff (confirmed missing from
   `diff_fields`); per-reader `entity.hidden`.
9. Re-auth only checks the credential — **Accept**: the principal is re-derived each re-auth and a
   changed `scope_key()` ends the stream with `principal_changed`.
10. `env:`/`file:` guarded by credential type — **Accept**: confirmed (`is_project()` at
    auth.rs:716); operator-only (config or dev, no act-as), an allowlist of reference-accepting
    actions, and errors that name the reference, never the value.
11. Shared allow-list and metadata bypass — **Accept**: `(host, CIDRs)` rules, never for restricted
    endpoints, metadata and link-local always denied, `Url::join` with a host assertion; no
    per-project allow lists in phase 1 (operators only).
12. "Reversible" writeback lets agents and automations write externally — **Accept**: external
    effects make plans not reversible and at least Medium unless the action type declares
    `unattended`; the spec now says automations refuse Medium risk (confirmed at action_types.rs:942).
13. Plans stale at commit — **Accept**: preconditions and conditions are re-evaluated inside the
    `transact` closure (confirmed it receives `&Graph`), for runs and for `apply_action`; phase 1
    limits automations to one action effect.
14. Depth guard escapes through external loops — **Accept**: cause in the signed CloudEvent,
    `x-onto-cause-depth` from config tokens only and only upward, plus a per-(automation, subject)
    hourly budget.
15. Refusals that drop work — **Accept**: per-endpoint cursors with resume or an explicit gap
    delivery; over-limit triggers become `skipped` run rows that can be rerun.
16. Monitors not idempotent; leases unfenced — **Adjust**: database clock and epoch fencing apply to
    every phase 1 lease and claim; the monitor-specific fixes (timer-slot keys, occurrence epochs,
    unfired results kept out) are recorded in section 15 because monitors are deferred.
17. Streams interfere with eviction — **Adjust**: `Weak` producers and a `closing` signal ending
    streams with `tenant_closed`, a process subscriber cap, a ring that exists only while subscribed
    and is counted in `estimated_bytes`; streams still count toward the idle timer (to avoid a
    reconnect-reload cycle every idle period) but not toward count or budget eviction.
18. Wake-up cost at scale — **Accept**: a shared `wake` table maintained at write time with a CAS
    drain, one elected scanner with LISTEN and a fallback poll, NOTIFY plus `Notify` for delivery;
    closed tenants are never scanned and phase 1 has no monitors to pin them.
19. Backfill limit causes a reconnect loop — **Accept**: one unit (seqs, `max_backfill_seqs`), a
    distinct `too_far_behind`, SDKs page with `changes.poll` then return to the stream.
20. Filtering blocks writers — **Accept**: `spawn_blocking` with a per-page budget and per-slice read
    locks, grouping by `(scope_key, filter)`, endpoints and automations indexed by `(kind, op)`, and
    onto-bench numbers before the stream ships.
21. Payloads rendered at enqueue — **Accept**: change-sourced rows hold only the cursor and are
    rendered and visibility-checked at send time; expired changes go `dead: expired`. Effect payloads
    stay stored (they are what the run decided, rendered strictly as the run principal).
22. Mirrors break after migrations and renames — **Accept**: migrations included by default for
    streams, poll and webhooks (automations still skip them); `schema.change` carries renames; the
    rename commit rewrites endpoint filters in the same `Extra.kv`.
23. `reload` promises an unsupported resync — **Accept** (the documentation option): `reload` becomes
    `gap`, the docs say consistent resync is not offered, lint warns change-triggered automations
    ignore imports; `changes.snapshot` is planned for phase 1.1.
24. `compile` depends on process config — **Accept**: limits enforced at apply and propose only.
25. Automations visible to every schema reader — **Adjust**: accept the exposure and state it, since
    `schema.get` already shows action types' effects, roles and risk to every reader; definitions may
    not contain secret references, and URLs and secrets stay in KV.
26. Template renderer breaks strictness and visibility — **Accept**: confirmed (`filter_map` at
    action_types.rs:439-458, unchecked lookup at 399-413); a shared helper with strict mode and
    visibility-checked lookups, and the intersected principal in `automation.test`.
27. Core ownership — **Adjust**: phase 1 already follows the recommendation (no cron, ontology-native
    triggers only, CloudEvents with Standard Webhooks implemented, SQL writeback deferred, enablement
    TTL for local principals); the ownership calls themselves are product decisions and are listed in
    section 16.
28. Untrue claims — **Accept**: "enqueued once, delivered at least once"; no "exactly-once effects";
    the new document is joined from `entity_versions` by `change_seq` instead of copied; receivers
    dedupe on the signed id and the SDK verify helper returns the parsed envelope.
29. v1 over-scoped — **Adjust**: the cut is adopted (change feed with SSE only, async webhooks,
    change-triggered automations with action and webhook effects), but async HTTP writeback stays as
    the last phase 1 slice because the owner asked for writeback in phase 1 and it reuses the outbox
    with no new failure class; SQL and sync writeback are deferred.
30. Missing tests — **Accept**: all listed cases are in section 12, except monitor double-fire on
    lease handoff, which moves with monitors to the next phase (lease skew and fencing are tested).
31. Smaller gaps — **Accept**, with one adjustment: SDK `retry:` parsing added (confirmed missing);
    the GET SSE route repeats `run`'s checks through `Intelli::admit` and `guard_call`; forks turn
    capture off; generated secrets need a sealer, otherwise `env:`/`file:`; per-process in-flight
    limits documented; half-open breaker; Records types (which do not exist in code yet) get a
    `Schema::captures` hook so they are excluded when they land.

## Build notes

Choices made while building, where this document was silent or out of date.

Build step 8 (outbox and delivery):

- Migrations: `0011_outbox.sql` (section 9 named it `0010_outbox.sql`; 0010 was already taken).
  `delivery_attempts` is keyed by `(outbox_id, claim)`, so a replayed row's attempts never collide
  with its earlier ones; it is append-only like the audit trail (retention alone deletes).
- Store additions beyond B.1: `outbox_claim` also takes the targets to skip (disabled, paused,
  breaker-open or at their in-flight limit), so their rows are not claimed and released every pass;
  `outbox_release` gives a claimed row back unsent without counting the attempt (kill switch, a lease
  too short to finish in); `outbox_get`, `outbox_attempts`, `outbox_next_due` (for the wake row) and
  `outbox_vacuum` (`[webhooks] retain_days`). A claim token is `<holder>:<uuid>`.
- `changes_get(cursor)` is `Ontology::change_at(principal, cursor, fields)`: the record as the
  endpoint principal sees it now, `NotVisible` (row `cancelled: not_visible`) or `Expired` (row
  `dead: expired`). The delivery worker already renders change rows; fan-out (step 9) enqueues them.
- Breakers live in `webhook.control` (`breaker:<endpoint>`), not in the endpoint value, so a pause
  never bumps the endpoint `version` (which is part of automation closure hashes) and never races an
  admin's update. Consecutive failures are counted per process; the open state is shared.
- A signing secret without the `whsec_` prefix is used as its bytes (a `whsec_` secret is base64
  after the prefix); the Rust, TypeScript and Python verifiers do the same. The verifiers accept a
  body whose envelope `id` is absent and refuse one whose `id` is not the `webhook-id`.
- Errors that are policy refusals (a URL the SSRF guard refuses, including at DNS time) make the row
  `dead`; a secret reference that does not resolve is retried, since an operator can fix it.
- Deferred: delivery traces (B.7 `[traces] deliveries`); only `traceparent` is sent in step 8.


Build step 9 (endpoints and fan-out):

- No migration: the outbox, consumers and leases of 0010/0011 hold everything. The Store gains
  `outbox_open_counts` (undelivered rows per target) for the backlog cap.
- Backlog pauses live in `webhook.control` (`backlog:<endpoint>`, `{since, pending}`), like breakers,
  not in `Endpoint.paused`: delivery must keep draining a backlogged endpoint (the delivery worker
  skips endpoints whose own `paused` is set), and a pause must not move the endpoint `version`.
  Outputs show it as `status: paused` with `paused.reason = "backlogged"`. A round never enqueues
  past `pending_cap`; the endpoint's cursor stops before the first record that did not fit.
- Expiry uses the floor's edge: a cursor is complete at `(floor - 1, u32::MAX)` (where a gap leaves
  it), expired below it. The gap delivery is a change-sourced row without a cursor
  (`onto.changes.gap`, payload `{from, to, reason, message}`, dedupe `gap:<from>:<to>`), sent as
  stored; a cursor ahead of the head (a restored store) gets the same with `reason: ahead_of_head`.
- A new subscription starts at the head: `webhook.add` (or `webhook.update` setting a filter) sets
  the endpoint's consumer before the configuration commit. Disabled endpoints keep their cursor and
  resume from it (or get a gap) when enabled again.
- A change row's CloudEvent `type` follows the record as rendered at send time
  (`onto.entity.hidden` when the endpoint no longer sees the entity), not the type at enqueue.
- Rows of a removed endpoint are `cancelled: endpoint_removed`, not dead (no `webhook.dead`
  event per row).
- While a tenant has an enabled subscription, commits that put or delete entities or links bump
  its wake row (`Ontology::set_wake_on_change`, section 10), so the wake scanner opens a closed
  tenant for fan-out. Fan-out runs whether or not this process sends (`[webhooks] enabled` is the
  delivery switch); the kill switch holds deliveries, not fan-out.
- `webhook.list` is admin-only; for endpoints the caller does not dominate it shows the URL's origin
  only, and no cursor or pending count. `outbox.list` hides rows of endpoints the caller does not
  dominate (and of removed targets, except to admins of every workspace) and reports how many.
  `webhook.kill` on needs admin; off needs dominance over every endpoint (it resumes all of them).
  Dominance refusals are audited as `authorization.denied` (`Ontology::refuse_configure`).
- Restriction is sticky: once a project credential registers or changes an endpoint it stays
  `restricted`; an operator removes and re-adds it to lift that.
- URL references: write-time and delivery errors for an `env:`/`file:` URL name the reference
  only; the refused host or address (part of the value) is not echoed. Names are judged at every
  delivery (DNS); a write checks scheme, userinfo, fragment and IP literals.
- HTTP: `POST /v1/webhooks/kill` is the switch, so `kill` is not a valid endpoint name.
- UI: `pages/automate/AutomatePage.tsx` with `WebhooksTab` (the Automations tab comes with step
  11); whoami gains `configure_outbound` (admins, never agents). CLI: `onto webhook list|add|rm`
  (`--sees` names the principal's workspaces, since `--workspace` is a global flag).

Build step 10 (automation definitions):

- Migration: `0012_automate.sql` (section 9 named it `0011_automate.sql`; 0011 holds the outbox). It
  holds `automation_runs` only: `timers` is in 0010. Besides the indexes of section 9 it indexes
  `(started_at DESC, id DESC)` for `automation.runs` without a name.
- The spec's `closure(schema, def, endpoints, targets)` also takes the automation's name: the name is
  the principal id (`automation:<name>`), so it is part of the hash. Items are
  `automation:<name>`, `action_type:<name>`, `endpoint:<name>` and `target:<name>`; a reached item
  that does not exist hashes as `missing`, so registering it later stops the automation too.
  Writeback targets are read from the action type document (`effects[].writeback.target`), so the
  step 12 field enters closures without another change here.
- Definitions refuse unknown fields (`deny_unknown_fields` on every automation struct): a typo such
  as `kind` for `kinds` or `condition` for `conditions` would otherwise widen what runs. The cost is
  that a field added by a later build does not load on an earlier one; that is accepted, since an
  automation that runs broader than written is worse.
- Validation (compile) checks structure and references only: names, trigger ops (entity and link
  ops; `gap`, `entity.hidden` and `schema.change` never start runs), kinds, relations, action types,
  endpoint and event names, `run_as` (a role, no `system` or `agent`, workspaces), conditions read
  only `subject`, template roots, secret-looking strings (refused by path, never echoing the value).
  Parameters, properties and states are checked by lint (`automation_reference`), which reports
  every problem at once, as `shape_reference` does.
- `max_automations` is `OntologyOptions::max_automations` (from `[automation]`), checked by
  `schema.plan`, `schema.apply` and `schema.propose` (and an approval's commit). A change that adds
  no automation passes even over a lowered limit.
- `schema.plan` (and proposals, which re-plan for each reader) carries `lint`: the
  `automation_closure_changed` findings for enabled automations the change would stop (enabled and
  matching under the active schema, not under the planned one), each also a medium risk reason.
  `schema.lint` runs the same check on the active schema; without stored enablements (a bare
  `lint::lint` call) the check is listed in `not_run` when the schema defines automations.
- `automation_loop` is an approximation from the schema: the changes an action type can write (its
  entity parameters' kinds, literal states, link relations) are matched against each trigger
  (kinds, ops, relations, props, workspaces of the writer's `run_as`, sources with the writer's
  `automation:<name>` source, the state guard). `automation_gap_blind` is a warning on every
  change-triggered automation, as the review decision says.
- `[automation]` is parsed and checked now (all of section 8's keys); only `max_automations` is
  read in this step. Schema locks cover the `automations` section; packs may ship automations
  (counted and listed by `pack.show`), and installing a pack never enables one.
- Trigger matching (`ChangeTrigger::matches`, `Automation::admits`, `depth_limit`) is defined here
  as pure functions over committed records: migrations are skipped unless `include_migrations`,
  gaps never match, and a run's own changes are skipped unless `allow_self`. Rate limits, the
  subject budget and the runtime that uses these come with step 11.
