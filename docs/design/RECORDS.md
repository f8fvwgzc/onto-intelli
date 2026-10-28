# Records: ontology types as real tables (proposal)

Status: **proposal**, nothing here is built. It answers one question: how can a product keep
high-volume business records (orders, tickets, invoices) under its ontology, with CRUD APIs and field
permissions, without loading every row into onto's in-memory graph?

## 1. Why

onto keeps the whole graph in memory: entities, links, a BM25 index and vectors per tenant. That is right for
the knowledge graph (people, teams, services, tables, decisions: thousands to a few hundred thousand
things) and wrong for records that grow by millions. Today a product can only crawl such records in
(`sync = true` object types read rows a page at a time, `ONTOLOGY_BUILDER.md#row-sync`), and every
synced row becomes an in-memory entity.

**Records** are ontology object types whose instances live in real PostgreSQL tables in the
customer's own database, outside the graph. The ontology stays the single description of the data:
types, properties, states, links and action types come from the live schema. The rows stay where SQL
is good at them.

## 2. Model

- An object type opts in with `storage = "records"` in the schema (`[object_types.order]`). Its
  properties become columns, its `key` the primary key, its states a `CHECK` list, a link with
  cardinality one a foreign-key column, a many-to-many link a join table.
- Records live in the **customer's PostgreSQL** (a database the tenant names; the same pgvector
  server as onto's tenant schemas, or another one), in a schema per tenant, `records_{tenant}`. The
  table is `records_{tenant}.{type}`. onto owns these tables: it creates and migrates them; the
  customer reads them with any SQL tool.
- Every table gets the columns the permission model needs: `id`, `tenant`, `parent` (the scope, see
  §4), `ancestors` (the scope path, for list filters), `created_at`, `updated_at`, `version`
  (optimistic concurrency), and `deleted_at` (soft delete for undo receipts).
- Schema changes go through the existing live-schema pipeline (`schema.plan`, proposals, approvals).
  A change set on a records type plans a **table migration** next to the ontology change: an added
  property is `ALTER TABLE … ADD COLUMN` (nullable, or with a default); a removal is a rename to
  `_dropped_{name}_{version}` that a later vacuum drops, so a revert can bring it back; a type
  change is refused unless it widens (int → bigint, varchar(n) → text). The impact plan shows the
  rows affected, from `pg_class.reltuples`.
- Links between records and graph entities (an order `handled_by` a team) are stored as foreign keys
  to the entity id (text). The graph never holds the records; it holds their **type**, and graph
  queries that cross into records (`graph.traverse` from a team to its orders) run one indexed SQL
  query at the boundary, with a limit.

## 3. API

CRUD per type, the same shape over HTTP, gRPC and the action layer:

| Call | HTTP | Notes |
|---|---|---|
| `records.create` | `POST /v1/records/{type}` | Validated against the type: required properties, enum values, states, link targets exist. |
| `records.get` | `GET /v1/records/{type}/{id}` | Field mask applied (§4). |
| `records.update` | `PATCH /v1/records/{type}/{id}` | `If-Match: <version>`; a stale version is `409 conflict`. Write needs Write on every field it touches. |
| `records.delete` | `DELETE /v1/records/{type}/{id}` | Soft delete with an undo receipt; hard delete only through retention. |
| `records.list` | `GET /v1/records/{type}?filter=…&sort=…&cursor=…` | Keyset pagination. The filter is a small typed AST, never SQL text. Permissions are a WHERE clause (§4). |
| `records.aggregate` | `POST /v1/records/{type}/aggregate` | count, sum, avg, min, max by up to three properties, for governed metrics (`metrics.rs`). |

Action types work on records unchanged: their effects (`patch`, `create`, `link`) compile to the
records calls in the same transaction, with the same plan, risk, confirmation and receipt. Agents use
records through action types only (as for the graph's raw writes, f8fvwgzc/onto-intelli#3).

Every write records an audit event in the tenant's append-only `events` (who, which record, which
fields, never the values of fields the reader of the audit log may not read). Versions of a record are
kept in `records_{tenant}.{type}_versions` (append-only, like `entity_versions`), so `as_of` and
history work for records too.

## 4. Permissions: Tutela's model

Field and row permissions come from Tutela (f8fvwgzc/tutela, `docs/PERMISSION-MODEL.md`), not from
onto's `visibility`/`acl` properties. The rules that matter here:

- Four levels on every item, field and feature: None < Read < Write < Full. **Field level = the lower
  of the role level and the closest field rule.** Rules only narrow.
- Every record has exactly one parent scope (its `parent` column: a project, a team, an org); the
  closest role assignment walking up decides; a private scope stops inheritance.
- **Links never grant access.** A link from a record to a team does not let the team read it.
- **Lists are filtered inside SQL** by the same evaluator that answers `check()`. onto calls
  `Pdp::list_filter(principal, "{type}:read")` and compiles its three parts (`ids_in`,
  `container_in`, the condition AST) into the WHERE clause over `id`, `ancestors` and the mapped
  columns, with every value a bound parameter. Counts and totals use the same filter, so a page never
  reveals hidden rows.
- **Field masks everywhere data leaves:** the SELECT list comes from the decision's `field_mask.read`
  (masked columns are not selected at all); responses, search, exports, audit events and the context
  sent to an LLM all go through the same mask. `records.update` checks Write on each field it sets.
- An **agent** acting for a person gets the overlap of both decisions, for every call.

onto registers each records type with Tutela as a resource type, and each property as a field (its
`field_registry`): a property marked `confidential` or `secret` in the schema starts at None, as
Tutela's defaults say. Decisions are cached per (principal, type, model revision), so a list call
costs one evaluator round trip, not one per row.

Graph entities keep onto's own model (`visibility`, `acl`, `member_of`): records add Tutela on top,
they do not replace the graph's model.

## 5. What stays out of the graph, and what goes in

- Rows never enter the in-memory graph, the BM25 index or the in-memory vectors, so a tenant's
  memory (and its estimate for eviction budgets, f8fvwgzc/onto-intelli#3) does not grow with them.
- The graph keeps, per records type: its definition, row count, freshness and its links to other
  types (for the schema view, lineage and the ontology builder), and **aggregates** that questions
  need often (counts by state, by owner), refreshed after writes in a background job, as facts with
  provenance.
- `intelli.ask` answers record questions through `records.list` and `records.aggregate` calls
  planned from the question (the deterministic intents `count`, `list`, `metric`), with the field
  mask applied before anything reaches the LLM.
- Semantic search over records, when a type asks for it (`search = true`), uses a pgvector column on
  the records table and an HNSW index, filtered by the same SQL permission filter, never the
  in-memory index.

## 6. Migration from synced types

A type crawled with `sync = true` from a source database stays as it is: the source is the system of
record. `storage = "records"` is for data onto owns. A tenant can move a synced type to records with
a one-time import (`records.import {type, from: connector}`) that copies rows in key order, then
flips the type; the import is a planned change with an impact report and approval, like any schema
change.

## 7. Open questions

1. One database per tenant, or one schema per tenant in a shared database? (The same question as
   onto's own tenants; start with one schema per tenant.)
2. Tutela's `ancestors` column: array, `ltree` or closure table (Tutela's week-1 spike decides).
3. Cross-tenant records (a customer shared by two projects): out of scope; one record has one parent.
4. Back-pressure for bulk writes (imports of millions of rows): COPY with batched audit events.

## 8. Plan, if accepted

1. Schema: `storage = "records"`, validation, the table mapping, the migration planner (no API yet).
2. `records.*` calls with onto's own principals and no field masks, behind a feature flag, with
   load tests (10M rows, p99 list and write latency).
3. Tutela integration: resource and field registration, `check()` and `list_filter`, field masks on
   every exit, the agent overlap.
4. Action types, history, `as_of`, aggregates as facts, `intelli.ask` over records.
5. Records semantic search; the synced-type import.
