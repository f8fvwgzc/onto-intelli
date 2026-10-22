# Control plane

onto-intelli can run as a hosted console, the way Clerk, Supabase and Langfuse
do: a team signs in, creates a project, gets API keys and copy-paste
snippets for its services and monorepo, brings its own LLM account, and
watches every question, tool call and model call in a monitoring UI. The
same single binary and one PostgreSQL database serve both the console and
the data plane, so a self-hosted install has all of it.

This document is the design: data model, migrations, crates and files,
authentication, cryptography, traces, HTTP and gRPC surfaces, UI pages,
SDKs, the repository connector, security and the test plan. The last
section splits the work into stages.

Non-goals: billing and payments, sending email, operating a SaaS
deployment.

## Contents

1. [Concepts](#1-concepts)
2. [Architecture](#2-architecture)
3. [Configuration](#3-configuration)
4. [Control store: data model](#4-control-store-data-model)
5. [Roles and principals](#5-roles-and-principals)
6. [API keys](#6-api-keys)
7. [Console sign-in](#7-console-sign-in)
8. [Bring your own LLM](#8-bring-your-own-llm)
9. [Traces](#9-traces)
10. [HTTP routes and gRPC services](#10-http-routes-and-grpc-services)
11. [Console UI](#11-console-ui)
12. [SDKs](#12-sdks)
13. [Repository connector](#13-repository-connector)
14. [Security](#14-security)
15. [Setup](#15-setup)
16. [Test plan](#16-test-plan)
17. [Implementation stages](#17-implementation-stages)

## 1. Concepts

| Term | Meaning |
|---|---|
| **User** | A person who signs in to the console. Identified by one or more IdP identities `(provider, subject)`. |
| **Organization** | A team. Users are **members** with one role: `owner`, `admin`, `developer` or `viewer`. |
| **Project** | One ontology. A project **is** a Hub tenant: its `ref` (20 lowercase characters, like `k3v9q2m8x1p4t7w5r6y0`) is the tenant id, so its data lives in the PostgreSQL schema `onto_<ref>`. |
| **API key** | A secret a service presents to reach one project's data plane, with scopes `read`, `write`, `agent`, `admin`. |
| **Console session** | An `onto1.…` session token held by the console tab. It names the user; the principal of each request is derived from the user's membership in the project the request addresses. |
| **Trace** | One unit of work in a project (an ask, a governed action, an LLM-using call) with its **spans** (intent, resolution, retrieval, generation, tool) and **scores**. A **session** groups traces by `session_id`. |

What the console borrows, and from whom:

* **Clerk**: an organization with roles, invitations, sign-in through a
  hosted IdP, and short-lived tokens held by the client instead of cookies.
* **Supabase**: projects with a short `ref`, named secret keys with a
  visible prefix that can be created, rotated and revoked individually,
  shown once, and a Connect dialog with ready-to-paste snippets.
* **Langfuse**: traces made of observations (spans, generations, events),
  sessions that replay a conversation, scores for evaluations and user
  feedback, a metrics endpoint for aggregates separate from trace listing,
  and OpenTelemetry export using the GenAI semantic conventions.

## 2. Architecture

```text
                       browser (console, /ui)                 services, SDKs, MCP clients
                               │ onto1 console session               │ onto_sk_ API key
                               ▼                                     ▼
 ┌───────────────────────────── onto serve (one binary) ─────────────────────────────────┐
 │  onto-grpc   auth.rs: config tokens · API keys · sessions · dev  ──►  Caller            │
 │              http.rs: data plane /v1/…        console.rs: /v1/console/…   mcp_http.rs   │
 │                          │                           │                                  │
 │  onto-control            │            ControlStore (orgs, members, projects, keys,      │
 │   (new crate)            │            llm settings, usage, audit, oidc state)           │
 │                          │            OIDC client · AES-256-GCM vault · key hashing     │
 │                          ▼                           │ TenantHooks (provider, limits)    │
 │  onto-intelli   Hub ── tenant(ref) ── Intelli ◄──────┘                                  │
 │                 ask / call / complete ── Tracer ── TraceSink ──► Store::trace_write      │
 │                                                       └───────► OTLP/HTTP (optional)    │
 │  onto-core      Store (PgStore per tenant · MemoryStore) · migrations 0001…0005          │
 └───────────────────────────────────────┬────────────────────────────────────────────────┘
                                         ▼
                 PostgreSQL 16+ / pgvector: onto_control · onto_<ref> · onto_<ref> · …
```

### Crates and files

| Crate | Files | Change |
|---|---|---|
| `onto-core` | `migrations/0005_traces.sql` | New tenant migration: `traces`, `spans`, `scores`. Added to `MIGRATIONS`. |
| | `migrations/0006_trace_otel.sql` | New tenant migration: `traces.otel_trace_id` and `traces.parent_span` (the caller's `traceparent`), indexed. |
| | `src/policy.rs` | `Principal::can_observe()`. |
| | `src/store/postgres.rs` | `PgShared::drop_tenant(prefix, tenant)`. |
| | `src/trace.rs` | New: `TraceRow`, `SpanRow`, `ScoreRow`, `TraceRecord`, `TraceDetail`, `TraceQuery`, `TracePage`, `MetricsQuery`, `TraceMetrics`, `SessionQuery`, `SessionRow`, `SessionPage`, the in-memory query and aggregate code shared with `MemoryStore`, `percentile_cont`, cursors, W3C ids and `parse_traceparent`. |
| | `src/store/mod.rs` | `Store` gains trace methods with default "unsupported" bodies; `MemoryStore` implements them (bounded ring). |
| | `src/store/postgres.rs`, `src/store/postgres/traces.rs` | `PgStore` implements the trace methods (in the child module `traces.rs`). |
| | `src/error.rs` | New `Error::Limited(String)`, code `resource_exhausted`, and `Error::retry_after_secs` (the N of "retry in N s", else 60). HTTP maps it to 429 with `retry-after`, gRPC to `RESOURCE_EXHAUSTED`, the CLI's remote client back to `Limited`; the FFI header lists the codes. |
| | `schemas/code.toml` | New built-in schema package `builtin:code` (package object type, links). |
| `onto-intelli` | `src/config.rs` | `[console]`, `[traces]` sections; `LlmConfig.api_key: Option<Secret>` (never serialized); `Secret` newtype with redacted `Debug`. |
| | `src/hub.rs` | `TenantSettings`, `TenantHooks`, `Hub::set_hooks`, `Hub::refresh_tenant`, `Hub::default_llm`, `Hub::evict`, `Hub::drop_tenant` (evict, then `DROP SCHEMA … CASCADE` through `PgShared::drop_tenant`), the reserved control schema. |
| | `src/intelli.rs` | Swappable provider (`set_llm`), `set_limits`, `set_meter`, `take_ask`, `try_ask_stream`, the budget check in `complete`; stage 4: `ask_traced`, `try_ask_stream_traced`, the spans of an ask, the generation span in `complete`, `flush_traces`, `vacuum_traces`, `set_trace_settings`, the retention task. |
| | `src/trace.rs` | New: `TraceCtx`, `AskTrace`, `Traced<T>`, `TraceSettings`, `Tracer` (task-local current trace), `TraceSink` (batched background writer). |
| | `src/otlp.rs` | Optional, **not built** (stages 1 to 6): OTLP/HTTP JSON exporter (section 9.6). |
| | `src/limits.rs` | New: `Limits`, per-tenant token bucket, `UsageMeter` trait. |
| | `src/llm/*.rs` | Providers read `cfg.api_key` before the environment variable; `llm::build_with_client` builds a provider on a given `reqwest::Client`; `max_output_tokens` caps each request, and `LlmProvider::max_output_tokens()` reports it, so asks, `ontology.refine` and `ontology.advise` ask for the provider's own limit (a project's) rather than the server's `[llm]` one. |
| | `src/modeling.rs` | `ontology.refine` and `ontology.advise` take the rate limit and record usage. |
| | `src/actions.rs` | `call_traced` (`call` delegates to it); actions `trace.list`, `trace.get`, `trace.metrics`, `trace.sessions`, `trace.session`, `trace.score`. |
| | `src/connectors.rs`, `src/connectors/repo.rs` | `kind = "repo"`: scan a working tree or git URL into packages, owners and dependencies (stage 6: `ConnectorSpec.repo`, `CrawlReport.repo`, `ConnectorState.stamp`, `crawl_one` dispatches `repo`; `repo.rs` holds git URL parsing, the `git` runner, the walk, the four ecosystems, CODEOWNERS and the plan). `onto-intelli` depends on `sha2` (already in the workspace) for the clone directory name. |
| | `src/eval.rs` | Each eval case's trace gets a `score` (`eval.pass`, 0 or 1). |
| `onto-control` (new) | `src/lib.rs` | `Control` (store, key pepper, caches, rate limit, bootstrap and dev sign-in, maintenance task; stage 2 adds OIDC, stage 3 the vault, the LLM test rate limit, meters and provider errors), re-exports. |
| | `src/model.rs` | `User`, `Identity`, `Org`, `Member`, `OrgRole`, `Invitation`, `Project`, `ApiKey`, `KeyScope`, `ProjectLlm`, `Sealed`, `UsageDay` (= `UsageDelta`), `LLM_PROVIDERS`, `AuditEntry`. |
| | `src/store.rs` | `ControlStore` async trait; `MemoryControl`. |
| | `src/pg.rs` | `PgControl` on its own `PgPool`; migration runner for `onto_control`. |
| | `migrations/0001_control.sql`, `0002_llm_usage.sql` | Control schema migrations (`{schema}` placeholder). |
| | `src/keys.rs` | Key generation, parsing, peppered SHA-256, constant-time check, key cache, batched `last_used_at`; the random, SHA-256 and HMAC helpers the rest of the crate uses. |
| | `src/vault.rs` | AES-256-GCM envelope under `ONTO_MASTER_KEY`. |
| | `src/llm.rs` | BYO LLM on `Control`: `LlmUpdate` (the `PUT …/llm` body), validation, `put_llm`, `delete_llm`, `llm_view`, `provider_for`, `test_llm`, `usage_report`, `rotate_master_key`, `limits_of`. |
| | `src/net.rs` | The SSRF guard: `public_ip`, the `PublicOnly` DNS resolver, `check_url`, `check_headers`, `client`; stage 6: `check_git_url` (section 14). |
| | `src/oidc.rs` | Discovery, PKCE, JWKS cache, JWT verification, token exchange, userinfo; `Provider` per `[[console.oidc]]` entry. |
| | `src/signin.rs` | The OIDC sign-in flow on `Control`: `oidc_start`, `oidc_callback` (section 7.4's rules), `exchange_ticket`, the start rate limit and the startup checks of the providers. |
| | `src/hooks.rs` | `ControlHooks` (the `TenantHooks` implementation: per-project provider, limits, usage meter, trace settings), `Meter`, `install` (sets the hooks on the Hub and starts the usage and settings task), `Control::flush_usage`, `reload_usage`, `poll_settings`; stage 4: `trace_settings_of`, `check_trace_settings`. |
| `onto-grpc` | `src/auth.rs` | `authenticate` becomes `async`; API keys and console sessions; `SessionClaims.console` and `SessionClaims.key`; `Credential`; `guard_project_call` (stage 6: `async`, with `allow_private`, checks git URLs; `Auth::allow_private_urls`). |
| | `src/console.rs` | New: `/v1/console/…` routes. |
| | `src/observe.rs` | New: `/v1/traces`, `/v1/sessions`, `/v1/metrics` (thin wrappers over the trace actions); `trace_ctx`, the trace context of a request (HTTP and gRPC). |
| | `src/mcp_http.rs` | New: `POST /mcp`, the MCP Streamable HTTP transport (JSON responses). `onto_intelli::mcp` makes `SUPPORTED` public and gains `McpServer::with_trace_ctx`. |
| | `src/lib.rs` | `ServeOptions.control`; `check_console` (startup rules of section 3); routes merged; `router_hub` calls `onto_control::hooks::install`. |
| | `proto/onto_intelli/v1/onto_intelli.proto`, `src/routes.rs` | `ObservabilityService`. |
| `onto-cli` | `src/main.rs` | `onto serve` opens the control store when `[console] enabled`; `onto console …` admin commands (stage 3: `rotate-master-key`); `onto connect repo`. |
| `ui` | `src/pages/console/*`, `src/lib/console.ts`, `src/lib/observe.ts`, `src/components/charts.tsx`, `consoleUi.tsx`, `ConsoleShell.tsx`, `ProjectSwitcher.tsx` | Console pages (section 11). |
| `sdk/typescript`, `sdk/python` | `src/{index,types,sse}.ts`, `test/*.test.ts`; `onto_intelli/{__init__,client,sse,errors}.py`, `tests/test_client.py` | New SDKs (section 12). |
| `crates/onto-intelli/tests` | `repo.rs`, `fixtures/repos/{cargo,npm,pnpm,go,uv}` | The repository connector's tests and fixture monorepos (section 16). |

New workspace dependency: `aws-lc-rs = { version = "1.18", default-features
= false, features = ["aws-lc-sys", "prebuilt-nasm"] }`, the features rustls
already enables, so it adds no new build. It provides AES-256-GCM,
RSA/ECDSA/Ed25519 signature verification, SHA-256, HMAC and a CSPRNG.
`onto-control` depends on `onto-core` (`onto_core::sqlx`, ids, errors,
roles) and `onto-intelli` (`ConsoleConfig`), and uses `base64`, `serde`,
`tokio`, `async-trait`, `parking_lot` and `tracing`, all already in the tree;
stage 2 adds `reqwest` and `url` for OIDC (and `onto-grpc`'s tests use
`aws-lc-rs`, `parking_lot` and `url` for the mock IdP); stage 3 turns on
`onto-intelli`'s `http-llm` feature for `onto-control` (project providers). Run one `cargo build` without
`--locked` to record the new edges in `Cargo.lock` (everything is in the
local registry), then the gate runs with `--locked`.

## 3. Configuration

New sections in `onto-intelli.toml` (and the annotated template in
`crates/onto-intelli/templates/onto-intelli.toml`):

```toml
[console]
# Serve the control plane: organizations, projects, API keys, BYO LLM, traces.
# Requires engine.tenancy = "multi" (startup fails otherwise).
enabled = false
# Public base URL of this server; the OIDC redirect URI is
# {public_url}/v1/console/auth/oidc/callback.
public_url = "http://127.0.0.1:50051"
# PostgreSQL schema of the control store (memory when engine.storage = "memory").
schema = "onto_control"
# First sign-in through an IdP creates the user. When false, only invited
# users (by verified email) and existing users can sign in.
allow_signup = true
# A new user with no membership gets an organization of their own.
create_org_on_signup = true
# Verified email domains allowed to sign up; empty = any.
allowed_email_domains = []
# Environment variable with a bootstrap admin token (at least 32 characters;
# a shorter value refuses to start): POST /v1/console/auth/token signs in as
# the local administrator. Unset or empty variable: no bootstrap sign-in.
bootstrap_token_env = "ONTO_ADMIN_TOKEN"
# Console session lifetime (refreshed by the UI while the tab is open).
session_ttl_secs = 28800
# Allow project LLM base URLs and git URLs that resolve to loopback or
# private addresses (self-hosted Ollama, an internal git server); also lets
# API keys and console sessions register ssh:// and user@host:path git URLs.
allow_private_urls = false
# Projects per organization (0 = unlimited).
max_projects_per_org = 20

[[console.oidc]]
id = "clerk"                       # [a-z0-9-], used in URLs
name = "Clerk"                     # shown on the sign-in button
issuer = "https://example.clerk.accounts.dev"
client_id = "…"
client_secret_env = "ONTO_OIDC_CLERK_SECRET"   # omit for a public client (PKCE only)
scopes = ["openid", "email", "profile"]
# Extra accepted `aud` values besides client_id.
audiences = []

[traces]
enabled = true
retention_days = 30                # per project override in project settings, 1..400
# Store question, prompt and answer text (clipped). false: metadata only.
capture_content = true
max_content_bytes = 4096
# Which engine calls become traces besides intelli.ask: "writes" (writing
# actions and LLM-using actions), "all", or "none".
calls = "writes"
# Background writer: flush every this many ms or records.
flush_ms = 500
flush_records = 200
queue = 10000                      # records waiting; beyond this they are dropped and counted
```

Environment variables:

| Variable | Use |
|---|---|
| `ONTO_SESSION_SECRET` | Signs `onto1.` sessions (existing). Required with the console when more than one replica runs. |
| `ONTO_MASTER_KEY` | 32 bytes, base64 or 64 hex characters. Encrypts project LLM keys. Without it, storing an LLM key is refused. |
| `ONTO_MASTER_KEY_PREVIOUS` | The previous master key during a rotation (decrypt only). |
| `ONTO_KEY_PEPPER` | At least 32 characters, mixed into API key hashes. Without it, the pepper is derived from `ONTO_MASTER_KEY` (HMAC-SHA256 with the label `onto api key pepper v1`). Without either, API keys cannot be created (except with the memory control store, which uses a random per-process pepper). |
| `ONTO_ADMIN_TOKEN` | Bootstrap admin sign-in (name set by `console.bootstrap_token_env`). |
| `ONTO_OIDC_<ID>_SECRET` | OIDC client secrets (names set per provider). |
| `OTEL_EXPORTER_OTLP_ENDPOINT`, `OTEL_EXPORTER_OTLP_TRACES_ENDPOINT`, `OTEL_EXPORTER_OTLP_HEADERS`, `OTEL_SERVICE_NAME` | Optional OTLP/HTTP export of traces (section 9.6). **Not built**: the variables are not read. |

Startup rules for `onto serve` with `console.enabled = true`:

* `engine.tenancy` must be `multi`.
* `grpc.http` must be on (the console API is HTTP/JSON).
* At least one sign-in path must exist: an `[[console.oidc]]` provider, the
  bootstrap token, or dev mode. Config `[[auth.tokens]]` are then optional
  (`Auth::from_config_with` accepts none when the control plane has an OIDC
  provider or the bootstrap token; without a control plane it still refuses).
* `console.public_url` must be `https://` unless its host is loopback
  (`localhost`, `127.0.0.0/8`, `[::1]`).
* `[console] enabled` without a control store (`ServeOptions.control`) is
  refused. `onto serve` builds it: `PgControl` in `console.schema` on the
  engine's database URL, or `MemoryControl` when `engine.storage = "memory"`.
  It also starts the maintenance task (key uses written every 10 s, expired
  OIDC logins and tickets swept every 10 minutes).
* Each `[[console.oidc]]` entry is checked when the control plane is built
  (`Control::new`): `id` of `[a-z0-9-]` (1 to 40 characters, unique), a
  `name`, an `issuer` that is `https://` unless its host is loopback and
  has no query or fragment, a `client_id`, `openid` among the `scopes`, and,
  when `client_secret_env` is named, a non-empty value in that variable
  (`ControlOptions::from_env` reads it into `ControlOptions.oidc_secrets`).
  Discovery is not fetched at startup: the first sign-in start fetches it,
  and an unreachable or misconfigured IdP is a 503 from that start route
  naming the provider, its issuer and the reason.
* Config tokens may not start with `onto1.` or `onto_sk_`: those forms are
  sessions and API keys.

`onto_grpc::check_console(config, control, dev)` applies these rules;
`serve_hub_on` calls it.

## 4. Control store: data model

The control store is one PostgreSQL schema (`console.schema`, default
`onto_control`) with its own versioned migrations, embedded from
`crates/onto-control/migrations/*.sql` and recorded in
`{schema}.schema_migrations`, applied under
`pg_advisory_xact_lock(hashtext(schema))` exactly like tenant migrations in
`PgStore::migrate`. Never edit a released migration; add the next one.
`MemoryControl` implements the same trait for tests and `engine.storage =
"memory"`.

The control schema must never be a tenant schema: `Hub` refuses a tenant
whose `tenant_schema(prefix, id)` equals `console.schema` (with the default
names, tenant `control`). Project refs never collide (they are random).

`PgControl` owns a small pool (`max_connections = 4`) created on the
server's runtime, so all its queries are plain `async` sqlx calls; it does
not use the tenant stores' blocking runtime.

Times are Unix milliseconds (`BIGINT`), like the tenant tables. Ids are
`new_id`-style strings with a type prefix (`usr_`, `org_`, `prj_`, `key_`,
`inv_`). JSON columns are written as text cast to `jsonb` and read back as
text. A unique violation (`23505`) is `Error::Conflict`, a missing parent
row (`23503`) `Error::NotFound`.

### `0001_control.sql`

```sql
CREATE TABLE {schema}.users (
    id TEXT PRIMARY KEY,                    -- usr_…
    email TEXT,                             -- from the IdP; may be NULL
    email_verified BOOLEAN NOT NULL DEFAULT false,
    name TEXT NOT NULL DEFAULT '',
    avatar_url TEXT,
    created_at BIGINT NOT NULL,
    last_login_at BIGINT,
    -- Sessions issued before this are refused (sign out everywhere).
    sessions_valid_after BIGINT NOT NULL DEFAULT 0,
    disabled BOOLEAN NOT NULL DEFAULT false
);
CREATE INDEX users_email ON {schema}.users (lower(email));

CREATE TABLE {schema}.identities (
    provider TEXT NOT NULL,                 -- console.oidc id, or 'local' (bootstrap), 'dev'
    subject TEXT NOT NULL,                  -- the IdP's `sub`
    user_id TEXT NOT NULL REFERENCES {schema}.users (id) ON DELETE CASCADE,
    email TEXT,
    claims JSONB NOT NULL DEFAULT '{}',     -- name, picture, email_verified at last login
    created_at BIGINT NOT NULL,
    last_login_at BIGINT NOT NULL,
    PRIMARY KEY (provider, subject)
);
CREATE INDEX identities_user ON {schema}.identities (user_id);

CREATE TABLE {schema}.orgs (
    id TEXT PRIMARY KEY,                    -- org_…
    slug TEXT NOT NULL UNIQUE,              -- [a-z0-9-], 3..40
    name TEXT NOT NULL,
    created_at BIGINT NOT NULL,
    created_by TEXT NOT NULL
);

CREATE TABLE {schema}.members (
    org_id TEXT NOT NULL REFERENCES {schema}.orgs (id) ON DELETE CASCADE,
    user_id TEXT NOT NULL REFERENCES {schema}.users (id) ON DELETE CASCADE,
    role TEXT NOT NULL CHECK (role IN ('owner', 'admin', 'developer', 'viewer')),
    created_at BIGINT NOT NULL,
    PRIMARY KEY (org_id, user_id)
);
CREATE INDEX members_user ON {schema}.members (user_id);

CREATE TABLE {schema}.invitations (
    id TEXT PRIMARY KEY,                    -- inv_…
    org_id TEXT NOT NULL REFERENCES {schema}.orgs (id) ON DELETE CASCADE,
    email TEXT NOT NULL,                    -- matched against a verified email
    role TEXT NOT NULL CHECK (role IN ('admin', 'developer', 'viewer')),
    code_hash TEXT NOT NULL UNIQUE,         -- SHA-256 of the invitation code in the link
    invited_by TEXT NOT NULL,
    created_at BIGINT NOT NULL,
    expires_at BIGINT NOT NULL,             -- 7 days
    accepted_at BIGINT,
    accepted_by TEXT
);

CREATE TABLE {schema}.projects (
    id TEXT PRIMARY KEY,                    -- prj_…
    org_id TEXT NOT NULL REFERENCES {schema}.orgs (id),
    ref TEXT NOT NULL UNIQUE,               -- the Hub tenant id
    name TEXT NOT NULL,
    status TEXT NOT NULL DEFAULT 'active' CHECK (status IN ('active', 'deleted')),
    -- {default_workspace, trace_retention_days, capture_content, asks_per_minute, …}
    settings JSONB NOT NULL DEFAULT '{}',
    settings_version BIGINT NOT NULL DEFAULT 1,   -- bumped on settings or LLM changes
    created_at BIGINT NOT NULL,
    created_by TEXT NOT NULL,
    deleted_at BIGINT
);
CREATE INDEX projects_org ON {schema}.projects (org_id);

CREATE TABLE {schema}.api_keys (
    id TEXT PRIMARY KEY,                    -- key_…
    project_id TEXT NOT NULL REFERENCES {schema}.projects (id) ON DELETE CASCADE,
    name TEXT NOT NULL,
    prefix TEXT NOT NULL UNIQUE,            -- onto_sk_ + 8 characters: shown in lists and logs
    hash TEXT NOT NULL,                     -- hex SHA-256(pepper || full key)
    scopes TEXT[] NOT NULL,                 -- read | write | agent | admin
    workspaces TEXT[] NOT NULL DEFAULT '{*}',
    impersonate BOOLEAN NOT NULL DEFAULT false,
    impersonate_roles TEXT[] NOT NULL DEFAULT '{}',
    created_at BIGINT NOT NULL,
    created_by TEXT NOT NULL,
    expires_at BIGINT,
    revoked_at BIGINT,
    revoked_by TEXT,
    last_used_at BIGINT,
    rotated_from TEXT                       -- the key this one replaced
);
CREATE INDEX api_keys_project ON {schema}.api_keys (project_id);

-- Pending OIDC logins (state → verifier, nonce). Single use, 10 minutes.
CREATE TABLE {schema}.oidc_logins (
    state_hash TEXT PRIMARY KEY,
    provider TEXT NOT NULL,
    nonce TEXT NOT NULL,
    code_verifier TEXT NOT NULL,
    binding_hash TEXT NOT NULL,
    invite_code_hash TEXT,
    return_to TEXT NOT NULL,
    created_at BIGINT NOT NULL,
    expires_at BIGINT NOT NULL
);

-- One-time tickets the callback hands to the console. Single use, 60 seconds.
CREATE TABLE {schema}.login_tickets (
    ticket_hash TEXT PRIMARY KEY,
    user_id TEXT NOT NULL REFERENCES {schema}.users (id) ON DELETE CASCADE,
    binding_hash TEXT NOT NULL,
    expires_at BIGINT NOT NULL
);

-- Who changed what in the control plane. Append-only.
CREATE TABLE {schema}.audit_log (
    seq BIGSERIAL PRIMARY KEY,
    ts BIGINT NOT NULL,
    org_id TEXT,
    project_id TEXT,
    actor TEXT NOT NULL,                    -- user id, or 'system'
    action TEXT NOT NULL,                   -- key.create, key.revoke, member.role, llm.update, …
    target TEXT,
    detail JSONB NOT NULL DEFAULT '{}'      -- never secrets
);
CREATE INDEX audit_org_ts ON {schema}.audit_log (org_id, ts);
CREATE OR REPLACE FUNCTION {schema}.refuse_update() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN RAISE EXCEPTION '%.% is append-only', TG_TABLE_SCHEMA, TG_TABLE_NAME; END $$;
CREATE TRIGGER audit_append_only BEFORE UPDATE ON {schema}.audit_log
    FOR EACH ROW EXECUTE FUNCTION {schema}.refuse_update();
```

Expired `oidc_logins` and `login_tickets` rows are deleted by a sweeper
every 10 minutes.

### `0002_llm_usage.sql` (stage 3)

```sql
CREATE TABLE {schema}.project_llm (
    project_id TEXT PRIMARY KEY REFERENCES {schema}.projects (id) ON DELETE CASCADE,
    provider TEXT NOT NULL,                 -- anthropic | openai | ollama | openai-compatible | none
    model TEXT NOT NULL DEFAULT '',
    base_url TEXT,
    api_key_ct BYTEA,                       -- vault envelope (section 8.2), NULL = no key
    api_key_last4 TEXT,
    max_output_tokens INT NOT NULL DEFAULT 4096,
    temperature REAL,
    effort TEXT,
    timeout_secs INT NOT NULL DEFAULT 120,
    pricing JSONB NOT NULL DEFAULT '{}',    -- {input_per_mtok, output_per_mtok, cached_input_per_mtok}
    headers JSONB NOT NULL DEFAULT '{}',    -- non-secret extra headers
    monthly_budget_usd DOUBLE PRECISION,    -- NULL = no budget
    monthly_token_budget BIGINT,
    asks_per_minute INT,                    -- NULL = no limit
    updated_at BIGINT NOT NULL,
    updated_by TEXT NOT NULL
);

-- Usage per project, UTC day and model. Kept 400 days (budgets need the
-- whole month even when traces are kept 30 days).
CREATE TABLE {schema}.usage_daily (
    project_id TEXT NOT NULL REFERENCES {schema}.projects (id) ON DELETE CASCADE,
    day DATE NOT NULL,
    provider TEXT NOT NULL,
    model TEXT NOT NULL,
    asks BIGINT NOT NULL DEFAULT 0,
    llm_calls BIGINT NOT NULL DEFAULT 0,
    deterministic BIGINT NOT NULL DEFAULT 0,
    input_tokens BIGINT NOT NULL DEFAULT 0,
    output_tokens BIGINT NOT NULL DEFAULT 0,
    cached_tokens BIGINT NOT NULL DEFAULT 0,
    cost_usd DOUBLE PRECISION NOT NULL DEFAULT 0,
    errors BIGINT NOT NULL DEFAULT 0,
    PRIMARY KEY (project_id, day, provider, model)
);
```

### `ControlStore`

```rust
#[async_trait]
pub trait ControlStore: Send + Sync {
    // users and identities
    async fn user(&self, id: &str) -> Result<Option<User>>;
    async fn users(&self) -> Result<Vec<User>>;                                 // operator CLI
    async fn identity(&self, provider: &str, subject: &str) -> Result<Option<Identity>>;
    async fn upsert_login(&self, login: LoginUpsert) -> Result<User>;          // one transaction
    async fn sign_out_everywhere(&self, user: &str, at: i64) -> Result<()>;
    // organizations and members
    async fn create_org(&self, name: &str, slug: &str, owner: &str) -> Result<Org>;
    async fn org(&self, id_or_slug: &str) -> Result<Option<Org>>;
    async fn rename_org(&self, org: &str, name: &str) -> Result<Org>;
    async fn delete_org(&self, org: &str) -> Result<()>;                         // conflict with active projects
    async fn orgs_of(&self, user: &str) -> Result<Vec<(Org, OrgRole)>>;
    async fn role_of(&self, org: &str, user: &str) -> Result<Option<OrgRole>>;
    async fn members(&self, org: &str) -> Result<Vec<MemberView>>;
    async fn add_member(&self, org: &str, user: &str, role: OrgRole) -> Result<()>; // add or change; keeps ≥1 owner
    async fn set_role(&self, org: &str, user: &str, role: OrgRole) -> Result<()>; // keeps ≥1 owner
    async fn remove_member(&self, org: &str, user: &str) -> Result<()>;          // keeps ≥1 owner
    async fn invite(&self, inv: NewInvitation) -> Result<(Invitation, String)>;  // code shown once
    async fn invitations(&self, org: &str) -> Result<Vec<Invitation>>;           // not yet accepted
    async fn invitation_by_code(&self, code: &str) -> Result<Option<(Invitation, Org)>>; // pending, unexpired
    async fn invitation_pending(&self, verified_email: Option<&str>, code_hash: Option<&str>) -> Result<bool>;
    async fn accept_invitations(&self, user: &User, code_hash: Option<&str>) -> Result<Vec<String>>;
    async fn revoke_invitation(&self, org: &str, id: &str) -> Result<()>;
    // projects
    async fn create_project(&self, org: &str, name: &str, by: &str) -> Result<Project>;
    async fn adopt_project(&self, org: &str, tenant: &str, by: &str) -> Result<Option<Project>>; // None: already a project
    async fn project(&self, id: &str) -> Result<Option<Project>>;
    async fn project_by_ref(&self, r: &str) -> Result<Option<Project>>;
    async fn projects(&self, org: &str) -> Result<Vec<Project>>;                 // active
    async fn projects_of(&self, user: &str) -> Result<Vec<(Project, OrgRole)>>;
    async fn update_project(&self, r: &str, patch: ProjectPatch, by: &str) -> Result<Project>;
    async fn delete_project(&self, r: &str, by: &str) -> Result<()>;
    async fn membership(&self, user: &str, project_ref: &str) -> Result<Option<(Project, OrgRole)>>;
    // API keys
    async fn create_key(&self, k: NewKey) -> Result<ApiKey>;                     // hash computed by caller
    async fn key(&self, id: &str) -> Result<Option<ApiKey>>;
    async fn key_by_prefix(&self, prefix: &str) -> Result<Option<ApiKey>>;
    async fn keys(&self, project_id: &str) -> Result<Vec<ApiKey>>;
    async fn revoke_key(&self, id: &str, by: &str, at: i64) -> Result<()>;
    async fn expire_key(&self, id: &str, at: i64) -> Result<()>;                // rotation grace; only moves earlier
    async fn touch_keys(&self, used: &[(String, i64)]) -> Result<()>;            // batched last_used_at
    // OIDC state
    async fn put_login(&self, l: PendingLogin) -> Result<()>;
    async fn take_login(&self, state_hash: &str, now: i64) -> Result<Option<PendingLogin>>; // DELETE … RETURNING
    async fn put_ticket(&self, t: LoginTicket) -> Result<()>;
    async fn take_ticket(&self, ticket_hash: &str, now: i64) -> Result<Option<LoginTicket>>;
    async fn sweep(&self, now: i64) -> Result<u64>;
    // LLM settings and usage (stage 3)
    async fn llm(&self, project_id: &str) -> Result<Option<ProjectLlm>>;
    async fn llms(&self) -> Result<Vec<ProjectLlm>>;                            // operator: master key rotation
    async fn put_llm(&self, s: ProjectLlm) -> Result<()>;                       // upsert + settings_version += 1
    async fn delete_llm(&self, project_id: &str) -> Result<bool>;               // had settings; then settings_version += 1
    async fn replace_llm_key(&self, project_id: &str, old: &Sealed, new: &Sealed) -> Result<bool>; // rewrap, compare-and-swap
    async fn add_usage(&self, rows: &[UsageDelta]) -> Result<()>;               // INSERT … ON CONFLICT DO UPDATE +=
    async fn usage(&self, project_id: &str, from_day: &str, to_day: &str) -> Result<Vec<UsageDay>>;
    async fn settings_versions(&self) -> Result<Vec<(String, i64)>>;            // (ref, settings_version), active projects
    // audit
    async fn audit(&self, e: AuditEntry) -> Result<()>;
    async fn audit_log(&self, org: &str, before: Option<i64>, limit: usize) -> Result<Vec<AuditEntry>>;
}
```

Semantics the shared test body checks on both stores:

* `upsert_login` keeps an identity's user and updates its claims and
  `last_login_at`; an email, when given, replaces the user's email and its
  `email_verified`. Concurrent first logins of one identity are serialized
  with an advisory lock.
* `accept_invitations` marks every pending, unexpired invitation whose email
  equals the user's email (case-insensitive, only when `email_verified`),
  and the one whose code hash is `code_hash` (whatever its email), as
  accepted by the user, then adds the memberships (an existing membership
  keeps its role). Each invitation is accepted once; it returns the
  organizations joined. It takes the hash, not the code, because the
  pending OIDC login stores only the hash of the code from the invitation
  link. `invitation_pending` answers whether such an invitation exists
  without accepting it (the closed sign-up check of section 7.4).
* `update_project` merges `settings` into the stored object (a `null` value
  removes a key) and bumps `settings_version` only when settings change.
* `delete_org` refuses an organization with active projects; its deleted
  projects (and their keys, by `ON DELETE CASCADE`) are deleted with it.
* Invitation codes are 24 random bytes, base64url; only their SHA-256 is
  stored.
* `put_llm` and `delete_llm` bump the project's `settings_version` in the
  same transaction; `replace_llm_key` (a rewrap) does not, since the key
  itself is unchanged. `add_usage` merges deltas with the same `(project,
  day, provider, model)` first (one statement may not update a row twice)
  and skips rows of projects that no longer exist. `sweep` also deletes
  `usage_daily` rows older than 400 days. `UsageDelta` is the same type as
  `UsageDay`; `ProjectLlm.api_key_ct` is a `Sealed` (its `Debug` shows only
  the length).


`create_project` generates the ref (20 characters from `a-z0-9`, CSPRNG,
rejection-sampled so every character is equally likely), inserts the row,
and the console route then opens the tenant (`hub.tenant(ref)`, which runs
the tenant migrations). If opening fails the project is marked deleted and
the error returned. `delete_project` marks it `deleted`; the owner-only
route then evicts the tenant from the Hub and drops `onto_<ref>`
(`Hub::drop_tenant`: `DROP SCHEMA … CASCADE`; in memory the store goes with
its last reference) after the caller retypes the project name. The project
row stays (status `deleted`) until its organization is deleted. Other
replicas close the tenant on their next `settings_version` poll (within
30 s: a project that is no longer active is evicted), and none opens it
again: `ControlHooks::settings` answers `not_found` for the tenant of a
deleted project, so a request in that window cannot recreate its schema
(a tenant no project has, such as a config tenant, gets the defaults).

A user may belong to several organizations. Every organization keeps at
least one owner: demoting or removing the last owner is a `conflict`.

## 5. Roles and principals

### Organization roles

| Capability | owner | admin | developer | viewer |
|---|:-:|:-:|:-:|:-:|
| See projects, overview, traces, sessions, usage, connect snippets | ✓ | ✓ | ✓ | ✓ |
| Score traces (feedback) | ✓ | ✓ | ✓ | |
| Create projects, edit project settings | ✓ | ✓ | | |
| Delete a project | ✓ | | | |
| Create, rotate, revoke API keys with `read`, `write`, `agent` | ✓ | ✓ | ✓ | |
| … with `admin` scope, or impersonation | ✓ | ✓ | | |
| See LLM settings (never the key), test the connection | ✓ | ✓ | ✓ | ✓ (no test) |
| Change LLM settings, budgets, limits | ✓ | ✓ | | |
| Invite members, change roles below owner, remove members | ✓ | ✓ | | |
| Make or remove owners, rename or delete the organization | ✓ | | | |
| Read the control-plane audit log | ✓ | ✓ | | |

A developer may rotate or revoke only keys they created, and never a key
with the `admin` scope or impersonation (one they made while they were an
admin, for example); admins and owners manage every key. Any member may
leave an organization (`DELETE …/members/{own id}`), except its last owner.
Only owners make, demote or remove owners. A user who is not a member of an
organization gets 403 on its routes; an unknown organization or project is
404. Organization routes accept the id or the slug.

### Principals in a project

The data plane keeps today's model: each request has one `Principal`
(roles, workspaces, entity) and the tenant it addresses. The control plane
maps its identities onto it:

| Caller | Principal id | Roles | Workspaces |
|---|---|---|---|
| Console member, owner or admin | `user:<user_id>` | `admin` | `*` |
| Console member, developer | `user:<user_id>` | `member`, `developer` | `*` |
| Console member, viewer | `user:<user_id>` | `viewer`, `observer` | `*` |
| API key | `key:<prefix>` | union of scopes: `read`→`viewer`, `write`→`member`, `agent`→`agent`, `admin`→`admin` | the key's `workspaces` |
| API key with `impersonate` acting for a user | the `x-onto-act-as` value, `via = key:<prefix>` | from `x-onto-act-roles`, limited to `impersonate_roles` ⊆ the key's roles | ⊆ the key's |
| Config token, CLI, dev | unchanged | unchanged | unchanged |

`developer` and `observer` are custom roles (`Role::Custom`). Reading
traces, sessions and metrics needs `Principal::can_observe()`: `admin`,
`system`, `developer` or `observer`. An API key therefore reads traces only
with the `admin` scope. Governance (`[governance]`) applies as before: by
default admins apply schema changes, members and agents propose.

## 6. API keys

### Format

```text
onto_sk_7f3k9q2m_Vb1wQ9…(43 characters, base64url of 32 random bytes)
└──────┬───────┘ └───────────────┬───────────────────────────────┘
   prefix (stored, shown)                secret (never stored)
```

* `prefix` = `onto_sk_` + 8 characters of `[a-z0-9]`, unique (retry on
  collision). Lists, logs, traces and the audit log show only the prefix.
  The fixed `onto_sk_` start lets secret scanners recognise leaked keys.
* `hash` = lowercase hex of `SHA-256(pepper || full_key)`; the pepper is
  `ONTO_KEY_PEPPER` or derived from `ONTO_MASTER_KEY` (section 3).
* The full key is returned once, by the create and rotate routes. It cannot
  be shown again.

### Authentication

`Auth::authenticate` becomes `async fn authenticate(&self, h: &HeaderMap)
-> Result<Caller, AuthError>` (both callers, `http::caller` and
`DynamicGrpc::call`, are already async). The presented credential comes from
`authorization: Bearer …` or `x-api-key` as today, and is classified by its
form:

1. `onto1.` → session token (section 7.5).
2. `onto_sk_` → API key:
   1. Parse `prefix` and `secret`; malformed → 401 `invalid API key`.
   2. Look up `prefix` in the key cache (TTL 10 s; misses cached 5 s), else
      `key_by_prefix`. Unknown → 401.
   3. Compute `SHA-256(pepper || key)` and compare with the stored hash in
      constant time. Mismatch → 401 (same message as unknown).
   4. Refuse revoked keys, expired keys, keys of deleted projects (401).
   5. `x-onto-tenant`, when sent, must equal the key's project ref (403).
   6. Build the principal (section 5); apply `x-onto-act-*` if the key may
      impersonate.
   7. Record `(key_id, now)` in an in-memory set; a task writes
      `last_used_at` with `touch_keys` at most once a minute per key.
3. Anything else → config tokens (constant-time comparison against each, as
   today), then dev mode.

Revocation evicts the key from this replica's cache at once; other replicas
stop accepting it within the 10-second TTL. Rotation creates a new key with
the same name, scopes and workspaces (`rotated_from` set) and revokes the
old one, either immediately or after a grace period (`grace_secs`, up to
86 400, stored as the old key's `expires_at`).

`Caller` gains `credential: Credential` (`Config`, `ApiKey { id, prefix,
project }`, `Console { user, role, org }`, `Session`, `Dev`), used by traces
(`api_key` column), by `whoami` and by the project-call guard (section 14).
`AuthError` gains `Unavailable` (the control store could not be reached):
HTTP 503, gRPC `UNAVAILABLE`.

A key whose x-onto-tenant is absent addresses its own project. `whoami`
shows a project credential only its own tenant in `tenants`.

Sessions minted with a key (`POST /v1/auth/session`) are the key's
delegates: `SessionClaims.key` holds the key's prefix, the session is bound
to the key's project, its credential is the key's (so the project-call
guard applies), and it is refused as soon as the key is revoked, expires or
its project is deleted (checked through the key cache, without the secret,
since the session's HMAC already proves it). Console sessions cannot mint
sessions (403).

Validation when a key is created: a name of 1 to 100 printable characters,
at least one scope, workspaces non-empty names without commas (default
`["*"]`), `expires_at` (Unix ms) in the future, `impersonate_roles` only
with `impersonate = true` and not empty then, each one a role of the key's
scopes. Without a pepper the route answers `400 invalid: neither
ONTO_KEY_PEPPER nor ONTO_MASTER_KEY is set; refusing to create API keys`.
A prefix collision retries with a new prefix (at most 5 times). Rotation
keeps `expires_at`; rotating a revoked or expired key is a conflict.

## 7. Console sign-in

### 7.1 Providers

Any OpenID Connect provider that supports the authorization-code flow with
PKCE works: Clerk (OAuth applications), Auth0, Supabase Auth (OAuth 2.1
server), Google, Keycloak, Okta, Microsoft Entra. Each `[[console.oidc]]`
entry names an issuer and a client; the server is the OIDC client and does
the token exchange itself, so the browser never talks to the IdP's token
endpoint and the CSP stays `connect-src 'self'`.

`oidc.rs`:

* **Discovery**: `GET {issuer}/.well-known/openid-configuration` (issuer
  without a trailing slash, path kept), cached 1 hour. The document's
  `issuer` must equal the configured issuer (after trimming one trailing
  slash on both); `authorization_endpoint`, `token_endpoint` and `jwks_uri`
  are required and must be `https` unless the host is loopback.
  `code_challenge_methods_supported`, when present, must include `S256`.
* **JWKS**: `GET jwks_uri`, cached 1 hour; on an unknown `kid` refetched at
  most once a minute.
* **HTTP client**: `reqwest`, 10-second timeout, no redirects followed for
  the token endpoint, 1 MB response limit.

### 7.2 Flow

```text
 console tab                        onto server                                 IdP
 ───────────                        ───────────                                 ───
 binding = random 32 B (sessionStorage "onto.login")
 GET /v1/console/auth/oidc/{id}/start?binding=b64url(SHA-256(binding))&return_to=#/overview
                                    state, nonce, verifier = random 32 B each
                                    put_login(SHA-256(state), id, nonce, verifier,
                                              binding_hash, return_to, +10 min)
                                    302 → authorization_endpoint?response_type=code
                                          &client_id&redirect_uri&scope&state&nonce
                                          &code_challenge=b64url(SHA-256(verifier))
                                          &code_challenge_method=S256
                                                                   ───────────►  user signs in
                                    GET /v1/console/auth/oidc/callback?code&state ◄──────────
                                    take_login(SHA-256(state))   (single use, not expired)
                                    POST token_endpoint: grant_type=authorization_code, code,
                                      redirect_uri, code_verifier, client_id
                                      (+ client_secret_basic or _post, per discovery)
                                    verify id_token (7.3); upsert user (7.4)
                                    ticket = random 32 B; put_ticket(SHA-256(ticket), user,
                                                                     binding_hash, +60 s)
                                    302 → {public_url}/ui/#/auth/callback?ticket=…&return_to=…
 POST /v1/console/auth/exchange {ticket, binding: b64url(binding)}
                                    take_ticket; b64url(SHA-256(binding)) must equal
                                    binding_hash (constant time)
                                    → { token: onto1…, expires_at, user }
 sessionStorage: token; ticket removed from the address
```

Why each piece:

* **state** ties the callback to a login this server started; it is stored
  hashed and consumed once.
* **nonce** ties the ID token to that login (replay of another ID token
  fails).
* **PKCE (S256)** makes a stolen authorization code useless without the
  verifier, which never leaves the server.
* **binding** prevents login CSRF: an attacker who completes a login with
  their own account and sends the victim the callback link produces a
  ticket the victim's tab cannot exchange, because the victim's
  `sessionStorage` does not hold the attacker's binding.
* **ticket in the fragment** keeps the session token out of server logs,
  `Referer` headers and browser history; the fragment never reaches a server.
* **No cookies**: the console sends the session as a bearer header, so there
  is no ambient credential and no CSRF on API calls.

IdP errors (`?error=access_denied&state=…`) redirect to
`/ui/#/auth/callback?error=…` with a short message; the state is consumed.
`return_to` must start with `#/` (no open redirect).

As built:

* `binding` in `start` is base64url (no padding) of the SHA-256 of the
  tab's 32 random bytes, so exactly 43 characters that decode to 32 bytes;
  in `exchange` it is base64url of the 32 bytes themselves. `state`, `nonce`
  and the PKCE verifier are base64url of 32 random bytes (the verifier is
  43 characters, within PKCE's 43 to 128). `return_to` defaults to `#/` and
  is at most 512 characters without spaces or control characters; `invite`
  is at most 256 characters.
* The callback always answers 302 (`cache-control: no-store`,
  `referrer-policy: no-referrer`) to `{public_url}/ui/#/auth/callback`. On
  success the fragment carries `ticket` and the login's `return_to`
  (URL-encoded), since the ticket row has no `return_to`. On failure it
  carries `error` and a short `error_description` (at most 200
  characters): the IdP's own error code reduced to `[a-z0-9_]`, or one of
  `invalid_state` (unknown, expired or used state), `invalid_request` (no
  code), `idp_unavailable` (discovery, JWKS, token or userinfo endpoint
  unreachable or answering 5xx), `token_rejected` (the token endpoint
  refused the code, or the ID token or userinfo failed a check of 7.3),
  `signup_closed`, `domain_not_allowed`, `user_disabled`, `server_error`.
  The reason is also logged.
* `exchange` consumes the ticket before it compares the binding, so a
  wrong binding spends the ticket. An unknown, used or expired ticket, a
  wrong binding and a disabled user are all 401.
* The token request always sends `client_id` in the body. With a secret it
  uses `client_secret_basic` when discovery lists it (or lists no methods,
  the OIDC default), else `client_secret_post`; client id and secret are
  form-encoded before the Basic header is built (RFC 6749 section 2.3.1).
  A 3xx answer is an error (redirects are not followed); a 4xx is
  `token_rejected`, a 5xx `idp_unavailable`. Userinfo is fetched without
  following redirects too; discovery and JWKS follow at most 3.
* The JWKS refetch on an unknown `kid` is limited separately from the
  hourly cache: at most one refetch per minute per provider, counted from
  the last refetch, so the first unknown `kid` after a normal fetch does
  refetch. JWKS entries that are not keys this code reads are skipped.

### 7.3 ID token verification

1. Split the compact JWS; reject anything other than three base64url parts.
2. Header: `alg` ∈ {`RS256`, `RS384`, `RS512`, `PS256`, `PS384`, `PS512`,
   `ES256`, `ES384`, `EdDSA`}; `none` and `HS*` are refused; a `crit` header
   is refused.
3. Key: the JWK with the header's `kid` (or, without `kid`, the only key of
   the matching `kty`); its `use`, when present, must be `sig`, its `alg`,
   when present, must equal the header's.
4. Signature, with `aws-lc-rs`: RSA from `n`/`e`
   (`RsaPublicKeyComponents`, `RSA_PKCS1_2048_8192_SHA*` / `RSA_PSS_*`),
   ECDSA from `x`/`y` (`UnparsedPublicKey` over `0x04‖x‖y`,
   `ECDSA_P256_SHA256_FIXED`, `ECDSA_P384_SHA384_FIXED`), Ed25519 from `x`.
5. Claims: `iss` equals the discovered issuer exactly; `aud` (string or
   array) contains `client_id` or a configured audience, and when `aud` has
   more than one value, `azp` equals `client_id` (an `azp` that is present
   must equal it in any case); `exp` > now − 60 s; `iat` < now + 60 s
   (both are required); `nonce` equals the stored nonce; `sub` is
   non-empty.
6. Email: `email` is trusted for invitations and domain checks only when
   `email_verified` is `true` (the JSON value `true`, or the string
   `"true"`, which some IdPs send). When the ID token lacks `email`, call the
   `userinfo_endpoint` with the access token and require its `sub` to equal
   the ID token's; without a userinfo endpoint or access token the user
   signs in without an email.

The identity keeps only profile claims (`email`, `email_verified`, `name`,
`picture`, `preferred_username`), never the token. The user's name is
`name`, else `given_name family_name`, else `preferred_username`.

### 7.4 Users, organizations, invitations

On a verified login:

1. Find the identity `(provider, sub)`. If it exists, update
   `last_login_at` and the profile claims; the user must not be disabled.
2. Otherwise, if `allow_signup` is false and no pending invitation matches
   the verified email (or the invitation code from the link), refuse
   (`/ui/#/auth/callback?error=signup_closed`). Enforce
   `allowed_email_domains` on the verified email (`error=domain_not_allowed`;
   an unverified or missing email never passes). The domain rule applies to
   every new identity, invited or not; a configured domain may carry a
   leading `@` and is compared case-insensitively.
3. Create the user and the identity. Accounts are **not** linked across
   providers by email (one person signing in with two IdPs gets two users).
   An invitation code does not link identities either: it makes the new
   user a member of the inviting organization (step 4) whatever the email.
4. Accept invitations: every pending, unexpired invitation whose email
   equals the verified email (case-insensitive), plus the one named by the
   invitation code carried through `start?invite=` (stored hashed in the
   pending login).
5. If the user now has no membership and `create_org_on_signup`, create an
   organization named after the user (`"<name>'s team"`, slug from the email
   local part plus 4 random characters) with them as owner, and an empty
   project `"My first project"`.

As built: steps 4 and 5 run on every sign-in, not only the first, so an
existing user who opens an invitation link joins that organization, and a
user left without any membership gets a new organization of their own.
The name falls back to the email's local part, then to `My team`; the slug
base to the slugified name. The first project's tenant is opened at once
(through the console's `hub.tenant`); if that fails the project is marked
deleted and the sign-in still succeeds. A disabled user is refused
(`error=user_disabled`) after the login is recorded.

There is no email sending: an invitation returns a link
(`{public_url}/ui/#/invite?code=…`) for the inviter to share. The invite page
shows the organization name and starts sign-in with the code.

### 7.5 Console sessions

`SessionClaims` gains an optional field, so existing tokens stay valid:

```rust
pub struct SessionClaims {
    pub principal: Principal,
    pub tenant: Option<String>,
    pub exp: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub console: Option<ConsoleClaims>,   // { user: String, iat: u64 (Unix ms) }
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,              // prefix of the API key that minted it (section 6)
}
```

`iat` is in milliseconds, the unit of `sessions_valid_after`, so a session
issued right after "sign out everywhere" is not refused for the rest of
that second.

A console session is minted with `principal = user:<id>` (no roles) and no
tenant, lifetime `console.session_ttl_secs` (capped by
`auth.session_max_ttl_secs`). For each request it is resolved like this:

1. Verify the HMAC and expiry (as today).
2. Load the user (cached 30 s): it must exist, not be disabled, and
   `iat >= sessions_valid_after`.
3. Console routes (`/v1/console/…`) authorize against the organization or
   project they name, with the role table in section 5.
4. Data-plane routes need `x-onto-tenant` naming a project ref; the
   membership `(user, ref)` (cached 30 s, evicted locally on change) gives
   the role, and the principal follows section 5. No membership → 403. A
   console session cannot impersonate.

`POST /v1/console/auth/refresh` returns a fresh token while the user is
still valid (the UI refreshes when less than a quarter of the lifetime is
left). `POST /v1/console/auth/logout {everywhere?}` sets
`sessions_valid_after = now` when `everywhere` is true and evicts the user
from this replica's cache (other replicas within 30 s); otherwise the tab
just drops its token.

Console routes read roles from the store on every request (they are
management calls); the 30-second caches serve data-plane requests. Every
change made through the console on a replica evicts what it affects there:
the user (sign-in, logout), the organization's memberships (role changes,
removals, deletion), the project's memberships and keys (deletion).

Bootstrap and dev sign-ins return `{token, expires_at, user, org}`; refresh
returns `{token, expires_at, user}`.

### 7.6 Self-hosting without an IdP

* **Bootstrap admin token**: `POST /v1/console/auth/token` with
  `authorization: Bearer $ONTO_ADMIN_TOKEN` (constant-time comparison, at
  least 32 characters). It signs in as the local administrator (identity
  `local/admin`, user name "Administrator"). On first use it creates the
  organization `default`, makes the administrator its owner, and adopts the
  tenants that already exist (`hub.known_tenants()`) as projects of that
  organization with `ref = tenant id` (refs of adopted projects keep their
  existing names). Later sign-ins adopt tenants that appeared since. The
  sign-in page shows a token form when the server reports `bootstrap: true`.
* **Dev mode** (`onto serve --dev`, loopback only, as today):
  `POST /v1/console/auth/dev` signs in as `dev/dev` without credentials,
  owner of the organization `dev`, which adopts existing tenants the same
  way. The UI signs in automatically when `dev_mode` is reported.
* **CLI** (operator, opens the database directly):
  `onto console bootstrap` (create the default organization and adopt
  tenants), `onto console users`, `onto console add-owner <user-id> <org>`,
  `onto console keys <project-ref> [--create NAME --scope … ] [--json]`
  (scope `read` when none is given; the global `--workspace` limits the key
  to one workspace; the secret is printed once), and from stage 3
  `onto console rotate-master-key`. They refuse `engine.storage = "memory"`
  (that control store lives inside `onto serve` only). Changes the CLI
  makes are audited with the actor `system`.

## 8. Bring your own LLM

### 8.1 Settings

Each project may override the server's `[llm]` provider:

| Field | Notes |
|---|---|
| `provider` | `anthropic`, `openai`, `ollama`, `openai-compatible`, `none`. (`mock` only in dev mode.) |
| `model` | Empty = provider default. At most 200 characters. |
| `base_url` | Required for `openai-compatible` and `ollama`, optional for the others (a proxy); `https` required unless `console.allow_private_urls` (section 14). No credentials, query or fragment. |
| `api_key` | Write-only. Stored encrypted; responses show `api_key_last4` (keys of 12 characters or more) and `has_api_key` only. Required for `openai` and `anthropic`. |
| `max_output_tokens`, `temperature`, `effort`, `timeout_secs`, `headers` | As `[llm]` (`max_output_tokens` 1 to 1,000,000, caps each request; `temperature` 0 to 2; `effort` low, medium, high, xhigh, max; `timeout_secs` 1 to 600). `headers`: at most 20, names stored in lowercase, and names that carry credentials are refused: `authorization`, `proxy-authorization`, `x-api-key`, `api-key`, `cookie`, and any name with a part (between `-`, `_` or `.`) that is `key`, `token`, `secret`, `password`, `passwd`, `credential(s)`, `cookie` or `signature`, starts with `auth`, or ends with `token`, `secret`, `password` or `apikey` (`x-goog-api-key`, `helicone-auth`, `x-portkey-api-key`). Header values are stored in plaintext and every project member can read them; `GET …/llm` shows `***` for a credential header stored before this rule. |
| `pricing` | USD per million input, output and cached input tokens; empty uses the provider's built-in list prices where they exist (Anthropic today), else cost is 0. |
| `monthly_budget_usd`, `monthly_token_budget` | Per calendar month, UTC. The token budget counts input plus output tokens. |
| `asks_per_minute` | Per project, per replica. Without it, `asks_per_minute` in the project settings (`PATCH …/projects/{ref}`) applies. |

`PUT …/llm` replaces the settings (fields left out take their defaults:
4096 output tokens, 120 s) except the key: a body without `api_key` keeps
the stored key, `"api_key": ""` removes it. A stored key stays only with the
provider and base URL it was entered for: a `PUT` that changes either
without sending `api_key` again is refused (400), so whoever changes the
base URL never receives a key someone else entered. Unknown fields are
refused (400). The new settings must build a provider before they are
stored.

The project's `LlmConfig` is built from these fields with the decrypted key
in the new `LlmConfig.api_key: Option<Secret>` (`#[serde(skip)]`, `Debug`
prints `Secret(****)`); providers use it before `api_key_env`. A project can
never name an environment variable for its key: that would let a project
admin send any server secret (`ONTO_MASTER_KEY`, database URL) to a base URL
of their choosing. A body with `api_key_env` is refused (400), and a
project's `LlmConfig` has `api_key_env = ""`, which reads no variable at all
(not even the provider's conventional one, such as `OPENAI_API_KEY`).

### 8.2 Encryption at rest (`vault.rs`)

`ONTO_MASTER_KEY` is the key-encryption key (KEK). Each secret gets its own
data key (DEK):

```text
blob = 0x01                              version
     ‖ kek_id[8]                         first 8 bytes of SHA-256(KEK)
     ‖ nonce_k[12] ‖ AES-256-GCM(KEK, nonce_k, DEK[32],  aad = "onto/dek/v1/" ‖ project_id)   (48 bytes)
     ‖ nonce_d[12] ‖ AES-256-GCM(DEK, nonce_d, secret,   aad = "onto/llm-key/v1/" ‖ project_id)
```

* AES-256-GCM is `aws_lc_rs::aead::LessSafeKey` with `AES_256_GCM`. Nonces
  and DEKs come from the CSPRNG (`aws_lc_rs::rand::SystemRandom`).
* The AAD binds a ciphertext to its project and purpose: a blob copied to
  another project's row does not decrypt.
* `kek_id` picks `ONTO_MASTER_KEY` or `ONTO_MASTER_KEY_PREVIOUS` (decryption
  only). Both are 32 bytes, as base64 (standard or URL-safe, padded or not)
  or 64 hex characters; a malformed value refuses to start.
  `onto console rotate-master-key` rewraps every DEK under the current KEK
  (the secrets themselves are not re-encrypted; each row is replaced only if
  it still holds the blob that was rewrapped) and reports how many rows it
  changed, how many already were current, and the ones that failed to
  decrypt (then it exits non-zero). Rotation: set the new key as
  `ONTO_MASTER_KEY` and the old one as `ONTO_MASTER_KEY_PREVIOUS` on every
  replica, run the command, then remove the previous key.
* Without `ONTO_MASTER_KEY`, `PUT …/llm` with an `api_key` is refused
  (`400 invalid: ONTO_MASTER_KEY is not set; refusing to store LLM API
  keys`); settings without a key (Ollama, `none`) still work. `GET …/llm`
  answers `master_key: false`. A blob whose `kek_id` matches no configured
  key, or that does not authenticate, fails to decrypt: the project falls
  back to the server default provider, the error is logged, and its LLM
  page shows it (`error` in `GET …/llm`).
* Plaintext keys live only in the provider object and are never logged,
  traced, audited or returned.

### 8.3 Per-project providers in the Hub

```rust
pub struct TenantSettings {
    pub llm: Option<Arc<dyn LlmProvider>>,   // None: the server default
    pub limits: Limits,
    pub meter: Option<Arc<dyn UsageMeter>>,
    pub version: u64,                        // projects.settings_version
    pub traces: TraceSettings,               // stage 4: retention_days, capture_content (None: [traces])
}

#[async_trait]
pub trait TenantHooks: Send + Sync {
    /// Settings for a tenant; called when it opens and on refresh.
    async fn settings(&self, tenant: &str) -> Result<TenantSettings>;
}
```

* `Hub::set_hooks(Arc<dyn TenantHooks>)`; `Hub::open_tenant` calls
  `settings` and passes the provider to `Intelli::open_with_store`, then
  `set_limits` and `set_meter`. A failing `settings` fails the open (the
  next request tries again). `Hub::default_llm()` is the server's provider.
* `onto_grpc::router_hub` calls `onto_control::hooks::install(control, hub,
  dev)` when a control plane is given: it sets `ControlHooks` on the Hub,
  refreshes the tenants already open, and starts (once per `Control`) the
  task of section 8.5 that also polls `settings_version`.
* `Intelli.llm` is a `parking_lot::RwLock<Arc<dyn LlmProvider>>`;
  `Intelli::llm()` returns a clone of the `Arc`; `set_llm` swaps it and
  clears the in-memory completion cache and the answer cache (answers carry
  the provider that made them). A question uses one provider from start to
  end even if it is swapped meanwhile.
* `Hub::refresh_tenant(t)` re-reads settings for an open tenant (false when
  it is not open). The console calls it after every LLM change and every
  project settings change on the same replica. Every replica also polls
  `SELECT ref, settings_version FROM projects` (`settings_versions`) every
  30 s and refreshes open tenants whose version moved since their settings
  were read (`Control::poll_settings`), so other replicas pick up a change
  within 30 s.
* A project without `project_llm` uses the server's `[llm]` provider. A
  tenant that is not a project gets `TenantSettings::default()` (no limits,
  no meter).
* Project providers run on the SSRF-guarded client of section 14
  (`onto_control::net::client`, with the project's `timeout_secs`), built
  with `llm::build_with_client`.

### 8.4 Test connection

`POST /v1/console/projects/{ref}/llm/test` with optional draft settings
builds the provider and sends one completion: system `"Reply with the
single word OK."`, user `"ping"`, `max_tokens = 8`. Without a body (or with
`{}`) it tests the stored settings, or the server's provider when there are
none. A draft is validated like `PUT …/llm`; the stored key is used when the
draft has none and has the stored provider and base URL (otherwise no key
is sent). It returns `{ ok, latency_ms, provider, model, response_model,
usage, error }` (200 whether or not the provider answered). Error text is
the provider's status and at most 200 characters of its message; response
bodies are never echoed in full, and request URLs are left out of transport
errors. It does not count against the budget and is not metered. Rate
limited to 10 per minute per project (429 with `retry-after: 60`).

### 8.5 Budgets and rate limits (`limits.rs`)

* **Rate limit**: a token bucket per tenant (`asks_per_minute`, burst equal
  to the per-minute rate; a new rate starts with a full bucket) taken at the
  start of `intelli.ask`, `intelli.ask_stream`, `ontology.refine` and
  `ontology.advise`. Empty → `Error::Limited("asks_per_minute (60) reached;
  retry in 3 s")`: HTTP 429 with `retry-after` (the seconds in the message),
  gRPC `RESOURCE_EXHAUSTED`. Streaming routes take the token before the
  stream starts (`Intelli::try_ask_stream`), so they too answer 429 or
  `RESOURCE_EXHAUSTED`; `Intelli::ask_stream` turns a refusal into one
  `error` event. Limits are per replica.
* **Budget**: `Intelli::complete` asks the meter for month-to-date cost and
  tokens before calling the provider (after the completion cache, so cached
  completions are still served). At or over budget, the LLM step is
  skipped: deterministic answers still work, LLM questions get the
  `Fallback` mode with `note = "monthly LLM budget of $50.00 reached"` (or
  `"monthly LLM token budget of N tokens reached"`), and `ontology.refine`
  and `ontology.advise` return their deterministic result with that note.
  The trace records it: `budget_exceeded: true` in the trace's `metadata`
  and in the generation span's attributes (section 9.2).
* **Meter** (`onto-control/hooks.rs`): keeps month-to-date totals per
  project in memory, loaded from `usage_daily` when the tenant opens and
  re-read every 60 s; `record` adds to the totals and queues a `UsageDelta`
  (project, day, provider, model, counts) flushed with `add_usage` every
  10 s (a failed write stays queued). Overshoot is bounded by the calls in
  flight plus 60 s of other replicas' traffic; usage queued when the process
  stops (at most 10 s) is lost.
* **Counters**: each question (`intelli.ask`, answer cache hits included) and
  each `ontology.refine` or `ontology.advise` adds one `asks`; `llm_calls`
  counts provider calls (calls that reported tokens or failed),
  `deterministic` the questions answered without the LLM step, `errors` the
  failed provider calls; tokens and `cost_usd` come from the provider's
  usage. Budget-skipped questions are asks without a call or an error.

## 9. Traces

### 9.1 Model

Following Langfuse: a **trace** is one unit of work; **spans** are its
observations, typed `span`, `generation` (an LLM call), `retrieval`, `tool`
(a governed action or engine call) or `event` (a point in time); **scores**
attach evaluations and feedback to a trace; a **session** is every trace
with the same `session_id`.

Ids follow W3C Trace Context so they join OpenTelemetry traces: `trace_id`
32 lowercase hex characters, `span_id` 16. Every trace gets a new id of its
own. An incoming `traceparent` header is kept on the trace as
`otel_trace_id` (the caller's trace id) and `parent_span` (the caller's
span, also the parent of the root span): one upstream request often makes
several calls (an ask, then an action; every `tools/call` of an MCP batch),
and each is a trace of its own that `GET /v1/traces?otel_trace_id=…` finds
together.

Spans of one ask:

```text
intelli.ask                                  span        workspace, intent, mode, revision
├─ cache.answer                              event       hit | miss
├─ intent                                    span        intent, why, action
├─ resolve                                   span        entities, cache hit
├─ command | clarify | deterministic         span        when taken
├─ retrieve                                  retrieval   memories, facts used/dropped,
│                                                        context_tokens, naive_tokens, saved_tokens
├─ llm                                       generation  provider, model, input/output/cached tokens,
│                                                        cost_usd, latency_ms, ttft_ms, finish_reason,
│                                                        completion cache hit|miss, budget_exceeded, error
└─ remember                                  span        conversation turn, episode, audit event
```

Governed actions (`action.apply`, `action.undo`) become a trace named
`action.apply:<type>` with a `tool` span carrying the action type, risk,
receipt id, number of edits and outcome. Other engine calls become a trace
named after the action with one `tool` span, as `[traces] calls` selects.
LLM-using actions (`ontology.refine`, `ontology.advise`) get their
`generation` spans the same way as asks. `eval.run` makes one trace per case
with a score.

### 9.2 Recording (`onto-intelli/src/trace.rs`)

```rust
pub struct TraceCtx {
    pub otel_trace_id: Option<String>,     // from traceparent: stored, never the trace's own id
    pub parent_span: Option<String>,
    pub session_id: Option<String>,        // x-onto-session-id, or AskRequest.trace.session_id
    pub user_id: Option<String>,           // AskRequest.trace.user_id, else principal.entity, else principal.id
    pub source: &'static str,              // http | grpc | mcp | cli | ffi | sdk | scheduler
    pub api_key: Option<String>,           // prefix
    pub tags: Vec<String>,
    pub metadata: Value,                   // object, ≤ 4 KB
    pub name: Option<String>,
}
```

* `Intelli::ask_traced(p, req, ctx)` and `Intelli::call_traced(p, action,
  input, ctx)`; `ask` and `call` pass `TraceCtx::default()` with `source =
  "ffi"` / `"cli"` as their callers set. `AskRequest` gains an optional
  `trace: { session_id, user_id, tags, metadata, name }` object so SDKs set
  them without headers.
* A `Tracer` (spans with start offsets, attributes, status) is placed in a
  `tokio::task_local!` for the duration of the call; `complete()` and the
  action code add spans to the current tracer when there is one, so nested
  LLM calls land in the right trace. `ask_stream` sets the scope inside its
  spawned task.
* `conversation_id` becomes the session id when no session id is given.
* Finishing produces a `TraceRecord { trace: TraceRow, spans: Vec<SpanRow> }`
  sent to the tenant's `TraceSink`: a bounded `mpsc` (`[traces] queue`) read
  by one task that writes batches (`flush_ms` / `flush_records`) with
  `Store::trace_write` inside `spawn_blocking`. A full queue drops the
  record and counts it (`ledger.traces_dropped`); a failed write is logged
  and retried once. Recording never adds latency to the request beyond
  building the record. `Intelli::flush_traces().await` waits for the queue
  (tests, `onto eval`, shutdown).
* Content: with `capture_content`, the question, the prompt messages
  (system prompt as its hash plus length; it is the same for every call) and
  the answer are stored, each clipped to `max_content_bytes` at a character
  boundary. Without it, only lengths and token counts. Facts are never
  stored, only their count.
* `AskResponse` gains `trace_id`; HTTP responses add `x-onto-trace-id`.

As built:

* `ask_traced` and `call_traced` return `Traced<T> { result, trace_id }`
  (`trace_id` is `None` when the call was not traced), so callers can name
  the trace of a failed call too. `try_ask_stream_traced` returns the
  receiver and the trace id, known before the stream starts. `TraceCtx`
  defaults to `source = "sdk"`; `BlockingIntelli` (the FFI) passes `ffi`,
  `McpServer` `mcp`, the CLI's local backend `cli`, HTTP `http`, gRPC
  `grpc`. Rate-limited refusals (`Error::Limited` from `take_ask`) are not
  traced.
* `TraceCtx` from the caller wins over `AskRequest.trace` for the session
  and user; tags are added (at most 20 of 64 characters, deduplicated),
  metadata objects merged (larger than 4 KB serialized: `{"truncated":
  true}`), `AskRequest.trace.name` names the trace when the caller did not.
  Session and user ids are clipped to 200 characters.
* Span names of an ask: `intelli.ask` (the root, kind `span`, attributes
  `workspace`, `intent`, `mode`, `revision`), `cache.answer` (an `event`,
  `result: hit | miss | off`), `intent`, `resolve` (`entities`, `cache`),
  `command`, `clarify` or `deterministic` when taken, `retrieve`
  (`retrieval`), `llm` (`generation`, added by `Intelli::complete`) and
  `remember` (`conversation_turn`, `episode`, `audit`). The root span's
  parent is the `traceparent` span; every other span's parent is the root.
  Span ids are a random 8-hex-character prefix per trace plus a sequence
  number, so spans that start in the same millisecond list in the order
  they were recorded.
* Generation spans carry the request model in `model` and the tokens in
  their columns; attributes `provider`, `model`, `response_model`,
  `max_tokens`, `latency_ms`, `ttft_ms` (streamed calls), `finish_reason`,
  `cache` (`hit | miss | off`, the completion cache), `budget_exceeded`,
  `estimated`, `error`. With `capture_content`, `input` is `{system: {hash,
  length}, messages: [{role, content}]}` (each content clipped) and
  `output` `{text}`; without it, `input` is `{system: {hash, length},
  messages: <count>, length: <characters>}` and `output` `{length}`.
* Trace totals (`llm_calls`, tokens, `cost_usd`, `provider`, `model`) are
  summed from the generation spans; `llm_calls` counts provider calls
  (completion cache hits and budget refusals are not calls, a failed call
  is). `cache_hits` counts an answer cache hit plus completion cache hits.
  `deterministic` is `mode = deterministic`. A failed LLM call marks the
  trace `status = error` with `error_code = "llm_error"` (the answer is
  still the `fallback`); an error returned by the call itself marks it with
  the error's code (`invalid_argument`, …).
* `call_traced`: `intelli.ask` becomes an ask trace; `action.apply` a trace
  of kind `action` named `action.apply:<type>`, `action.undo` one named
  `action.undo:<type>` (from the receipt), with a root `tool` span whose
  attributes are `action_type`, `risk`, `receipt`, `edits` (planned
  mutations) and `outcome` (the receipt's status); other calls a trace of
  kind `call` named after the action with a root `tool` span, when `[traces]
  calls` selects them (`writes`: actions marked `write` in `ACTIONS` plus
  `LLM_ACTIONS` = `ontology.refine`, `ontology.advise`; `all`; `none`).
  `eval.run` and the `trace.*` actions are never traced themselves. With
  `capture_content`, a call's `input` is `{input: <JSON text>}` (plus
  `question` when the input has one) and `output` `{output: <JSON text>}`,
  both clipped.
* `eval.run`: each case is a trace of kind `eval` named `eval.case`
  (`metadata.case` is the case's name, tag `eval`, the session and source
  of the caller) with an `eval.pass` score (1 or 0, `source = "eval"`,
  `author` the principal, `comment` the failures). Scores go through the
  sink after their trace. `CaseResult` gains `trace_id`.
* `TraceSink`: one writer task per tenant; a `Flush` message makes the
  writer write at once and answer when everything queued before it is
  written. `ledger().traces_dropped` (also in `system.stats`) counts
  dropped records. A failed write is retried once after 200 ms, then the
  batch's records are written one at a time, so one record the store
  refuses is dropped and logged alone.
* Retention runs 5 minutes after a tenant opens, then hourly, on the
  blocking pool (`Intelli::vacuum_traces`), with the project's
  `trace_retention_days` (`TenantSettings.traces`) or `[traces]
  retention_days`. `onto-control` reads `trace_retention_days` (1 to 400)
  and `capture_content` (a boolean) from the project's settings;
  `PATCH /v1/console/projects/{ref}` refuses other values for them (400).
* The CLI's local backend flushes traces after each traced call (and after
  `eval.run`), so `onto ask` and `onto eval` leave their traces written
  when the process exits.

### 9.3 Tenant migration `0005_traces.sql`

```sql
CREATE TABLE IF NOT EXISTS {schema}.traces (
    id TEXT PRIMARY KEY,                    -- 32 hex
    ts BIGINT NOT NULL,                     -- start, ms
    duration_ms DOUBLE PRECISION NOT NULL,
    name TEXT NOT NULL,                     -- intelli.ask, action.apply:assign, ontology.refine, …
    kind TEXT NOT NULL,                     -- ask | action | call | eval
    status TEXT NOT NULL,                   -- ok | error
    error_code TEXT,
    error_message TEXT,
    workspace TEXT NOT NULL,
    principal TEXT NOT NULL,
    via TEXT,
    user_id TEXT,
    session_id TEXT,
    api_key TEXT,                           -- prefix
    source TEXT NOT NULL,
    request_id TEXT,
    intent TEXT,
    mode TEXT,                              -- AnswerMode for asks
    deterministic BOOLEAN NOT NULL DEFAULT false,
    provider TEXT,
    model TEXT,
    llm_calls INT NOT NULL DEFAULT 0,
    cache_hits INT NOT NULL DEFAULT 0,      -- answer + completion cache
    input_tokens BIGINT NOT NULL DEFAULT 0,
    output_tokens BIGINT NOT NULL DEFAULT 0,
    cached_tokens BIGINT NOT NULL DEFAULT 0,
    context_tokens BIGINT NOT NULL DEFAULT 0,
    saved_tokens BIGINT NOT NULL DEFAULT 0,
    cost_usd DOUBLE PRECISION NOT NULL DEFAULT 0,
    input JSONB,                            -- {question} clipped, NULL without capture_content
    output JSONB,                           -- {answer} clipped
    metadata JSONB NOT NULL DEFAULT '{}',
    tags TEXT[] NOT NULL DEFAULT '{}',
    version TEXT NOT NULL                   -- onto-intelli version
);
CREATE INDEX IF NOT EXISTS traces_ts ON {schema}.traces (ts DESC, id DESC);
CREATE INDEX IF NOT EXISTS traces_session ON {schema}.traces (session_id, ts) WHERE session_id IS NOT NULL;
CREATE INDEX IF NOT EXISTS traces_user ON {schema}.traces (user_id, ts) WHERE user_id IS NOT NULL;
CREATE INDEX IF NOT EXISTS traces_name_ts ON {schema}.traces (name, ts);
CREATE INDEX IF NOT EXISTS traces_errors ON {schema}.traces (ts) WHERE status = 'error';

CREATE TABLE IF NOT EXISTS {schema}.spans (
    trace_id TEXT NOT NULL,
    id TEXT NOT NULL,                       -- 16 hex
    parent_id TEXT,
    name TEXT NOT NULL,
    kind TEXT NOT NULL,                     -- span | generation | retrieval | tool | event
    ts BIGINT NOT NULL,
    duration_ms DOUBLE PRECISION NOT NULL,
    status TEXT NOT NULL,
    error TEXT,
    model TEXT,
    input_tokens BIGINT NOT NULL DEFAULT 0,
    output_tokens BIGINT NOT NULL DEFAULT 0,
    cached_tokens BIGINT NOT NULL DEFAULT 0,
    cost_usd DOUBLE PRECISION NOT NULL DEFAULT 0,
    attrs JSONB NOT NULL DEFAULT '{}',
    input JSONB,
    output JSONB,
    PRIMARY KEY (trace_id, id)
);
CREATE INDEX IF NOT EXISTS spans_ts ON {schema}.spans (ts);
CREATE INDEX IF NOT EXISTS spans_model_ts ON {schema}.spans (model, ts) WHERE kind = 'generation';

CREATE TABLE IF NOT EXISTS {schema}.scores (
    id TEXT PRIMARY KEY,
    trace_id TEXT NOT NULL,
    span_id TEXT,
    ts BIGINT NOT NULL,
    name TEXT NOT NULL,                     -- eval.pass, user.feedback, accuracy, …
    value DOUBLE PRECISION,                 -- numeric and boolean (0/1) scores
    label TEXT,                             -- categorical scores
    source TEXT NOT NULL,                   -- eval | api | console
    author TEXT,
    comment TEXT
);
CREATE INDEX IF NOT EXISTS scores_trace ON {schema}.scores (trace_id);
CREATE INDEX IF NOT EXISTS scores_name_ts ON {schema}.scores (name, ts);

DROP TRIGGER IF EXISTS traces_append_only ON {schema}.traces;
CREATE TRIGGER traces_append_only BEFORE UPDATE ON {schema}.traces
    FOR EACH ROW EXECUTE FUNCTION {schema}.refuse_update();
DROP TRIGGER IF EXISTS spans_append_only ON {schema}.spans;
CREATE TRIGGER spans_append_only BEFORE UPDATE ON {schema}.spans
    FOR EACH ROW EXECUTE FUNCTION {schema}.refuse_update();
DROP TRIGGER IF EXISTS scores_append_only ON {schema}.scores;
CREATE TRIGGER scores_append_only BEFORE UPDATE ON {schema}.scores
    FOR EACH ROW EXECUTE FUNCTION {schema}.refuse_update();
```

`refuse_update()` exists since migration 3. Rows are only inserted; the
retention task deletes them. Spans are written in the same transaction as
their trace (multi-row `INSERT`, `CHUNK` rows per statement).

**Retention**: an hourly task per open tenant deletes traces, spans and
scores older than the project's `trace_retention_days` (default `[traces]
retention_days`) in batches of 5 000 (`DELETE … WHERE ctid IN (SELECT ctid …
LIMIT 5000)`), at most 60 s per run. Each table is cut by its own `ts`
(spans first, then scores, then traces).

As built, a batch is written with one `INSERT … SELECT … FROM
jsonb_to_recordset($1)` per table and chunk of `CHUNK` (2 000) rows, all in
one transaction, `ON CONFLICT DO NOTHING` (a trace written twice is
skipped).

### 9.4 Store methods

```rust
// in trait Store, defaults return Error::Unavailable("traces are not supported by this store")
fn trace_write(&self, batch: &[TraceRecord]) -> Result<()>;
fn trace_list(&self, q: &TraceQuery) -> Result<TracePage>;
fn trace_get(&self, id: &str) -> Result<Option<TraceDetail>>;   // trace + spans + scores
fn trace_metrics(&self, q: &MetricsQuery) -> Result<TraceMetrics>;
fn trace_sessions(&self, q: &SessionQuery) -> Result<SessionPage>;
fn score_write(&self, s: &ScoreRow) -> Result<()>;
fn trace_vacuum(&self, before_ms: i64, budget: Duration) -> Result<u64>;
// as built, also: a session's traces, oldest first, with bodies (trace.session)
fn trace_session(&self, id: &str, limit: usize) -> Result<Vec<TraceRow>>;
```

`MemoryStore` keeps the newest 50 000 traces in a ring and computes queries
and aggregates in Rust (`onto_core::trace`), so behaviour matches PostgreSQL
and tests can run without a database.

`TraceQuery`: `since`, `until`, `name`, `kind`, `status`, `user_id`,
`session_id`, `model`, `otel_trace_id`, `workspace`, `mode`, `tag`, `min_ms`, `q` (substring
of the stored question, `ILIKE` with escaped wildcards), `limit` (≤ 200,
default 50), `cursor` (opaque base64url of `ts:id`, keyset on `(ts DESC, id
DESC)`). Lists return rows without `input`/`output` bodies but with a
120-character preview.

`MetricsQuery`: `since`, `until` (default last 7 days), `bucket` (`hour` |
`day`), `name`, `workspace`. `TraceMetrics`:

```json
{
  "requests": 1204, "errors": 7, "error_rate": 0.0058,
  "p50_ms": 41.2, "p95_ms": 1830.0, "p99_ms": 3120.5,
  "input_tokens": 912000, "output_tokens": 81000, "cached_tokens": 640000,
  "cost_usd": 4.21,
  "deterministic_share": 0.63, "cache_hit_rate": 0.18, "llm_calls": 402,
  "series": [{ "bucket": "2026-10-21", "requests": 180, "errors": 1, "p50_ms": 39.0,
               "p95_ms": 1700.0, "input_tokens": 130000, "output_tokens": 11000, "cost_usd": 0.61 }],
  "by_model": [{ "model": "claude-…", "calls": 402, "input_tokens": 912000,
                 "output_tokens": 81000, "cost_usd": 4.21, "p95_ms": 2100.0 }],
  "by_name": [{ "name": "intelli.ask", "requests": 1100, "errors": 2, "p95_ms": 1900.0 }],
  "scores": [{ "name": "eval.pass", "count": 40, "avg": 1.0 }]
}
```

PostgreSQL computes percentiles with `percentile_cont(…) WITHIN GROUP
(ORDER BY duration_ms)` and buckets with `date_trunc(bucket,
to_timestamp(ts / 1000.0) AT TIME ZONE 'UTC')`. `deterministic_share` is
deterministic asks over all asks.

`SessionQuery` (`since`, `until`, `user_id`, `workspace`, `limit`, `cursor`;
only traces of `workspace` count when it is set) returns one
row per session: `session_id`, `user_id`, `first_ts`, `last_ts`, `traces`,
`errors`, tokens, `cost_usd`, and the first question's preview.

As built:

* Windows: `since` is inclusive and `until` exclusive (Unix ms) in every
  query; the metrics default is the 7 days up to and including now. A
  metrics window is at most 400 days, at most 31 days with `bucket =
  hour`; an unknown bucket or `since >= until` is 400.
* Keyset order compares ids with `COLLATE "C"` (byte order, as in Rust).
  Pages fetch `limit + 1` rows; `next` is set only when there is more.
  `TracePage { traces, next }`, `SessionPage { sessions, next }`; a bad
  cursor is 400.
* `q` matches `input->>'question'` with `ILIKE`, `\`, `%` and `_` escaped
  (in memory: a lowercase substring). `model` filters the trace's model
  (traces that called an LLM). `preview` is `left(input->>'question', 120)`.
* Bucket labels: `2026-10-21` by day, `2026-10-21T13:00` by hour (UTC).
  `cache_hit_rate` is asks with `cache_hits > 0` over all asks. `by_model`
  counts the generation spans of the window's traces (completion cache
  hits included, with no tokens), ordered by calls; `by_name` by requests;
  `scores` are the scores on the window's traces, by name, `avg` over
  numeric values (`null` when all are labels). PostgreSQL returns raw
  aggregates and `TraceMetrics::from_parts` (shared with memory) computes
  the ratios and rounds: milliseconds to 0.001, costs to 0.000001, ratios to
  0.0001. A session's `user_id` and preview come from its first trace (by
  `ts`, then id) that has them.

### 9.5 Actions

| Action | Input | Access |
|---|---|---|
| `trace.list` | `TraceQuery` | `can_observe` |
| `trace.get` | `{id}` | `can_observe` |
| `trace.metrics` | `MetricsQuery` | `can_observe` |
| `trace.sessions` | `SessionQuery` | `can_observe` |
| `trace.session` | `{id, limit?}`: the session's traces, oldest first, with inputs and outputs | `can_observe` |
| `trace.score` | `{trace_id, span_id?, name, value? \| label?, comment?}` | `can_observe` and write access, or the trace's own principal |

The trace actions are not themselves traced.

As built: the read actions refuse principals that cannot observe (403,
`permission_denied`). A principal whose workspaces are not `*` reads one
of its workspaces at a time: `trace.list` and `trace.metrics` take its
only workspace or the one named (403 for another, 403 when it has several
and names none), and so does `trace.sessions` (sessions are aggregated
over that workspace's traces only), `trace.get` and
`trace.session` answer 404 for traces outside its workspaces. `trace.get`
flushes the tenant's queue and looks once more when the trace is not
written yet (on a multi-threaded runtime). `trace.session` answers
`{session_id, traces}` (404 when empty). `trace.score` validates a name of 1
to 100 printable characters, a finite `value` or a `label` of 1 to 100
characters, a `comment` of at most 2 000 characters and an existing
`span_id`; it writes the score at once (not through the queue) with
`source = "console"` for console members (`user:…` principals), else
`"api"`, and `author` the principal's id, and answers the score.

### 9.6 OTLP export (optional, `otlp.rs`)

**Not built.** It was optional in stage 4 and no later stage built it, so
the `OTEL_*` variables below are not read and traces are only in the
store. The design stands for a later change.

When `OTEL_EXPORTER_OTLP_TRACES_ENDPOINT` or `OTEL_EXPORTER_OTLP_ENDPOINT`
(then `+ /v1/traces`) is set, the sink also posts each batch as OTLP/HTTP
JSON (`resourceSpans`, no protobuf dependency) with the headers of
`OTEL_EXPORTER_OTLP_HEADERS` (`k=v,k=v`, percent-decoded). Resource:
`service.name` (`OTEL_SERVICE_NAME`, default `onto-intelli`),
`service.version`, `onto.tenant`. Generation spans carry the GenAI semantic
conventions (`gen_ai.operation.name = chat`, `gen_ai.provider.name`,
`gen_ai.request.model`, `gen_ai.response.model`, `gen_ai.usage.input_tokens`,
`gen_ai.usage.output_tokens`), every trace `session.id` and `user.id`
(Langfuse reads these, so pointing the endpoint at Langfuse's OTLP endpoint
with its basic-auth header works). Content attributes are sent only with
`capture_content`. Export failures are logged at most once a minute and
dropped. Export is server-wide (the operator's collector), not per project.

## 10. HTTP routes and gRPC services

Errors keep the existing shape `{"error": {"code", "message"}}`;
`Error::Limited` maps to 429 with `retry-after`.

### Console (`onto-grpc/src/console.rs`), console sessions only

| Method and path | Purpose | Role |
|---|---|---|
| `GET /v1/console/auth/providers` | `{providers: [{id, name}], bootstrap, dev_mode, signup}` | public |
| `GET /v1/console/auth/oidc/{id}/start?binding=&return_to=&invite=` | 302 to the IdP | public |
| `GET /v1/console/auth/oidc/callback` | IdP redirect target; 302 to `/ui/#/auth/callback?ticket=` | public |
| `POST /v1/console/auth/exchange` | `{ticket, binding}` → `{token, expires_at, user}` | public |
| `POST /v1/console/auth/token` | bootstrap admin sign-in | bootstrap token |
| `POST /v1/console/auth/dev` | dev sign-in | dev mode |
| `POST /v1/console/auth/refresh` | new token | session |
| `POST /v1/console/auth/logout` | `{everywhere?}` | session |
| `GET /v1/console/me` | user, organizations with roles, projects with roles | session |
| `GET /v1/console/invitations/{code}` | organization name and role of an invitation | public |
| `POST /v1/console/orgs` | `{name}` → organization (caller is owner) | session |
| `GET/PATCH/DELETE /v1/console/orgs/{org}` | read, rename, delete (no active projects) | viewer / owner / owner |
| `GET /v1/console/orgs/{org}/members` | members with roles | viewer |
| `PATCH/DELETE /v1/console/orgs/{org}/members/{user}` | `{role}`, remove | admin (owner for owners) |
| `GET/POST /v1/console/orgs/{org}/invitations` | list; `{email, role}` → `{invitation, link}` | admin |
| `DELETE /v1/console/orgs/{org}/invitations/{id}` | revoke | admin |
| `GET /v1/console/orgs/{org}/audit?before=&limit=` | control-plane audit log | admin |
| `GET/POST /v1/console/orgs/{org}/projects` | list; `{name, seed?: "demo"}` → project | viewer / admin |
| `GET/PATCH /v1/console/projects/{ref}` | project with settings; `{name, settings}` | viewer / admin |
| `DELETE /v1/console/projects/{ref}` | `{confirm: "<project name>"}` | owner |
| `GET/POST /v1/console/projects/{ref}/keys` | list; `{name, scopes, workspaces?, expires_at?, impersonate?, impersonate_roles?}` → `{key, secret}` | viewer / developer |
| `POST /v1/console/projects/{ref}/keys/{id}/rotate` | `{grace_secs?}` → `{key, secret}` | developer |
| `DELETE /v1/console/projects/{ref}/keys/{id}` | revoke | developer (own keys) / admin |
| `GET/PUT/DELETE /v1/console/projects/{ref}/llm` | settings (never the key); update; back to server default | viewer / admin / admin |
| `POST /v1/console/projects/{ref}/llm/test` | test connection | developer |
| `GET /v1/console/projects/{ref}/usage?from=&to=` | `usage_daily` rows, month-to-date totals, budget | viewer |
| `GET /v1/console/projects/{ref}/connect` | `{api_url, grpc_url, mcp_url, ref, keys: [{prefix, name, scopes}]}` | viewer |

Notes on the console routes as built:

* `GET /v1/console/auth/providers` lists the `[[console.oidc]]` entries in
  configuration order; `bootstrap` and `dev_mode` say which of the other
  sign-ins work, and `POST /v1/console/auth/token` and `/auth/dev` answer 404
  when theirs is off.
* `GET …/oidc/{id}/start` answers 302 to the IdP; 404 for an unknown
  provider, 400 for a bad `binding`, `return_to` or `invite`, 503 when the
  IdP's discovery document cannot be fetched or fails the checks of 7.1,
  and 429 (`retry-after: 60`, code `resource_exhausted`) beyond 20 starts
  in a minute from one client address (an IPv6 address counts by its /64),
  or 600 from all clients, on the server, counted before the other checks.
  The client address is the TCP peer; when the peer is not a public
  address (a reverse proxy on loopback or the private network), it is the
  last `X-Forwarded-For` entry, the one that proxy appended.
* `GET …/oidc/callback` always answers 302 to the console (section 7.2).
  `POST …/auth/exchange` answers 401 for any refused ticket or binding.
* Request bodies are at most 64 KB (`413` beyond). Creating or rotating a key
  counts against 10 per minute per project: beyond that, `429` with
  `retry-after: 60` and the code `resource_exhausted`, the same response
  as `Error::Limited` anywhere else.
* `POST /v1/console/orgs` makes the slug from the name (lowercase letters
  and digits, other runs become `-`, at most 30 characters) plus `-` and 4
  random characters; the slugs `default` and `dev` are made only by the
  bootstrap and dev sign-ins. Responses for organizations and projects carry
  the caller's `role`.
* `POST …/projects` answers the project with `seeded: {dataset, workspace,
  entities, links, events}` when `seed: "demo"` was asked (loaded into the
  engine's default workspace), else `seeded: null`; an unknown seed is 400;
  `console.max_projects_per_org` active projects at most (409 beyond).
* `PATCH /v1/console/projects/{ref}` merges `settings` (an object; `null`
  values remove keys).
* `GET …/audit` answers `{entries, next}`; `next` is the `before` of the
  next page (default `limit` 50, at most 200).
* `GET …/keys` lists every key of the project, revoked ones too, without
  hashes; `secret` appears only in the create and rotate responses.
* `GET /v1/auth/whoami` adds `console: {user, role, org}` (null for other
  credentials), `credential` (`token`, `api_key`, `console`, `session`,
  `dev`) and `capabilities.observe`.
* Audit actions written in stage 1: `auth.signin`, `auth.logout_everywhere`,
  `org.create`, `org.rename`, `org.delete`, `member.role`, `member.remove`,
  `invitation.create`, `invitation.revoke`, `project.create`,
  `project.adopt`, `project.update`, `project.delete`, `key.create`,
  `key.rotate`, `key.revoke`. Details carry names, prefixes, scopes and
  roles, never secrets or invitation codes.
* `GET …/llm` answers `{llm, error, master_key, server_default}`: `llm` is
  the stored settings (section 8.1) with `has_api_key` and
  `api_key_last4`, never the key or its ciphertext, or `null`; `error` why
  the project's own provider is not in use (section 8.2), else `null`;
  `master_key` whether `ONTO_MASTER_KEY` is set; `server_default`
  `{provider, model}` of `[llm]`. `PUT …/llm` takes the settings and answers
  the same object; `DELETE …/llm` answers `{deleted}` (whether there were
  settings). Both refresh the tenant on this replica.
* `GET …/usage?from=&to=` (days `YYYY-MM-DD`, default the current month up
  to today, at most 400 days apart) answers `{from, to, rows,
  month_to_date: {month, cost_usd, tokens}, budget: {monthly_budget_usd,
  monthly_token_budget, asks_per_minute, reached}}`; `rows` are the
  `usage_daily` rows, `month_to_date` comes from this replica's meter when
  the project is open here (it includes usage not yet written), else from
  the rows, and `reached` is the note of section 8.5 or `null`.
* Audit actions written in stage 3: `llm.update` (detail: provider, model,
  base URL, `api_key: set | kept | removed`, budgets, rate limit, header
  names) and `llm.delete`. Never the key or header values.
* `GET …/connect` (stage 5) answers `api_url` and `grpc_url`, both
  `console.public_url` without a trailing slash (the data plane, gRPC and
  MCP share the server's port), `mcp_url` (`{public_url}/mcp`), `ref`, and
  `keys`: the project's usable keys (not revoked, not expired) as `{prefix,
  name, scopes}`, never a secret.
* Stage 2 adds, for each OIDC sign-in: `auth.signin` once per organization
  the user belongs to (one entry without organization when there is none;
  detail `{provider, new_user, invitations}`), `invitation.accept` per
  organization joined, `org.create` and `project.create` (detail `signup:
  true`) for a first organization, and `auth.signin_refused` (actor
  `system`, no organization, detail `{provider, reason}`) when a callback
  with a valid state is refused for any reason but `server_error`.

### Data plane observability (`onto-grpc/src/observe.rs`)

Any credential whose principal `can_observe` (console session with
`x-onto-tenant`, admin-scope API key, operator token):

| Method and path | Action |
|---|---|
| `GET /v1/traces?…` | `trace.list` (query string as `TraceQuery`) |
| `GET /v1/traces/{id}` | `trace.get` |
| `POST /v1/traces/{id}/scores` | `trace.score` |
| `GET /v1/metrics?…` | `trace.metrics` |
| `GET /v1/sessions?…` | `trace.sessions` |
| `GET /v1/sessions/{id}` | `trace.session` |

Existing routes accept two new request headers: `x-onto-session-id` and
`traceparent`; responses carry `x-onto-trace-id`.

As built: every data-plane route goes through `call_traced` with
`observe::trace_ctx` (session id, `traceparent`, an invalid one ignored,
source `http`, the API key's prefix from `Caller.credential`);
`x-onto-trace-id` is set when the call was traced (asks, `/v1/ask/stream`
too, and calls `[traces] calls` selects), not on untraced reads. In the
query strings of these routes `since`, `until`, `limit` and `min_ms` are
numbers and everything else stays text (`q=2024` is text). The dynamic
gRPC handler builds the same context from the request metadata (source
`grpc`) and returns `x-onto-trace-id` in the response metadata. The
built-in proto's `AskRequest` gains `google.protobuf.Struct trace = 9` and
`AskResponse` `string trace_id = 20`.

### MCP over HTTP (`onto-grpc/src/mcp_http.rs`)

`POST /mcp`: the MCP Streamable HTTP transport, JSON responses only (no
server-initiated stream; `GET /mcp` returns 405). Authenticated like any
data-plane request (an `agent`-scope key is the intended credential), the
tenant is the key's project, and the body (one JSON-RPC message or a batch)
goes to `McpServer::handle`, which is already transport independent. Writes
are allowed when `[mcp] allow_writes` and the principal may write. The
`Mcp-Session-Id` header is not required (the server keeps no MCP session
state); `Mcp-Protocol-Version` is checked against the supported list.

As built (stage 5):

* A notification, or a batch of notifications only, is `202 Accepted` with no
  body. A batch answers an array of the replies to its requests, in order;
  an element that is not an object gets a `-32600` error in its place. An
  empty batch or a body that is neither an object nor an array is `400` with
  `-32600`; a body that is not JSON is `400` with `-32700` (`id: null`).
* `Mcp-Protocol-Version`, when sent, must be one of
  `onto_intelli::mcp::SUPPORTED` (`2025-06-18`, `2025-03-26`, `2024-11-05`),
  else `400` with `-32600`; it is checked before authentication.
  `initialize` negotiates the version as over stdio.
* `GET` and `DELETE /mcp` answer `405` with `allow: POST`.
* Authentication and the tenant are those of any data-plane request
  (`caller_full`): no credential is `401`, a key with `x-onto-tenant` naming
  another project `403`. The write tools (`apply_action`, `undo_action`,
  `crawl`) are listed and run only when `[mcp] allow_writes` is on and the
  principal `allows(Write)` (`member`, `agent`, `admin`); otherwise
  `tools/list` leaves them out and calling one answers `isError`.
* Calls are traced through `McpServer::with_trace_ctx` with
  `observe::trace_ctx(headers, caller, "mcp")`: source `mcp`, the
  request's `x-onto-session-id` and `traceparent`, and the API key's prefix.
* No MCP tool registers connectors, so `guard_project_call` (which concerns
  `connector.add`) has nothing to refuse here.
* Tests: `crates/onto-grpc/tests/mcp_http.rs` (`initialize`, a
  notification, `tools/list` with an agent key and with a read key, a
  refused write tool, a batch, the trace's source and key prefix, the
  protocol version header, malformed bodies, `GET`, no credential, a key of
  another project, the Connect route and a revoked key); the role table of
  `tests/console.rs` has the Connect route (viewer).

### gRPC

A new `ObservabilityService` in the built-in proto, with `Struct` requests
and responses like `AnalyticsService`:

```proto
service ObservabilityService {
  rpc ListTraces(google.protobuf.Struct) returns (google.protobuf.Struct);   // trace.list
  rpc GetTrace(EntityRef) returns (google.protobuf.Struct);                   // trace.get
  rpc Metrics(google.protobuf.Struct) returns (google.protobuf.Struct);      // trace.metrics
  rpc ListSessions(google.protobuf.Struct) returns (google.protobuf.Struct); // trace.sessions
  rpc GetSession(EntityRef) returns (google.protobuf.Struct);                 // trace.session
  rpc Score(google.protobuf.Struct) returns (google.protobuf.Struct);        // trace.score
}
```

mapped in `BUILTIN_ROUTES`. gRPC calls are authenticated with API keys the
same way (metadata `authorization`). The console API stays HTTP/JSON: it
serves a browser.

## 11. Console UI

New pages in `ui/` with the existing look (tokens in `styles.css`,
components in `components/ui.tsx`, TanStack Query, hash routes). Charts are
small SVG components in `components/charts.tsx` (line, stacked bars,
sparkline, latency histogram); no chart library is added.

### Navigation

When `/v1/console/me` succeeds the shell shows an **organization and
project switcher** at the top of the sidebar (and in the phone header), then
two groups:

* **Project**: Overview, Traces, Sessions, Usage, Connect, API keys, LLM,
  Settings.
* **Ontology**: Map, Explorer, Entities, Schema, Builder, Proposals (the
  existing pages, now per project: the switcher sets `session.tenant` to the
  project ref and every query key already includes the tenant).
* Footer: Members (organization), the user menu (sign out, sign out
  everywhere).

Without the console (operator token, embedded) the shell is unchanged.
Items a role cannot use are hidden or disabled with a tooltip saying which
role is needed (`/v1/console/me` returns the role per project).

### Pages

| Route | Page | API calls |
|---|---|---|
| `#/signin` | Provider buttons, bootstrap token form, dev auto sign-in | `GET /v1/console/auth/providers`, `…/start`, `POST …/token`, `POST …/dev` |
| `#/auth/callback` | Exchanges the ticket, removes it from the address, opens `return_to` | `POST /v1/console/auth/exchange` |
| `#/invite?code=` | Invitation details, "Sign in to accept" | `GET /v1/console/invitations/{code}` |
| `#/projects` | Projects of the organization (cards: name, ref, created, 7-day requests), **New project** dialog (name, seed demo) | `GET/POST /v1/console/orgs/{org}/projects`, `GET /v1/metrics` per card |
| `#/overview` | Metric cards (requests, error rate, p50/p95, tokens, cost, deterministic share, cache hit rate) with deltas against the previous period; charts: requests and errors per day, latency p50/p95, cost by model; recent errors; a "Get connected" checklist while the project has no traces | `GET /v1/metrics?since=&bucket=`, `GET /v1/traces?status=error&limit=5`, `GET /v1/traces?limit=1`, `GET …/keys`, `GET …/llm` |
| `#/traces` | Filter bar (time range, name, status, user, session, model, mode, tag, text), table (time, name, preview, mode, latency, tokens, cost, user), keyset paging, live refresh toggle (10 s) | `GET /v1/traces` |
| `#/traces/{id}` | Header (status, latency, tokens, cost, user, session, API key prefix), **waterfall** of spans (bars positioned by offset and duration, indented by parent, coloured by kind), span panel (attributes, input/output, tokens), scores with **Add score** (thumbs up/down, value, comment) | `GET /v1/traces/{id}`, `POST /v1/traces/{id}/scores` |
| `#/sessions`, `#/sessions/{id}` | Sessions table; session replay: each turn's question and answer with its latency, mode and a link to the trace | `GET /v1/sessions`, `GET /v1/sessions/{id}` |
| `#/usage` | Month selector; month-to-date cost and tokens against the budget (progress bar, warning at 80 %), daily stacked bars by model, table by model; asks, LLM calls, deterministic answers | `GET …/usage` (the budget comes with it) |
| `#/connect` | Tabs: **curl**, **TypeScript**, **Python**, **MCP** (Claude Code `claude mcp add --transport http …`, a generic `mcp.json`), **Env vars**, **Monorepo** (`onto connect repo`). Key picker: an existing key's prefix fills `onto_sk_7f3k9q2m_…` placeholders; **Create key** inserts the full secret into the snippets once. Copy buttons. | `GET …/connect`, `POST …/keys` |
| `#/keys` | Table (name, prefix, scopes, created by, created, last used, expires, status); **Create key** dialog (name, scopes, workspaces, expiry, impersonation for admins) then a one-time reveal with copy and a warning; **Rotate** (grace period), **Revoke** (confirm) | `GET/POST …/keys`, `POST …/rotate`, `DELETE …/keys/{id}` |
| `#/llm` | Provider select, model, base URL, API key (write-only, shows `•••• last4`, **Replace**), pricing, max output tokens, temperature, effort, budgets, rate limit; **Test connection** with the result inline; **Use server default** | `GET/PUT/DELETE …/llm`, `POST …/llm/test` |
| `#/members` | Members (name, email, role select, remove), invitations (email, role, link with copy, revoke), **Invite** dialog | `GET/PATCH/DELETE …/members`, `GET/POST/DELETE …/invitations` |
| `#/settings` | Project name, default workspace, trace retention, capture content, delete project (type the name); organization name; audit log (admins) | `PATCH /v1/console/projects/{ref}`, `DELETE …`, `GET …/audit` |

`lib/console.ts` holds the console client (same `parse` and error handling
as `api.ts`), `lib/session.ts` gains `console: boolean` and the login
binding helpers, and a refresh timer renews the token. Phone width: the
switcher moves into the header, tables become stacked rows, the waterfall
scrolls horizontally inside its panel only. Keyboard: every control
reachable, `/` focuses the trace filter on the Traces page, `j`/`k` move
through trace rows, `Esc` closes panels. Light and dark follow the existing
tokens; chart colours come from CSS variables.

`npm run build` writes `crates/onto-grpc/ui-dist`, which is committed and
must match the sources.

As built (stage 5):

* Boot: the UI asks `GET /v1/console/auth/providers`. When it answers, and
  the tab holds no token or a console token, the console runs: `GET
  /v1/console/me` with the token, or the sign-in page without one (or on
  401). Embedded views, `#…?token=` links, `postMessage` sessions and the
  operator-token form (`#/signin/token`, linked from the sign-in page) keep
  the operator UI unchanged; their tokens are marked `console: false`.
* `lib/session.ts`: `Session` gains `console`, `org`, `issued_at` and
  `expires_at`; `setConsoleToken`; `newLoginBinding` (32 random bytes in
  `sessionStorage` `onto.login`, returns base64url of their SHA-256 from
  `crypto.subtle`) and `takeLoginBinding` (read once); `startSessionRefresh`
  renews the token with `POST /v1/console/auth/refresh` once less than a
  quarter of its lifetime (from `issued_at`, the tab's receipt time, to
  `expires_at`) is left, retries in 30 s on a network or server error, drops
  the token on 401, and checks again when the tab becomes visible.
* Dev mode signs in automatically once per page load, except after an
  explicit sign-out in that tab (`sessionStorage` `onto.signedout`); the page
  then offers **Continue as developer**.
* `#/auth/callback` reads `ticket`, `return_to` and `error` once, removes
  them from the address at once, exchanges the ticket with the tab's binding
  and opens `return_to` (only `#/…`); a missing binding (the sign-in started
  in another tab or browser), a refused ticket or the IdP's `error` codes of
  section 7.2 are explained on the page.
* Selecting a project sets `session.tenant` to its ref, `session.org` to its
  organization and `session.workspace` to the project's
  `settings.default_workspace` (a console-side setting; the server stores it
  like any other key of `settings`). When the session's tenant is not one of
  the user's projects, the first project of the session's organization (else
  any) is selected.
* Routes as in the table, plus `#/traces/{id}`; `#/projects?new=1` opens the
  New project dialog. The ontology pages and the Project pages need a
  selected project and say so otherwise.
* Overview deltas compare with the window of the same length just before;
  empty buckets are filled in on the client (`lib/observe.ts`
  `fillSeries`). The Traces page's latency histogram covers the rows loaded.
  Usage charts cost by default, tokens when nothing in the month cost
  anything.
* Invitation links: the server keeps only a hash of the code, so the
  Members page can copy a link only in the tab that created it; later the row
  says so (revoke and invite again for a new link).
* The Connect page's TypeScript and Python tabs show a dependency-free
  snippet (`fetch`, `urllib`) first and the SDK (section 12) second; the
  Monorepo tab registers a repository by git URL (an API key may not register
  a local path, section 14).
* Console pages are lazy chunks (`React.lazy`); charts, tables and dialogs
  use the existing tokens and components. Tables carry the class
  `stack-table`: below 768 px each row becomes a stack of label and value
  lines (`data-label`). Shared fixes in `components/ui.tsx`: `Panel` closes
  on Esc (unless a modal dialog is open) and `Dialog` focuses its first field
  instead of the Close button. `/` and `j`/`k` on the Traces page are taken
  in the capture phase, ahead of the shell's `/` (the command palette).

## 12. SDKs

Both SDKs read `ONTO_URL`, `ONTO_API_KEY` and `ONTO_TENANT` (optional: a key
implies its project) when no options are given, send `authorization: Bearer
<key>`, map error bodies to an `OntoError` (`status`, `code`, `message`,
`trace_id`), retry 429 and 503 up to twice with `retry-after`, and never log
the key.

### TypeScript (`sdk/typescript`, package `onto-intelli`)

Zero dependencies, `fetch` only (Node 18+, Deno, Bun, browsers through a
backend-minted session). Source in TypeScript using erasable syntax only (no
enums, namespaces or parameter properties), so tests run with Node's type
stripping.

```ts
import { Onto } from "onto-intelli";

const onto = new Onto({ url, apiKey, tenant?, workspace?, sessionId?, fetch?, headers?, timeoutMs?, maxRetries?, maxRetryDelayMs? });

await onto.ask("Who owns the payments service?", { sessionId, userId, metadata, tags, mode, conversationId, signal });
for await (const ev of onto.askStream({ question, conversationId: "new" })) { /* stage | entities | delta | done | error */ }
await onto.call("graph.impact", { id: "service:payments" });
await onto.search("payments", { kinds: ["service"], limit: 10 });
await onto.entities.get(id); .find(spec); .upsert(input); .patch(patch); .delete(id); .history(id);
await onto.links.create({ from, rel, to }); .delete({ from, rel, to });
await onto.actions.list(); .describe(name); .plan(action, params); .apply(action, params, { confirm }); .undo(receipt);
await onto.objects.types(); .explore(query);
await onto.traces.list(filter); .get(id); .metrics(query); .score(traceId, { name, value, comment });
await onto.sessions.list(filter); .get(id);
await onto.auth.mintSession({ actAs, roles, workspaces, entity, ttlSecs }); .whoami(); // backend → browser
```

Layout: `src/index.ts`, `src/types.ts` (`AskRequest`, `AskResponse`,
`AskEvent`, `Entity`, `TraceRow`, `TraceDetail`, `TraceMetrics`, …, matching
the Rust serde shapes), `src/sse.ts` (SSE parser over a `ReadableStream`),
`test/*.test.ts` (`node --experimental-strip-types --test`, against a
`node:http` mock server that records requests: headers, bodies, SSE framing,
errors, retries), `README.md`, `package.json` (`"type": "module"`, `exports`,
`scripts.test`, `scripts.build` with `tsc` for publishing, not needed by the
tests). An optional integration test runs when `ONTO_URL` is set (against
`onto serve --dev`).

As built (stage 6):

* Routes: `ask` → `POST /v1/ask`; `askStream` → `POST /v1/ask/stream`
  (`accept: text/event-stream`; each SSE `data` is an `AskEvent`, and the
  iterator ends after `done` or `error`); `call(action, input)` → `POST
  /v1/call/{action}`; `search` → `entity.search`; `entities.*`, `links.*`
  and `actions.*` → `entity.get|find|upsert|patch|delete|history`,
  `link.create|delete`, `action.list|describe|plan|apply|undo` through
  `/v1/call`; `objects.types` → `GET /v1/objects/types`, `objects.explore` →
  `POST /v1/objects/explore`; `traces.*` and `sessions.*` → the routes of
  section 10 (`GET /v1/traces`, `GET /v1/traces/{id}`, `POST
  /v1/traces/{id}/scores`, `GET /v1/metrics`, `GET /v1/sessions`, `GET
  /v1/sessions/{id}`); `auth.mintSession` → `POST /v1/auth/session`;
  `auth.whoami` → `GET /v1/auth/whoami`.
* Request option objects are camelCase and sent snake_case (`userId` →
  `user_id`; `ask`'s `sessionId`, `userId`, `tags`, `metadata` and `name`
  become `AskRequest.trace`); `since` and `until` take Unix milliseconds or a
  `Date`. Results keep the server's snake_case fields. `sessionId` (client
  or call) is also sent as `x-onto-session-id`, `workspace` as
  `x-onto-workspace`, `tenant` as `x-onto-tenant`.
* `OntoError`: `status` (0 without a response), `code` (the body's
  `error.code`, else one derived from the status; `unavailable`,
  `deadline_exceeded` or `cancelled` without a response), `message`,
  `traceId` (also readable as `trace_id`, from `x-onto-trace-id`) and
  `retryAfter`. Retries wait `retry-after` seconds (or an HTTP date), capped
  at `maxRetryDelayMs` (default 30 s), else 0.5 s then 1 s; `askStream`
  retries only before the stream starts. `timeoutMs` (default 30 s) bounds
  each attempt up to the response headers.
* The key is held in a private field (`#apiKey`): `console.log(onto)` and
  `JSON.stringify(onto)` never show it.
* `scripts.test` is `node --experimental-strip-types --no-warnings --test
  test/*.test.ts`: Node 22.12 reads a bare `test/` as a file name, so the
  script names the files. The tests need Node 22.6+ (type stripping); the
  package needs Node 18+. `tsconfig.json` sets `erasableSyntaxOnly` and
  `rewriteRelativeImportExtensions` (the sources import `./types.ts`; the
  JavaScript output imports `./types.js`, and TypeScript resolves the
  `.ts` imports left in the declarations to the emitted `.d.ts` files), so
  publishing needs TypeScript 5.8+.
* Tests: `test/client.test.ts` (headers including bearer, `x-onto-tenant`,
  `x-onto-workspace` and `x-onto-session-id`; environment defaults; the key
  never printed; the body of every helper; query strings; error mapping
  with and without a JSON body and without a server; retries of 429 and
  503, `maxRetries` and `maxRetryDelayMs`; timeouts and aborts; an SSE
  stream split every 7 bytes, inside a multi-byte character and a `\r\n`;
  a stream ending at `error`; a refused stream; the parser's multi-line
  data, ids, comments and a missing final blank line) and `test/live.test.ts`
  (skipped without `ONTO_URL`).

### Python (`sdk/python`, package `onto_intelli`)

Standard library only (`urllib.request`, `json`), Python 3.9+.

```python
from onto_intelli import Onto, OntoError

onto = Onto(url=None, api_key=None, tenant=None, workspace=None, timeout=30.0,
            session_id=None, headers=None, max_retries=2, max_retry_delay=30.0)
onto.ask("Who owns the payments service?", session_id=None, user_id=None, metadata=None, mode=None)
for ev in onto.ask_stream("…", conversation_id="new"): ...
onto.call("graph.impact", {"id": "service:payments"})
onto.search("payments", kinds=["service"], limit=10)
onto.entities.get(id); onto.entities.find(**spec); onto.entities.upsert(...); onto.entities.patch(id, ...)
onto.entities.delete(id); onto.entities.history(id)
onto.links.create(from_id, rel, to_id); onto.links.delete(from_id, rel, to_id)
onto.actions.list(); onto.actions.plan(...); onto.actions.apply(..., confirm=True); onto.actions.undo(receipt)
onto.objects.types(); onto.objects.explore({...})
onto.traces.list(**filters); onto.traces.get(id); onto.traces.metrics(since=..., bucket="day"); onto.traces.score(...)
onto.sessions.list(); onto.sessions.get(id)
onto.auth.mint_session(act_as=..., roles=[...]); onto.auth.whoami()
```

Layout: `onto_intelli/__init__.py`, `client.py`, `sse.py`, `errors.py`,
`py.typed`, `pyproject.toml` (no dependencies), `tests/test_client.py`
(`python3 -m unittest`, against `http.server` in a thread), `README.md`.
Results are plain `dict`s; typed `TypedDict`s document the shapes.

As built (stage 6): the same routes, headers, error mapping and retries as
the TypeScript SDK, in snake_case. `OntoError` has `status`, `code`,
`message`, `trace_id` and `retry_after`; `str(e)` is the message.
`ask_stream` opens the request when called (a refusal raises then) and
yields events parsed from `read1` chunks by `onto_intelli.sse.parse_sse`.
`since` and `until` take Unix milliseconds or a `datetime` (naive ones are
UTC). The key is kept in a name-mangled attribute and `repr(onto)` says only
whether one is set. `tests/test_client.py` covers what the TypeScript tests
cover, and its `LiveTest` runs when `ONTO_URL` is set. Run with `cd
sdk/python && python3 -m unittest discover -s tests`.

## 13. Repository connector

`onto connect repo <path | git URL> [--name N] [--branch B] [--include GLOB]…
[--exclude GLOB]… [--owner @org/team=team:payments]… [--every 1h]
[--no-crawl]` registers a connector of `kind = "repo"` and crawls it. It maps
a monorepo's structure into the ontology. It reads **metadata only**:
manifests and `CODEOWNERS`, never source files.

### Schema: `builtin:code`

A new built-in package (`crates/onto-core/schemas/code.toml`), added to the
default `ontology.extend` after `builtin:data-catalog` and to the config
template:

* object type `package`: `ecosystem` (`cargo` | `npm` | `go` | `python`),
  `path` (relative to the repository root), `version`, `description`,
  `manifest`, `private` (bool), `language`; searchable title and
  description.
* link `in_repository` (package → repository, inverse `has_package`).
* `depends_on` and `owns` are redefined with their current endpoints plus
  `package` (link types are replaced by name when a layer redefines them,
  so the layer repeats the full lists of `enterprise` and `data-catalog`).

The connector refuses to crawl when the active schema has no `package` type,
with a message naming `builtin:code`.

As built: `package` also has `module` (Go: the full module path,
searchable), category `artifact`; `ecosystem` is required. `owns` keeps the
weight data-catalog gives it (0.8). The glossary maps `packages`, `crate`
and `crates` to `package`. Because the layer names data-catalog's types in
`owns`, it must come after `builtin:data-catalog`; the default is `extend =
["builtin:data-catalog", "builtin:code"]` (`OntologyConfig::default` and the
template), and `onto_core::schema::PACKAGES` lists `code`.

### Ids

`repository:{name}`, `package:{name}.{ecosystem}.{package}` (package names
reduced to `[a-z0-9_.-]`, `@scope/pkg` → `scope.pkg`), owners
`team:{org}-{team}` for `@org/team`, `person:gh-{user}` for `@user`,
`person:{local}` for an email, unless `--owner` maps them to existing ids.
Provenance: `source = connector:repo`, the connector name, `external_ref =
<git URL or path>@<commit>`, `crawled_at`.

As built: package names are lowercased, a leading `@` dropped, `/` becomes
`.` and any other character outside `[a-z0-9_.-]` becomes `-`; Python names
are PEP 503 normalized first (`Acme_Core` → `acme-core`, which is also the
title), Go modules use the whole module path (`package:mono.go.example.com.mono.api`,
title `api`, the module path as an alias). Owner handles are lowercased with
characters outside `[a-z0-9_-]` turned into `-` (an email's local part keeps
`.`). Provenance `at` is the crawl's start; `run` its id; `external_ref`
carries `@<commit>` for git URLs and for local directories that are git
checkouts (read from `.git` without running git, worktrees included), and is
the bare path otherwise. Two packages with the same id: the first (in
ecosystem, path order) wins, the other is a note. The repository entity has
`url` (the spec's URL or path) and `default_branch` (the branch crawled).

Owners: an owner id already in the graph is linked, never written (a curated
`team:payments` keeps its title); an owner that does not exist is created by
the connector (kind `team` or `person` from the id, the handle as title and
alias) and stays managed by it, so it is deleted when CODEOWNERS no longer
names it. `--owner` keys match CODEOWNERS owners case-insensitively and must
map to `team:…` or `person:…` ids.

### Discovery

Starting at the root, without following symlinks out of the tree, skipping
`.git`, `node_modules`, `target`, `vendor`, `dist`, `build`, `.venv`, hidden
directories and `--exclude` globs, depth ≤ 12, at most 100 000 directory
entries, manifests ≤ 1 MB:

| Ecosystem | Workspace members | Package | Internal dependencies |
|---|---|---|---|
| Cargo | root `Cargo.toml` `[workspace] members` (globs) minus `exclude`; a lone `[package]` otherwise | `[package] name, version, description` (`version.workspace = true` resolved) | `[dependencies]`, `[dev-dependencies]`, `[build-dependencies]` entries with `path`, or `workspace = true` whose `[workspace.dependencies]` entry has a `path`, that resolve to a member |
| npm | root `package.json` `workspaces` (array or `{packages}`), `pnpm-workspace.yaml` `packages:` list | `name, version, description, private` | `dependencies`, `devDependencies`, `peerDependencies`, `optionalDependencies` keys naming a workspace package (any version, `workspace:*`) |
| Go | `go.work` `use` directives; every `go.mod` otherwise | `module` path (title: last segment) | `require` lines naming another workspace module; `replace X => ../y` |
| Python | `[tool.uv.workspace] members`; every `pyproject.toml` otherwise | `[project] name, version, description`, or `[tool.poetry]` | PEP 508 names in `[project] dependencies` and `optional-dependencies`, Poetry dependency keys, normalized (PEP 503), that name a workspace package |

Each dependency becomes `package depends_on package` with `props.kind` =
`normal` | `dev` | `build` | `peer` | `optional`. External dependencies are
not mapped (metadata stays small; a future option could add them).

**CODEOWNERS** (`.github/CODEOWNERS`, `CODEOWNERS`, `docs/CODEOWNERS`, first
found): each package's directory is matched against the rules with
gitignore-style patterns (`/` anchors, trailing `/` is a directory, `*`,
`**`); the last matching rule wins; its owners `owns` the package. The
repository root's rule owns the repository.

As built (`crates/onto-intelli/src/connectors/repo.rs`):

* Symlinks are never followed (a symlinked directory or manifest is
  skipped). Depth counts directory levels below the root; the first
  directory deeper than 12 adds a note. Beyond 100 000 entries the walk
  stops with a note. A manifest over 1 MB, or one that does not parse, is
  skipped with a note. The notes are in the crawl report.
* Workspace roots are found anywhere in the tree, not only at the root, so
  a polyglot repository with `rust/Cargo.toml` and `web/pnpm-workspace.yaml`
  works. A workspace's members are the directories with a manifest under it
  that a member glob matches (relative to the workspace, `*` within a
  segment, `**` across segments, `./` ignored) and no exclude glob matches;
  a manifest inside a workspace's directory that is not a member is not a
  package. Manifests outside every workspace are packages on their own
  (the table's "otherwise", applied per directory).
* Cargo: the workspace's own `[package]` is a member too; `version` and
  `description` with `workspace = true` come from `[workspace.package]`;
  `publish = false` (or `publish = []`) is `private`; `[target.*]`
  dependency tables count; `language` is `rust`.
* npm: `pnpm-workspace.yaml` is read by hand (the `packages:` block list or
  a flow list `[a, b]`, quotes and comments removed); patterns starting with
  `!` exclude. A workspace root is not itself a package. `language` is
  `typescript` when the package has a `tsconfig.json` or depends on
  `typescript`, else `javascript`. Dependency names resolve against every
  npm package of the repository.
* Go: a `go.work` claims the `go.mod` files under its directory; only those
  it `use`s are modules. `require` names (block or single line, comments
  removed) and `replace … => ./dir | ../dir` targets resolve to modules.
  `language` is `go`.
* Python: a uv workspace's root `[project]` is a package too. Dependencies
  also come from `[dependency-groups]` (kind `dev`), Poetry's
  `[tool.poetry.dev-dependencies]` and `[tool.poetry.group.*.dependencies]`
  (kind `dev`; `python` ignored). A `Private :: …` classifier is `private`.
  `language` is `python`.
* One `depends_on` link per pair of packages: when several kinds name the
  same target, the strongest wins in the order `normal`, `build`, `peer`,
  `optional`, `dev`. Self-dependencies are dropped. Dependencies of an
  ecosystem resolve only within that ecosystem.
* `--include` patterns (gitignore-style, like CODEOWNERS) keep the packages
  whose directory they cover; `--exclude` patterns (the same rules) skip
  directories during the walk. A pattern without a `/` (other than a
  trailing one) matches at any depth; one that covers a directory covers
  everything under it; the root is covered only by `*`, `**` and `/` (also
  written `/*` and `/**`).
* CODEOWNERS: blank lines, `#` comments (also after a rule) and section
  headers (`[Docs]`, `^[Docs]`) are skipped; `\#` and `\ ` escape; tokens
  without `@` are not owners; a rule without owners leaves its paths
  unowned. Owners that cannot be mapped (`@org/` alone) are notes.

### Git URLs and incremental crawls

* Accepted: `https://…` and `ssh://` / `git@host:path`. Cloned with the
  `git` binary into `$XDG_CACHE_HOME/onto/repos/{sha256(url, branch)[..16]}`
  (`--depth 1 --filter=blob:none --single-branch [--branch B]`), later
  crawls `git fetch --depth 1 -- origin <B or HEAD>` and `git reset --hard
  FETCH_HEAD`. Every
  invocation passes `-c protocol.allow=never -c protocol.https.allow=always
  -c protocol.ssh.allow=always`, `GIT_TERMINAL_PROMPT=0`, a 120 s timeout,
  and `--` before the URL. Credentials come from the operator's git
  configuration, never from the spec.
* The crawl is incremental like the database connector: the commit (or, for
  a plain directory, a hash of the manifests' sizes and modification times)
  is compared with the stored state; unchanged → no work. Otherwise the
  mapped entities go through `catalog::plan_changes` with the connector's
  fingerprints and `managed_rels = ["depends_on", "owns", "in_repository"]`,
  so only changed packages and links are written, packages that disappeared
  are deleted, and the batches go through the audited write path.
* `ConnectorSpec` gains an optional `repo: { branch, include, exclude,
  owners }` object; `connector_for` keeps its role for database kinds and
  `crawl_one` dispatches `repo` to `connectors::repo::crawl`.

As built:

* `connectors::repo::parse_git_url` accepts `https://host[:port]/path`
  (no user name or password: the operator's git configuration supplies
  credentials), `ssh://[user@]host[:port]/path` (no password) and
  `user@host:path`; it refuses an empty URL, a leading `-`, whitespace and
  control characters, transport helpers (`ext::`, `fd::`, any
  `<transport>::`), `file://`, `http://`, `git://` and other schemes, a
  query or fragment, a host starting with `-` or `.`, a path starting with
  `-` (scp form) and port 0. `--branch` must be a plain branch name
  (letters, digits, `._/-+`, no leading `-` or `/`, no `..` or `@{`).
  A `repo` connector's `url` is a git URL or an absolute directory path
  (the CLI makes a local path absolute; with `--remote` it must already be
  absolute, since it names a directory on the server).
* The clone goes to a temporary directory next to the cache entry and is
  renamed into place, so an interrupted clone leaves no half entry. The
  cache directory is `$XDG_CACHE_HOME` (when absolute), else
  `$HOME/.cache`, else the system temporary directory, then
  `onto/repos/{first 16 hex of sha256(url, NUL, branch)}`: one clone per URL
  and branch, the default branch being its own (a `--single-branch` clone
  fetches only its branch). A fetch names its ref (`HEAD`, the remote's
  default branch, when no branch is set). A crawl holds its clone directory
  from the checkout to the last read of the tree: a process-wide claim on
  the directory and an exclusive lock on `{dir}.lock`, so crawls of the
  same URL and branch by other connectors, tenants or processes sharing the
  cache wait instead of checking out under each other. A fetch or reset
  that fails removes the entry and clones again. The commit is `git
  rev-parse HEAD` of the clone; the branch is `--branch` or the clone's
  current branch. git
  runs with `GCM_INTERACTIVE=never` too, stdin closed, and is killed after
  120 s (`git <verb> timed out after 120 s`); a failure is
  `unavailable: git <verb> failed: <stderr, clipped to 500 characters>`.
* The stamp compared with the stored state is the commit (`commit:<sha>`)
  or, for a directory, a hash of every manifest's path, size and
  modification time and of the CODEOWNERS file's; it also covers the
  connector's name, URL, workspace, title and `repo` options, so changing
  `--include` re-plans. It is kept in `ConnectorState.stamp`. An unchanged
  stamp writes no entity, link or event (only the crawl state: last crawl
  time and count), unless an entity of the last crawl has since been
  deleted or the last crawl failed. The report then says "unchanged since
  the last crawl: nothing written" and repeats the last `repo` statistics.
* Planning runs `catalog::plan_changes` with those `managed_rels`. Because
  `owns` points into packages and the repository, the connector also
  removes `owns` links into a changed package or the repository that
  CODEOWNERS no longer gives (whoever made them), and drops duplicate link
  mutations. Writes go through `Ontology::apply` in batches of 500 as the
  principal `connector:{name}` (role `system`) with source
  `connector:{name}`; when anything was written, OntoRank runs and a
  `connector.crawled` event records the counts and the commit.
* `CrawlReport` gains `repo: {commit, branch, manifests, packages,
  ecosystems, dependencies, owners, codeowners}` (absent for database
  crawls). `connector.remove` with `purge` deletes the packages, the
  repository and the owners the connector created.
* Test-only switches, never set by the server or the CLI:
  `repo::testing_allow_file_urls(true)` accepts `file://` URLs and adds `-c
  protocol.file.allow=always`; `repo::testing_cache_dir(Some(dir))` clones
  into `dir`.
* `onto connect repo` defaults `--name` to the last path or URL segment
  without `.git`, and `--every` to `1h` (`15m` for postgres). `--owner`,
  `--exclude`, `--include` and `--branch` fill `repo`; the crawl prints
  packages per ecosystem, dependencies, owners and the commit.
* Project credentials: `guard_project_call` (section 14) refuses local
  paths and checks git URLs before the connector is stored.

## 14. Security

* **Tenant isolation**: every data-plane request resolves exactly one
  project from its credential (API key → its project; console session →
  `x-onto-tenant` checked against membership; config tokens → their
  `tenants`). A credential for project A addressing project B gets 403. Each
  project is its own PostgreSQL schema.
* **Secrets at rest**: API keys are stored as peppered SHA-256 (they are
  high-entropy, so a slow hash is unnecessary); LLM keys under AES-256-GCM
  envelopes bound to their project; the IdP client secret only in the
  environment; session tokens are not stored.
* **Constant-time comparisons** for key hashes, config tokens, the bootstrap
  token, session HMACs and ticket bindings. Unknown prefix and wrong secret
  return the same 401.
* **No secret in responses, logs or traces**: keys are shown once; logs and
  traces carry key prefixes; `LlmConfig`'s `Debug` redacts the key; the audit
  log never stores secrets; provider errors are clipped.
* **No cookies**: bearer tokens only, so no CSRF; login CSRF is prevented by
  the binding (section 7.2); `state`, `nonce` and PKCE as specified; tickets
  are single use, 60 s, hashed at rest.
* **SSRF**: project LLM base URLs and the git and PostgreSQL URLs of
  connectors registered by project credentials must resolve only to public
  addresses unless `console.allow_private_urls` (LLM base URLs and git URLs
  must also be `https`); for LLM base URLs the check
  runs on the resolved IP before connecting (a custom `reqwest` resolver,
  `onto_control::net::PublicOnly`, that refuses a name when any of its
  addresses is loopback, private, link-local, CGNAT, multicast,
  unspecified, broadcast, documentation or reserved, IPv4 and IPv6, with
  IPv4-mapped and NAT64 addresses judged by their IPv4 part;
  `onto_intelli::net::public_ip`). An IP literal
  host (which the client does not resolve) is checked when the settings are
  stored and when the provider is built. Redirects are not followed, no
  proxy is used, and a base URL may not carry credentials, a query or a
  fragment.
  A connector that an API key or a console session registers is marked
  `restricted` by the server (whatever the input says), and its URL is
  checked when it is registered (`guard_project_call`, below) and again
  before every crawl, which then connects only to the addresses checked:
  * Git URLs (`connectors::repo::restricted_reach`, also behind
    `onto_control::net::check_git_url`): `https://` only (without
    `allow_private_urls`, `ssh://` and `user@host:path` are refused, since
    git would use the server's SSH credentials), and the host, or an IP
    literal, must resolve only to public addresses. git then runs with
    `-c http.followRedirects=false` (a public server cannot redirect the
    clone to an internal one) and, for a host name,
    `-c http.curloptResolve=host:port:ip` with the address checked (git
    2.37 or later), so a name that resolves differently when git connects
    reaches nothing else. With `allow_private_urls`, only redirects are
    turned off.
  * PostgreSQL URLs (`connectors::postgres::check_url`, also behind
    `onto_control::net::check_pg_url`): `postgres://` or `postgresql://`
    with a TCP host and a user; a Unix socket (a host starting with `/`,
    also percent-encoded), an empty host and every query parameter except
    `sslmode` and `application_name` are refused (`host`, `hostaddr` and
    `port` would move the connection, `sslrootcert`, `sslcert` and `sslkey`
    name the server's files, `options` sets server parameters). The
    connection options are built from the URL alone: the password is the
    URL's or empty, never `PGPASSWORD` or `~/.pgpass`, and the database,
    user and SSL mode never come from `PG*` variables. Unless
    `allow_private_urls`, the host must resolve only to public addresses and
    the connection goes to the first of them (with `sslmode=verify-full` it
    goes to the name, whose certificate check then refuses another server).
    The server's `PGSSLCERT`, `PGSSLKEY`, `PGSSLROOTCERT` and `PGOPTIONS`
    still apply (sqlx reads them and has no way to clear them): leave them
    unset on a server whose projects register connectors.
  Connectors registered by project credentials before `restricted` existed
  are not marked; re-register them.
  git also uses the server's own credential helpers for `https://` URLs, so
  a server whose git configuration can read private repositories lets any
  admin-scope key map those repositories' manifests into its project: run
  `onto serve` without git credentials, or with credentials limited to the
  repositories its projects may read.
* **Project calls that touch the server**: a request authenticated by an
  API key or a console session may not register connectors whose URL is
  `env:VAR` (it would read the server's environment) or a filesystem path
  (repository connectors), nor use `mock` providers outside dev mode. The
  check sits in `onto-grpc` (`guard_project_call`) before `Intelli::call`,
  on the HTTP routes and on gRPC routes (including the generic `Call`);
  operators (config tokens, the CLI) are not limited. A path is a URL that
  starts with `file:`, `/`, `.`, `~` or `\`, has no `://` and is not
  `user@host:path`, or an input with a `path` member. For `kind = "repo"`
  the guard also applies `check_git_url`, and for `postgres` (the default
  kind) `check_pg_url` (403 with the reason), and it sets `restricted` on
  the input. Sessions
  minted by keys carry the key's credential, so the guard applies to them
  too. As built (stage 6) the guard is `async fn guard_project_call(credential,
  action, &mut input, allow_private)`; HTTP routes, the generic `Call` and the
  dynamic gRPC routes pass `Auth::allow_private_urls()`.
* **Roles**: enforced on the server for every console route (section 5); the
  UI only hides what the server refuses anyway. The last owner cannot be
  removed or demoted.
* **Revocation**: keys within 10 s on every replica (at once locally);
  members within 30 s; `sign out everywhere` within 30 s.
* **Abuse limits**: 10 key creations per minute per project, 10 LLM tests
  per minute, 20 sign-in starts per minute per client address and 600 in
  all per server process (section 10, console routes, says how the address
  is found), request bodies on console routes ≤ 64 KB.
* **Content capture**: traces store questions and answers only with
  `capture_content` (default on, clipped); a project turns it off in
  Settings. Retention deletes old traces. Trace reads need `can_observe`.
* **Audit**: every control-plane change (members, invitations, keys,
  LLM settings, project settings, deletions, sign-ins) is in `audit_log`;
  data-plane writes keep the existing tenant event log.
* **Dev mode** stays loopback only.

## 15. Setup

### Self-hosted with Clerk as the IdP

1. In the Clerk Dashboard, open **OAuth applications** and add one: name
   `onto-intelli console`, redirect URI
   `https://onto.example.com/v1/console/auth/oidc/callback`, scopes
   `openid profile email`. Copy the client ID and the client secret (Clerk
   shows the secret once; a public, PKCE-only application has none).
2. The issuer is your instance's Frontend API URL (for example
   `https://<slug>.clerk.accounts.dev` in development, or
   `https://clerk.<your-domain>` in production). Check it by opening
   `<issuer>/.well-known/openid-configuration`: its `issuer` field must be
   exactly that URL.
3. Configure and start:

   ```toml
   [engine]
   tenancy = "multi"

   [console]
   enabled = true
   public_url = "https://onto.example.com"

   [[console.oidc]]
   id = "clerk"
   name = "Clerk"
   issuer = "https://<slug>.clerk.accounts.dev"
   client_id = "<client id>"
   client_secret_env = "ONTO_OIDC_CLERK_SECRET"
   ```

   ```sh
   export ONTO_DATABASE_URL=postgres://…  ONTO_SESSION_SECRET=$(openssl rand -hex 32)
   export ONTO_MASTER_KEY=$(openssl rand -base64 32)  ONTO_OIDC_CLERK_SECRET=…
   onto serve --listen 0.0.0.0:50051
   ```

4. Open `https://onto.example.com/ui/`, choose **Continue with Clerk**. The
   first user gets an organization and a first project; invite the team
   from **Members**.

Other IdPs differ only in the issuer: Auth0 `https://<tenant>.auth0.com/`
(note the trailing slash, which Auth0's discovery document carries),
Google `https://accounts.google.com`, Keycloak
`https://<host>/realms/<realm>`, Supabase Auth (OAuth 2.1 server)
`https://<project-ref>.supabase.co/auth/v1`. Always check the discovery
document's `issuer`.

### Self-hosted without an IdP

```sh
export ONTO_ADMIN_TOKEN=$(openssl rand -hex 32)
onto serve        # with [console] enabled and engine.tenancy = "multi"
```

Open `/ui/`, choose **Sign in with admin token**, paste the token. Existing
tenants appear as projects of the `default` organization. For local
development, `onto serve --dev` signs in automatically.

### Connecting a service

```sh
curl -s "$ONTO_URL/v1/ask" -H "authorization: Bearer $ONTO_API_KEY" \
     -H 'content-type: application/json' -d '{"question":"Who owns payments?"}'
```

The **Connect** page shows the same for TypeScript, Python, MCP and the
monorepo connector with the project's URL and key prefix filled in. With the
SDKs (section 12):

```ts
import { Onto } from "onto-intelli";               // sdk/typescript
const onto = new Onto();                           // ONTO_URL, ONTO_API_KEY
const res = await onto.ask("Who owns payments?", { sessionId: "chat-42" });
```

```python
from onto_intelli import Onto                      # sdk/python
res = Onto().ask("Who owns payments?", session_id="chat-42")
```

To map a monorepo, register it with an admin-scope key (the CLI uses
`ONTO_REMOTE` and `ONTO_TOKEN`):

```sh
export ONTO_REMOTE=https://onto.example.com ONTO_TOKEN=$ONTO_API_KEY
onto connect repo https://github.com/acme/platform --every 1h --owner @acme/payments=team:payments
```

An operator on the server can register a local working tree instead
(`onto connect repo /srv/checkouts/platform`), and a private git server
with `console.allow_private_urls` or a config token.

## 16. Test plan

Every test that needs PostgreSQL runs when `DATABASE_URL` (or
`ONTO_TEST_DATABASE_URL`) is set, uses its own control schema
(`onto_control_test_<random>`) and tenants (`t_<random>`), and drops them at
the end, also on failure (a guard's `Drop`). Without a database the same
behaviour is tested on `MemoryControl` and `MemoryStore`.

| Area | Tests |
|---|---|
| Control store | Migrations apply once, are recorded, a newer recorded version is refused; CRUD for every table on memory and PostgreSQL through one shared test body; last owner cannot leave; invitations accept only by verified email or code, expire, are single use; deleted projects disappear from `membership`; `take_login`/`take_ticket` are single use and honour expiry. |
| API keys | Format and parsing; hash with pepper; constant-time check; unknown prefix and wrong secret give the same 401; revoked, expired and deleted-project keys refused; revocation visible at once locally and after the TTL (time-controlled cache); rotation with and without grace; scope → role mapping; `x-onto-tenant` mismatch 403; impersonation limited to `impersonate_roles`; `last_used_at` batched. |
| Auth | Existing tests unchanged; console sessions resolve roles per project, refuse non-members, honour `sessions_valid_after`; old session tokens without `console` still verify. |
| OIDC | A mock IdP inside the test (axum on 127.0.0.1: discovery, JWKS, authorize redirect, token endpoint) with keys generated by `aws-lc-rs` (ES256, Ed25519) and the RS256 example of RFC 7515 appendix A.2 for RSA verification; full flow start → callback → exchange; rejected: wrong `iss`, wrong `aud`, multi-`aud` without `azp`, expired, future `iat`, wrong nonce, `alg: none`, `HS256`, unknown `kid` (with one JWKS refetch), tampered payload, replayed state, replayed ticket, wrong binding, `return_to` not starting with `#/`, unverified email with `allowed_email_domains`; signup off; first sign-in creates organization and project. As built: `crates/onto-grpc/tests/oidc.rs` (the flow, on `MemoryControl` and, with `DATABASE_URL`, on `PgControl` in a dropped schema; also `crit`, a JWK `alg` mismatch, EdDSA, userinfo, invitations by verified email and by code, the IdP's own `error=`, the start rate limit, an unreachable IdP and the startup checks) and the unit tests of `onto_control::oidc` (the RFC 7515 A.2 vector, PS256 refusing a PKCS#1 signature, key selection, every claim rule, discovery rules, the RFC 7636 PKCE vector). |
| Vault | Round trip; wrong project id (AAD) fails; tampered byte fails; previous master key decrypts; rotation rewraps; no master key → refusal. As built: the unit tests of `onto_control::vault` and, through the console, `tests/byo_llm.rs`. |
| BYO LLM | A mock OpenAI-compatible server (axum) checks that the project key is sent and the server default is used without settings; settings change takes effect after `refresh_tenant`; test connection success and clipped failure; budget exceeded → `Fallback` with note and no provider call; rate limit → 429 with `retry-after` over HTTP and `RESOURCE_EXHAUSTED` over gRPC; SSRF guard refuses `http://127.0.0.1` unless allowed; `api_key_env` cannot be set by a project; `headers` refuses credential names. As built: `crates/onto-grpc/tests/byo_llm.rs` (the flow on `MemoryControl` and, with `DATABASE_URL`, on `PgControl` in a dropped schema; tests turn `allow_private_urls` on to reach the mock; also the 30 s poll, the stored key never sent to another base URL, a key that does not decrypt, usage rows and month to date, streaming 429, the LLM test limit, the audit log and `Debug` redaction; a second server with the guard on and no master key: `http://` and private IP literals refused, `https://localhost` refused where it resolves, the exact no-master-key message; master key rotation), `crates/onto-intelli/tests/hub.rs` (`TenantHooks` with a counting provider and meter: refresh, budget, rate limit), the unit tests of `onto_control::{net, hooks, llm}`, `onto_intelli::{limits, config, llm}` and `onto_core::error`, the LLM routes in the role table of `tests/console.rs`, and the `project_llm` and `usage_daily` methods in the shared control store test. |
| Traces | One ask produces the documented spans with token counts; deterministic, cached, LLM, fallback and error asks; streaming asks; action traces; nested generation spans for `ontology.refine`; `traceparent` adopted; session id from header, request and conversation; `capture_content = false` stores no text; list filters and keyset paging; metrics (percentiles, buckets, by model, deterministic share) equal on memory and PostgreSQL for the same fixture; scores; retention deletes old rows only; append-only trigger refuses `UPDATE`; queue overflow counted, not blocking; OTLP JSON body shape against a mock collector (when built). As built: `crates/onto-core/tests/traces.rs` (one fixture through `MemoryStore` and, with `DATABASE_URL`, `PgStore` in a dropped tenant: filters including escaped `ILIKE` wildcards and Unicode, keyset paging, details, sessions, daily and hourly metrics, scores and retention compared equal; `UPDATE` refused on the three tables), `crates/onto-intelli/tests/traces.rs` (spans and tokens of an LLM ask, deterministic, answer cache, completion cache, error and failing-model asks, a reached budget, streaming, `traceparent`, sessions and users, `capture_content = false`, clipping at a character boundary, action and call traces with `[traces] calls`, `ontology.refine` and `ontology.advise`, eval scores, the trace actions and their access rules, retention, `traces_dropped`), the unit tests of `onto_core::trace` (`percentile_cont` against PostgreSQL's values, cursors, `traceparent`, windows), `onto_intelli::trace` (queue overflow without blocking, batching by size and time, clipping, context merging) and `onto_control::hooks` (trace settings), and `crates/onto-grpc/tests/observe.rs`. OTLP is not built. |
| HTTP and gRPC | Console routes: each role allowed and refused per section 5 (table-driven, `GET …/connect` included); observability routes (as built, `tests/observe.rs`: a console session, an admin-scope key and a read-scope key, 403; `traceparent`, `x-onto-session-id` and the key prefix on traces; `x-onto-trace-id` on HTTP and gRPC responses); `ObservabilityService` through the generated client; `POST /mcp` `initialize` and `tools/list` with an agent key; `guard_project_call` refuses `env:` connectors from keys. |
| Repo connector | Fixture monorepos under `crates/onto-intelli/tests/fixtures/repos/` (Cargo workspace with `workspace = true` path deps, npm and pnpm workspaces, `go.work`, uv workspace, CODEOWNERS with overlapping rules); expected packages, `depends_on` with kinds, owners; second crawl unchanged writes nothing; removing a package deletes it and its links; git URL validation refuses `ext::`, `file://`, options; a local bare repository clone (`file://` allowed only in tests through a test-only switch). As built: `crates/onto-intelli/tests/repo.rs` on copies of the fixtures in temporary directories (`cargo`: workspace members and `exclude`, inherited version and description, `publish = false`, normal, dev, build and `[target.*]` dependencies, CODEOWNERS in `.github/` with overlapping rules, an email owner, provenance; `npm`: workspaces, a non-member, `tsconfig.json`, normal over optional; `pnpm`: a `!` exclusion and a peer dependency; `go`: `go.work` with a module it does not use, `require` plus `replace`; `uv`: the root project, an excluded member, PEP 503 names, optional and dependency-group kinds, a Poetry member; incremental crawls: an unchanged crawl leaves the ontology revision and history as they were, a removed package and its links and owners go, moved ownership, untouched packages keep `updated_at`, changed `include` re-plans, purge; existing owners and the owner map; the walk skips `node_modules`, `target`, `dist`, `build`, `vendor`, `.venv`, hidden directories, `--exclude`, symlinks, levels below 12 and manifests over 1 MB; refusals: no `builtin:code`, bad URLs and branches, a relative path, a missing directory, a non-admin; a local bare repository cloned through `file://` with the test switch, unchanged at the same commit, fetched after a push, and a missing repository's git error), the unit tests of `onto_intelli::connectors::repo` (git URLs including `ext::`, `file://`, `--upload-pack`, credentials and IPv6; branch names; path globs and gitignore-style coverage; CODEOWNERS parsing and last-rule-wins; owner ids; names and ids; `pnpm-workspace.yaml`; `go.mod` blocks), `onto_control::net` (`check_git_url`), `onto_grpc::auth` (the guard on git URLs with and without `allow_private`) and `crates/onto-grpc/tests/console.rs` (an admin-scope key over HTTP: private, scp-like and path sources refused with 403, a public https URL accepted). |
| UI | `npm run build` (type check) passes; manual check at 375 px and desktop, light and dark, keyboard only: sign in (dev), create project, create key, copy snippet, ask through curl, see the trace and its waterfall, set budget, test LLM against the mock provider. As built: done in stage 5 against `onto serve --dev` with memory storage on loopback (also rotate and revoke a key, add a score, sign out and back in, the callback's error page, an unknown invitation, and no horizontal scrolling on any console page at 375 px). |
| SDKs | `cd sdk/typescript && npm test`; `cd sdk/python && python3 -m unittest`; both against their mock servers offline; optional live run against `onto serve --dev` with `ONTO_URL`. As built: see section 12; `python3 -m unittest discover -s tests`; the live tests ran against `onto serve --dev` with memory storage on loopback (health, ask, stream, search, a 404, traces and metrics, the session recorded with its user). |
| Gate | `cargo fmt --all --check`; `cargo clippy --workspace --all-targets --locked -- -D warnings`; `DATABASE_URL=postgres://onto:onto@127.0.0.1:5441/onto cargo test --workspace --locked`; `env -u DATABASE_URL cargo test --workspace --locked`; `cd ui && npm ci && npm run build` with `ui-dist` matching; `cd sdk/typescript && npm test`; `cd sdk/python && python3 -m unittest discover -s tests`; `onto eval` on a fresh tenant all-pass (with `builtin:code` in the default layers), then drop its schema. |

## 17. Implementation stages

Six stages, in order. Each ends with the full gate green and leaves the
system usable; later stages build on earlier ones and do not rework them.

1. **Control store, API keys, bootstrap sign-in.** `onto-control` crate,
   migrations `0001`, `MemoryControl` and `PgControl`, organizations,
   members, invitations, projects as tenants, API keys and their
   authentication, async `Auth`, console sessions with the bootstrap token
   and dev sign-in, console routes for everything except LLM and usage,
   audit log, `onto console` CLI.
2. **OIDC sign-in.** Discovery, PKCE, JWKS, ID token verification with
   `aws-lc-rs`, binding and tickets, signup and invitation rules, refresh
   and logout, mock-IdP tests, sign-in page support in the providers route.
3. **Bring your own LLM.** Vault, migration `0002`, `TenantHooks`,
   swappable providers, test connection, budgets, rate limits and
   `Error::Limited`, usage metering, SSRF guard. Built; the trace field of
   `TenantSettings` and `budget_exceeded` on traces come with stage 4.
4. **Traces.** Tenant migration `0005`, `Store` trace methods, recorder and
   sink, retention, trace actions, HTTP routes, `ObservabilityService`,
   scores from evals, optional OTLP export. Built, without the optional
   OTLP export (section 9.6).
5. **Console UI and MCP over HTTP.** All console pages, project switcher,
   charts, waterfall, Connect snippets, `/v1/console/projects/{ref}/connect`,
   `POST /mcp`, rebuilt `ui-dist`. Built; the SDKs and `onto connect repo`
   that the Connect page shows came with stage 6 (which also changed the
   Monorepo tab to the CLI's `ONTO_REMOTE` and `ONTO_TOKEN`).
6. **SDKs, repository connector, docs.** TypeScript and Python SDKs with
   tests, `builtin:code` and `onto connect repo`, README section, SDK
   READMEs, `docs/UI.md` and this document brought in line with what was
   built. Built; the optional OTLP export of section 9.6 remains not built.
