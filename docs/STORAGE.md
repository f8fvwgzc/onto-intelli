# Storage and multi-process consistency

onto-intelli keeps hot indexes in memory: the graph, BM25, vectors,
redirects, the active schema and memory items. They are caches of a durable
store:

| `engine.storage` | Use | Shared by several processes |
|---|---|---|
| `sqlite` (default) | one node; a single file with WAL, compiled in | yes, on one machine; followers catch up before each write and on `sync()` |
| `postgres` | several replicas, or several servers, on one database | yes; followers are told through LISTEN/NOTIFY |
| `memory` | tests, scratch work, demos | no |

## PostgreSQL

```toml
[engine]
storage = "postgres"
read_consistency = "eventual"   # or "strong"
follow_interval_ms = 1000

[engine.postgres]
url_env = "ONTO_DATABASE_URL"   # postgres://user:pass@host:5432/db?sslmode=require
tenant = "acme"
schema_prefix = "onto_"         # tenant tables live in schema onto_acme
max_connections = 10
connect_timeout_secs = 10
```

**Tenants.** Each tenant has its own PostgreSQL schema,
`{schema_prefix}{tenant}`, reduced to `[a-z0-9_]` and at most 63 bytes.
Tables are created on first use, under an advisory lock, so replicas
starting at the same moment don't race. One database can hold any number of
tenants; nexc uses one per workspace.

**Tables per tenant:**

| table | holds |
|---|---|
| `entities` | the latest document of each entity |
| `links` | the latest document of each link |
| `events` | the audit log |
| `entity_versions` | entity history |
| `kv` | schema versions, proposals, receipts, redirects, memory, conversations, caches |
| `commits` | the commit log |
| `head` | the commit sequence |
| `meta` | the storage format version |

**TLS.** Pass `sslmode=require`, or `verify-full` with `sslrootcert=…`, in the
URL. TLS is rustls with aws-lc-rs and needs no system library.

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
3. Otherwise the batch is written: entities, links, events, entity versions,
   KV and a commit record `n + 1` listing everything it touched. `NOTIFY` on
   the tenant's channel is delivered when, and only if, the transaction
   commits.

Validation therefore always runs against the latest committed state.
Concurrent edits from different replicas never overwrite each other blindly:
revisions stay sequential and every edit lands in history.

**Reads** are served from memory, at microsecond cost. Every process runs a
follower:

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
`PgStore::vacuum(keep_events)` deletes expired KV rows and, optionally, old
events.

## Embedding notes

`PgStore::connect(url, tenant, opts)` owns a small Tokio runtime (2 workers)
for its queries. `PgStore::with_pool(pool, tenant, opts)` reuses an
application's `sqlx::PgPool` (sqlx 0.9) and runs queries on that pool's
runtime.

The engine API is synchronous. A call made from a Tokio worker thread yields
the worker while it waits (`block_in_place`). A shared pool therefore needs a
multi-threaded runtime, or calls made from `spawn_blocking`. Reads never
wait on the database unless `read_consistency = "strong"`.

`Store::bulk_import` (`COPY` on PostgreSQL) loads entities and links without
events or versions, in one commit that tells followers to reload. It is meant
for initial imports and benchmarks.
