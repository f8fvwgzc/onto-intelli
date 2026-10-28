# Storage and multi-process consistency

onto-intelli is one binary plus PostgreSQL. PostgreSQL 16 or newer with the
pgvector extension (0.7 or newer) is the only durable store. Every process
keeps the hot indexes in memory: the graph, BM25, redirects, the active
schema and memory items. They are caches of the database. Entity vectors live
in the database, in an HNSW index.

| `engine.storage` | Use |
|---|---|
| `postgres` (default) | everything that should survive a restart; any number of processes share one database |
| `memory` | tests, scratch work and demos: nothing is written anywhere |

## Running PostgreSQL

**Locally**, `docker-compose.yml` runs `pgvector/pgvector:pg16` on host port
5441, so it stays clear of other PostgreSQL servers on 5432:

```bash
make db-up      # start it, wait until it accepts connections
make db-down    # stop it; the data stays in the onto-pg volume
make db-reset   # stop it and delete its data
export ONTO_DATABASE_URL=postgres://onto:onto@127.0.0.1:5441/onto
```

**Elsewhere**, use any PostgreSQL 16+ where pgvector is available: the
`pgvector/pgvector` images, or a managed service that offers the `vector`
extension (most do). The role onto connects as must be able to run
`CREATE EXTENSION vector` once, or a superuser runs it beforehand.

## Configuration

```toml
[engine]
storage = "postgres"
# database_url = "postgres://user:pass@host:5432/db?sslmode=require"
read_consistency = "eventual"   # or "strong"
follow_interval_ms = 1000

[engine.postgres]
tenant = "acme"
schema_prefix = "onto_"         # tenant tables live in schema onto_acme
max_connections = 10
connect_timeout_secs = 10
```

The URL comes from the first of:

1. `onto --database-url URL`,
2. `engine.database_url` in the config file,
3. the `ONTO_DATABASE_URL` environment variable,
4. the `DATABASE_URL` environment variable.

The URL holds credentials, so keep it in the environment rather than in the
file. **TLS:** add `sslmode=require`, or `verify-full` with `sslrootcert=…`.
TLS is rustls with aws-lc-rs and needs no system library.

## What happens on connect

1. **Server check.** `server_version_num` must be at least 160000. An older
   server is refused with a message that names its version.
2. **pgvector.** `CREATE EXTENSION IF NOT EXISTS vector` runs under an
   advisory lock. If it fails (the extension is not installed, or the role
   lacks the privilege), the error says how to fix it. pgvector older than
   0.7 is refused, since it has no `halfvec`. With 0.8 or newer, filtered
   vector searches use iterative index scans.
3. **Migrations.** Opening a tenant applies the SQL migrations embedded in
   the binary (`crates/onto-core/migrations/*.sql`) that its schema has not
   had yet, in order, inside one transaction and under an advisory lock.
   Replicas that start at the same moment don't race. `schema_migrations`
   records what was applied and when. A tenant migrated by a newer build is
   refused, with a request to upgrade, rather than misread.

| migration | adds |
|---|---|
| `0001_init` | entities, links, events, entity and link history, KV, the commit log and head |
| `0002_vectors` | `entities.embedding halfvec(512)` with an HNSW index (cosine), and `entities.embedded` |
| `0003_append_only` | triggers that refuse `UPDATE` on `events`, `entity_versions` and `link_versions` |
| `0004_hot_updates` | `fillfactor = 85` on `entities`, so updates that change no indexed column stay HOT |
| `0005_traces` | traces, spans and scores (docs/CONTROL_PLANE.md section 9), insert-only |
| `0006_trace_otel` | the OpenTelemetry trace and parent span of a traced call |
| `0007_tenant_identity` | the tenant id that owns the schema |
| `0008_audit_no_delete` | triggers that refuse `DELETE` and `TRUNCATE` on the audit trail and history (retention excepted) |
| `0009_changes` | the change feed: `changes` (append-only), `changes_meta` (capture switch and `floor`), `entity_versions.change_seq`, `head.min_writer`, and the deferred trigger that turns an uncaptured commit into a `gap` record |
| `0010_leases` | `now_ms()` (the database clock), `leases` with fencing epochs, `consumers` (change cursors), `timers` |
| `0011_outbox` | `outbox` (webhook deliveries and writeback calls) and `delivery_attempts` (append-only) |
| `0012_automate` | `automation_runs` |
| `0013_outbox_claim` | outbox indexes for claims (due rows, expired leases) and for open rows per target |

Released migrations never change; a schema change is a new migration.

### Upgrades and rollbacks

The newer-schema refusal applies to every replica, at every open. A
replica still on the old build keeps writing to tenants it already has open
(its commits reach the change feed as `gap` records), but it cannot open a
tenant again once a newer build has migrated it, and replicas re-open
tenants routinely (after `engine.tenant_idle_secs`, or when
`engine.max_open_tenants` or the memory budget closes one). During a rolling
deploy, either drain the old replicas before the new ones open tenants, or
raise `tenant_idle_secs` and `max_open_tenants` on the old replicas for the
duration of the roll. Otherwise some requests on old replicas fail with a
request to upgrade until the roll finishes.

Rolling back past `0009_changes` and `0010_leases` (the change feed and
leases): both are additive for older writers and idempotent (`IF NOT
EXISTS`, `OR REPLACE`, `ON CONFLICT DO NOTHING`). To start an older build on
a migrated tenant, delete its `schema_migrations` rows with `version >= 9`
first. The tables and the gap trigger stay; the older build ignores the
tables, and the trigger turns each of its state-changing commits into a
`gap` record, so change-feed readers see what they missed. A later upgrade
applies the migrations again, and its first commit records changes again.

Rolling back past `0011_outbox`, `0012_automate` and `0013_outbox_claim`
(webhooks, automations and writeback): the tables and indexes are additive
and an older build ignores them; delete the `schema_migrations` rows with
`version >= 11` (or `>= 9`, as above) first. The schema needs one more step.
An older build reads the head schema version whole when it opens a tenant:
a version whose `changes` hold `put_automation` or `delete_automation`
(builds before step 10 do not know them), or a head schema with an action
type that has a `writeback` effect (builds before step 12 count it as no
effect), makes it refuse the tenant. Before starting such a build:

1. Remove what it does not know with `schema.apply`: `delete_automation`
   for every automation, and `put_action_type` without the `writeback`
   effects (or `delete_action_type`) for action types that have one.
2. Apply one more ordinary schema change (for example a `put_glossary_term`),
   so the head version's `changes` hold only operations the older build
   knows; removing the automations alone leaves `delete_automation` in the
   head.

Rolling forward again needs nothing: the newer build reads every version.

**Reserved tenant ids.** Since the change feed, `{prefix}_sys` (`onto_sys`)
is the deployment's shared schema (the `wake` table), so the tenant ids
`sys` and `_sys` (any case) are refused. A tenant created under such an id
by an earlier version must be renamed before upgrading. The new build
refuses to create the shared schema inside it and says how. Example for
`onto_`:

```sql
ALTER SCHEMA onto_sys RENAME TO onto_sys_tenant;
UPDATE onto_sys_tenant.tenant_identity SET tenant = 'sys_tenant';
```

After the rename, the tenant is addressed as `sys_tenant`.

## Tenants and tables

Each tenant has its own PostgreSQL schema, `{schema_prefix}{tenant}`, reduced
to `[a-z0-9_]` and at most 63 bytes. One database holds any number of
tenants; nexc uses one per workspace. Ids that differ only in case or in
`-`, `.` and `_` reduce to the same name: the first id to open a schema
claims it (`tenant_identity`), and opening or dropping it under any other
id is refused with `conflict`.

**Open tenants.** A process keeps a PostgreSQL tenant in memory while it is
used, and closes it (`engine.tenant_idle_secs`, default 1800) when it is not.
At most `engine.max_open_tenants` (default 512, 0 for no limit) stay open, and
while the open ones' estimated memory is above `engine.tenant_memory_budget_mb`
(default 0: no budget) the least recently used close; the one just used
always stays. A closed tenant opens again on its next request.
`GET /v1/admin/tenants` (operators) lists the open tenants with their
`estimated_bytes` (`Ontology::estimated_bytes`, an estimate from their
contents), and `system.stats` shows a tenant's own. A console sign-up
creates the first project's schema without opening it.

| table | holds |
|---|---|
| `entities` | the latest JSONB document of each entity, its kind and workspace, and its vector |
| `links` | the latest JSONB document of each link |
| `events` | the audit log: append-only, indexed by workspace, target, actor and action, each with time |
| `entity_versions` | entity history: append-only, indexed by entity and by time |
| `link_versions` | link history: append-only, indexed by either end and time |
| `kv` | JSONB values with an optional `expires_at`: schema versions, proposals, receipts, redirects, memory, conversations, caches |
| `commits` | the commit log |
| `head` | the commit sequence |
| `schema_migrations` | the migrations applied to this tenant |
| `tenant_identity` | the tenant id that owns this schema |
| `traces`, `spans`, `scores` | traces of the engine's work (docs/CONTROL_PLANE.md section 9) |
| `changes`, `changes_meta` | the change feed and its capture switch and floor (below) |
| `leases`, `consumers`, `timers` | background work: who holds it, where each consumer is in the feed, what is due |
| `outbox`, `delivery_attempts` | webhook deliveries and writeback calls, and every attempt to send one (below) |
| `automation_runs` | one row per trigger an automation acted on (below) |

The deployment also has one shared schema, `{prefix}_sys`, holding the
`wake` table: one row per tenant whose background work (fan-out, deliveries,
automation retries) is due, read by the elected wake scanner so closed
tenants are opened when, and only when, they have work.

## Change feed, outbox and automation runs

These tables hold phase 1 of docs/design/EVENTS.md; docs/AUTOMATE.md shows
them in use. Every time in them is the database clock (`now_ms()`), never a
process clock.

**Change feed.** The commit that changes entities, links or the schema also
inserts one `changes` row per changed item, keyed `(seq, n)` (the cursor
`<seq>.<n>`). A row holds the record without the entity document; create and
update records join the version that commit wrote (`entity_versions` by
`change_seq`), so documents are not stored twice. `changes` is append-only
like the audit trail. A state-changing commit that recorded no changes (an
older build, `Store::bulk_import`, anything that bypasses the engine's commit
path) gets a `gap` record from a deferred trigger on `commits`, when the
tenant's `changes_meta.capture` is on.

**Outbox.** One row per webhook delivery or writeback call, unique on
`(target, dedupe)`: a change is enqueued once per endpoint (`dedupe` = the
cursor), an effect once per receipt or run (`<receipt>:<effect>`,
`<run>:<effect>`). Rows from actions and automation runs are inserted in the
commit that made the change. Change-sourced rows hold only the cursor; the
body is rendered when the row is sent. Workers claim rows with `FOR UPDATE
SKIP LOCKED` and a claim token and lease (`inflight`, `lease_until`); a
worker whose lease expired cannot finish a row another worker claimed since.
States: `pending`, `inflight`, `retry`, `delivered`, `dead`, `cancelled`.
Each send appends a `delivery_attempts` row (append-only; retention alone
deletes). Delivery is at least once: receivers dedupe on the row id, which is
the CloudEvent `id` and `webhook-id`.

**Automation runs.** A run is claimed with `INSERT … ON CONFLICT (automation,
trigger_key) DO NOTHING RETURNING id` in its own statement, so a duplicate
trigger is settled by the claim and never retried. The run's graph edits,
receipt, outbox rows and finished run row then commit in one transaction,
fenced by the consumer's lease epoch: a `running` row means nothing of that
commit happened, and recovery executes it again. Over-limit triggers are
`skipped` rows that `automation.rerun` can run. Retries are `timers` rows
(`retry:<run>`).

**Leases and fencing.** Fan-out (`webhook:fanout`) and the automation
consumer (`automation:consumer`) each run in one process per tenant under a
`leases` row. The epoch grows when the holder changes, and their writes
commit only while the writer still holds the lease at its epoch. A process
that stops does not release its leases (also on Ctrl-C), so after a restart
that work resumes when the old lease expires (60 s at most); consumers resume
from their cursors in `consumers`, so nothing is skipped.

**Retention.** One task per open tenant, 5 minutes after it opens and then
hourly, trims: change records by `[changes] retain_days` (7) and
`retain_max_rows`, whole commits at a time, raising `changes_meta.floor` in
the same transaction (a cursor below the floor is expired and readers get
`reset`); finished outbox rows and their attempts by `[webhooks]
retain_days` (14); final run rows by `[automation] retain_days` (30). The
`automation.run` audit events stay with the event log.

## Vectors

Search fuses two rankings. BM25 runs in memory, where it ranks better than
`ts_rank`. Vectors are pgvector's:

- Each entity's searchable text is embedded as 512 feature-hashed word and
  character-trigram buckets, quantized to integers in -127..127. These are
  exact in `halfvec` (2 bytes a component, 1 KB per entity).
- Writes never wait for the vector index. A write marks the entities it
  puts whose searchable text changed (`embedded = false`) and keeps the
  stored vector of the others. Right after the commit, a background thread
  in the writing process computes the marked vectors in batches. A new or
  retitled entity is found lexically at once and by vector a few
  milliseconds later. `Ontology::flush_vectors()` waits for it, for tests
  and bulk loaders.
- An update that changes no searchable text (a state, a number) touches no
  indexed column. With the page room that `fillfactor = 85` leaves, it is a
  HOT update and writes no index entry at all. That matters because an
  insert into a million-row HNSW index costs milliseconds.
- A schema change that alters what is searchable marks the affected kinds
  in the commit that changes the schema; the vectorizer re-embeds them.
- A search embeds the query and asks the HNSW index for the nearest entities
  by cosine distance (`hnsw.ef_search` sized to the request). Workspace and
  kind filters go into the SQL query. Permissions are then checked in
  memory, as for every read.
- Rows still without a vector after a crash, a bulk import or a migration
  are computed by the first process that opens the tenant. An advisory
  lock keeps processes from doing the same work twice. Above 20,000 such rows it drops the HNSW index and
  builds it once at the end, which is much faster than inserting row by row.
  An index left dropped by a crash is rebuilt on the next open.

The vectors therefore take no memory in the onto process, and every replica
searches the same, current index.

## Consistency model

Every write that changes entities, links or KV (schema versions, proposals
and redirects included) goes through the same steps:

1. It is staged and validated against the in-memory state of the writing
   process, which has reached commit `n`.
2. It commits in one transaction that locks the tenant's `head` row
   (`FOR UPDATE`). If the head is no longer `n`, another process committed
   first. The write is refused as *stale*, nothing is written, and the
   process catches up from the commit log and stages again. Retries are
   bounded, then the write fails with `conflict`.
3. Otherwise the batch is written: entities, links, events, entity and
   link versions, KV, and a commit record `n + 1` listing everything it
   touched (vectors follow, see above). `NOTIFY` on the shared channel
   `onto_intelli` (the payload names the schema), and on the tenant's own
   channel for replicas of older versions, is delivered when, and only if,
   the transaction commits.

Validation therefore always runs against the latest committed state.
Concurrent edits from different replicas never overwrite each other blindly:
revisions stay sequential and every edit lands in history.

**Reads** are served from memory, at microsecond cost, except vector
candidates: one indexed query, run on another thread while BM25 ranks the
same question in memory. Every process runs a follower:

- On a notice from another process it reads the new commit records, fetches
  the current rows of what they touched and patches the indexes. If a schema
  version moved, it adopts it and re-indexes kinds whose properties changed.
  Redirects are updated as well.
- It also polls the head every `follow_interval_ms`, in case a notice was
  lost while the LISTEN connection reconnected. A reconnect triggers a
  catch-up on its own.
- One LISTEN connection per pool serves every tenant on it: a LISTEN holds
  its connection for as long as it listens, so one per tenant would leave
  no connection for queries past `max_connections` tenants.
- A reader too far behind (pruned commits) or a bulk import triggers a full
  reload.

With `read_consistency = "eventual"`, which is the default, another replica's
write is visible a few milliseconds after it commits. With `"strong"`, every
read first compares its sequence with the store's head (one indexed
single-row query) and catches up when behind. That gives read-your-writes
across replicas behind a load balancer. `Ontology::sync()` forces a catch-up
on demand.

**Memory.** Writes to long-term and episodic memory send their own notice
(`kv` with the item key), and other processes reload that item.
Conversations, receipts, schema history and proposals are read straight from
the store, so they are always current.

**Retention.** Commit records older than the last 100,000 are pruned. A
follower that falls further behind reloads the whole tenant.
`PgStore::vacuum(keep_events)` deletes expired KV rows and, optionally,
events older than a cutoff. It is the only delete the history tables allow:
`events`, `entity_versions` and `link_versions` refuse `UPDATE`, `DELETE` and
`TRUNCATE` with triggers (migrations 0003 and 0008), and `vacuum` sets
`onto.retention` for its own transaction to pass them. The triggers stop
application bugs and stray statements, not a role that may alter them.

## Embedding notes

`PgStore::connect(url, tenant, opts)` owns a small Tokio runtime (2 workers)
for its queries. `PgStore::with_pool(pool, tenant, opts)` and
`PgShared::from_pool(pool)` reuse an application's `sqlx::PgPool` (sqlx 0.9;
`onto_core::sqlx` re-exports the same types) and run queries on that pool's
runtime.

The engine API is synchronous. A call made from a Tokio worker thread yields
the worker while it waits (`block_in_place`). A shared pool therefore needs a
multi-threaded runtime, or calls made from `spawn_blocking`. Graph reads
never wait on the database unless `read_consistency = "strong"`.

`Store::bulk_import` (`COPY`) loads entities and links without events or
versions, in one commit that tells followers to reload. It is meant for
initial imports and benchmarks; vectors are computed on the next open.

## Tests

Integration tests that need PostgreSQL run when `DATABASE_URL` (or
`ONTO_TEST_DATABASE_URL`) is set, each in tenant schemas of its own that it
drops afterwards. Without it they are skipped, so `cargo test` passes
anywhere. `make test-db` starts the local database and runs everything.
