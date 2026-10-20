# Benchmark: one million entities on PostgreSQL

`crates/onto-bench` generates a synthetic enterprise graph, bulk-loads it into
a PostgreSQL tenant, opens it the way a replica does, and times what an
application asks of it. Every number below comes from that run; reproduce it
with:

```bash
cargo run --release -p onto-bench -- --url postgres://user:pass@host/db --entities 1000000 --iterations 1000
```

The tenant schema is dropped afterwards unless you pass `--keep`.
`--skip-load` reuses a kept tenant.

## Data set

The graph is seeded and deterministic, with 1,000,000 entities and 2,781,010
links:

| entities | count | links |
|---|---|---|
| teams | 1,000 | owner of services and projects |
| people | 20,000 | `member_of` a team |
| services | 2,000 | 1–2 `depends_on` other services (a DAG), owned by a team; 1 in 3 degraded or down |
| projects | 5,000 | owned by a team, `depends_on` a service; 1 in 6 blocked |
| deployments | 1,000 | `deployed_to` a service; 10% failed |
| issues | 339,850 | `belongs_to`, `assigned_to`, `affects`; 1 in 8 `blocked_by` another issue |
| tasks | 631,150 | `belongs_to`, `assigned_to`; half `part_of` an issue, 1 in 6 `blocked_by` another task |

Titles and descriptions draw from a 50-word engineering vocabulary, so search
terms match many entities, which is the hard case for ranking.

## Setup

- **Hardware:** Apple M2 Pro (10 cores), 16 GB RAM.
- **Database:** PostgreSQL 17.11 in Docker on the same machine (aarch64,
  `shared_buffers = 512MB`), over loopback.
- **Software:** a release build, one onto process with indexes in memory,
  plus a second process to measure replication. The LLM is the offline mock,
  so the asks measure orchestration, not provider latency.
- **Run:** queries go to random targets, with 1,000 runs of each read (100
  of the asks and snapshots, 250 writes). The tenant had been loaded by an
  earlier run (`--skip-load`), so it holds a few hundred extra entities from
  earlier writes.

## Results

**Load and open:**

- Bulk import with `COPY` and one commit: **62.8 s** for 1,000,000
  entities and 2,781,010 links.
- Opening the tenant the way a replica does (stream every row, build the
  graph, BM25 and vector indexes): **24.9 s**, with **2.6 GB** of resident
  memory for the process.

| Operation | What | Runs | p50 ms | p99 ms | mean ms | max ms |
|---|---|---:|---:|---:|---:|---:|
| get | entity by id | 1000 | 0.019 | 0.100 | 0.026 | 0.188 |
| search | hybrid BM25 + vector, 2 words, top 10 | 1000 | 13.616 | 18.756 | 13.848 | 223.286 |
| search (lexical) | BM25 only, 2 words, top 10 | 1000 | 7.950 | 11.259 | 7.911 | 14.226 |
| traverse | from a project, depth 2, ≤200 nodes | 1000 | 0.753 | 13.501 | 1.468 | 22.859 |
| path | person → service, ≤5 hops | 1000 | 2.304 | 8.732 | 2.895 | 10.801 |
| impact | of a service, depth 4 | 1000 | 0.026 | 0.663 | 0.069 | 1.788 |
| root cause | of a project, depth 6 | 1000 | 2.030 | 2.747 | 2.051 | 4.782 |
| snapshot | graph around a project 30 days ago, depth 1 | 100 | 101.926 | 152.436 | 80.647 | 166.837 |
| ask (deterministic) | "Who owns project:pN?" full pipeline, 0 tokens | 100 | 1.198 | 4.402 | 31.153 | 2969.820 |
| ask (reasoning) | "Why is project:pN blocked?" context graph + mock LLM | 100 | 48.184 | 81.295 | 52.010 | 245.597 |
| write | upsert one entity (PostgreSQL commit) | 250 | 0.931 | 2.620 | 1.188 | 4.651 |
| replication | write on one replica → visible on another | 50 | 1.165 | 30.197 | 2.007 | 30.197 |

## Reading the numbers

- **Graph reads come from memory.**
  - Gets, impact, traverse and root cause stay at or below a couple of
    milliseconds at a million entities.
  - Path uses bidirectional breadth-first search. In a graph this dense, a
    one-sided search took 411 ms at p99 before the change.
- **Search is the worst case here.** With a 50-word vocabulary, every term
  matches about a tenth of all documents, so BM25 scores around 200,000
  postings per query. Real text has far more distinct terms. BM25 runs on
  integer document slots with dense score arrays and top-k selection. The
  vector side scans 512 MB of contiguous quantized vectors on up to 8
  threads. Both used to scan hash maps: hybrid search was 196 ms at p50
  before this rewrite.
- **Asks.**
  - A deterministic answer costs about a millisecond.
  - The reasoning path builds a context graph and packs facts to a token
    budget in about 48 ms; the provider's own latency comes on top.
  - The one 3 s outlier in the deterministic row is the first ask after
    open. It profiles every populated pattern of the graph for the
    cache-stable system prompt. The profile is reused and recomputed at most
    every 30 s after writes.
- **Writes and replication.**
  - A write takes about 1 ms. It validates against memory and commits in one
    transaction: entity, version, link history, events, commit record and
    `NOTIFY`.
  - Another replica applies it about 1 ms later at p50. The p99 includes
    the occasional catch-up that waits for the follower's thread.
- **Snapshot** (the history slider) issues one query per node for entity
  and link history, about 100 ms for a depth-1 neighbourhood. Batching those
  queries is the obvious next step.
- **Memory and startup.** Every process holds its tenants' indexes: about
  2.6 GB per million entities on this data set, and about 25 s to open.
  Size replicas for the tenants they serve. Hub tenants stay open once
  used; there is no eviction yet.
