# Benchmark: one million entities on PostgreSQL

`crates/onto-bench` generates a synthetic enterprise graph and bulk-loads it
into a PostgreSQL tenant. It opens the tenant the way a replica does, then
times what an application asks of it. Reproduce it with:

```bash
cargo run --release -p onto-bench -- --url postgres://user:pass@host/db --entities 1000000 --iterations 1000
```

The tenant schema is dropped afterwards unless you pass `--keep`.
`--load-only` imports and opens once (computing every vector and building
the HNSW index), then stops and keeps the tenant; `--skip-load` measures a
kept tenant.

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
terms match many entities. That is the hard case for ranking.

## Setup

- **Hardware:** Apple M2 Pro (10 cores), 16 GB RAM: a development laptop.
  During every run, other sessions' workloads were running on it (one
  container used 2–3 cores throughout). Treat the numbers as an upper
  bound.
- **Database:** PostgreSQL 16.15 with pgvector 0.8.7 (`pgvector/pgvector:pg16`)
  in Docker on the same machine, over loopback, with `shared_buffers = 2GB`,
  `maintenance_work_mem = 1GB`, `effective_cache_size = 4GB` and 2 GB of
  `/dev/shm`. At a million entities the HNSW index is 1.3 GB and the
  entities table 1.7 GB.
- **Software:** a release build: one onto process with the graph and BM25 in
  memory and vectors in PostgreSQL, plus a second replica in the same
  process to measure replication. The LLM is the offline mock, so the asks
  measure orchestration, not provider latency.
- **Runs:** reads come from a 1,000-iteration run (100 asks and snapshots).
  Writes and replication come from a later run of the same length (250
  writes, 50 replications), made after writes stopped waiting for the vector
  index (see below).

## Results

**Load and open:**

- Bulk import with `COPY` and one commit: **64.2 s** for 1,000,000 entities
  and 2,781,010 links.
- The first open after the import computes every entity's vector, stores it
  in pgvector and builds the HNSW index: **5.7 to 8.8 minutes** (343 s and
  530 s in two runs). It happens once per bulk load.
- Opening the tenant as another replica (stream every row, build the graph
  and BM25 in memory): **25.0 s**, with **1.4 GB** of resident memory. That
  is down from 2.6 GB when vectors were held in memory.

| Operation | What | Runs | p50 ms | p99 ms | mean ms | max ms |
|---|---|---:|---:|---:|---:|---:|
| get | entity by id | 1000 | 0.017 | 0.733 | 0.043 | 1.350 |
| search | hybrid BM25 + pgvector, 2 words, top 10 | 1000 | 36.226 | 105.048 | 40.976 | 301.281 |
| search (vector) | pgvector HNSW only, 2 words, top 10 | 1000 | 17.758 | 80.900 | 25.068 | 115.377 |
| search (lexical) | BM25 only, 2 words, top 10 | 1000 | 10.078 | 17.995 | 10.212 | 47.405 |
| traverse | from a project, depth 2, ≤200 nodes | 1000 | 4.111 | 33.293 | 7.025 | 83.675 |
| path | person → service, ≤5 hops | 1000 | 4.041 | 25.796 | 5.398 | 54.891 |
| impact | of a service, depth 4 | 1000 | 0.036 | 0.967 | 0.106 | 2.245 |
| root cause | of a project, depth 6 | 1000 | 2.799 | 8.036 | 3.138 | 13.505 |
| snapshot | graph around a project 30 days ago, depth 1 | 100 | 95.395 | 173.729 | 72.222 | 184.655 |
| ask (deterministic) | "Who owns project:pN?" full pipeline, 0 tokens | 100 | 1.551 | 5.239 | 36.133 | 3427.743 |
| ask (reasoning) | "Why is project:pN blocked?" context graph + mock LLM | 100 | 31.137 | 67.084 | 33.447 | 73.283 |
| write | upsert one entity (PostgreSQL commit) | 250 | 3.149 | 10.197 | 3.491 | 11.851 |
| replication | write on one replica → visible on another | 50 | 2.805 | 12.418 | 3.738 | 12.418 |

## Reading the numbers

- **Graph reads come from memory.** Gets and impact take microseconds;
  traverse, path and root cause take a few milliseconds at a million
  entities. These operations did not change with the move to PostgreSQL;
  the spread between runs on this shared machine (traverse p50 0.8 ms in an
  earlier, quieter run, 4 ms here) is machine load, not the code.
- **Vectors moved into PostgreSQL.** Replicas no longer hold 512 MB of
  vectors per million entities. Every replica searches the same index, and
  that index is current the moment the vectorizer commits. The price is
  latency: a vector query is a round trip plus an HNSW scan of a 1.3 GB
  index, 18 ms at p50, where the in-memory scan took about 6 ms. In this run
  hybrid search issued the vector query and then ran BM25. The code now runs
  them in parallel; a clean measurement of that is still to do on a quiet
  machine.
- **BM25 is the hard part of lexical search here.** With a 50-word
  vocabulary, every term matches about a tenth of all documents, so BM25
  scores around 200,000 postings per query. Real text has far more distinct
  terms.
- **Writes don't wait for the vector index.**
  - The first version stored vectors in the write transaction. Every entity
    update also inserted into the million-row HNSW index (even without a
    vector change, because the update was not HOT), and writes took 20 ms at
    p50.
  - Now a write marks entities whose searchable text changed and commits.
    A background thread computes those vectors right after, in batches.
    Updates that change no searchable text keep their vector, and with
    `fillfactor = 85` they can be HOT updates that touch no index.
  - A write is 3.1 ms at p50 on this loaded machine: entity, version, link
    history, events, commit record and `NOTIFY` in one transaction. Another
    replica applies it 2.8 ms after the write started.
- **Asks.** A deterministic answer costs about 1.5 ms. The reasoning path
  builds a context graph and packs facts into a token budget in about 31 ms;
  the provider's own latency comes on top. The one 3 s outlier in the
  deterministic row is the first ask after open: it profiles every populated
  pattern of the graph for the cache-stable system prompt. The profile is
  reused, and recomputed at most every 30 s after writes.
- **Snapshot** (the history slider) issues one query per node for entity and
  link history: about 95 ms for a depth-1 neighbourhood. Batching those
  queries is the obvious next step.
- **Memory and startup.** Every process holds its tenants' graph and BM25:
  about 1.4 GB per million entities on this data set, and about 25 s to
  open. Size replicas for the tenants they serve. Hub tenants stay open once
  used; there is no eviction yet.
