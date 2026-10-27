# The data catalog: connectors, OntoRank, learned facts

onto-intelli can map a data estate the way Databricks Genie's knowledge
layer does, but outside any one platform. Connectors read the metadata of
your databases, never their rows, into the ontology. (The one exception is
opt-in: tables that back a synced object type, see
[ONTOLOGY_BUILDER.md](ONTOLOGY_BUILDER.md#row-sync).) A learner infers how
the data is used, and OntoRank decides what to trust. Questions like "Which
table holds revenue?" are then answered from the graph, deterministically
and with zero tokens.

```bash
onto connect postgres env:WAREHOUSE_URL --name warehouse --every 15m
onto ask "Which table holds revenue?"
onto ask "How do orders join to customers?"
onto ask "Which revenue metric is the trusted one?"
onto rank table:warehouse.finance.revenue_recognized
onto learned --type metric
```

`onto seed demo` loads a demo warehouse (the same pipeline, fed a fixed
snapshot) so you can try all of this without a database of your own.

## The model

The `data-catalog` schema package is layered on the enterprise schema by
default (`[ontology] extend = ["builtin:data-catalog", "builtin:code"]`;
`builtin:code` adds the packages of the repository connector,
[CONTROL_PLANE.md section 13](CONTROL_PLANE.md#13-repository-connector)). It is TOML like
any other layer
([`schemas/data-catalog.toml`](../crates/onto-core/schemas/data-catalog.toml)).

| type | id | what |
|---|---|---|
| `data_source` | `data_source:{connector}` | a crawled database |
| `db_schema` | `db_schema:{connector}.{schema}` | a namespace |
| `table` | `table:{connector}.{schema}.{name}` | tables, partitioned and foreign tables, materialized views |
| `view` | `view:{connector}.{schema}.{name}` | views, with their SQL |
| `column` | `column:{connector}.{schema}.{table}.{column}` | type, nullability, primary key, `unique`, `enum_values` (enum labels or a `CHECK` list), comment |
| `query` | `query:{connector}.{queryid}` | a normalized statement from `pg_stat_statements` and how often it runs |
| `learned_fact` | `learned_fact:{type}.{hash}` | an inferred join path, metric definition or synonym |

| relation | between |
|---|---|
| `contains` (inverse `contained_in`) | source → schema → table/view → column |
| `references` | foreign keys: column → column, and table → table with the join condition |
| `reads_from` (inverse `read_by`) | query or view → the tables and views it reads |
| `owns` (inverse `owned_by`) | team or person → asset |
| `concerns` (inverse `learned_facts`) | learned fact → the assets it is about |

## Connectors

A connector is registered once and stored in the database, so every replica
sees it. A background crawler, one lease per connector across all
processes, crawls it every `every_secs`.

```bash
onto connect postgres env:WAREHOUSE_URL \
  --name warehouse \
  --schema 'sales' --schema 'finance' \
  --every 15m \
  --owner finance_owner=team:finance \
  --map-grants --grant analyst=team:analytics
onto connectors                       # what is registered, last crawl, errors
onto crawl warehouse                  # crawl now
onto connectors --remove warehouse --purge
```

`onto connect repo <path | git URL>` registers the other kind of
connector, a repository: it maps a monorepo's packages, their internal
dependencies and CODEOWNERS (section 13 of
[CONTROL_PLANE.md](CONTROL_PLANE.md#13-repository-connector)).

- **Credentials.** Pass `env:VAR` and the URL is read from the environment
  at crawl time, so it never reaches the store. A literal URL also works;
  every API output masks its password.
- **What PostgreSQL gives.** The connector reads, in one read-only
  transaction with a statement timeout:
  - schemas, tables, views, columns, types, primary keys, `COMMENT ON` text
    and foreign keys from `pg_catalog`;
  - the labels of enum types (also behind a domain), single-column unique
    constraints and unique indexes, and the value lists of single-column
    `CHECK (col IN (…))` constraints;
  - view dependencies from `pg_depend`;
  - grants from `relacl`;
  - scans, writes and last analyze from `pg_stat_user_tables`;
  - with `pg_stat_statements` installed, the most-called normalized
    `SELECT`s. Literals in those are `$n`; utility statements are skipped,
    since they are not normalized. Grant `pg_read_all_stats` to see other
    roles' statements.

  It never selects from a table for the catalog. Rows are read only for
  object types whose schema says `backing.sync = true`, and only their key
  and mapped columns ([row sync](ONTOLOGY_BUILDER.md#row-sync)).
- **Incremental.** Each crawled entity and its links get a fingerprint.
  Usage counters only count as a change when they cross a power of two, so a
  busy table is not rewritten on every crawl. A crawl writes only what
  changed, deletes what disappeared (only entities this connector created),
  and drops foreign keys or reads that no longer exist. An idle database
  costs no writes.
- **Never in the way.** The crawl reads the source with no lock on the
  ontology. It writes through the normal audited path in batches of 500
  mutations, so queries are served between batches. Each entity carries
  provenance: `connector:postgres`, the connector name, the crawl run and
  `crawled_at`. Each crawl leaves a `connector.crawled` event.
- **Permissions.** Crawled assets live in the connector's workspace, and
  every read is permission-filtered as usual. With `map_grants`, a table
  PUBLIC cannot `SELECT` becomes `visibility = "restricted"`, with an ACL of
  the principals its readers map to (`grant_map`; unmapped roles appear as
  `role:{name}`). Its columns follow it. A query is visible to whoever may
  read every table it reads.
- **Owners.** `owner_map` turns a database role into an `owns` link from a
  team or person. Owners set by hand are never touched.

### Other databases: MySQL and MariaDB, SQL Server, MongoDB

The same registration, crawl, fingerprints, provenance and permissions apply to three more kinds
(`connector.add` with `kind`, or `POST /v1/catalog/connectors`):

| `kind` | URL | Metadata | Rows of synced types |
|---|---|---|---|
| `mysql` (also MariaDB) | `mysql://user:pass@host:3306/db?ssl-mode=REQUIRED` | `information_schema`: databases as schemas, tables, views (definitions; the tables they read on MySQL 8.0.13+), columns with `COLUMN_TYPE`, keys, comments and `ENUM` labels, foreign keys, `SELECT` grants, and the most-run statements of `performance_schema` when readable | `JSON_OBJECT` pages in key order |
| `sqlserver` | `sqlserver://user:pass@host:1433/db?encrypt=true` | the catalog views of one database: schemas, tables and views (row counts from `sys.partitions`, definitions, dependencies), columns with keys, `MS_Description` comments and column `CHECK` lists, foreign keys, `SELECT` grants | `FOR JSON PATH` pages in key order |
| `mongodb` | `mongodb://user:pass@host:27017/db?authSource=admin` | databases as schemas, collections as tables, views with their pipeline; fields **inferred from a sample** (`sample_rows`, default 100, at most 1000; `$sample`): the most common BSON type (`mixed(a\|b)` when several), nullable when missing or null in some, nested documents as `a.b`; `_id` is the key; single-field unique indexes; a field `x_id`/`xId` of object ids becomes a foreign key to a collection `x` or `xs` of the same database (a heuristic) | `find` pages in key order (one key field; object ids as their hex text) |

Every connector only reads: a read-only MySQL session with a statement time limit, `SET LOCK_TIMEOUT`
on SQL Server, `maxTimeMS` on MongoDB. No row is read for the snapshot; rows are read only for synced
object types, with only the key and mapped columns. The ontology builder (`ontology.draft`) drafts
object types, states (from `ENUM` labels and `CHECK` lists), properties and links from these
catalog entities exactly as from PostgreSQL's.

**Restricted connectors** (registered by an API key or a console session) follow the same SSRF rules
as PostgreSQL's: one TCP host (no socket, no host list, no `mongodb+srv`), a user for the SQL engines,
only the query parameters each kind allows (`ssl-mode`; `encrypt`, `trust_server_certificate`;
`authSource`, `authMechanism`, `tls`, `appName`), no `env:` URL, and, unless
`console.allow_private_urls`, a host that resolves only to public addresses. The connection goes to
the checked address: for SQL Server the TCP stream is pinned while TLS still verifies the name;
MySQL with `ssl-mode=VERIFY_IDENTITY` and MongoDB with `tls=true` connect by name, since their
certificates are checked against it. MongoDB always connects directly to that one host
(`directConnection`), so a replica set cannot redirect the crawl.

SQL Server: `encrypt` is `true` by default (everything encrypted, failing without TLS); `true`,
`yes`, `1`, `on`, `mandatory` or `required` keep that, `false`, `no`, `0`, `off` or `optional`
encrypt the login only, and any other value (`strict` too: TDS 8.0 is not supported yet) is
refused rather than read as off. `trust_server_certificate` takes `true` or `false` alike; both
names are read in any case (`Encrypt`, `TrustServerCertificate`). With `map_grants`, only a
table-level `GRANT SELECT` makes a reader: a column grant does not, nor (fewer readers, never
more) a grant on a schema or the database. A `DENY` wins: on the table, a column, its schema or
the database, or membership in `db_denydatareader`, it blocks the denied principal and everyone in
it through nested roles. As the ACL only lists readers, it fails closed: once anyone is denied,
`public` is no reader and neither is a granted role that holds a denied principal. A `DENY` to
`public` leaves no reader. A permission row that cannot be read counts as a `DENY`. A `DENY` of
`CONTROL` counts as one of `SELECT`, which it covers. Windows and Entra ID groups (principal types
`G` and `X`), and role members of a type that cannot be read, are opaque: their members live
outside the database. A `DENY` to one counts as a `DENY` to everyone, and so does a `DENY` to a
role that holds one. Once anyone is denied, a granted principal that holds one (directly or
through nested roles) is no reader.

**`map_grants` on SQL Server needs `VIEW DEFINITION` on the database** (`GRANT VIEW DEFINITION TO
<crawl user>`). Without it, SQL Server shows the crawl account only its own permission rows and
role memberships, so a `DENY` to anyone else would look absent. The crawl checks
(`HAS_PERMS_BY_NAME(DB_NAME(), 'DATABASE', 'VIEW DEFINITION')`) and, without it, does not read the
permission catalogs at all: it maps every table as restricted with no readers and says so in the
crawl's notes. When reading them fails anyway, the crawl goes on the same way, with the error in its
notes. The crawl account otherwise needs
only `db_datareader` (or `SELECT` on the crawled schemas).

Drivers: `sqlx-mysql`, `tiberius` (rustls) and the official `mongodb` driver, all MIT or Apache-2.0.
CI runs the connectors against MySQL 8.4, MariaDB 11.4, SQL Server 2022 and MongoDB 8 containers
(`crates/onto-intelli/tests/connectors_dbs.rs`; set `ONTO_TEST_MYSQL_URL`, `ONTO_TEST_MARIADB_URL`,
`ONTO_TEST_SQLSERVER_URL` or `ONTO_TEST_MONGODB_URL` to run them locally).

## OntoRank

Every crawl, and `onto rank --recompute` (`catalog.rerank`), scores data
assets and knowledge from 0 to 1. Each component is stored with its
evidence, so a score can always be explained:

```text
$ onto rank view:warehouse.finance.revenue_recognized
view:warehouse.finance.revenue_recognized  authority 0.82  rank 3
  centrality     0.49  PageRank 1.64
  usage          1.00  9800 scans and calls
  freshness      1.00  0.0 days since activity
  owner          1.00  team:finance
  documentation  1.00  100% of columns documented
  certification  0.50  subject of learned_fact:metric.f2a46d3a0c79
```

| component | weight | from |
|---|---:|---|
| centrality | 0.25 | personalized PageRank: queries and views pass authority to what they read (more calls, more weight), foreign keys to what they reference, columns to their table; usage seeds the teleport |
| usage | 0.25 | scans plus query calls (tables, views), calls (queries), half the table's (columns), support (learned facts); log-scaled within each kind |
| freshness | 0.10 | `exp(-days / 30)` since the last activity or change |
| owner | 0.15 | owned by a team 1, a person 0.8, only a database role 0.3; inherited from the container at 0.8 |
| documentation | 0.15 | described (0.5) plus the share of documented columns (0.5) |
| certification | 0.10 | certified by a person, or a certified governed metric (1); the subject of a certified fact, such as the view defining a certified metric (0.5) |

Weights are an input (`catalog.rerank {"weights": {...}}`). Scores are kept
in memory by every replica and refreshed through the commit log. They lift
search results (×1 to ×1.5) and context packing. When the token budget is
tight, the facts that survive are the authoritative ones, and the best data
assets for a question enter the context as snippets of their own.

## Learned facts

After each crawl the learner infers:

- **join paths** from foreign keys (confidence 0.9 and up) and from the join
  conditions queries actually use. Both together reach about 0.97;
  query-only joins start at 0.45 and grow with the number of queries and
  calls.
- **metric definitions** from aggregates in queries and views:
  `SUM(o.amount) AS revenue` gives `revenue = sum(orders.amount)`, and a view
  named for a metric gives that view's definition.
- **synonyms** from names (`cust_id` gives "cust" means customer, and 30
  common abbreviations) and from comments ("also known as GMV").

Facts are `learned_fact` entities, apart from the curated schema. Each has a
confidence, a support count, its evidence (constraint names, query ids,
assets) and `concerns` links. They start `suggested`. A person decides:

```bash
onto learned                                   # certified first, then by confidence
onto learned --certify learned_fact:metric.3f2a… --note "Board definition"
onto learned --reject learned_fact:synonym.91c0…
```

Certifying and rejecting are governed action types (`certify_fact`,
`reject_fact`): admins and approvers only, never agents, audited and
undoable. The learner never overrides a decision and never suggests a
rejected fact again. Suggestions that nothing supports any more are
withdrawn. `onto quality` lists confident suggestions awaiting review, and
undocumented and unowned tables. `onto schema suggest` lists the
suggestions to review.

## From the catalog to an ontology

The catalog describes tables; the [ontology builder](ONTOLOGY_BUILDER.md)
turns them into business objects. `onto model draft <connector>` proposes an
object type per table (typed properties from the column types, keys, titles,
states from enum columns), link types from foreign keys, join tables and
learned joins, and action types. Applied, the drafted types are backed by
their tables, and each crawl syncs their rows into objects.

```bash
onto model draft warehouse
onto model apply warehouse --propose
```

## Questions answered from the graph

| question | intent | how |
|---|---|---|
| Which table holds revenue? | `data_location` | catalog search over tables, views, columns and metric facts, credited to the table they point at, times OntoRank |
| How do orders join to customers? | `join_path` | certified, then learned join paths, else foreign-key chains, up to 4 hops, conditions written from the asked side |
| Which revenue metric is the trusted one? | `trusted_metric` | certified metric facts first, then governed certified metrics, then the strongest candidate with how to certify it |

All three are deterministic: no LLM call, and the facts they used come back
as evidence. Anything else about the catalog goes through the normal
context graph, which now carries the most authoritative assets.

## API

| action | gRPC `CatalogService` | HTTP | MCP tool |
|---|---|---|---|
| `connector.add` | `AddConnector` | `POST /v1/catalog/connectors` | |
| `connector.list` | `ListConnectors` | `GET /v1/catalog/connectors` | |
| `connector.remove` | `RemoveConnector` | `DELETE /v1/catalog/connectors/{name}?purge=true` | |
| `connector.crawl` | `Crawl` | `POST /v1/catalog/connectors/{name}/crawl`, `POST /v1/catalog/crawl` | `crawl` (writes) |
| `catalog.find` | `FindData` | `GET /v1/catalog/find?term=` | `find_data` |
| `catalog.join` | `JoinPath` | `GET /v1/catalog/join?from=&to=` | `join_path` |
| `catalog.metric` | `TrustedMetric` | `GET /v1/catalog/metric?term=` | `trusted_metric` |
| `catalog.rank` | `Rank` | `GET /v1/catalog/rank/{id}` | `authority` |
| `catalog.top` | `Top` | `GET /v1/catalog/top?kinds=table,view&limit=` | |
| `catalog.rerank` | `Rerank` | `POST /v1/catalog/rerank` | |
| `learned.list` | `LearnedFacts` | `GET /v1/catalog/facts?fact_type=&state=&about=` | `learned_facts` |
| `learned.run` | `Learn` | `POST /v1/call/learned.run` | |
| `learned.certify` | `Certify` | `POST /v1/catalog/facts/{id}/certify` | |
| `learned.reject` | `Reject` | `POST /v1/catalog/facts/{id}/reject` | |

The ontology builder and Object Explorer actions (`ontology.*`,
`objects.*`) are listed in [ONTOLOGY_BUILDER.md](ONTOLOGY_BUILDER.md#api).

Connectors are added, removed and crawled by admins. Everything else follows
the caller's roles and ACLs.
