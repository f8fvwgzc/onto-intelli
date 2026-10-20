# The nexc-execution vocabulary package

`builtin:nexc-execution` turns the per-graph ontology of nexc-engine into
ontology data. Today nexc stores a list of node types and relation types with
each graph; with this package nexc reads and writes them in onto-intelli
instead. Every edit is validated, versioned, audited, permission-checked and
visible on the workspace map.

```toml
[ontology]
schema = "builtin:enterprise"           # or builtin:empty
extend = ["builtin:nexc-execution"]     # plus nexc-audit.toml for the run trace, if used
```

## Model

| kind | id | properties (nexc's field names) |
|---|---|---|
| `execution_graph` | `execution_graph:{graph}` | title, `description` |
| `node_type` | `node_type:{graph}.{key}` | `key`, `label`, `description`, `color`, `icon`, `default_role`, `default_executor` (`llm`, `agent`, `symphony`), `stage` (int), `produces_artifact`, `allow_code_exec` |
| `relation_type` | `relation_type:{graph}.{key}` | `key`, `label`, `description`, `blocking` |
| `node` | (nexc's run trace) | linked `instance_of` → `node_type` |

**Link types:**

- `node_type | relation_type -defined_in-> execution_graph`, inverse
  `defines`. Single-valued.
- `node -instance_of-> node_type`, inverse `instances`.

Keys are unique per graph because they are part of the id.

**Validation** mirrors nexc's rules:

- keys match `^[a-z][a-z0-9_]*$`, at most 40 characters;
- labels are at most 60 characters, descriptions at most 500;
- `color` is `#rrggbb`;
- `default_executor` is one of the three executors.

Violations are refused with the field and the rule that failed. The type
count limit (`TYPES_MAX`, 40) stays a nexc-side check.

## API (nexc's shape)

```json
{ "node_types": [ { "key": "code", "label": "Code", "description": "Writing or changing source code.",
                    "color": "#f59e0b", "icon": "code", "default_role": "engineer",
                    "default_executor": "llm", "stage": 2, "produces_artifact": false, "allow_code_exec": true } ],
  "relation_types": [ { "key": "depends_on", "label": "Depends on", "description": "…", "blocking": true } ] }
```

| | Library (`Ontology`) | gRPC `onto_intelli.v1.ExecutionService` | HTTP | Action |
|---|---|---|---|---|
| list graphs | `find(kind = execution_graph)` | `Graphs` | `GET /v1/execution/graphs` | `execution.graphs` |
| new graph (starter vocabulary unless `starter: false` / `empty`, or a given one) | `init_execution_graph(ctx, graph, title, Some(Vocabulary::starter()))` | `InitGraph` | `POST /v1/execution/graphs {graph, title}` | `execution.init_graph` |
| read a graph's vocabulary | `execution_vocabulary(p, graph)` | `Vocabulary` | `GET /v1/execution/graphs/{graph}/vocabulary` | `execution.vocabulary` |
| replace it (create, update, remove in one atomic write) | `set_execution_vocabulary(ctx, graph, vocab)` | `SetVocabulary` | `PUT /v1/execution/graphs/{graph}/vocabulary` | `execution.set_vocabulary` |

`Vocabulary::starter()` is nexc's `Ontology::starter()`:

- node types topic, research, task, code (`allow_code_exec`), document and
  output (`produces_artifact`);
- relation types `depends_on` (blocking) and `relates_to`.

## Governed edits

For people in the UI, and for agents and nexc's assistant over MCP or
actions, the package defines action types:

| action type | risk | does |
|---|---|---|
| `define_node_type` | medium: confirmation, since it can allow code execution | create or change `node_type:{graph}.{key}` |
| `define_relation_type` | low | create or change `relation_type:{graph}.{key}` |
| `retire_execution_type` | high: confirmation | delete a node or relation type |

Each run is planned (with its inverse), applied with an undo receipt, and
audited. To change the package's kinds themselves, or to add link types or
actions next to them, use the live schema API or proposals
([SCHEMA.md](SCHEMA.md)).

## Migration path for nexc

1. Load `builtin:nexc-execution` in the tenant of each nexc workspace. On
   an existing tenant, add it to `extend`; the next open imports it as a
   schema version.
2. For each existing graph, call `execution.set_vocabulary {graph, …}` with
   its stored ontology.
3. Read `execution.vocabulary` where the executor, planner and canvas read
   `Ontology` today: `role_for`, `stage_of`, `produces_artifact`,
   `allows_code_exec`, the blocking relation.
4. Delete nexc's per-graph ontology storage.
