# The ontology builder

Connect a database and onto-intelli drafts an ontology from it, the way
Palantir Foundry's ontology builder does:

| Palantir Foundry | onto-intelli |
|---|---|
| object type backed by a dataset | object type with a **backing**: a table of a connected source, its key, and a column per property |
| property types | typed properties (string, text, int, float, bool, timestamp, enum, list, json) read from the column types, constraints and comments |
| link types from foreign keys and join datasets | link types from foreign keys, join tables (many-to-many) and learned joins |
| action types | create, edit, state transition and relink action types, with parameters, preconditions and risk |
| objects indexed from rows | **row sync**: the rows of opted-in tables become objects, incrementally, on every crawl |
| Object Explorer | `objects.explore`: filters, search, sort, facets and statistics, permission-filtered |
| AIP assist | an AI advisor that refines the draft and answers modeling questions |
| ontology proposals and branches | schema governance: people with an apply role apply, everyone else (and every agent) proposes |

The draft is deterministic. Every object, property, link and action carries
the reason it was proposed. The LLM only suggests edits to the draft, and
each suggestion is kept only if the result is still a valid schema change.
Nothing changes until someone applies the draft or approves its proposal.

Related guides: [CATALOG.md](CATALOG.md) (connectors and the catalog the
draft reads), [SCHEMA.md](SCHEMA.md) (change sets, versions, proposals),
[UI.md](UI.md) (the Explorer and Builder views).

## Contents

- [Quick start: the demo warehouse](#quick-start-the-demo-warehouse)
- [Quick start: your own PostgreSQL](#quick-start-your-own-postgresql)
- [How tables become an ontology](#how-tables-become-an-ontology)
- [Backing in the schema](#backing-in-the-schema)
- [Row sync](#row-sync)
- [Object Explorer](#object-explorer)
- [The AI advisor](#the-ai-advisor)
- [Agents](#agents)
- [API](#api)

## Quick start: the demo warehouse

`onto seed demo` loads a demo warehouse catalog (`sales.customers`,
`sales.orders`, `sales.order_items`, a finance view and a staging table).
It is a catalog only, with no connector behind it, so you can draft and
apply but no rows are synced.

```bash
onto seed demo
onto model draft warehouse
```

```text
Draft from warehouse: 2 object types · 1 link type · 5 action types

order Order ← sales.orders  key id · title none · thing · confidence 0.99
    amount                 float      numeric(12,2)         numeric(12,2) → float
    created_at             timestamp  timestamptz           timestamptz → timestamp
    status                 string     text                  text → string
    · key id (primary key)
    · status has no value list (enum or CHECK): kept as a property
customer Customer ← sales.customers  key id · title cust_name · actor · confidence 0.98
    …
Link types
  customer             order → customer (one, foreign key) inverse orders  foreign key orders_customer_id_fkey on …
Action types
  create_order               create     order          risk low     …
  set_customer               relink     order          risk low     order has one customer (customer_id)
Not modeled
  staging.revenue_tmp              excluded by staging.*
  finance.revenue_recognized       a view (draft with views to model it)
  sales.order_items                no primary key
```

Ask about it. Modeling questions are answered from the draft, with 0
tokens:

```bash
onto model advise "Which link types should orders have?"
onto ask "What object types can we create from the warehouse?"   # intent "model"
onto model refine warehouse      # with no LLM configured: the same draft and a note
onto model apply warehouse       # schema v2; a hint says no connector named warehouse exists to sync rows
```

## Quick start: your own PostgreSQL

1. **Connect and crawl.** The crawl reads metadata only (see
   [CATALOG.md](CATALOG.md#connectors)).

   ```bash
   export SHOP_URL=postgres://reader:…@db.internal/shop
   onto connect postgres env:SHOP_URL --name shop --schema docs_shop
   ```

2. **Draft.** Review the object types, properties, links, actions and the
   tables that were left out, each with its reason.

   ```bash
   onto model draft shop
   onto model draft shop --table 'sales.*' --exclude '*_audit' --prefix crm --no-actions
   onto model draft shop --json > draft.json
   ```

3. **Refine and ask (optional).** These use the configured LLM.

   ```bash
   onto model refine shop --instructions "use the names the sales team uses"
   onto model advise "How should I model the order tables?" --source shop
   ```

4. **Apply or propose.** Applying needs an apply role
   (`[governance] apply_roles`). It creates a schema version and, because
   the drafted types sync rows, crawls the source at once so the rows
   become objects.

   ```bash
   onto model apply shop                  # Applied shop as schema v2 (3 object types, 2 link types, 11 action types)
   onto model apply shop --propose --title "Shop ontology"   # file it for review instead
   onto schema approve prop_…             # an approver applies the proposal
   ```

5. **Explore and act.**

   ```bash
   onto explore                                       # object types and how many objects you can see
   onto explore order --filter 'amount>=100' --filter state=paid,shipped --sort=-amount
   onto explore customer --search ada
   onto do ship_order item=order:2 --confirm          # a generated transition (medium risk: needs --confirm)
   onto crawl shop --json                             # later crawls report objects and links synced
   ```

   Pass a descending sort as `--sort=-amount`: with a space, the shell
   argument parser reads `-amount` as a flag.

In the web UI the same flow is the **Builder** view (`#/build`): connectors
and their crawl status, a form to add one (admins), draft options, review
with toggles and renames, **Refine with AI**, the advisor, and a plan
preview before **Apply** or **Propose for review**. The **Explorer** view
(`#/explore`, `#/explore/<type>`) browses the objects. See [UI.md](UI.md).

## How tables become an ontology

The draft reads the catalog entities a connector wrote for one source:
tables, views, columns (type, nullability, primary key, unique, value list,
comment), foreign keys and learned join facts. It reads them as the caller
sees them, so tables and columns hidden from the caller are left out. It
never reads rows and never writes.

Draft options (all optional except the source):

| option | CLI | default | meaning |
|---|---|---|---|
| `source` | positional | the only crawled source | connector name |
| `tables` | `--table` (repeatable) | all | `schema.table` names or globs; a pattern without a dot also matches the bare table name |
| `exclude` | `--exclude` (repeatable) | `staging.*`, `*_tmp`, `*_old`, `*_bak`, `*_backup` | globs to leave out; an explicit empty list (`no_default_exclude` in gRPC) leaves nothing out |
| `prefix` | `--prefix` | none | put before every object type name (`crm` → `crm_customer`) |
| `sync` | `--no-sync` | true | set `backing.sync`, so rows become objects |
| `actions` | `--no-actions` | true | draft action types |
| `views` | `--views` | false | model views too |

Globs use `*` and are case-insensitive.

### Object types

One object type per table that has a key. Views only with `views`.

| decision | rule |
|---|---|
| name | singular snake_case of the table name: `orders` → `order`, `order_items` → `order_item`, `categories` → `category`, `addresses` → `address`, `caches` → `cache`, `causes` → `cause`, `statuses` → `status`, `aliases` → `alias`, `people` → `person`; `data`, `staff`, `inventory` and similar stay as they are. Only the last word changes. The prefix comes first when given. |
| label | the name in Title Case: `Order Item` |
| description | the table's `COMMENT`, else "A row of {table} in {source}." |
| key | the primary key columns. With no primary key, a single-column unique column (NOT NULL first). For a view, an `{object type}_id` or `id` column. Otherwise the table is skipped: "no primary key". |
| title | the first of `name`, `title`, `label`, `display_name`, `full_name`, then any `*_name`, then `email`, `code`, `sku`, `slug`. Foreign key columns never qualify. With none, objects are titled by their key. |
| category | **actor** when the table holds people (`user`, `customer`, `employee`, `person`, `member`, `contact`, `staff`, `author`, or `account` with an email column); **container** when two or more other tables reference it, or it is an `organization`, `company`, `team`, `project`, `department`, `store`, `warehouse` or `account`; else **thing** |
| states | a column named `status`, `state` or `stage` with a value list (a PostgreSQL enum, or a `CHECK (col IN (…))` constraint) becomes the type's states and `backing.state`, not a property. Values with a word `done`, `closed`, `cancel…`, `complete…`, `shipped`, `delivered`, `resolved`, `archived`, `paid`, `refunded`, `inactive`, `deleted`, `rejected` or `expired` are terminal states; `fail…`, `block…`, `overdue`, `error…`, `declined`, `past_due`, `suspended` or `on_hold` are alert states. Words are split at `_`, `-` and spaces and must start the word, so `unpaid`, `incomplete`, `undelivered` and `unblocked` are neither, and a word after `not`, `non`, `no`, `un` or `in` (`not_paid`) does not count. A status column without a value list stays a property, and the reasons say so. |
| confidence | 0.9 for a primary key, 0.75 for a unique column, 0.6 for a view's id column, plus up to 0.09 from the table's OntoRank. Types are listed most authoritative first. |

A table whose only purpose is to relate two others is a **join table** and
becomes a link type instead (see below).

### Properties

Every column becomes a property except the key columns, the columns that
become links and the state column. The property name is the column name in
snake_case; `visibility` and `acl` (reserved for access control) become
`visibility_value` and `acl_value`.

| column type | property type |
|---|---|
| any array (`text[]`, `int[]`, …) | `list` |
| a value list (PostgreSQL enum, or a single-column `CHECK` of `IN (…)`, `= ANY (ARRAY[…])` or `= '…'`) | `enum` with those values |
| `smallint`, `integer`, `bigint`, `int2`/`int4`/`int8`, `serial`, `bigserial`, `smallserial` | `int` |
| `numeric`, `decimal`, `real`, `double precision`, `float4`/`float8`, `money` | `float` |
| `boolean` | `bool` |
| `json`, `jsonb` | `json` |
| `date`, `timestamp…`, `time…` | `timestamp` |
| `text`, `varchar`, `char`, `uuid`, `citext`, `name` | `string`; `text` when the column is named (or ends in `_`) `description`, `notes`, `note`, `body`, `comment`, `comments`, `content`, `summary`, `details` or `bio` |
| anything else (`inet`, `bytea`, …) | `string` |

Further rules:

- **required** when the column is `NOT NULL`;
- **searchable** for the title column and `text` properties;
- **unique** (a single-column unique constraint or unique index) goes into
  the type's `constraints.unique`;
- a `string` property whose name contains `email` gets the pattern
  `^[^@\s]+@[^@\s]+$`;
- the column comment becomes the property description.

### Link types

| source | link type |
|---|---|
| a single-column foreign key to the key of a drafted table | one link per foreign key, `cardinality = "one"`, confidence 0.95. Name: the column without `_id`, `_fk` or `_key` (`customer_id` → `customer`). Inverse: the plural of the source type (`order` → `orders`). The column becomes `backing.links`, not a property. |
| a join table | a many-to-many link type with a `backing` of its own, confidence 0.9. A join table has exactly two single-column foreign keys on different columns, and besides them only its key and timestamp columns: either its primary key is the two foreign key columns, or it has no key or a surrogate key of its own. Name: the plural of the second foreign key's stem; inverse: the plural of the first's. |
| a learned join path with no foreign key (`a.x = b.y`, confidence at least 0.6, not rejected) | a `cardinality = "one"` link from the side that holds the other side's key, confidence = the fact's; its column goes into `backing.links` like a foreign key's. Skipped when a foreign key already relates the two tables. |

A foreign key that points at a table that is not drafted, or at a column
other than its key, stays a property, and a note says why. A join table
whose foreign keys do not both reference the single key column of the
table they point at is skipped, with the reason. Composite foreign keys
are not drafted as links (a note lists them).

### Action types

| action | when | parameters | effect | risk |
|---|---|---|---|---|
| `create_{type}` | always, unless a required property is a list or json | the key, when it is a single text column people type (a natural key such as `sku`); the title property; every required property | `create` the object, keyed by the natural key or `{new_id}`, in the first state | low |
| `edit_{type}` | the type has a scalar property | `item`, and up to 8 optional scalar properties (not timestamps) | `patch` the given properties; missing ones are left alone | low |
| `{verb}_{type}` | per state value with a known verb: cancelled/canceled → `cancel`, shipped → `ship`, delivered → `deliver`, paid → `pay`, refunded → `refund`, closed → `close`, archived → `archive`, approved → `approve`, rejected → `reject`, completed → `complete`, resolved → `resolve`, suspended → `suspend` | `item` | precondition `state_allowed`; `patch` the state | medium for a terminal state, else low |
| `set_{link}` | per foreign key link | `item`, `target` (of the link's target type) | `link` with `replace = true` | low |

Parameters named `actor`, `now`, `created`, `new_id`, `item` or `target`
are renamed `{name}_value`, because action templates provide those names.
Every action gets a description and phrases (`ship order`, `mark shipped`),
so an imperative in `ask` that starts with one maps to the action. Medium and high risk actions need
confirmation (`confirm: true`, `onto do … --confirm`), like any action type.

### Names and collisions

Every name matches `[a-z][a-z0-9_]*` and has at most 64 characters.

- An object type name already used by an interface, by a catalog kind
  (`data_source`, `db_schema`, `table`, `view`, `column`, `query`,
  `learned_fact`) or by an existing object type that is not this table's
  earlier draft becomes `{source}_{name}` (`team` → `shop_team`), with a
  note. The enterprise types (`team`, `person`, `project`, …) are never
  replaced.
- Two tables with the same name in different schemas: the second becomes
  `{schema}_{name}`.
- A link name used twice in the draft, or by a link type or inverse that is
  not this source's, becomes `{from}_{name}`; an inverse that collides
  becomes `{stem}_{inverse}`.
- An action name that exists already becomes `{source}_{name}`.
- Whatever still collides gets `_2`, `_3`, ….

Drafting the same source again reuses the names of its earlier draft, so
re-applying updates the types in place. A table that backs an object type
of the source keeps that type's name, even when it was renamed, prefixed or
refined (a new `prefix` applies to new types only); a foreign key or
learned link keeps the name and inverse of the link its column backs, and a
join table those of the link it backs. A schema change that makes two
object types sync the same table is refused by `schema.plan`.

### The change set

The draft's `changes` is an ordinary [change set](SCHEMA.md#change-sets):
one `put_object_type` per object type (with its backing), then one
`put_link_type` per link type, then one `put_action_type` per action type.
`schema.plan` validates it like any other change.

## Backing in the schema

`backing` is part of an object type's definition, in schema TOML, JSON and
change sets. You can write it by hand for a table the draft did not model,
or edit what the draft wrote (`onto schema plan`/`apply`).

```toml
[object_types.order]
label = "Order"
states = ["placed", "paid", "shipped", "cancelled", "refunded"]
terminal_states = ["paid", "shipped", "cancelled", "refunded"]
properties.amount = { type = "float" }
properties.placed_at = { type = "timestamp", required = true }

[object_types.order.backing]
source = "shop"                 # the connector name
table = "docs_shop.orders"      # schema.table
key = ["id"]                    # key columns; object ids are order:{id}, or {v1}.{v2} for two columns
sync = true                     # materialize rows as objects (off when absent)
title = "id"                    # optional: the title column (the key when absent)
state = "status"                # optional: the column mapped to the object state
max_rows = 100000               # optional: the default
properties = { amount = "amount", placed_at = "placed_at" }   # property = column
links = { customer = "customer_id" }                           # link type = foreign key column

[link_types.customer]
from = ["order"]
to = ["customer"]
inverse = "orders"
cardinality = "one"

# A many-to-many link type backed by a join table.
[link_types.products]
from = ["order"]
to = ["product"]
inverse = "orders_with"
cardinality = "many"
backing = { source = "shop", table = "docs_shop.order_products", from = "order_id", to = "product_sku" }
```

Validation (the schema does not compile otherwise):

- a backing needs `source`, `table` and at least one key column;
- each `properties` key is a property of the type (or inherited);
- each `links` key is a link type the type may be the source of;
- a link backing needs `source`, `table`, `from` and `to`;
- at most one object type with `sync = true` per source table (`schema.plan`
  refuses a change that adds a second one).

`onto schema` prints `backed by shop:docs_shop.orders (synced)` next to a
backed type. A later schema layer that redefines a type without a backing
keeps the backing it had.

## Row sync

Row sync is opt-in per object type: only types whose **active** schema says
`backing.sync = true` are read. A draft in a proposal syncs nothing until it
is approved. Everything else a connector reads is metadata.

**When.** Every crawl of the connector, scheduled or `onto crawl`, after
the catalog is written and before learning and OntoRank. `onto model apply`
(and `ontology.apply_draft` without `no_crawl`) crawls right away when the
connector is registered here.

**What is read.** For each synced type, the key columns and the mapped
columns (properties, link columns, title and state), never any other
column. The PostgreSQL connector reads 1,000 rows a page, one read-only
transaction per page with a 60-second statement timeout, in key order with
keyset paging (`WHERE (k1, k2) > (…) ORDER BY k1, k2`). Identifiers are
quoted and the previous page's key is bound as a parameter, cast to the key
columns' own types, so the comparison and the order agree. Rows with a null
key are skipped. A mapped column the crawl did not see is skipped with a
note.

**Limits.** At most `backing.max_rows` rows per type (100,000 by default),
the first ones by key. When a table has more, the crawl report marks the
type `truncated` and adds a note. Rows past the limit are not synced, and
objects an earlier sync made from them are deleted. Join tables sync at
most 100,000 rows.

**How rows become objects.** Each row becomes `{type}:{key}` (key values
joined with `.`; strings as they are, numbers as JSON prints them):

- title: the title column, else the key;
- state: the state column; a null clears the state;
- properties: the mapped columns, coerced to the property type: numbers
  from numeric text and money values, bools from `t`/`f`, `yes`/`no`,
  `1`/`0`, `on`/`off`, timestamps from RFC 3339, dates and times of day
  (to Unix milliseconds), enums and strings as text, a scalar into a
  one-item list, json as it is. A value that cannot be read as the type is
  cleared, and a null clears the property;
- links: each `backing.links` column with a value links the object to
  `{target type}:{value}`;
- provenance: `connector:postgres`, the connector name, `external_ref`
  `{schema.table}#{key}`, the crawl run and time.

Rows the schema rejects (a state or enum value it does not list, a missing
required value, an id longer than 256 characters) are counted as `invalid`.
Their objects keep what they had, and a note gives their number. The note
names no row, key or value: the report is kept in the connector state that
every reader may list.

**Incremental.** Each object's fingerprint is kept per type in the store
(`connector_state`, key `{connector}/objects/{type}`; join tables under
`{connector}/links/{link}`). A crawl writes only rows that changed, deletes
objects whose rows vanished (only objects this sync wrote) and costs no
writes when nothing changed. Writes go through the audited path as the
connector, in batches of 500. Types are written targets first, so links
find their objects; a link to an object that exists nowhere (a type that is
not synced, a row past the limit) is dropped.

**Join tables.** A link type with a join table backing syncs when both of
its ends are synced types. New rows add links, vanished rows remove them,
and rows naming an object that does not exist are counted as `skipped`.
Links have no ACL of their own: whoever sees both ends sees the link. So
with `map_grants`, a join table that is restricted beyond the tables it
joins (no end is restricted to principals the join table also admits) is
not synced, a note says so, and the links synced from it before are
removed.

**Failures.** A table that cannot be read is a note for its type, and
nothing of that type is deleted. A failed write is a note; the next crawl
retries. Neither fails the crawl.

**The crawl report** (`onto crawl --json`, `connector.crawl`) has, per
type and per join link:

```json
"objects": { "order": { "rows": 3, "upserted": 0, "unchanged": 3, "deleted": 1, "invalid": 0, "truncated": false } },
"links":   { "product_skus": { "rows": 4, "linked": 0, "unlinked": 0, "skipped": 0 } }
```

The human-readable `onto crawl` line shows the catalog counts only.

**Access control.** With `map_grants` on the connector, objects follow
their table: a table PUBLIC cannot read makes its objects
`visibility = "restricted"` with the table's ACL, and a readable table
makes them `internal` (lifting an earlier restriction). Without
`map_grants`, objects get no ACL of their own and the workspace rules
apply.

**Source-owned properties vs ontology edits.** The source owns what the
backing maps: the mapped properties, the title, the state and the links of
backed link types. Edits made in the ontology (an `edit_order`, a
`set_customer`, a transition) stay until that row changes in the source;
then the row is written again and its values win, including its foreign
key links. Properties the backing does not map, and links of other types,
are never touched by a sync. Deleting the row deletes the object. Objects
created by actions are not rows, so a sync never deletes them, and nothing
is written back to the database.

The fingerprint covers the state, so a row whose only change is its state
column is written again.

**Removing.** `onto connectors --remove shop --purge` deletes the synced
objects as well as the catalog entities, and their fingerprints.

## Object Explorer

`objects.types` lists every object type with how many objects the caller
can see (in a workspace, or everywhere they may read), its backing, its
number of properties, the link types it may start, and its action types.

`objects.explore` returns the objects of one type or interface:

```json
{
  "kind": "order",
  "filters": [
    { "prop": "amount", "op": "gte", "value": 100 },
    { "prop": "state", "op": "in", "value": ["paid", "shipped"] },
    { "prop": "link:customer", "op": "eq", "value": "customer:1" }
  ],
  "search": "",
  "sort": "-amount",
  "limit": 50,
  "offset": 0
}
```

| field | meaning |
|---|---|
| `kind` | object type or interface (required) |
| `workspace` | only objects of this workspace |
| `search` | lexical search within the kind; the best 1,000 matches are then filtered |
| `filters` | conditions that must all hold |
| `sort` | `prop` ascending, `-prop` descending: a property, `title`, `state`, `id`, `created_at` or `updated_at` (see below for names a property shares). Objects without the value come last. Default: by title, or by relevance when searching. |
| `limit`, `offset` | page; `limit` is 50 by default (also when 0) and at most 1,000 |
| `facets` | fields to count; omitted picks them (below) |
| `stats` | fields to summarize; omitted picks them (below) |

Filters name a property, `title`, `state`, `workspace`, `id`, `created_at`,
`updated_at`, or `link:<rel>` (the ids linked through `rel`, which may be
an inverse name). A property the type declares wins over the object field
of the same name: `created_at` of a table with a `created_at` column is
that column, not the time the object was synced. The object fields are
always reachable with a leading underscore (`_title`, `_state`,
`_workspace`, `_id`, `_created_at`, `_updated_at`), and the result's
title and state columns are named `_title` and `_state` when a property
takes the plain name.

| op | meaning |
|---|---|
| `eq`, `ne` | equal, not equal (`ne` also matches objects without the value); on a list property, `eq` matches any item |
| `in` | equal to one of a list (`value` must be a list) |
| `gt`, `gte`, `lt`, `lte` | compared as numbers when either side is a number or the field is a timestamp (dates such as `2026-10-17` work), else as text; need a value |
| `contains` | case-insensitive substring (any item of a list) |
| `exists`, `missing` | the field is set, or not |

On `link:<rel>`, `eq`, `ne` and `in` compare linked ids, `contains`
matches a linked object's id or title, and `exists`/`missing` test for any
link. Range operators never match links. Only linked objects the caller
may see count: a link to a hidden object matches nothing and counts as no
link.

The CLI writes filters as `prop=value`, `prop!=value`, `prop=a,b` (any of),
`prop>n`, `prop>=n`, `prop<n`, `prop<=n`, `prop~text`, `prop?` (set),
`!prop` (missing) and `link:rel=id`.

**The result:**

| field | |
|---|---|
| `kind`, `label` | |
| `total` | objects matching, before paging |
| `objects` | the page: full entities |
| `columns` | `title`, `state` (when the type has states) and every property except `visibility` and `acl`, with label and type |
| `facets` | field → its most frequent values with counts, at most 12, over everything that matched (not just the page) |
| `stats` | field → `count`, `min`, `max` and, for numbers, `mean`, `sum` and a 10-bucket `histogram` (`lo`, `hi`, `count`) |
| `links` | link types the kind starts, and inverse names of those that point at it, with their target kinds |
| `actions` | action types with a parameter that takes this kind |

Automatic facets are the state, enum and bool properties, and string
properties with at most 50 distinct values in the filtered set that are not
identifier-like (unique-constrained properties and properties where every
object has its own value are left out). Automatic statistics cover int,
float and timestamp properties; timestamps get count, min and max only.

Every read is permission-filtered: objects the caller may not see are
neither listed nor counted, in totals, facets or statistics.

## The AI advisor

### `ontology.draft`

The deterministic draft described above. No LLM, no tokens.

### `ontology.refine`

Sends the configured LLM a compact summary of the draft (types with table,
key, title, category and states; properties with column types and value
lists; links with how they are backed; actions; what was left out) and the
modeler's `instructions`, and asks for strict JSON suggestions:

```json
{ "suggestions": [
  { "target": "link:product_skus", "field": "name", "from": "product_skus", "to": "products", "why": "…" },
  { "target": "property:customer.region", "field": "description", "to": "Sales region", "why": "…" }
] }
```

| target | fields |
|---|---|
| `object:<name>` | `name` (renames the links and actions that use it), `label`, `description`, `category`, `transition` (`to` is a state, or `{state, name}`; the default name is `mark_{type}_{state}`) |
| `property:<object>.<property>` | `name`, `type` (`integer`, `numeric`, `date`, `uuid` and similar are accepted), `values` (makes it an enum; only for a column with a value list in the database, and keeping every value of it), `description`, `required` (`true` only for a `NOT NULL` column), `searchable` |
| `link:<name>` | `name` (renames its `set_` action), `inverse`, `label`, `description` |
| `action:<name>` | `name`, `description`, `risk` |

The reply is parsed leniently: the first JSON object or array, fenced or in
prose; at most 50 suggestions. Each one is applied to a copy of the draft,
the change set is rebuilt and checked with `schema.plan`, and the
suggestion is kept only if the plan has no error that the draft, as the
suggestions kept so far left it, did not have. Suggestions that rename onto
an existing type, collide inside the draft, name an unknown state or type,
ask more of a column than its rows guarantee (enum values the database does
not limit it to, `required` on a nullable column: rows that do not fit the
schema never become objects), or break the plan are returned in `rejected`
with the reason.

The result: `{draft, suggestions, applied, rejected, llm, tokens}`.

**Offline.** With the `mock` or `none` provider, an unavailable model, a
failed call or an empty draft, refine returns the deterministic draft with
a note explaining why, `llm: "none"` and 0 tokens. It is never an error.

### `ontology.advise`

`{question, source?}` → `{answer, mode, facts, source, tokens, note?}`.

The answer comes from the draft of the source the question names (by
connector name or title), or of the only crawled source. It covers what the
question is about: object types by default; link types for "link",
"relationship", "join", "foreign key", "reference"; action types for
"action", "transition", "workflow", "edit"; property and column types for
"column", "property", "field", "data type". It narrows to the types the
question names (by name, plural, label or table). `facts` carry the reason
behind each type.

Questions that need reasoning ("why", "how should", "recommend", "better",
"best", "explain", "trade-off", "instead", "how would") go to the LLM when
one is configured, grounded in the draft summary and the same facts; the
`mode` is then `llm`. Otherwise, or when the model fails, the answer is
deterministic (`mode: "deterministic"`, 0 tokens, with a `note` on
failure).

### The `model` intent in `ask`

`ask` routes modeling questions ("What object types can we create from the
warehouse?", "How should I model the sales tables?", "Which link types
should orders have?", "What column types does the orders table get in the
ontology?") to the intent `model`. It is listed in
`[reasoning] deterministic_intents` by default, so these are answered from
the draft with 0 tokens. The demo eval suite has three such cases
(`onto eval`).

## Agents

Agents propose; people apply.

| MCP tool | action | notes |
|---|---|---|
| `draft_ontology` | `ontology.draft` | read-only |
| `refine_ontology` | `ontology.refine` | read-only; spends the server's LLM tokens |
| `advise_ontology` | `ontology.advise` | |
| `propose_ontology` | `ontology.apply_draft` | always files a proposal (`mode: "propose"`, source `mcp`), whatever the arguments say (`propose`, `preview` and `no_crawl` are dropped; null arguments are no arguments), even with `[mcp] allow_writes = true` |
| `object_types` | `objects.types` | |
| `explore_objects` | `objects.explore` | |

All six are available without `allow_writes`: none of them changes data
or the schema directly.

Through any surface, a principal with the `agent` role that calls
`ontology.apply_draft` with `mode: "apply"` is refused (`Denied`), with a
hint to propose instead. Agents never apply or approve schema changes; see
[SCHEMA.md](SCHEMA.md#governance). Running the generated action types is
governed like any action: `apply_action` over MCP needs `allow_writes`,
and medium-risk transitions need `confirm`.

### `ontology.apply_draft`

| input | |
|---|---|
| draft options | draft afresh from the source |
| `draft` | or a reviewed draft (from `ontology.draft` or `ontology.refine`, possibly edited); its change set is rebuilt from its objects, links and actions, which are all it needs (an empty `source` is resolved from the options) |
| `mode` | `apply` (default) or `propose`; gRPC uses `propose: true` |
| `title`, `rationale` | for a proposal; defaults name the source and list the types |
| `no_sync` / `sync: false` | turn row sync off for every type |
| `no_crawl` | do not crawl after applying |
| `preview` | apply nothing: return the draft with its change set rebuilt (the UI plans it with `schema.plan`) |
| `proposal_source` | where a proposal comes from (`ui`, `mcp`); `ontology.apply_draft` by default |
| `workspace` | the workspace the change is recorded in |

Results: `{mode: "applied", source, counts, version, revision, crawl?, hint?}`,
`{mode: "proposed", source, counts, proposal}` or
`{mode: "preview", source, draft}`.

## API

Every action is also `POST /v1/call/{action}` and `onto call {action} '{…}'`.

| action | gRPC (`onto_intelli.v1`) | HTTP | MCP tool | CLI |
|---|---|---|---|---|
| `ontology.draft` | `OntologyBuilderService/DraftOntology` | `POST /v1/ontology/draft` | `draft_ontology` | `onto model draft [source]` |
| `ontology.refine` | `OntologyBuilderService/RefineOntology` | `POST /v1/ontology/refine` | `refine_ontology` | `onto model refine [source] --instructions …` |
| `ontology.advise` | `OntologyBuilderService/AdviseOntology` | `POST /v1/ontology/advise` | `advise_ontology` | `onto model advise "<question>" [--source s]` |
| `ontology.apply_draft` | `OntologyBuilderService/ApplyDraft` | `POST /v1/ontology/apply` | `propose_ontology` (propose only) | `onto model apply [source] [--propose] [--title t] [--rationale r] [--no-crawl]` |
| `objects.types` | `ObjectService/ObjectTypes` | `GET /v1/objects/types?workspace=` | `object_types` | `onto explore` |
| `objects.explore` | `ObjectService/Explore` | `POST /v1/objects/explore` | `explore_objects` | `onto explore <type> [--filter …] [--search …] [--sort=…] [--limit n] [--offset n]` |

`onto model draft|refine|apply` take `--table`, `--exclude`, `--prefix`,
`--no-sync`, `--no-actions` and `--views`; every `model` and `explore`
command takes `--json`. `onto explore` shows 25 objects a page by default.

The gRPC requests are `DraftRequest` (also for `RefineOntology`, with
`instructions`), `AdviseRequest`, `ApplyDraftRequest`,
`ObjectTypesRequest` and `ExploreRequest`; responses are
`google.protobuf.Struct`. Proto3 drops `false`, so the flags are negative:
`no_default_exclude`, `no_sync`, `no_actions`, `no_crawl`. `ApplyDraftRequest`
carries every `ontology.apply_draft` input: the draft options, `draft`,
`propose`, `title`, `rationale`, `no_crawl`, `preview`, `proposal_source`
and `workspace`.

```bash
grpcurl -plaintext -H "authorization: Bearer $ONTO_TOKEN" -d '{"source":"shop"}' \
  localhost:50051 onto_intelli.v1.OntologyBuilderService/DraftOntology
curl -s localhost:50051/v1/objects/explore -H "authorization: Bearer $ONTO_TOKEN" \
  -H 'content-type: application/json' \
  -d '{"kind":"order","filters":[{"prop":"state","op":"eq","value":"paid"}],"sort":"-amount"}'
```

Connectors themselves (`connector.add`, `connector.crawl`, …) are listed in
[CATALOG.md](CATALOG.md#api).
