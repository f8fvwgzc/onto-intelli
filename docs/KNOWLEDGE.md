# Knowledge: documents and passages

onto-intelli keeps a knowledge base next to the ontology. A **document** is
split into **passages**, and a search returns the passages that best answer a
question, each with a citation. Documents are not entities. They never enter
the graph, entity search, reasoning or the change feed, so a manual or a
contract cannot turn into nodes by accident.

The module is `crates/onto-intelli/src/knowledge.rs`. The actions are
`knowledge.*`, the gRPC service is `KnowledgeService`, and on HTTP every
action is `POST /v1/call/knowledge.<name>`.

## Documents in

`knowledge.ingest` takes one of three forms:

| Input | Use |
|---|---|
| `text` | Plain text or Markdown. `#` headings open sections. A `\|` table becomes a table, and a `\|---\|` line after its first row makes that row the header. A form feed (as `pdftotext` writes) starts a page. |
| `blocks` | What a parser returns: `{kind: heading\|text\|table, level?, text?, page?, rows?, header_rows?}`, plus `pages`. |
| `passages` | Passages already cut: `{id?, content, page?, section_path?, kind?, embedding?}`, plus `embedding_model`. An import keeps its passage ids and, when its model matches, its vectors. |

Other fields: `id` (keep this document id; an existing document with it is
replaced), `name`, `workspace`, `collection`, `sensitivity`, `acl`,
`media_type`, `source`, `meta`.

How text is cut. These are nexc-engine's rules, so one document gives the
same passages in both:
- Text gathers under its headings, up to about 1,600 characters (at most 2,400).
- A long paragraph is cut at a sentence end or a space.
- A heading starts a new passage, and `section_path` holds the headings above it ("Report › Revenue › By region").
- A table becomes `label: value` lines, one per data row. A two-row header names a column "2024 / Q1", and an empty header cell under a merged one takes its left neighbour's label.
- A document keeps at most 20,000 passages. A longer one is cut there and marked `truncated`.

The same content (same text, blocks or passages) is kept once per workspace.
Ingesting it again returns the existing document with `created: false`.
Re-ingesting an `id` replaces that document's passages, and fewer passages
remove the rest. It keeps `created_at`.

## Search

`knowledge.search {query, workspace?, collection?, documents?, k?, max_sensitivity?, embedding?, budget_chars?, mode?}`

Two rankings are fused by reciprocal rank fusion (k = 60):

- **Keywords:** BM25 over the passage and its section path.
- **Meaning:**
  - With `embedding: {model, vector}`, the cosine of the query vector with passages embedded by the same model (imported with `embedding_model`). Vectors of another model do not compare.
  - Otherwise, the hashed word and character-trigram vectors onto-intelli uses for entities, which match other forms of a word ("deployments" and "deploy") but not synonyms.

`score` runs from 0 to 1: 1 is first in both rankings. Scores compare within
one search only. `signals` gives each ranking's rank (`lexical_rank`,
`embedding_rank` or `vector_rank`). `mode` is `hybrid` (default), `lexical` or
`vector`. `budget_chars` stops adding passages once their text would pass it;
the first passage is always kept. Each hit carries `passage_id`,
`document_id`, `document_name`, `collection`, `sensitivity`, `page`,
`section_path`, `kind`, `content` and `citation` ("Report.pdf, p. 12 ›
Revenue").

## Who reads what

- **Workspace:** a read role in the document's workspace.
- **Sensitivity:** `public` < `internal` (default) < `confidential` < `restricted`. A search can lower the ceiling with `max_sensitivity`. Agents never get above `[knowledge] agent_max_sensitivity`, which defaults to `internal`, whatever they ask; this is the `ai_reach` of nexc-engine's data labels.
- **ACL:** a document with an `acl` is read only by the principals listed (by principal id or by entity id) and by admins. Unlike entity ACLs, teams are not followed through `member_of`.
- **Writing:**
  - Members ingest, replace, rename, regroup and delete documents.
  - Lowering a document's sensitivity, or widening or removing its `acl`, widens who reads it, so only admins may do it, whether through `knowledge.update` or by re-ingesting the document's `id`. Setting or narrowing an `acl` is anyone's who may write.
  - Agents may not write knowledge: the knowledge actions are not on `AGENT_WRITES`.

## Other actions

- `knowledge.get {id, passages?}`: one document, with its passages if asked.
- `knowledge.list {workspace?, collection?, limit?}`: documents, newest first.
- `knowledge.update {id, name?, collection?, clear_collection?, sensitivity?, acl?, set_acl?, meta?}`: `set_acl` with no `acl` clears it; `collection: ""` or `clear_collection` takes the document out of its collection.
- `knowledge.delete {id}`.
- `knowledge.collections {workspace?}`: collections, with their document and passage counts.
- `knowledge.stats`: documents, passages, and embedded passages by model.

## Storage

Documents and passages live in the tenant's KV store, namespace `knowledge`
(`d/<document>` and `p/<document>/<ordinal>`). On PostgreSQL that is the
tenant schema's `kv` table. Indexes are kept in memory and rebuilt when a
tenant opens. Other processes on the same store hear about every write
(`REPLICATED_KV`). A passage row whose document row is missing (an ingest
that has not finished, or a delete in progress) is not served. The passages
are written first and the document row last; a delete removes the document
row first.

Memory grows with the passages: about the text, plus 512 bytes of hashed
vector, plus 4 bytes per dimension of an imported embedding. A tenant with a
very large knowledge base is better served by a PostgreSQL index of its own;
that is not built yet.

## Moving nexc-engine's knowledge base here

nexc-engine's `docs/PRODUCT-MIGRATION.md` §7.4 maps its tables like this:

| nexc-engine | onto-intelli |
|---|---|
| `documents` row | a document: `id` = the document's uuid, `name`, `source` = `nexc:<uuid>` |
| `document_chunks` (`ordinal`, `page`, `section_path`, `kind`, `content`) | its `passages`, `id` = the chunk's uuid, same order |
| `document_chunks.embedding` + `embedding_model` | `passages[].embedding` + `embedding_model`, copied as they are when the model is kept; otherwise ingest the `text` or `blocks` again and let the built-in vectors (or a new model) apply |
| data label `public`, `internal`, `confidential`, `restricted` | `sensitivity`, same names |
| `knowledge_topics` | `collection` (a passage's topic becomes its document's collection; topics that split one document are not kept) |
| `workspace_knowledge_settings.passages`, `budget_chars` | `k` and `budget_chars` on each search |
| agent clearance (`ai_reach`) | `max_sensitivity` on the search, capped for agent principals by `agent_max_sensitivity` |

For the shadow period, ask `knowledge.search` with the same query, `k`, and
query embedding (`embedding: {model, vector}`) that the product uses. Passage
ids are the chunk ids, so the overlap of the top *k* is a set intersection.
Fusion and normalisation follow nexc-engine's `fuse`: two rankings, RRF with
k = 60, divided by the best possible score. One difference remains: nexc's
keyword ranking is PostgreSQL full text (`to_tsvector('simple', …)`), and this
one is BM25 with stopwords removed and plural `s` stripped. Some keyword
ranks will differ.
