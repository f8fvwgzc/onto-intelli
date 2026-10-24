# Live schema

The ontology schema is data. Object types, properties, link types,
interfaces, action types, rules, metrics, shapes and glossary terms can be created,
changed and deleted at runtime. This works the same through the Rust library,
the gRPC `SchemaService`, HTTP/JSON, the CLI and MCP.

Every change set goes through the same five steps:

1. **Validated.** The resulting schema must compile: references resolve,
   inverses don't collide, and rules, metrics and action types still check out.
2. **Previewed.** A dry-run impact report shows which entities and links fall
   in scope, whether the change is breaking, and the migration it implies.
3. **Migrated explicitly.** A change that breaks data in use is refused until
   every break has a migration.
4. **Versioned.** The change commits atomically with its migration as a new
   schema version. Each version holds the full document, the change list, the
   diff, who made it and why.
5. **Audited and reversible.** A `schema.change` event records the principal.
   Any version can be read back, compared, restored or undone.

## Change sets

```json
{
  "message": "track incidents",
  "expected_version": 4,
  "changes": [
    { "op": "put_object_type", "name": "incident",
      "def": { "label": "Incident", "extends": "issue", "color": "#ef4444",
               "properties": { "sev": { "type": "int" } } } },
    { "op": "put_link_type", "name": "pages",
      "def": { "from": ["incident"], "to": ["person"], "inverse": "paged_by", "cardinality": "many" } }
  ]
}
```

`expected_version` is optional. When the active version differs, the set is
refused, which gives you optimistic concurrency for editors. `override_locks: true`
lets an admin apply a change that touches locked definitions ([Locks](#locks)).

| op | fields | notes |
|---|---|---|
| `put_object_type` | `name`, `def`, `migrations?` | create or replace; `def` is an object type (`label`, `description`, `color`, `icon`, `category`, `extends`, `properties`, `states`, `terminal_states`, `alert_states`, `implements`, `derived`, `constraints`, `backing`: the table its objects come from, see [ONTOLOGY_BUILDER.md](ONTOLOGY_BUILDER.md#backing-in-the-schema)) |
| `delete_object_type` | `name`, `migrations?` | drops the glossary words that named it (unless another type or interface keeps the name) |
| `rename_object_type` | `from`, `to` | rewrites references; entity ids move from `from:key` to `to:key` and the old ids redirect |
| `put_property` | `object_type`, `name`, `def`, `migrations?` | `def`: `type` (string, text, int, float, bool, timestamp, enum, list, json), `required`, `values`, `searchable`, `description` |
| `delete_property` | `object_type`, `name`, `migrations?` | |
| `rename_property` | `object_type`, `from`, `to` | moves the values |
| `put_link_type` | `name`, `def`, `migrations?` | `from`/`to` kinds or interfaces (empty means any), `inverse`, `cardinality` (`many`/`one`), `dependency`, `symmetric`, `transitive`, `provenance`, `weight`, `label`, `backing` (a join table) |
| `delete_link_type` | `name`, `migrations?` | drops the glossary words that named it or its inverse (unless another link type keeps the name) |
| `rename_link_type` | `from`, `to` | moves links; rewrites rules, interfaces, glossary, constraints, metrics, actions |
| `put_interface` / `delete_interface` | `name`, `def` | |
| `put_action_type` / `delete_action_type` | `name`, `def` | parameters, preconditions, effects, roles, risk |
| `put_rule` / `delete_rule` | `def` / `name` | `{ name, chain: [...], infer }` or `{ name, when: [[s, p, o], ...], then: [s, p, o] }` ([Rules](#rules)) |
| `put_shape` / `delete_shape` | `name`, `def` / `name` | a shape ([Shapes](#shapes)) |
| `put_metric` / `delete_metric` | `name`, `def` | |
| `put_glossary_term` / `delete_glossary_term` | `section` (`kinds`, `relations`, `dimensions`), `word`, `target` | |
| `set_analysis` | `def` | root-cause, impact, scope and containment relations |

New names must match `[a-z][a-z0-9_]*`.

You do not have to write a change set by hand to model a database:
`onto model draft <connector>` drafts one from the crawled catalog (object
types backed by tables, link types from foreign keys and join tables,
action types), and `onto model apply` applies it or files it as a proposal
through the same steps as any other change set
([ONTOLOGY_BUILDER.md](ONTOLOGY_BUILDER.md)).

## Impact and breaking changes

`schema.plan` (the dry run) returns:

```json
{
  "base_version": 4, "valid": true, "breaking": true,
  "needs_migration": ["2 issue entities are in state 'triaged', which the new schema does not allow: add map_states"],
  "warnings": [],
  "changes": [ { "op": "put_object_type", "target": "issue", "summary": "change object type issue",
                 "breaking": true, "reasons": ["…"], "entities": 214, "links": 0, "sample": ["issue:1", "…"] } ],
  "diff": { "entries": [ { "section": "object_types", "name": "issue", "change": "modified", "fields": ["states"],
                           "before": {}, "after": {} } ] },
  "entities_affected": 214, "links_affected": 0,
  "migration": { "entities_updated": 2, "entities_deleted": 0, "entities_retyped": 0,
                 "links_deleted": 0, "links_moved": 0, "mutations": 2 }
}
```

Every plan computed by the engine also carries semantic fields
([ASSURANCE.md](ASSURANCE.md#semantic-plans) explains them):

| Field | Content |
|---|---|
| `errors` | why the change set does not compile (then `valid` is false) |
| `consequences` | `{added, removed, conservative, verdict, new_over_existing_vocabulary, by_rule, samples_added, witnesses, samples_removed, fixpoint_reached, refused?, skipped?, means}`: the facts the reasoner's closure gains and loses under the planned schema, over the current graph (`refused`: terms the two runs could not spell, which make the verdict `undetermined`) |
| `risk`, `risk_reasons` | `low`, `medium` or `high`, and why |
| `locked_violations` | `[{section, name, change, touch, reason, renamed_to?, lock_reason, locked_by, overridden}]` |
| `likely_renames` | `[{section, from, to, score, evidence}]`: a delete and a put in one section that look like one rename |
| `shapes` | `{before, after, new_results, resolved, samples, complete, skipped?, means}`: the shape report's verdict and results under each schema |

`verdict` is `conservative`, `not_conservative` or `undetermined`. Plans
stored before these fields existed still load; their missing fields are
empty, except the risk: a proposal stored without one has it assessed from
its plan when it is read, rather than read as `low`. Consequences and the
shape delta are skipped, with the reason, when the graph holds more than
500,000 asserted facts.

A change is **breaking** when existing data or vocabulary in use stops being
valid:

- the kind of existing entities is removed;
- a state that entities hold is removed;
- values no longer fit a property's new type or enum;
- a property that holds values is removed;
- a relation that links use is removed;
- links no longer fit the new `from`/`to` kinds;
- a relation becomes single-valued while sources hold several links;
- an inverse name that callers use is renamed;
- the symmetry of a relation in use changes.

Renames are breaking for callers, but they migrate their own data.

Each break must be resolved by a **migration**, given on the change or for
the whole set (`"migrations": [...]`). Selectors you leave out (`kind`,
`property`, `rel`) default to the change's own target.

| migration | resolves |
|---|---|
| `{ "action": "delete_entities" }` | entities of a removed kind |
| `{ "action": "retype", "to": "knowledge" }` | entities of a removed kind; ids become `to:key`, old ids redirect |
| `{ "action": "map_states", "map": { "triaged": "open" }, "default": "open" }` | removed states |
| `{ "action": "set_default", "property": "tier", "value": 1 }` | values that no longer fit; also fills a newly required property |
| `{ "action": "convert" }` | values coerced to the new type (`"3"` becomes 3, scalar becomes list); values it can't convert are dropped and reported |
| `{ "action": "drop_values" }` | values that no longer fit, or all values of a removed property |
| `{ "action": "delete_links" }` | links of a removed relation, links that no longer fit, extra links of a now single-valued relation |
| `{ "action": "move_links", "to": "relates_to" }` | links of a removed relation |
| `{ "action": "keep_latest" }` | a relation that becomes single-valued: keeps each source's newest link |
| `{ "action": "acknowledge" }` | vocabulary-only breaks (a renamed inverse); in non-strict mode, keeping removed kinds, properties or relations as untyped data |

The migration is staged against the new schema during the dry run, so
validation errors show up in the plan. Applying commits the migration's entity
and link edits, entity versions, the schema version and the audit event in one
transaction. Migrated entities record the caller as the actor.

## Transitive and symmetric link types

`symmetric = true` makes a link type read the same both ways (`a tied_with b`
is also `b tied_with a`). `transitive = true` makes it chain: when `a part_of
b` and `b part_of c`, the reasoner derives `a part_of c`. Neither writes
links: the reasoner derives the facts and labels them `derived`
([ASSURANCE.md](ASSURANCE.md)).

```toml
[link_types.region_within]
from = ["sourcing_region"]
to = ["sourcing_region"]
inverse = "region_contains"
transitive = true
```

A transitive link type must be able to chain, so its `to` kinds must overlap
its `from` kinds (or one list is empty):

```text
$ onto schema plan bad-trans.toml
invalid on schema v12 · 0 entities · 0 links in scope
  error link type pages is transitive but its targets [person] can never be its sources [issue]
  risk high: the change set does not validate (1 error): it cannot be applied, and its effects are not shown; conservativity not shown: the change set does not compile
  consequences not computed: the change set does not compile
  shapes: 0 new results, 0 resolved
      the change set does not compile
```

Lint warns about a transitive link type with cardinality `one` (its own
closure breaks it) and a symmetric one whose `from` and `to` differ. Turning
`transitive` on for a link type in use is not breaking, but it changes what
the ontology entails; the plan says how (see the `depends_on` example in
[ASSURANCE.md](ASSURANCE.md#consequences-and-conservativity)).

## Rules

A rule derives a relation or a class from facts. It never writes: what it
derives is labelled with its name and kept out of the stored graph. Rule
names are unique; a layer with the same name replaces the rule. Two forms:

**Chain**: follow relations in order and infer one. A step can be a link
type, an inverse name, `^rel` (the link type read backwards) or a relation
another rule infers.

```toml
[[rules]]
name = "escalation"
chain = ["affects", "owned_by", "managed_by"]
infer = "escalate_to"
```

**when/then**: a body of atoms `[subject, predicate, object]` and one head.

```toml
[[rules]]
name = "critical_exposure"
description = "A service affected by a critical issue is exposed."
when = [["?i", "a", "issue"], ["?i", "prop.priority", "critical"], ["?i", "affects", "?s"]]
then = ["?s", "a", "critical_exposure_service"]
```

| Position | Accepts |
|---|---|
| subject | `?var`, or an entity id (it contains `:`) |
| predicate | `a`; `state`; `prop.<name>` (the property must exist on some object type); a link type (an inverse name or `^rel` is read as the link type with subject and object swapped); a relation some rule derives |
| object of `a` | `?var`, or an object type, interface, or a class some rule's `then` introduces |
| object of `state` and `prop.*` | `?var`, or any TOML scalar (compared as JSON) |
| object of a relation | `?var` or an entity id |
| `then` | `[s, "a", class]` (a derived class: any name `[a-z][a-z0-9_]*`) or `[s, relation, o]` (an existing or new derived relation) |

Every `then` variable must occur in `when`. Rules derive types and
relations, not property values or states. Rules may chain into each other and
recurse; the reasoner runs them to a fixpoint. There is no negation. The plan
of a new rule shows what it would derive today:

```text
$ onto schema plan rule.toml
non-breaking on schema v12 · 0 entities · 0 links in scope
  · add rule critical_exposure (?s a critical_exposure_service <= ?i a issue, ?i prop.priority critical, ?i affects ?s) (0 entities, 0 links)
  risk low
  consequences: +1 derived, -0 removed · conservative
      schema/critical_exposure 1
      + <service:payment-api> a #critical_exposure_service  by schema/critical_exposure from <issue:421> a #issue, <issue:421> prop.priority "critical", <issue:421> affects <service:payment-api>
  shapes: 0 new results, 0 resolved · violates → violates
```

A new rule that derives a new class or relation name is conservative: it adds
nothing stated in the names you already had. The errors name the rule:

```text
error rule probe: then: rules derive types and relations, not property values
error rule probe: when[0]: unknown relation assigned_too
error rule probe: head variable ?q does not occur in when
```

The per-entity rule walk behind `ask`, `context` and `onto show` evaluates
chain rules; the reasoner (`reason.*`) evaluates both forms.

## Shapes

A shape states conditions on the objects of a target, in the style of SHACL.
Validating is a measurement of the graph ([ASSURANCE.md](ASSURANCE.md#shapes)).

```toml
[shapes.production_order_shape]
description = "Every production order makes exactly one part, runs somewhere and has a positive quantity."
target = "production_order"
message = "Production orders need a part, a line or work center, and a quantity."
properties = [
    { path = "makes_part", min_count = 1, max_count = 1, class = "part" },
    { path = "scheduled_on", min_count = 1, message = "Schedule the order on a line or work center." },
    { path = "prop.quantity", min_count = 1, min_value = 1 },
]
```

| Shape field | Meaning |
|---|---|
| `target` | an object type (with its subtypes), an interface, or a derived class (selected when validating with `inferred`) |
| `target_ids` | these entities; a missing or invisible one is a `target_missing` result |
| `target_subjects_of`, `target_objects_of` | the subjects or objects of a relation |
| `severity` | `violation` (default), `warning` or `info` |
| `message` | the default result message |
| `closed`, `ignored_properties` | no properties besides the `prop.*` paths and these |
| `deactivated` | skip this shape (reported as skipped, without making the report undetermined) |
| `properties` | property shapes |

| Property shape field | Meaning |
|---|---|
| `path` | a link type (out), an inverse name or `^rel` (in), a sequence `a.b` (at most 16 steps), `prop.<name>`, `state` or `title` |
| `id` | a stable id for results and feedback (default `<shape>/<index>`) |
| `min_count`, `max_count` | how many values the path reaches |
| `class` | every linked entity is of this kind, a subtype, an implementer of this interface, or (with `inferred`) of this derived class |
| `datatype` | a property type (`string`, `int`, `float`, `bool`, `timestamp`, ...) |
| `in`, `has_value` | allowed values; a value that must be present |
| `pattern`, `min_length`, `max_length` | text constraints (a regular expression of at most 1,024 bytes) |
| `min_value`, `max_value` | numeric bounds |
| `node` | every linked entity conforms to that shape (nesting up to depth 5) |
| `severity`, `message` | override the shape's |

Any other key is kept but not evaluated: validation reports it as skipped,
which makes a report without results `undetermined`. Shapes are merged by
name across layers, checked at compile time (targets, paths, classes and
nested shapes must exist), and edited live with `put_shape` and
`delete_shape`. Renaming an object type or a link type rewrites the shapes
that use it. `onto shapes check <file>` checks definitions without saving
them; `onto shapes induce <kind>` drafts one from the data.

## Locks

A lock keeps a definition from being deleted, renamed or broken by a live
change. Only admins lock and unlock; agents never can, and denied attempts
are audited.

```bash
onto lock object_types vendor --reason "procurement reports read vendor ids"
```

```bash
onto locks
```

```bash
onto unlock object_types vendor
```

Sections: `object_types`, `link_types`, `interfaces`, `rules`, `metrics`,
`action_types`, `shapes` and `properties` (named `<object type>.<property>`).
The definition must exist. A change violates a lock when it deletes or
renames the locked definition (a property lock also by deleting or renaming
its object type, or by a `put_object_type` that drops the property), or when
it changes the locked definition and the change is breaking.

Every violation is in the plan's `locked_violations`, and the plan is not
applicable:

```text
$ onto schema apply drop-vendor.toml
error: conflict: schema change touches locked definitions: object_types/vendor (delete_object_type vendor: deletes the locked definition; locked by local: procurement reports read vendor ids); an admin may pass override_locks
```

`override_locks: true` on the change set is honoured for admins only. An
overridden violation stays in the plan, marked `overridden`, and in the
`schema.change` audit event. Deleting a locked definition with an override
releases the lock; renaming it moves the lock to the new name. Proposal
approval re-plans the change set, so locks guard proposals too, and a
proposal's override is honoured only when both its proposer and its approver
are admins. Lock events are `schema.lock` and `schema.unlock`.

| | Action | CLI | HTTP |
|---|---|---|---|
| list | `schema.locks` | `onto locks` | `POST /v1/call/schema.locks` |
| lock | `schema.lock {section, name, reason}` | `onto lock <section> <name> --reason R` | `POST /v1/call/schema.lock` |
| unlock | `schema.unlock {section, name}` | `onto unlock <section> <name>` | `POST /v1/call/schema.unlock` |

## Versions

- `schema.history {limit?}` lists versions, newest first: version, parent,
  time, actor, source, message, diff summary and migration counts.
- `schema.version {version}` returns one version with its full document,
  changes and diff.
- `schema.as_of {ts | days_ago}` returns the version in force at a point in
  time.
- `schema.diff {from, to?}` compares two versions; `to` defaults to the
  active one.
- `schema.revert {version, migrations?, dry_run?}` makes that version's schema
  active again, as a new version.
- `schema.undo {version, migrations?, dry_run?}` applies the inverse of one
  version on top of the active schema. Definitions changed since are left
  alone.

The `source` of a version is one of:

| source | meaning |
|---|---|
| `config` | first open |
| `import` | the schema files changed |
| `api` | applied directly |
| `proposal` | an approved proposal |
| `revert` | a revert |
| `undo` | an undo |

Restoring a version restores the schema only. Entities deleted by an earlier
migration stay deleted, but their versions remain in `entity.history`.

## Where the schema comes from

`[ontology] schema_mode` decides:

- **`live`** (default): the first open stores the configured schema (built-ins
  plus `extend` files) as version 1. From then on the stored schema is
  authoritative. When the schema files change, the next open imports the
  difference as a new version on top of the live edits. If the import would
  break data, it is skipped and the reason is reported in `schema.status`
  (`import_error`).
- **`config`**: the files are authoritative. Each change to them is recorded
  as a version, and API changes are refused.

## Governance

```toml
[governance]
live_edits = true              # accept schema changes through the API at all
apply_roles = ["admin"]        # may apply change sets directly
approve_roles = ["admin"]      # may approve or reject proposals
propose_roles = []             # empty: anyone who can read may propose
required_approvals = 1
allow_self_approval = false
```

Roles are the built-ins (`viewer`, `member`, `agent`, `admin`, `system`) or
any custom name (`schema_admin`, `approver`). Principals with the `agent`
role never apply or approve schema changes, whatever other roles they hold;
they file proposals. Denied attempts are audited as `authorization.denied`.
The ontology builder follows the same rule: `ontology.apply_draft` refuses
agents that ask to apply, and the MCP tool `propose_ontology` only proposes.

## Proposals

The flow is: **propose → review → approve or reject → apply atomically**.

1. **Propose.** Anyone allowed to propose (agents included) files a change
   set with a title and rationale. Invalid change sets are refused with their
   errors. Breaking ones are accepted, and their stored impact report lists
   what they need.
2. **Review.** Reviewers see the diff and impact. If the schema has moved on
   since filing, reading the proposal returns `stale: true` and a `preview`
   planned against the active version. Anyone who can read may comment.
3. **Approve or reject.** Only principals with an `approve_roles` role may
   approve or reject. Agents never can, and neither can the proposer, unless
   `allow_self_approval` is set. With `seen_version`, the approval is refused
   when the schema changed since the reviewer looked.
4. **Apply.** The approval that reaches `required_approvals` applies the
   change set. The schema version (source `proposal`), the migration, the
   proposal marked `applied` with its version, and the audit events all
   commit in one transaction. If the change no longer applies, the proposal
   becomes `failed` with the reason. The proposer can then revise it, which
   reopens it and resets approvals.

Statuses: `open`, `applied`, `rejected`, `failed`, `withdrawn`.

Audit events:

- `schema.proposal.create`, `.comment`, `.approve`, `.reject`, `.withdraw`,
  `.revise`, `.fail`;
- `schema.change`, which carries the proposal id.

Each event records the principal who acted.

```json
{ "title": "Track PR reviews",
  "rationale": "Reviews are mentioned in 12 conversations and 4 decisions",
  "source": "nexc-assistant",
  "changes": [ { "op": "put_link_type", "name": "reviews",
                 "def": { "from": ["person"], "to": ["artifact"], "inverse": "reviewed_by" } } ] }
```

| | Library | gRPC `SchemaService` | HTTP | Action | MCP tool |
|---|---|---|---|---|---|
| propose | `propose_schema(ctx, input)` | `Propose` | `POST /v1/schema/proposals` | `schema.propose` | `propose_schema_change` |
| list | `list_proposals(p, status, limit)` | `ListProposals` | `GET /v1/schema/proposals?status=` | `schema.proposals` | `list_proposals` |
| get | `get_proposal(p, id)` | `GetProposal` | `GET /v1/schema/proposals/{id}` | `schema.proposal` | `get_proposal` |
| approve / reject / comment | `review_proposal(ctx, id, decision, comment, seen_version)` | `Approve` / `Reject` / `Comment` | `POST /v1/schema/proposals/{id}/approve` (`reject`, `comment`) | `schema.approve` / `.reject` / `.comment` | |
| withdraw | `withdraw_proposal(ctx, id)` | `Withdraw` | `POST …/{id}/withdraw` | `schema.withdraw` | |
| revise | `revise_proposal(ctx, id, input)` | `Revise` | `POST …/{id}/revise` | `schema.revise` | |

MCP also offers `get_schema` and `plan_schema_change`, so an assistant can
inspect the schema and dry-run a change before proposing it. Proposing is
offered even when `[mcp] allow_writes = false`, because a proposal changes
nothing until a person approves it.

From the CLI:

```bash
onto schema propose change.json --title "Runbooks" --rationale "on-call asks for them"
```

```bash
onto schema proposals --status open
```

```bash
onto schema proposal prop_0199f…
```

```bash
onto schema approve prop_0199f… --comment "ok"
```

## Surfaces

| | Library (`Ontology`) | gRPC `onto_intelli.v1.SchemaService` | HTTP | Action |
|---|---|---|---|---|
| active document | `schema()` | `Get {version: 0}` | `GET /v1/schema` | `schema.get` |
| status | `schema_status(p)` | `Status` | `GET /v1/schema/status` | `schema.status` |
| dry run | `schema_plan(p, &set)` | `Plan` | `POST /v1/schema/plan` | `schema.plan` |
| apply | `schema_apply(ctx, set)` | `Apply` | `POST /v1/schema/apply` | `schema.apply` |
| history | `schema_history(p, limit)` | `History` | `GET /v1/schema/history?limit=` | `schema.history` |
| one version | `schema_version_get(p, v)` | `Version` / `Get {version}` | `GET /v1/schema/versions/{v}` | `schema.version` |
| point in time | `schema_as_of(p, ts)` | `AsOf` | `GET /v1/schema/as_of?ts=` | `schema.as_of` |
| diff | `schema_diff(p, from, to)` | `Diff` | `GET /v1/schema/diff?from=&to=` | `schema.diff` |
| revert | `schema_revert(ctx, v, migrations, dry_run)` | `Revert` | `POST /v1/schema/revert` | `schema.revert` |
| undo | `schema_undo(ctx, v, migrations, dry_run)` | `Undo` | `POST /v1/schema/undo` | `schema.undo` |

From the CLI:

```bash
onto schema status
```

```bash
onto schema plan change.json
```

```bash
onto schema apply change.toml
```

```bash
onto schema history
```

```bash
onto schema show 3
```

```bash
onto schema diff 1
```

```bash
onto schema revert 2 --dry-run
```

```bash
onto schema undo 4
```

Change set files can be JSON or TOML (`[[changes]]` tables).
