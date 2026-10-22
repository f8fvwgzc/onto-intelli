# Memory efficiency research

This note collects what published work and production systems do about agent memory and in-memory footprint. It records which of their ideas onto-intelli adopts now, which it defers, and which it rejects.

It covers two separate problems:

1. **Agent memory** (`crates/onto-intelli/src/memory.rs`): the store keeps redundant items, never forgets what it should, and ranks by age instead of use.
2. **Engine RAM** (`crates/onto-core/src/graph.rs`, `text.rs`): the in-memory graph and BM25 index cost far more bytes per entity than their payload.

Sources are linked inline. arXiv papers link to `https://arxiv.org/abs/<id>`. Numbers are the authors' own unless marked otherwise. Self-reported vendor figures are flagged as such.

---

## 1. Where we start

### 1.1 Agent memory before M1–M6

This table describes `memory.rs` before the changes in §4.1, which are now implemented.

| Aspect | Current behaviour (memory.rs) | Consequence |
|---|---|---|
| Recall score | `0.5·lex + 0.3·overlap + 0.1·recency + 0.05·salience + 0.1·[long-term]` | Ranking ignores how often a memory is used. |
| Recency | `exp(-(now - created_at)/7d)` for episodes, constant `1.0` for long-term | A year-old fact ranks the same as yesterday's. |
| `hits`, `last_access` | Bumped in RAM on recall, never persisted, never read | Reinforcement is lost on restart and on reload. |
| Write path | `remember()` and `record_episode()` always append a new id | The same question asked N times produces N episodes, and the same fact remembered twice produces 2 facts. |
| Contradictions | None: an old and a new fact both stay live | Recall can return both "Kenji is on call" and "Priya is on call". |
| Lexical normalisation | `lex = s / max_s` over the top 50 | The best match always gets 1.0, even when it is weak. |
| Diversity | None | A summary and the episodes it was built from can all be packed together. |
| Consolidation | Groups episodes by their first 3 sorted entity ids. Finds an existing summary with an O(N) scan. Uses a random `new_id` for each summary. Re-saves every episode. Runs inline every 25 asks. | Runs in O(groups × items). Replicas can create duplicate summaries. The whole answer cache is invalidated on every pass. |
| Revision | One process-wide counter, bumped on any long-term save or reload | Any write in any workspace invalidates every workspace's answer cache. |
| Leaks | Emptied `by_entity` sets are never removed | The map grows with every entity ever mentioned. |

### 1.2 Engine RAM today

I measured these with a counting global allocator linked against the real `onto_core::graph::Graph` and `TextIndex`, replaying the onto-bench generator (1,000,000 entities, 2,781,010 links). The figures are requested bytes, before malloc rounding.

| Structure | MB per 1M entities | Bytes/entity | Main cause |
|---|---:|---:|---|
| `Graph.names` | 2,155 | 2,260 | About 6 keys per entity, each with its own single-member `BTreeSet<String>` (a leaf node of about 280 B plus a cloned id) |
| `Graph.entities` | 1,447 | 1,518 | `Entity` is 344 B inline, and 2^21 buckets of key plus value. The key duplicates `e.id`, and `props` is a BTreeMap leaf of about 632 B. |
| `Graph.links` | 999 | 1,048 | The key holds 3 cloned Strings and the `Link` value copies them again. `get_link` allocates 3 Strings on every lookup. |
| `TextIndex` (BM25) | 777 | 814 | Every distinct term gets its own inner `HashMap<u32,u32>` (1.63M terms), and every doc stores its term Strings again |
| `out`/`inc` adjacency | 638 | 669 | `(String, Id)` tuples in both directions |
| Total | **≈ 6.1 GB** | | |

`docs/BENCHMARK.md` reports 1.4 GB per million entities. That figure comes from a macOS `ps` RSS delta taken under memory pressure, and RSS leaves out compressed pages. In the probe run, max RSS was 3.96 GB while the peak footprint was 15.5 GB. At 250k entities with no memory pressure, RSS (3.38 GB) matched the heap counter (about 3.2 GB). **The documented figure is about 4x too low.**

`Ontology::fork()`, which every what-if question uses, deep-clones the Graph (+5.2 GB, 61M allocations) and the TextIndex (+0.8 GB) at 1M entities.

---

## 2. Agent-memory algorithms

### 2.1 Retrieval scoring and decay

| Source | What it does | Evidence |
|---|---|---|
| **Generative Agents** (Park et al. 2023), [arXiv 2304.03442](https://arxiv.org/abs/2304.03442) | `score = recency + importance + relevance`, each min-max normalised to [0,1] (all weights 1). Recency = `0.995^hours` since the memory was **last retrieved**, not since it was created. Importance is an LLM rating from 1 to 10, set at write time. Reflection runs when the summed importance of recent events exceeds 150. | Ablations show observation, planning and reflection each improve believability. The study is qualitative, with no LoCoMo or LongMemEval numbers. |
| **MemoryBank** (Zhong et al., AAAI 2024), [arXiv 2305.10250](https://arxiv.org/abs/2305.10250) | Ebbinghaus retention `R = e^(-t/S)`. S starts at 1. Each recall does `S += 1` and resets `t = 0`. Memories with low R are forgotten. | The authors call it an "exploratory, highly simplified" model. Its evidence is simulated dialogs. A-MEM uses it as a weak baseline: multi-hop F1 13.18 vs A-MEM's 19.06. |
| **ACT-R base level, Petrov approximation** (ICCM 2006), [alexpetrov.com/pub/iccm06](https://alexpetrov.com/pub/iccm06) | Exact form: `B = ln(Σ t_i^-d)`, with d = 0.5 and t_i the time since the i-th use. Petrov's hybrid keeps only the use count n, the lifetime t_n and the k most recent lags: `B ≈ ln[Σ_{i≤k} t_i^-d + (n-k)(t_n^(1-d) - t_k^(1-d)) / ((1-d)(t_n - t_k))]`. | With k = 1 the approximation is "nearly perfect" even when uses are unevenly spaced, while the k = 0 approximation misses the peaks that follow bursts of use. The cost is O(1) memory per item. |
| **FSRS** (open-spaced-repetition), [wiki](https://github.com/open-spaced-repetition/awesome-fsrs/wiki/The-Algorithm) | Tracks difficulty, stability and retrievability per item. FSRS-6 curve: `R(t,S) = (1 + f·t/S)^(-w20)`. Stability grows more when an item is recalled at low R. | Its 21 default weights were fitted on human flashcard data. |
| **SM-2** (Wozniak 1990), [super-memory.com](https://super-memory.com/english/ol/sm2.htm) | Intervals of 1 day, then 6 days, then `I·EF`, with `EF' = EF + 0.1 - (5-q)(0.08 + 0.02(5-q))`. | A heuristic rule. It gives no calibrated recall probability. |
| **Murre & Dros** (PLOS ONE 2015), [journal page](https://journals.plos.org/plosone/article?id=10.1371/journal.pone.0120644) | Replicates Ebbinghaus's forgetting curve. | Power-law and summed-exponential curves fit far better than a single exponential. This is evidence about the shape of decay, not an algorithm. |
| **MemoryOS** (Kang et al. 2025), [arXiv 2506.06326](https://arxiv.org/abs/2506.06326) | `heat = N_visit + L_interaction + exp(-Δt/μ)`. Segments with heat above 5 are promoted. When a tier is full, the lowest-heat segment is evicted. | On LoCoMo with GPT-4o-mini, F1 rises 49% on average. It uses 3,874 tokens per question, against 16,977 for MemGPT. |

### 2.2 Write-time consolidation, dedup and temporal validity

| Source | What it does | Evidence |
|---|---|---|
| **Mem0** (Chhikara et al. 2025), [arXiv 2504.19413](https://arxiv.org/abs/2504.19413) | For each candidate fact, retrieve the top 10 similar memories. An LLM then chooses **ADD**, **UPDATE** (merge in more information), **DELETE** (the old fact is contradicted) or **NOOP**. The graph variant marks conflicting edges invalid instead of deleting them. | LoCoMo J score: Mem0 66.9 and Mem0g 68.4, vs full context 72.9. p95 latency is 1.44 s vs 17.1 s, and it uses about 7k tokens of memory per conversation. Its win is in cost, not accuracy. |
| **Mem0 OSS v3 source**, [scoring.py](https://github.com/mem0ai/mem0/blob/main/mem0/utils/scoring.py) | Deduplicates exact text with an MD5 hash. Normalises BM25 with a sigmoid whose midpoint depends on query length. Applies an entity-hub penalty `1/(1+0.001(n-1)²)`. | These are code constants, not benchmark results. |
| **Zep / Graphiti** (Rasmussen et al. 2025), [arXiv 2501.13956](https://arxiv.org/abs/2501.13956) | Bi-temporal edges carry `t_valid`/`t_invalid` (world time) and `t'_created`/`t'_expired` (ingestion time). When a contradiction overlaps in time, the old edge's `t_invalid` is set to the new edge's `t_valid`, and nothing is deleted. Edge dedup only compares edges between the same entity pair. | On LongMemEval_S with gpt-4o, accuracy is 71.2% vs 60.2% for full context, with context cut from 115k to 1.6k tokens. By question type (Table 3), temporal reasoning rises from 45.1% to 62.4% and single-session preference from 20.0% to 56.7%. Knowledge updates improve only with gpt-4o (78.2% to 83.3%) and fall with gpt-4o-mini (76.9% to 74.4%). |
| **Graphiti dedup source**, [dedup_helpers.py](https://github.com/getzep/graphiti/blob/main/graphiti_core/utils/maintenance/dedup_helpers.py) | Resolves duplicates in tiers: exact normalised name, then an entropy gate (1.5 bits), then MinHash over 3-grams (32 permutations, 8 bands of 4 rows) with an exact Jaccard check ≥ 0.9, and an LLM only after that. | These are code constants. Most duplicates are resolved without calling an LLM. |
| **Hindsight observations** docs, [hindsight.vectorize.io](https://hindsight.vectorize.io/developer/observations) | Treats a pair as a near-duplicate when cosine ≥ 0.97, but keeps them apart if their numbers, negations or entities differ. Raw facts are always kept, and a merge takes the union of their evidence. | The 0.97 threshold is the documented default. |
| **MinHash/LSH** (Broder 1997), [Wikipedia](https://en.wikipedia.org/wiki/MinHash); **SimHash** (Manku et al., WWW 2007), [paper](https://www.www2007.cpsc.ucalgary.ca/papers/paper215.pdf) | Near-duplicate detection on Jaccard or cosine similarity using u64 fingerprints. | Used to deduplicate web-scale crawls (AltaVista, and an 8B-page Google repository). |
| **SemDeDup** (Abbas et al. 2023), [arXiv 2303.09540](https://arxiv.org/abs/2303.09540), [NeMo docs](https://docs.nvidia.com/nemo/curator/latest/curate-text/process-data/deduplication/semdedup.html) | Removes pairs whose embedding cosine exceeds 1 − ε. | Removes up to 50% of web-scale training data with minimal loss in model quality. |
| **LightMem** (ICLR 2026), [arXiv 2510.18866](https://arxiv.org/abs/2510.18866) | The write path only inserts with a timestamp. Dedup and merge run later in a "sleep-time" pass, where only later entries update earlier ones, over bounded neighbour queues. | LongMemEval-S accuracy 68.6% vs A-MEM's 62.6%, with up to 38x fewer tokens (the table ratios are smaller than this headline). It has the lowest latency in the unified benchmark. |
| **Sleep-time compute** (Letta 2025), [arXiv 2504.13171](https://arxiv.org/abs/2504.13171); **LangMem**, [guide](https://langchain-ai.github.io/langmem/concepts/conceptual_guide/) | Move consolidation off the request path. LangMem debounces it per thread and leaves deletes off by default. | Letta reports about 5x less test-time compute for the same accuracy. |
| **Unified benchmark** (Zhou et al. 2026), [arXiv 2606.24775](https://arxiv.org/abs/2606.24775) | Re-runs about 12 memory systems under one harness. | No single architecture wins. In its LightMem storage ablation (Table 3), keeping raw user turns scores 38.9 Answer F1 on LoCoMo vs 15.6 for summaries only (31.4 vs 17.4 ROUGE-L F1 on LongMemEval). This is one system's ablation, not a comparison across systems. Local, incremental maintenance is cheaper than global rewrites. |

### 2.3 Recall under a budget: fusion, diversity and packing

| Source | What it does | Evidence |
|---|---|---|
| **MMR** (Carbonell & Goldstein, SIGIR 1998), [abstract](https://people.eng.unimelb.edu.au/ammoffat/sigir98/abstracts/carbonell.html) | Greedily picks `argmax λ·sim(q,d) − (1−λ)·max_{s∈S} sim(d,s)` over the remaining candidates. | Clearly better than non-MMR selection for non-redundant multi-document summaries. |
| **AdaGReS** (2025), [arXiv 2512.25052](https://arxiv.org/abs/2512.25052) | A greedy, token-budgeted variant of MMR. The gain is `α·sim(q,x) − β·Σ_{c∈C} sim(x,c)` divided by tokens, with a closed-form β. | IOU improves by 8–15 points on long Natural Questions questions. Only one preprint supports it. |
| **Hindsight** (Latimer et al. 2025), [arXiv 2512.12818](https://arxiv.org/abs/2512.12818), [retrieval docs](https://hindsight.vectorize.io/developer/retrieval) | Runs semantic, BM25, graph and temporal channels in parallel and fuses them with RRF, `Σ 1/(60 + rank)`. Recency, temporal and proof boosts are bounded multipliers `1 + α(signal − 0.5)`. Packing is greedy and skips any item larger than the remaining budget. | Self-reported: 83.6% on LongMemEval/LoCoMo with a 20B model, vs 39% for full context on the same model. |
| **Graphiti search source**, [search_utils.py](https://github.com/getzep/graphiti/blob/main/graphiti_core/search/search_utils.py) | Uses an RRF constant of 1 with 0-indexed ranks, which is much more top-heavy than Hindsight's 60. MMR uses λ = 0.5 with a 0.6 cosine floor. | These are code defaults. |
| **RAPTOR** (Sarthi et al., ICLR 2024), [arXiv 2401.18059](https://arxiv.org/abs/2401.18059) | "Collapsed tree" retrieval: summaries and leaves share one pool and fill a token budget. | QuALITY 82.6% with GPT-4. It trails dense retrieval on factual QA. |
| **LongMemEval** (Wu et al., ICLR 2025), [arXiv 2410.10813](https://arxiv.org/abs/2410.10813) | Indexes each memory on its value plus extracted facts, uses time-aware query expansion, and packs results as JSON with Chain-of-Note. | Fact-augmented keys add 9.4% to recall@k. Replacing raw values with summaries hurts. |
| **LoCoMo** (Maharana et al. 2024), [arXiv 2402.17753](https://arxiv.org/abs/2402.17753) | A benchmark of conversations of about 9k tokens. | Full context wins on accuracy, so memory systems win on tokens and latency. |

### 2.4 Graph and heavier architectures (context only)

| Source | Idea | Why it matters here |
|---|---|---|
| **HippoRAG / HippoRAG 2**, [arXiv 2405.14831](https://arxiv.org/abs/2405.14831), [arXiv 2502.14802](https://arxiv.org/abs/2502.14802) | Personalised PageRank (damping 0.5) seeded by query entities, each weighted `1/|passages containing it|`. | Average F1 59.8 vs 57.0 for dense retrieval. It could replace the flat entity-overlap term with multi-hop scoring. |
| **MemGPT / Letta**, [arXiv 2310.08560](https://arxiv.org/abs/2310.08560), [Letta settings](https://github.com/letta-ai/letta/blob/0.11.7/letta/settings.py) | Context paging: a warning at 70–75% full, then evict 30–50% into a recursive summary, while keeping the raw messages searchable. | DMR accuracy 92.5% vs 32.1%. It costs about 17k tokens per LoCoMo question. |
| **A-MEM**, [arXiv 2502.12110](https://arxiv.org/abs/2502.12110); **TiMem**, [arXiv 2601.02845](https://arxiv.org/abs/2601.02845) | Zettelkasten notes with LLM "evolution" of neighbouring notes, and a hierarchy of temporal summaries. | TiMem reports 52% shorter recalled memory on LoCoMo. |
| **MemoRAG**, [arXiv 2409.05591](https://arxiv.org/abs/2409.05591) | A trained global-memory model that drafts "clues" used as retrieval queries. | Needs a trained KV-compression model. |
| **Cognee**, [improve docs](https://docs.cognee.ai/core-concepts/main-operations/improve); **Supermemory**, [graph memory](https://supermemory.ai/docs/concepts/graph-memory) | Cognee updates node and edge weights from feedback with an EMA (α = 0.1). Supermemory chains updates with `isLatest` and decays memory per category. | Vendor docs. Their benchmark claims are self-reported. |

---

## 3. Compact in-memory data structures (Rust)

| Source | Technique | Evidence and fit |
|---|---|---|
| **lasso** 0.7.3, [GitHub](https://github.com/Kixiron/lasso); **string-interner** 0.20, [docs](https://docs.rs/string-interner/latest/string_interner/backend/index.html); **ustr** 1.1, [docs](https://docs.rs/crate/ustr/latest) | String interning with 4-byte keys (lasso `Spur` is a `NonZeroU32`). | Ids referenced many times shrink from about 40 B per copy to 4 B. ustr is global and never frees memory, so it suits small vocabularies only. |
| **compact_str** 0.10, [GitHub](https://github.com/ParkMyCar/compact_str); **smol_str** 0.3.7, [docs](https://docs.rs/crate/smol_str/latest) | Inline small strings (up to 24 or 23 B) in a 24-byte handle. | An id like `issue:i102697` would need no heap allocation. |
| **Tietz, HashMap overhead** (2022), [blog](https://www.ntietz.com/blog/rust-hashmap-overhead/); **hashbrown** 0.17, [docs](https://docs.rs/crate/hashbrown/latest); **rustc-hash**, [docs](https://docs.rs/crate/rustc-hash/latest) | SwissTable has a 7/8 load factor and doubles on resize, so overhead is 0.125–1.25x the element size. hashbrown's `HashTable` stores values without a separate key. | This explains the duplicated-key cost in `entities` and `links`. |
| **petgraph CSR** 0.8, [docs](https://docs.rs/petgraph/latest/petgraph/csr/struct.Csr.html) | `offsets[|V|+1]` plus `targets[|E|]`, with u32 indices. | About 15 MB per direction for 2.8M edges. The structure is static, so writes need a delta overlay. |
| **smallvec / thin-vec**, [smallvec](https://docs.rs/crate/smallvec/latest), [thin-vec](https://docs.rs/crate/thin-vec/latest) | Inline storage for small collections, or a one-pointer vector handle. | Suits per-entity lists that are usually empty or hold one item. |
| **bumpalo**, [docs](https://docs.rs/crate/bumpalo/latest) | A bump arena. | Good for per-query scratch space, not for a mutable store. |
| **fst** (Gallant), [blog](https://burntsushi.net/transducers/) | An FST term dictionary that shares prefixes and suffixes. | 1.65B URLs fit in 27 GB (20%). Tantivy uses fst for its term dictionary. It is immutable, so an LSM-style segment design is needed. |
| **Stream VByte**, [arXiv 1709.08990](https://arxiv.org/abs/1709.08990); **bitpacking** 0.9, [docs](https://docs.rs/crate/bitpacking/latest); **SIMD intersection**, [arXiv 1401.6399](https://arxiv.org/abs/1401.6399) | Compression for posting lists. | Decodes more than 4 billion integers per second. Tantivy-style blocks of 128 need 4–8 bits per posting instead of 64. Postings become append-only. |
| **Partitioned Elias-Fano** (Ottaviano & Venturini, SIGIR 2014), [ACM](https://dl.acm.org/doi/10.1145/2600428.2609615); **Quasi-Succinct Indices**, [arXiv 1206.4300](https://arxiv.org/abs/1206.4300) | Stores monotone sequences in about `2 + log2(u/n)` bits each, with random access. | Suits CSR offsets. The structure is static. |
| **Roaring**, [arXiv 1603.06549](https://arxiv.org/abs/1603.06549), [arXiv 1402.6407](https://arxiv.org/abs/1402.6407), [roaring-rs](https://github.com/RoaringBitmap/roaring-rs) | Compressed bitmaps that use array, bitmap or run containers. | Suits type, tenant and tombstone filters, but stores no term frequencies. |
| **WebGraph in Rust**, [paper](https://researchportal.ip-paris.fr/en/publications/webgraph-the-next-generation-is-in-rust/) (sux, mem_dbg, ε-serde) | Compressed graphs with Elias-Fano offsets and mmap snapshots. `mem_dbg` reports exact heap sizes. | Overkill for 2.8M edges, but `mem_dbg` is useful for measurement. |
| **dhat** 0.3.3, [docs](https://docs.rs/dhat/latest/dhat/) | A heap profiler that can also assert heap usage in tests. | Use it to find allocation sites and to write regression tests. |

---

## 4. What we adopt and why

The order follows value per unit of effort and risk. Every change keeps the `memory.remember/recall/list/forget/consolidate` actions, the gRPC and MCP surfaces, and the `KV ns "memory"` layout. New fields are `#[serde(default)]`, so old items still load.

### Agent memory (onto-intelli)

| # | Change | Taken from | Why this one |
|---|---|---|---|
| M1 | Injected clock and memory metrics (merged, superseded, pruned, reinforced, recall tokens) | Mem0 / MemoryOS reporting practice; LoCoMo efficiency metrics | Everything below must be deterministic in tests and measurable in production. |
| M2 | Write-time consolidation. A repeated question updates its episode in place. Facts and preferences go through a deterministic ADD / MERGE (NOOP+reinforce) / SUPERSEDE decision against the top 10 BM25 and entity candidates. | Mem0 ops, LightMem, Graphiti's tiered dedup, Hindsight's lexical guard | Removes the largest source of redundant items, KV writes and notices. Needs no LLM. |
| M3 | Bi-temporal validity (`valid_from`, `valid_to`, `superseded_by`). A superseded item is closed, not deleted. Recall supports as-of queries. | Zep / Graphiti, Mem0g invalidation, Supermemory `isLatest` | Gives correct knowledge updates and keeps provenance and history. Zep's bi-temporal edges come with its gain on temporal reasoning (45.1% to 62.4% with gpt-4o); its knowledge-update gain is small and depends on the model (+5.1 points with gpt-4o, −2.5 with gpt-4o-mini). |
| M4 | ACT-R base-level activation (Petrov approximation with k = 2) replaces created-at recency for all scopes. Recalls reinforce it and are persisted lazily without bumping the revision. The same activation drives pruning and per-scope caps. | ACT-R/Petrov, MemoryBank, Generative Agents (last-access clock), MemoryOS (heat eviction), Murre & Dros (power-law shape) | Uses fields that already exist. O(1) per item. Gives one principled signal for both ranking and forgetting. |
| M5 | Redundancy-aware recall: a saturating BM25 transform instead of max-normalisation, MMR selection (λ = 0.7) with hard dedup, and an optional token budget with greedy, skip-oversized packing. | MMR, AdaGReS, Hindsight packing, Mem0 sigmoid BM25 | Stops a summary and its source episodes from all being packed, and stops a weak lexical match from scoring 0.5. Measured: about 27 memory tokens per question on the eval suite (`context.build` 5,904 → 5,306 over 22 questions), and 523 → 216 recall tokens on the redundant unit stream. The gain grows with the repeat rate. |
| M6 | Consolidation rework: deterministic summary ids, O(1) lookup, no save when nothing changed, revision bumps only when visible content changes, a per-workspace revision for the answer cache, and the `by_entity` leak fixed. | Unified benchmark (local maintenance), sleep-time compute | Fixes duplicate summaries across replicas and stops the whole answer cache being invalidated every 25 asks. |

### 4.1 Implemented: agent memory (M1–M6)

All six items are in `crates/onto-intelli/src/memory.rs`, with the new settings in `MemoryConfig` (`config.rs`), `RecallIn`/`ListMemIn` and a new read-only `memory.stats` action in `actions.rs`, and the ask-path wiring in `intelli.rs`. Old stored items load unchanged: every new field is `#[serde(default)]` and is skipped when empty, so the KV layout in ns `memory` is the same.

| # | What landed | Where to look |
|---|---|---|
| M1 | `Clock` type and `Memory::open_with_clock`. Every time read goes through `self.now()`. Process-local counters, exposed in `MemoryStats` (`memory.stats`, `system.stats.memory`). New `ConsolidationReport` fields: `pruned`, `evicted`, `flushed`, `unchanged`, `skipped`. `Memory` is `Clone`. | `injected_clock_drives_episode_times_and_expiry`, `recall_counts_calls_items_and_tokens` |
| M2 | A repeated question updates its episode in place, via a `(workspace, subject, normalized question)` index. `remember()` gathers up to 50 candidates (BM25 top 10 plus entity postings). It merges a repeat only into a candidate with the same entities (or when the write names none): the same text, or similarity ≥ 0.85 with one text's content terms all contained in the other's and equal numbers and negation. A long sentence that changes one name passes the similarity threshold but not the containment check, so it supersedes instead. A merge never changes the candidate's entities. It supersedes a small change to a fact about the same entities when token Jaccard ≥ 0.5 and the numbers, the negation or at most 4 tokens per side differ. Otherwise it adds. A process-wide write lock serialises decisions. No LLM is called. | `remembering_the_same_fact_twice_merges`, `a_changed_fact_supersedes_the_old_one`, `negation_and_numbers_supersede_instead_of_merging`, `a_changed_name_in_a_long_sentence_supersedes_instead_of_merging`, `identical_text_about_different_entities_is_kept_apart`, `a_repeated_question_updates_one_episode` |
| M3 | `valid_from`, `valid_to`, `superseded_by` and `supersedes` on items. `RecallOptions { as_of, budget_tokens, include_superseded, exclude_key }` and `recall_with`, plus `list_with`. Recall and list hide superseded items by default. Forgetting the current item reopens the item it replaced; forgetting an older link of a chain joins its neighbours, so only one item stays current. An explicit `supersedes` must name an item in the same scope (shared or the same owner's private memories). Superseded items expire after `superseded_ttl_days`. | `as_of_recall_and_history`, `forgetting_the_replacement_reopens_the_old_fact`, `forgetting_a_middle_link_keeps_one_current_fact`, `explicit_supersede_keeps_the_scope`, `memory_actions_supersede_and_recall_as_of` (tests/intelli.rs) |
| M4 | `strength()` is the Petrov hybrid ACT-R estimate with k = 2 (d = 0.5, τ = −2.5, s = 1), floored at 0.5 for current facts and preferences, and it replaces created-at recency for both scopes. Recall reinforces `hits`, `last_access` and `recent` in RAM, marks the item dirty, and `flush_access()` (run first by consolidation) persists it without bumping the revision. The flush merges the access fields into the stored version under the write lock instead of writing the RAM copy, so a supersede or delete that reached the store first is kept. A reload of a dirty item keeps the larger access statistics. Consolidation prunes consolidated, never-recalled episodes below 0.4, which happens after about 14 days, and caps episodes and summaries per subject. | `strength_matches_reference_values`, `petrov_approximation_tracks_exact_activation` (within 5% of the exact sum), `recall_reinforces_and_persists_lazily`, `faded_consolidated_episodes_are_pruned`, `caps_evict_the_weakest_and_spare_facts`, `reload_keeps_unflushed_reinforcement`, `flushing_access_never_undoes_a_newer_store_write` |
| M5 | Saturating lexical score `s/(s+3)`. A pool of the top max(4k, 20). MMR selection (λ = 0.7) on the token overlap coefficient, with a hard drop at 0.8. Greedy packing that skips items over the remaining budget and returns the best item alone when nothing fits. The ask path uses `recall_budget_tokens` = 600 and excludes the asked question's own episode before selection. | `near_duplicates_are_not_recalled_together`, `a_summary_and_its_episodes_are_not_recalled_together`, `the_token_budget_bounds_recall`, `a_weak_lexical_match_ranks_below_an_entity_match` |
| M6 | Emptied `by_entity` sets and episode keys are removed. The revision is bumped only when visible long-term content changes. `revision_for(ws)` keys the answer cache per workspace. Summary ids are `mem_sum_<hash>`, and a legacy summary for the same group is reused. A summary is not rewritten when nothing changed. Consolidation is single-flight and runs on a blocking thread off the ask path. It plans each step without the write lock and takes the lock for one item or summary group at a time, re-checking the item first, so an ask waits for at most one write. | `replicas_write_one_summary_per_group`, `an_idle_consolidation_changes_nothing`, `revisions_are_per_workspace_and_ignore_recall`, `a_legacy_summary_is_updated_in_place`, `a_consolidation_does_not_hold_up_writes_for_the_whole_pass`, `answer_cache_survives_a_consolidation_that_changes_nothing` (tests/intelli.rs) |

Replication: every decision is made on the writing process and stored as whole items, so other replicas reload them by id from KV notices. `tests/postgres.rs` (`replicas_share_graph_schema_proposals_and_memory`) supersedes a fact written on replica A from replica B. It checks that A then recalls only the new fact, and that repeating the fact on A merges into B's item.

Differences from the plan above:

- When a write gives `valid_from`, the old item's `valid_to` is set to that `valid_from`, not to the write time (Zep's rule). Otherwise both items would be current between the two times. Without `valid_from`, the two are the same.
- An explicit `supersedes` target must also still be current.
- `RecallOptions` has an extra `exclude_key`, so the asked question's own episode does not use up the token budget. The `key != normalized` filter in `intelli.rs` is kept.
- A summary's text counts all current episodes of its group, not only the unconsolidated ones, so two replicas compute the same text from the same data. A group whose episodes are all consolidated counts as `unchanged`.

Measured results:

| Measurement | Before | After |
|---|---:|---:|
| Unit end-to-end stream (`a_redundant_stream_stores_less_and_recalls_cheaper_without_losing_facts`: 10 facts × 3 cosmetic repeats, 10 questions × 3 asks, 3 changed facts × 2): items stored | 66 | 23 (10 current facts, 3 superseded kept for history, 10 episodes) |
| Same stream: recall tokens over the 10 questions (ask path, k = 4) | 523 | 216 |
| Same stream: right fact among results / ranked first among facts | 10 / 10 | 10 / 10 |
| Same stream: stale (superseded) facts returned | 5 | 0 |
| `onto eval`, demo tenant, two runs: episodes stored | 40 | 20 |
| `onto eval`: passed | 22/22 | 22/22 |
| `onto eval`: prompt tokens, run 1 / run 2 | 2,579 / 2,580 | 2,579 / 2,573 |
| `context.build` for the 22 eval questions on the same stored data (legacy tenant after two old-binary eval runs): memory facts / memory tokens | 83 / 5,904 | 72 / 5,306 |

The eval suite asks each question once and has a single fact, so it contains little redundancy and the gain there is small (about 27 memory tokens per question). The gain grows with the repeat rate, as the unit stream shows.

### Engine RAM (onto-core)

| # | Change | Taken from | Expected effect at 1M entities |
|---|---|---|---|
| R0 | Measurement: a counting allocator in onto-bench, footprint rows, a regression test, and corrected BENCHMARK.md sizing | dhat, Tietz's counting-allocator method | Real numbers instead of RSS under compression. |
| R1 | `Graph.names` values become `NameIds::{One(Id), Many(Vec<Id>)}` instead of `BTreeSet<Id>` | Tietz, smallvec | About −1.3 to −1.5 GB (2,155 MB → about 700 MB). |
| R2 | `TextIndex` compaction: a term dictionary with u32 term ids, hybrid postings (small sorted Vec, promoted to a map when large), per-doc `Box<[u32]>` term ids, `Box<str>` ids, and a reused score buffer | fst/tantivy term-ordinal layout, bitpacking-style design without the codec | 777 MB → about 300 MB. The memory index shrinks the same way. |
| R3 | Key-less hashbrown `HashTable`s for `entities` and `links`, and `get_link` stops allocating | hashbrown `HashTable`, lasso's raw-entry idea | About −400 to −600 MB, and 3 fewer allocations per `get_link`. |

Together R1–R3 should take Graph+BM25 from about 6.1 GB to about 3.5–3.8 GB per million entities, with identical public behaviour. R0 checks that.

---

## 5. What we do not adopt (now) and why

| Idea | Source | Reason |
|---|---|---|
| LLM-decided ADD/UPDATE/DELETE on every write | Mem0 paper | Adds an LLM call to each write, can wrongly delete facts, and breaks the offline mock provider. The deterministic rules in M2 cover exact and near repeats. An LLM hook can be added later for the ambiguous band only. |
| LLM "memory evolution" (rewriting neighbouring notes) | A-MEM | Expensive, rewrites history and breaks provenance. The unified benchmark puts A-MEM last on latency. |
| Summary-only storage and recursive summaries in place of raw data | MemGPT, RAPTOR | In the unified benchmark's LightMem ablation on LoCoMo, raw user turns score 38.9 Answer F1 vs 15.6 for summaries only (31.4 vs 17.4 ROUGE-L F1 on LongMemEval), and LongMemEval finds summaries hurt QA. We keep raw episodes alongside summaries. |
| Full FSRS (21 weights, difficulty and stability) | FSRS | The weights were fitted on human flashcards and we have no graded reviews to fit our own. ACT-R gives the same power-law shape with 3 parameters. SM-2 is uncalibrated. |
| Embedding dedup (cosine ≥ 0.92–0.97) at write time | SemDeDup, Hindsight | Vectors are computed asynchronously and stay in PostgreSQL, so they are not available on the write path. A candidate for the sleep-time pass once the memory index has vectors. |
| MinHash/LSH or SimHash index | Broder, Manku, Graphiti | Memory sets are small per workspace and subject. BM25 top 10 plus entity postings, compared by trigram similarity and token sets over at most 50 candidates (`MAX_WRITE_CANDIDATES`), is cheaper and needs no extra index. Revisit above about 100k items per workspace. |
| HippoRAG Personalised PageRank recall | HippoRAG 2 | Medium effort with hub-entity risk. Do it after M5, measured with `onto eval`. |
| RRF across BM25, entity and pgvector channels | Hindsight, Zep | The memory layer has no vector channel yet. With two channels, a saturating BM25 transform plus overlap is enough. Revisit when pgvector memory recall exists. |
| Context paging / recursive conversation summary | MemGPT, Letta | onto-intelli packs per question with a hard budget already (`token::pack`). Conversation history is a separate concern. |
| Trained KV-compression memory | MemoRAG | Needs a trained model. |
| Feedback-weighted edges | Cognee | Needs a rating signal we do not collect yet. |
| Interning all graph ids into u32 (lasso), CSR adjacency, `Box<Provenance>` | lasso, petgraph CSR | The largest further cut (about −1 GB), but `out_edges`/`in_edges` return `&[(String, Id)]` (21 call sites) and `Entity.provenance` is a public field, so it is an API change. Planned as a follow-up after R0–R3 land. |
| FST term dictionary, bitpacked or Elias-Fano postings, Roaring | fst, bitpacking, PEF, Roaring | They need immutable segments and an LSM-style merge, a much bigger redesign than R2 for a smaller extra gain at our scale. |
| Copy-on-write `Ontology::fork` overlay | audit | The biggest peak-RAM win (no 6 GB spike per what-if question), but it is medium effort and touches every read path of the forked ontology. It is the next RAM item after R3, specified separately. |
| `ustr` global interner | ustr | Global and never freed, which leaks across tenants. |
| `serde_json` `preserve_order` for props | audit | Measured as a net loss: Link also embeds a `Map`. |

---

## 6. How we measure

- **Correctness gates:** `cargo test --workspace`, `cargo clippy --all-targets -- -D warnings`, and `onto eval` must stay 22/22 (the suite had 19 cases when this note was first written).
- **Memory quality:** unit tests with an injected clock (listed in §4.1), and the `memory.stats` counters (merged, superseded, pruned, evicted, reinforced, recalls, recall tokens).
- **RAM:** `onto-bench --entities 100000` and `--entities 200000`, before and after each R-item, with the new allocator-counted footprint rows plus the existing timing rows (load, ask, `by_name`, BM25 search, replication). A footprint regression test in `crates/onto-core/tests/footprint.rs` asserts a bytes-per-entity ceiling at 50k entities.

## Measured results

The estimates above guided the choices. What we measured after implementing them:

- **Agent memory** (unit test `a_redundant_stream_stores_less_and_recalls_cheaper_without_losing_facts`, a stream of 10 facts with repeats, 10 repeated questions and 3 changed facts): stored items 66 → 23, recall tokens 523 → 216, the right fact found 10/10 before and after, stale superseded facts returned 5 → 0. On the demo competency suite the gain is small (it has little redundancy): 22/22 before and after.
- **RAM** (onto-bench at 200,000 entities, allocator-counted live heap after a replica open, identical in two runs): 1,206 MB before; 900 MB after R1, 822 MB after R2, 724 MB after R3, that is 6,320 → 3,797 bytes per entity (−40%). Latencies stayed level. See [BENCHMARK.md](BENCHMARK.md) for the full table; macOS resident-memory deltas were too noisy to use.

