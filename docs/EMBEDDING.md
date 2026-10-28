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
  the iframe with `postMessage` (`onto:session`). The answer carries the
  session's `jti`: `POST /v1/auth/session/revoke {jti}` (the backend's
  token) revokes it and every session minted from it, `DELETE
  /v1/auth/session` (the session itself) signs out its whole login, on
  every replica within 10 seconds. A session may mint another one, which
  never outlives it and carries the same `sid`; that one mints no further
  sessions (403). A token for every tenant (`"*"`) revokes an id with or
  without `x-onto-tenant`. A token limited to some tenants revokes it for
  the sessions of all of its tenants only (the answer lists them; tenant
  ids are compared as their schema names are, ignoring case and `-`, `.`,
  `_`); with one tenant it needs no `x-onto-tenant`, with several it must
  name one of them (403 otherwise), as for any request. The id carries the
  session's expiry: the revocation lasts until that expiry plus 60
  seconds, whatever `auth.session_max_ttl_secs` is set to later (and at
  most a year ahead, unless sessions may live longer); `expires_at`
  matters only for ids without one (minted by earlier builds).
- Upgrading from an earlier build: replicas still on it ignore revocations
  limited to some tenants, so during a rolling deploy (or after rolling
  back) such a revoke holds only on upgraded replicas; a token for every
  tenant is not affected. Sessions minted by earlier builds from a
  session that was itself minted from a session are not cut off by
  revoking the middle one: revoke the login's `sid` instead.
- Signing out from any session of a login ends the whole login (the
  session it was minted from and its other sessions too): the user signs
  out, not one copy of a token, and every session of a login is the same
  principal in the same tenant anyway.
- Revocations of every tenant's sessions live in the default tenant's KV
  store (namespace `auth.revoked`), so session authentication reads the
  default tenant: it is in the same database as every tenant, a failed read
  refuses the session (503) rather than accepting it, and reads are cached
  (an id read as not revoked is read again after 10 seconds).
- A token whose `tenants` names some tenants (not `"*"`) never reaches the
  default tenant: with one tenant, a request without `x-onto-tenant`
  addresses that one; with several, it must name one.
- `GET /livez` and `GET /readyz` (one query on PostgreSQL) answer without a
  token, for probes; `/healthz` needs one.
- Agents (role `agent`) change the graph through action types only:
  `entity.upsert`, `link.create`, `graph.apply` and the other raw writes
  (`pack.install` with `sample` too: the sample's entities and links) are
  refused to them unless `[governance] agent_raw_writes = true`, and an
  agent's `confirm=true` does not confirm a high-risk or irreversible action
  (a person applies it).
- `member_of` links grant access to restricted entities (their `acl`), so
  only admins create or delete them.
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
`LLM_GATEWAY_URL` is set. Its connection (provider, endpoint, key and model) applies as one:
not at all when `[llm]` sets `base_url`, `api_key`, `api_key_env`, `api_key_file`, `model` or
`model_small`, or another `provider`. Then a warning says so, and `[llm]` applies as written.
The key is bound to the endpoint it was issued for: the gateway, `LLM_BASE_URL`, or else the
default endpoint of `LLM_PROVIDER` itself. With a named endpoint, a file `provider` of the same
wire format is fine (`anthropic`/`claude`, or `openai`/`openai-compatible`); without one, it
must be the same provider (`openai-compatible` defaults to a local server, `openai` to
api.openai.com). `LLM_TIMEOUT_SECS` and `LLM_MAX_RETRIES` apply key by key. An endpoint from
the contract without a key from it gets no key at all (not `OPENAI_API_KEY`). Other `LLM_*`
without `LLM_PROVIDER` or `LLM_GATEWAY_URL` are ignored, with a warning (`Ignoring LLM_MODEL
without LLM_PROVIDER or LLM_GATEWAY_URL`), as in texc-symphony. An
invalid value of an active contract is an error that names the variable (`LLM_MODEL can't be
blank`, `LLM_PROVIDER is invalid`) and stops `onto serve` at start. Keys are never logged,
and an unreadable key file fails the request with the file's path, not its contents.

In Kubernetes the chart (`deploy/helm/onto-intelli`, `global.llm.enabled = true`) reads the
ConfigMap `llm-config` (non-secret `LLM_*` keys, as `envFrom`) and mounts the Secret
`llm-credentials` at `/var/run/secrets/llm` (`api-key` → `LLM_API_KEY_FILE`, `gateway-token` →
`LLM_GATEWAY_TOKEN_FILE`). `global.llm.*` values (`provider`, `baseUrl`, `model`, `modelSmall`,
`timeoutSecs`, `maxRetries`, `gatewayUrl`) win over the ConfigMap. With `global.llm.gatewayUrl`
set, only the gateway token is mounted, so the pod holds no provider key.
