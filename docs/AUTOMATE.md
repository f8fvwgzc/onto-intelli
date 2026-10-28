# Change feed, webhooks and Automate

Status: in progress. The design is docs/design/EVENTS.md. This page will
describe the change feed, webhooks, automations and writeback once they are
built (build step 13). For now it holds the benchmark numbers the design asks
for before the change stream ships (build step 6).

## Benchmarks: change capture and live streams

Measured 2026-10-28 with `onto-bench --changes` (release build) on an Apple M2
Pro (10 cores, 16 GB), PostgreSQL 16 with pgvector in Docker on the same
machine (`127.0.0.1:5441`), one fresh tenant, 500 commits per row. Each
commit is one entity upsert through the engine (`Ontology::upsert`, one
PostgreSQL transaction). Subscribers are live streams in the same process,
each with its own principal, so no two share an evaluation (the worst case
for filtering); they drain their frames as fast as they arrive.

```text
cargo run --release -p onto-bench -- --url postgres://onto:onto@127.0.0.1:5441/onto --changes --writes 500
```

| Operation | What | Runs | p50 ms | p99 ms | mean ms | max ms |
|---|---|---:|---:|---:|---:|---:|
| commit, capture off | one entity upsert through the engine | 500 | 4.187 | 7.512 | 4.357 | 15.542 |
| commit, capture on | one entity upsert through the engine | 500 | 4.511 | 7.931 | 4.670 | 13.162 |
| commit, 0 streams | capture on, no subscribers | 500 | 4.431 | 7.806 | 4.565 | 12.083 |
| commit, 50 streams | capture on, 50 subscribers draining | 500 | 5.292 | 9.227 | 5.542 | 18.872 |
| commit, 200 streams | capture on, 200 subscribers draining | 500 | 6.694 | 10.653 | 6.898 | 14.003 |
| delivery lag | start of the commit until its frame reaches one subscriber (50 others open) | 100 | 7.091 | 11.649 | 7.502 | 11.935 |

What these numbers say and do not say:

- Capture on adds about 0.3 ms at p50 (about 8%) to a single-entity commit
  here. Batches with many entities were not measured.
- Commit latency rises with streams open in the same process (about +0.9 ms
  p50 at 50 subscribers, +2.3 ms at 200). The likely cause, not profiled, is
  filtering sharing the CPU and the graph read lock with the writer: a writer
  waits at most for one slice of filtering (64 records), never for a
  subscriber's channel. Streams served by other replicas were not measured.
- The ring held at most 600 records (about 290 KB) during these runs.
- The delivery lag includes the commit itself (about 4.5 ms of it at p50).
- Not measured yet: the per-page filtering time against
  `changes.eval_budget_ms` (50 ms stays the default until it is) and commits
  on a large graph.

## Benchmark: webhook fan-out during a crawl

Measured 2026-10-28 with `onto-bench --fanout` (release build), same machine
and database as above. A crawl writes 100,000 new entities in commits of 500
(`Ontology::apply`, five kinds in turn) on a fresh tenant with no endpoints,
then on one with 50 endpoints, each subscribed to `entity.create` of one of
the five kinds, so every entity matches ten endpoints. Deliveries are off in
this run: fan-out enqueues the outbox rows and nothing is sent.

```text
ONTO_BENCH_HOOK_SECRET=unused cargo run --release -p onto-bench -- --url postgres://onto:onto@127.0.0.1:5441/onto --fanout --crawl 100000
```

| Operation | What | Runs | p50 ms | p99 ms | mean ms | max ms |
|---|---|---:|---:|---:|---:|---:|
| crawl commit, 0 endpoints | 500 new entities per commit | 200 | 41.995 | 58.213 | 42.681 | 74.240 |
| crawl commit, 50 endpoints | 500 new entities per commit, fan-out running | 200 | 39.438 | 64.168 | 40.652 | 74.529 |

- The crawl took 8.6 s without endpoints and 8.2 s with 50: fan-out running
  beside it did not slow the commits measurably (p99 64 ms against 58 ms).
- Fan-out enqueued 1,000,000 rows (ten per entity) and reached the head of
  the change feed 99.9 s after the crawl began: about 10,000 rows, or 1,000
  entities, per second. It runs behind a crawl this fast; each endpoint's
  cursor keeps its place, so nothing is lost while it catches up, and an
  endpoint whose backlog passes `pending_cap` pauses until it drains.
- Delivery throughput was not part of this run.
