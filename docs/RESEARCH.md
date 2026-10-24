# Research behind v0.2

v0.2 was driven by three industry systems and a scan of the 50 most recent
ontology papers on arXiv (cs, October 2026). This page records what was taken
from where, so design decisions stay traceable.

## Industry systems

| System | Idea | onto-intelli feature |
|---|---|---|
| Palantir Ontology — action types | Governed, parameterized edits with submission criteria and side effects | `[action_types.*]`: typed parameters, preconditions, conditional effects, roles, risk, audit event, receipts (`action.plan/apply/undo`) |
| Palantir Ontology — interfaces | Polymorphism: one shape implemented by several object types | `[interfaces.*]`, `implements = [...]`; `kind = "work_item"` matches issues, tasks, milestones |
| Palantir Ontology — functions | Business logic over objects | Derived properties (`[object_types.X.derived]`: count/sum/avg/min/max over relations, transitive) |
| Palantir — scenarios | What-if analysis without touching live data | `Ontology::fork()`, `scenario.run`, "What if task 31 were done?" |
| Palantir — agents anchored to ontology scope and permissions | Agents use the same objects, permissions and actions as people | MCP server (`onto mcp`) exposing the permission-filtered ontology; writes only through action types |
| Databricks Unity Catalog — metric views | Governed KPI definitions (measure + dimensions), certification | `[metrics.*]` with measures, filters, dimensions, `certified`; `metric.query`; grouped NL questions |
| Databricks — business semantics / glossary | Words people use mapped to governed concepts | `[glossary]` kinds, relations, dimensions ("tickets" → issue, "per team") |
| Glean — enterprise + personal graph | Permission-aware results personalised by team and collaboration | Hybrid search (BM25 + vector, reciprocal rank fusion) boosted by graph distance from the caller and recency |
| Glean — expertise from the graph | Who to ask, inferred from work and ownership | `people.experts`, "Who knows about PostgreSQL?" with reasons |

Sources: [Palantir Ontology overview](https://palantir.com/docs/foundry/ontology/overview/),
[Databricks Unity Catalog semantics](https://docs.databricks.com/aws/en/uc-semantics),
[Glean Enterprise Graph](https://www.glean.com/product/enterprise-graph).

## open-ontologies: the assurance layer

The assurance layer ([ASSURANCE.md](ASSURANCE.md)) takes its philosophy from
[open-ontologies](https://github.com/fabio-rovai/open-ontologies) by Fabio
Rovai / Tesseract Semantics (MIT licence): *Open Ontologies: Tool-Augmented
Ontology Engineering with Stable Matching Alignment* (Rovai, 2026,
[arXiv:2605.09184](https://arxiv.org/abs/2605.09184)). open-ontologies works
on OWL and RDF; onto-intelli reimplements the ideas for its typed property
graph and shares no code with it.

| Idea in open-ontologies | onto-intelli feature |
|---|---|
| An inference is not an assertion; every inference carries a derivation certificate | `reason.*`: derived facts live in a closure, never in the graph; `reason.certify` writes `oi-cert/1` certificates |
| An independent checker re-derives each line and refuses forged ones | `crates/onto-cert`: depends on std, sha2 and serde_json only; `onto cert check` and `onto-cert check` run offline |
| Rules you wrote earn a different verdict word than the built-in semantics | `entailed` versus `entailed_under_supplied_rules` |
| Checker words belong to the checker | the engine says `derived`; only an onto-cert report carries a verdict |
| What the tool proves, measures or only suggests | the proved / measured / opinion table and every report's `means` field |
| Plan before apply, with semantic consequences and conservativity | `consequences`, `risk`, `locked_violations`, `likely_renames` and `shapes` in every schema plan; schema locks |
| SHACL validation as a three-valued measurement | `shapes.validate`: `conforms`, `violates`, `undetermined`; `shapes.check`, `shapes.induce` |
| Ontology lint with reviewer feedback | `schema.lint`, `schema.lint_feedback` |
| Alignment as an opinion, with SSSOM output and certification of mappings | `schema.align` (SSSOM TSV), `schema.align_certify` |
| A retrieval slice preserves a conclusion or it does not; coverage is a proxy | `reason.slice_check`; preservation of derived facts in the packed LLM context |
| Justifications, provenance semirings and claim verification | `reason.explain`, `reason.provenance`, `reason.verify` |
| One sheet in, one ontology out | `model.induce`, `ingest.sheet` |
| Domain packs | ten industry packs ([PACKS.md](PACKS.md)) |
| Tool profiles for agents | `onto mcp --profile` |

What differs: open-ontologies checks its certificates against proofs written
in Lean; onto-cert is a tested Rust checker without a machine-checked proof.
The rule language here is Horn rules over the property graph (kind
inheritance, interfaces, transitive and symmetric link types, chain and
when/then rules), not OWL.

## Papers (arXiv, cs, ontology — newest 50)

Papers that shaped features (most of the 50 are domain ontologies in medicine,
biology, robotics or audio and were read but not applicable):

| Paper | Takeaway | Feature |
|---|---|---|
| [POLAR: Ontology-Guided Risk Prevention for Tool-Calling LLM Agents (2610.08082)](https://arxiv.org/abs/2610.08082) | Score each action's reversibility by deriving an inverse; gate risky calls before execution | Every action plan derives per-effect inverses; `reversible`, `risk`, `needs_confirmation`; undo receipts |
| [OntoPlan (2610.07649)](https://arxiv.org/abs/2610.07649) | Check preconditions against the world state; ask follow-ups on ambiguity instead of committing; selective retrieval cuts tokens | Action preconditions; `clarify` answer mode with candidate options |
| [Label Agreement Does Not Measure Authorization (2610.04544)](https://arxiv.org/abs/2610.04544) | An agent's proposal is not entitlement to act; gate commitment | NL commands are proposals until `confirm`; roles per action type; denials audited |
| [Plant Memory (2610.19131)](https://arxiv.org/abs/2610.19131) | Verified facts with evidence; ontology drift proposals for new terms; serve agents over MCP | `schema.suggest` drift proposals; low-confidence facts in the quality report; MCP server |
| [QRAKEN: graph distillation (2610.08095)](https://arxiv.org/abs/2610.08095) | Ground LLMs in patterns that actually exist in the populated graph, not the schema | System prompt built from the populated pattern profile (`schema.profile`), cache-stable |
| [Ontology Tower (2610.11768)](https://arxiv.org/abs/2610.11768) | Narrow-and-deep knowledge: derived quantities and lessons attached to the entities agents ask about | Derived properties in context facts and lookups; entity-linked long-term memory |
| [Header-centric Data Quality Assessment (2610.10541)](https://arxiv.org/abs/2610.10541) | A taxonomy of data-quality issues rolled into a source-level score | `quality.report`: missing properties/links, invalid states, uniqueness, staleness, orphans, dangling provenance, duplicates, low confidence; per-kind and overall scores |
| [ERP Event Logs as canonical form for KPIs (2610.09903)](https://arxiv.org/abs/2610.09903) | Volumes, durations and rates fall out of one (case, activity, timestamp) log | Event and version-history measures: lead time (p50/p90), throughput, failure rate, event volume |
| [Reliability of LLM Judges for Entity Alignment (2610.09554)](https://arxiv.org/abs/2610.09554), [Typed Semantic Links (2610.01393)](https://arxiv.org/abs/2610.01393) | Entity alignment needs structural evidence, not name similarity alone | Duplicate detection from identity properties, names and shared neighbours; `entity.merge` with permanent id redirects |
| [EIO-Agents: semantic layer for agent evaluation (2610.07675)](https://arxiv.org/abs/2610.07675) | Evaluations must keep the evidence-to-decision chain | `onto eval`: competency questions with expectations, failures and the facts each answer used |
| [OntoInk (2610.05945)](https://arxiv.org/abs/2610.05945) | Visualisation, SHACL-style validation and reasoning in one workflow | Mermaid export with alert highlighting; constraint checks; Turtle/OWL export |
| [ECMO crisis-management ontology (2610.01326)](https://arxiv.org/abs/2610.01326) | Modular ontologies with domain modules | Additive schema layers (types now merge across layers) |
| [Uncertainty in KG representation learning (2610.06974)](https://arxiv.org/abs/2610.06974) | Confidence-scored triples need explicit handling | `confidence` / `verified` on entities and links surfaced in quality reports |
| [BEACON-SP ontology-grounded GraphRAG (2610.09026)](https://arxiv.org/abs/2610.09026) | Ontology-guided multi-hop retrieval beats vector RAG | Confirms the context-graph design; temporal facts added via version history |
| [OntoAct (2610.21983)](https://arxiv.org/abs/2610.21983) | Hypotheses verified symbolically against observations | Root-cause chains as checkable hypotheses; what-if scenarios on forks |
| [Learning to structure data (2610.01463)](https://arxiv.org/abs/2610.01463) | Induce schemas from data iteratively | Profile-driven schema suggestions (`onto schema suggest`) |
