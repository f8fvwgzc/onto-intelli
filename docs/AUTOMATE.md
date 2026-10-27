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
  `changes.eval_budget_ms` (50 ms stays the default until it is), commits on a
  large graph, and webhook fan-out throughput with 50 endpoints during a
  100k-entity crawl (build step 9).
