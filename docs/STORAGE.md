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

Released migrations never change; a schema change is a new migration.

## Tenants and tables

Each tenant has its own PostgreSQL schema, `{schema_prefix}{tenant}`, reduced
to `[a-z0-9_]` and at most 63 bytes. One database holds any number of
tenants; nexc uses one per workspace.

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

## Vectors

Search fuses two rankings. BM25 runs in memory, where it ranks better than
`ts_rank`. Vectors are pgvector's:

- Each entity's searchable text is embedded as 512 feature-hashed word and
  character-trigram buckets, quantized to integers in -127..127. These are
  exact in `halfvec` (2 bytes a component, 1 KB per entity).
- Every write stores the vectors of the entities it puts, in the same
  transaction. A schema change that alters what is searchable re-embeds the
  affected kinds in the commit that changes the schema.
- A search embeds the query and asks the HNSW index for the nearest entities
  by cosine distance (`hnsw.ef_search` sized to the request). Workspace and
  kind filters go into the SQL query. Permissions are then checked in
  memory, as for every read.
- Rows without a vector (`embedded = false`) come from bulk imports, or from
  databases migrated before vectors existed. The first process that opens
  the tenant computes their vectors; an advisory lock keeps others from
  doing the same work. Above 20,000 such rows it drops the HNSW index and
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
3. Otherwise the batch is written: entities with their vectors, links,
   events, entity and link versions, KV, and a commit record `n + 1` listing
   everything it touched. `NOTIFY` on the tenant's channel is delivered when,
   and only if, the transaction commits.

Validation therefore always runs against the latest committed state.
Concurrent edits from different replicas never overwrite each other blindly:
revisions stay sequential and every edit lands in history.

**Reads** are served from memory, at microsecond cost, except vector
candidates, which are one indexed query. Every process runs a follower:

- On a notice from another process it reads the new commit records, fetches
  the current rows of what they touched and patches the indexes. If a schema
  version moved, it adopts it and re-indexes kinds whose properties changed.
  Redirects are updated as well.
- It also polls the head every `follow_interval_ms`, in case a notice was
  lost while the LISTEN connection reconnected. A reconnect triggers a
  catch-up on its own.
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
events older than a cutoff. It is the only delete the history tables allow.

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
