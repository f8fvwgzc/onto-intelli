# Embedding onto-intelli in an application (nexc-engine)

There are two ways to use onto-intelli from an application:

- **Embedded as a library.** Each process (each nexc replica) opens the
  tenants it needs on a shared PostgreSQL database.
- **As a service.** One or more `onto serve` processes run on the same
  database, and the application calls them over gRPC or HTTP.

Both see the same data, schema versions and proposals.

## 1. Tenants: one ontology per workspace

A **tenant** is a separate ontology with its own entity kinds, link types,
actions, schema history, proposals, memory and data. In PostgreSQL each
tenant is its own schema, `{schema_prefix}{tenant}`, for example
`onto_ws_3f2a`. Inside a tenant, onto *workspaces* still partition data for
ACLs.

The suggested mapping is one nexc workspace to one tenant (`ws_<uuid>`), with
the onto workspace `main`, or one onto workspace per nexc project.

### Library

```rust
use onto_intelli::core::store::PgShared;
use onto_intelli::{Config, Hub, Principal};

// Once per process, on nexc's own sqlx 0.9 pool (same crate version), inside its Tokio runtime.
let mut cfg = Config::load("onto-intelli.toml")?;          // schema packages, LLM, governance...
cfg.engine.storage = "postgres".into();
cfg.engine.tenancy = "multi".into();
let hub = Hub::with_postgres(cfg, PgShared::from_pool(state.db.clone())?)?;

// Per request:
let ws = hub.tenant(&format!("ws_{}", workspace_id.simple())).await?;   // opened on first use, then cached
let me = Principal::named(format!("user:{user_id}"), &["member"], &["*"])?.with_entity(format!("person:{user_id}"));
let out = ws.call(&me, "graph.traverse", serde_json::json!({ "start": ["project:x"], "depth": 2 })).await?;
```

Other ways to build a hub:

- `Hub::open(cfg)` takes the backend from configuration: one PostgreSQL
  schema, or one in-memory store, per tenant.
- `PgShared::connect(url, &opts)` creates a pool of its own, on its own small
  runtime.
- `Hub::single(intelli)` wraps one ontology you already opened.

Every operation is `Intelli::call(&principal, action, json)`. The action
names are listed below and in `system.actions`. The typed methods behind
them live on `Intelli::ontology()`: `schema_plan`, `schema_apply`,
`propose_schema`, `review_proposal`, `traverse`, `impact`, `root_cause` and
the others.

> The engine API is synchronous underneath. Run it on a multi-threaded Tokio
> runtime; calls yield the worker while they wait for PostgreSQL. Reads come
> from memory and do not wait, unless `read_consistency = "strong"`.

### Service

A single `onto serve` (or several behind a load balancer) handles every
tenant when `engine.tenancy = "multi"`. The `x-onto-tenant` header, or gRPC
metadata, picks the tenant for each call.

## 2. Principals: each call is made as the user

**Library.** Pass the user's principal on each call:

```rust
Principal { id, roles, workspaces, entity, via }
```

- `roles` are built-ins or custom names, e.g. `member` and `approver`.
- `entity` is the user's node (`person:42`). It drives "my" questions and
  ACLs on restricted entities.

**Service.** The backend has one token configured with `impersonate = true`.
Each request names the user it is made for:

```text
authorization: Bearer $NEXC_ONTO_TOKEN
x-onto-tenant: ws_3f2a
x-onto-act-as: user:42
x-onto-act-roles: member,approver
x-onto-act-workspaces: main            (optional; default: the token's)
x-onto-act-entity: person:42           (optional)
```

The asserted roles must come from the token's `impersonate_roles`, so an
`admin` that isn't listed is refused, and workspaces must be within the
token's. Audit events record the user as the actor, with source
`api via nexc-backend`. Without `x-onto-act-as`, the call runs as the token's
own principal.

**Safe defaults.** Every caller over gRPC or HTTP must present a token. No
anonymous principal exists, and a server with no token refuses to start.
`onto serve --dev`, or `auth.mode = "dev"`, makes every caller
`auth.dev_principal`; it only listens on loopback addresses unless
`ONTO_DEV_ALLOW_REMOTE=1` is set. MCP clients act as `[mcp] principal`, an
agent by default. The CLI and the FFI open the store directly and act as
`[local] principal`. For a per-call principal over the FFI, use
`onto_call_as(handle, principal_json, action, input)`.

**Suggested role mapping for nexc:**

| nexc | onto roles |
|---|---|
| workspace owner or admin | `member`, `approver` (add `schema_admin` to apply directly) |
| member | `member` |
| viewer | `viewer` |
| the AI assistant | `agent`: reads, runs action types, proposes schema changes; never applies or approves them |

Then configure governance to match:

```toml
[governance]
apply_roles = ["schema_admin"]
approve_roles = ["approver"]
```

## 3. Schema as data: what nexc calls

| Need | Library (`Ontology`) | gRPC `onto_intelli.v1.SchemaService` | HTTP | Action |
|---|---|---|---|---|
| read the schema (entity kinds, link types, actions) | `schema()` | `Get` | `GET /v1/schema` | `schema.get` |
| version, mode, governance | `schema_status` | `Status` | `GET /v1/schema/status` | `schema.status` |
| dry run with impact | `schema_plan` | `Plan` | `POST /v1/schema/plan` | `schema.plan` |
| apply directly (apply roles) | `schema_apply` | `Apply` | `POST /v1/schema/apply` | `schema.apply` |
| history, version, as of, diff | `schema_history`, `schema_version_get`, `schema_as_of`, `schema_diff` | `History`, `Version`, `AsOf`, `Diff` | `GET /v1/schema/history`, `/versions/{v}`, `/as_of`, `/diff` | `schema.history`, `.version`, `.as_of`, `.diff` |
| revert, undo | `schema_revert`, `schema_undo` | `Revert`, `Undo` | `POST /v1/schema/revert`, `/undo` | `schema.revert`, `.undo` |
| assistant proposes | `propose_schema` | `Propose` | `POST /v1/schema/proposals` | `schema.propose` (MCP `propose_schema_change`) |
| inbox | `list_proposals`, `get_proposal` | `ListProposals`, `GetProposal` | `GET /v1/schema/proposals[/{id}]` | `schema.proposals`, `schema.proposal` (MCP `list_proposals`) |
| human approves or rejects | `review_proposal` | `Approve`, `Reject`, `Comment` | `POST /v1/schema/proposals/{id}/approve` (`reject`, `comment`) | `schema.approve`, `.reject`, `.comment` |
| withdraw, revise | `withdraw_proposal`, `revise_proposal` | `Withdraw`, `Revise` | `POST …/{id}/withdraw`, `…/revise` | `schema.withdraw`, `.revise` |

Change sets, migrations and the impact report are described in
[SCHEMA.md](SCHEMA.md).

### The execution vocabulary

With `extend = ["builtin:nexc-execution"]`, a graph's node types and relation
types live in onto-intelli, in nexc's shape
([NEXC_EXECUTION.md](NEXC_EXECUTION.md)):

| | Library | gRPC `ExecutionService` | HTTP | Action |
|---|---|---|---|---|
| graphs | `find` | `Graphs` | `GET /v1/execution/graphs` | `execution.graphs` |
| new graph | `init_execution_graph` | `InitGraph` | `POST /v1/execution/graphs` | `execution.init_graph` |
| read vocabulary | `execution_vocabulary` | `Vocabulary` | `GET /v1/execution/graphs/{g}/vocabulary` | `execution.vocabulary` |
| replace vocabulary | `set_execution_vocabulary` | `SetVocabulary` | `PUT /v1/execution/graphs/{g}/vocabulary` | `execution.set_vocabulary` |

## 4. The web UI

`/ui` is served by `onto serve`. Embed it, or link to it, as the workspace
map, schema editor and proposals inbox ([UI.md](UI.md)):

- `[ui] embed_origins` lists nexc's origins.
- The backend mints a browser session with `POST /v1/auth/session` (its
  impersonating token plus `{act_as, roles, entity, tenant}`) and hands it to
  the iframe with `postMessage` (`onto:session`).
- Deep links preselect the workspace and the focus:
  `#/map?workspace=main&focus=<id>`.

## 5. Configuration keys

```toml
[engine]
storage = "postgres"            # postgres | memory (tests and demos)
# database_url = "postgres://…"  # or ONTO_DATABASE_URL / DATABASE_URL in the environment
tenancy = "multi"               # single | multi (x-onto-tenant chooses)
read_consistency = "eventual"   # eventual | strong (read-your-writes across replicas)
follow_interval_ms = 1000
default_workspace = "main"

[engine.postgres]
tenant = "default"              # the default tenant
schema_prefix = "onto_"
max_connections = 10
connect_timeout_secs = 10

[ontology]
schema = "builtin:enterprise"
extend = []                     # builtin:<package> or TOML files
schema_mode = "live"            # live | config
strict = false

[governance]
live_edits = true
apply_roles = ["admin"]
approve_roles = ["admin"]
propose_roles = []              # empty: anyone who can read
required_approvals = 1
allow_self_approval = false

[auth]
mode = "token"                  # token | dev
[[auth.tokens]]
token_env = "NEXC_ONTO_TOKEN"
principal = { id = "nexc-backend", roles = ["member"], workspaces = ["*"] }
impersonate = true
impersonate_roles = ["viewer", "member", "agent", "approver"]
tenants = ["*"]

[mcp]
principal = { id = "mcp-agent", roles = ["agent"], workspaces = ["*"] }

[local]
principal = { id = "local", roles = ["admin"], workspaces = ["*"] }

[ui]
enabled = true
embed_origins = ["https://nexc.example.com"]
title = "Ontology"
```

Also set `ONTO_SESSION_SECRET` to the same value of at least 32 characters
on every replica, so browser sessions work across all of them.

## LLM connection contract

Every core of the platform reads the same `LLM_*` environment variables (contract v1, as in
texc-symphony), so one Kubernetes Secret and ConfigMap configure model access everywhere.

| Variable | Required | Meaning |
|---|---|---|
| `LLM_PROVIDER` | yes, unless gateway mode | `anthropic`, `openai` or `openai-compatible` (vLLM, Ollama, LM Studio, OpenRouter, Groq, Together, Azure OpenAI: anything that speaks OpenAI Chat Completions, at `LLM_BASE_URL`). In gateway mode, the wire format (default `anthropic`). |
| `LLM_BASE_URL` | for `openai-compatible` | The provider's API base URL (`http://` allowed). |
| `LLM_API_KEY_FILE` | | A file holding the key (a mounted Secret), read on every request: a rotated Secret applies to the next call. Wins over `LLM_API_KEY`. |
| `LLM_API_KEY` | | The key as text. |
| `LLM_MODEL` | yes | The model. |
| `LLM_MODEL_SMALL` | | A small, fast model. Read and kept (`[llm] model_small`); onto makes no small-model calls today. |
| `LLM_TIMEOUT_SECS` | | Timeout of one request (`> 0`). |
| `LLM_MAX_RETRIES` | | Retries of one request after a transport error, 408, 429 or 5xx (`0..=10`). |
| `LLM_GATEWAY_URL` | | **Gateway mode**: every call goes to the platform's model gateway (attache), at its root URL (a trailing `/v1` is ignored). onto then holds no provider key: `LLM_API_KEY` and `ANTHROPIC_API_KEY` are not used. |
| `LLM_GATEWAY_TOKEN_FILE` | with the gateway | A file holding the gateway token, sent as `Authorization: Bearer`. |

Precedence, highest first: a project's own provider (the console's bring-your-own LLM); the
keys that the configuration file's `[llm]` table sets; the contract; the defaults. A deployment
that sets no `LLM_*` variable is unchanged. The contract applies once `LLM_PROVIDER` or
`LLM_GATEWAY_URL` is set. Setting `LLM_MODEL`, `LLM_BASE_URL` or a key without one of them is
an error, so a typo does not silently disable it. Errors name the variable (`LLM_MODEL can't
be blank`, `LLM_PROVIDER is invalid`) and stop `onto serve` at start. Keys are never logged,
and an unreadable key file fails the request with the file's path, not its contents.

In Kubernetes the chart (`deploy/helm/onto-intelli`, `global.llm.enabled = true`) reads the
ConfigMap `llm-config` (non-secret `LLM_*` keys, as `envFrom`) and mounts the Secret
`llm-credentials` at `/var/run/secrets/llm` (`api-key` → `LLM_API_KEY_FILE`, `gateway-token` →
`LLM_GATEWAY_TOKEN_FILE`). `global.llm.*` values (`provider`, `baseUrl`, `model`, `modelSmall`,
`timeoutSecs`, `maxRetries`, `gatewayUrl`) win over the ConfigMap. With `global.llm.gatewayUrl`
set, only the gateway token is mounted, so the pod holds no provider key.
