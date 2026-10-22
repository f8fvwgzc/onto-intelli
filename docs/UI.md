# Web UI

`onto serve` serves an operator UI at `/ui`, embedded in the binary. It has
six views:

| View | What you do there |
|---|---|
| **Map** | Explore the workspace graph: entities coloured by kind, kind filters, search (⌘K or `/`), click to inspect, double-click or `e` to expand a neighbourhood. **Path** traces the shortest connection between two entities; **Impact** shows what depends on an entity and its likely root cause; **History** replays the graph around the selection at any past time. |
| **Explorer** | The Object Explorer. The start page lists every object type by category, with how many objects you can see, its backing table and its numbers of properties, links and actions (filter by name, or show only types with objects). A type opens a table of its objects: search, sortable columns (pick which ones; the choice is remembered per type in this browser), pages of 50, and a side panel of facets (click values to filter, several values of one facet mean any of them) and numeric statistics with histograms (click a bar to filter that range). Filters on the title, the state or any property are added by hand too, and stay in the address; the panel also lists the type's link types. A row opens the entity page; its action button runs an action type that takes the object (parameters, plan with checks and risk, then apply), and **New** runs a create action. ([ONTOLOGY_BUILDER.md](ONTOLOGY_BUILDER.md#object-explorer)) |
| **Entities** | Browse by kind or search. Each entity page shows its properties, computed values, relations and full version history, with as-of views. |
| **Schema** | Edit object types and typed properties, link types (from/to, cardinality, inverse), action types (parameters, preconditions, effects, roles, risk), rules and interfaces. Edits collect in a change set. Preview its impact and migrations, then **apply** it (apply roles) or **propose** it for review. Versions lists every schema version, with undo and restore. |
| **Builder** | The ontology builder. Connectors with their crawl status and last report, **Crawl** for admins, and **Add connector** (name, `env:VAR` or URL, schemas, schedule, grants). Pick a source and **Draft**: options for tables, exclusions, a name prefix, row sync, actions and views. Review the object types (rename them, leave out types or properties), links, actions and skipped tables, each with its reason; a new name that an object type, interface or action type of the schema already has is flagged, unless that type is this table's earlier draft. **Refine with AI** (with optional instructions) shows each suggestion and whether it was kept; refining drafts afresh, so toggles reset. The advisor answers modeling questions. **Review plan** shows the `schema.plan` impact, then **Apply** (apply roles) or **Propose for review** (title and rationale, then the proposal opens). After applying, admins can crawl the source to sync its rows; a source with no registered connector says so instead. ([ONTOLOGY_BUILDER.md](ONTOLOGY_BUILDER.md)) |
| **Proposals** | The review inbox: rationale, impact against today's data (re-planned when stale), diff, reviews. Approvers approve or reject; the proposer can withdraw. |

It works at phone width (bottom tab bar, bottom-sheet panels), honours
`prefers-reduced-motion` and the system light/dark preference, and every
control is keyboard reachable.

## The console

When the server runs the control plane (`[console] enabled`, see
[CONTROL_PLANE.md](CONTROL_PLANE.md)), `/ui` opens as a console: people sign
in, pick an organization and a project, and the six views above work on that
project (its ref becomes the tenant). The shell adds an **organization and
project switcher** at the top of the sidebar (in the header on phones), a
**Project** group above the ontology views, and **Members** and the user menu
(sign out, sign out everywhere) at the bottom. Controls a role cannot use are
disabled, with a tooltip naming the role they need; the server enforces the
roles in any case.

| Route | What you do there |
|---|---|
| `#/signin` | Sign in: a button per identity provider (OpenID Connect), the admin token form when `ONTO_ADMIN_TOKEN` is set, automatic sign-in as the developer under `onto serve --dev`. **Use an operator token instead** opens the token form of the operator UI. |
| `#/auth/callback` | Where the identity provider sends you back: the ticket is exchanged for a session and removed from the address, then the page you started from opens. Refused sign-ins say why. |
| `#/invite?code=…` | The organization and role of an invitation, and the provider buttons that accept it. |
| `#/projects` | The organization's projects with their requests over the last 7 days; **New project** (optionally with the demo data) and **New organization**. |
| `#/overview` | Requests, error rate, latency, tokens, cost, deterministic answers and cache hits for 24 hours, 7 or 30 days, each against the previous period; charts of requests and errors, latency p50 and p95, cost by model; recent errors; a "Get connected" checklist until the first trace arrives. |
| `#/traces` | Every traced call: search (`/`), time range, status, mode, and name, user, session, model and tag filters, all kept in the address; a latency histogram of the rows shown; `j` and `k` move through rows and Enter opens one; **Load more** pages; **Live** refreshes every 10 seconds. |
| `#/traces/<id>` | The trace's status, latency, tokens, cost, user, session and API key prefix; the question and answer; a waterfall of its spans by start and duration, coloured by kind (select one for its attributes, input, output and tokens; Esc closes the panel); scores, with **Good**, **Bad** and **Add score** for developers and above. |
| `#/sessions`, `#/sessions/<id>` | Sessions with their turns, errors, tokens and cost; a session replays each question and answer with its latency and mode and links to its trace. |
| `#/usage` | A month: cost and tokens against the budgets (a warning at 80 %), asks, LLM calls, deterministic answers, the rate limit, daily stacked bars by model (cost, tokens or asks) and a table by model. |
| `#/connect` | Ready-to-paste snippets with the project's URL and a key: curl, TypeScript and Python (dependency-free code first, then the SDKs of `sdk/typescript` and `sdk/python`), MCP (`claude mcp add --transport http …` and a generic `mcp.json`), environment variables and the monorepo connector (`onto connect repo` through the CLI with an admin-scope key). Pick a key to fill in its prefix, or **Create a key** to put its full secret in the snippets once. |
| `#/keys` | API keys: name, prefix, scopes, creator, last use, expiry and status. **Create key** (scopes, workspaces, expiry, impersonation for admins) shows the secret once; **Rotate** (with an optional grace period for the old key) and **Revoke** (confirmed). |
| `#/llm` | The project's own provider, model, base URL and API key (write-only: it shows `•••• last4`, with **Replace** and **Remove**), request settings, prices, budgets and rate limit; **Test connection** shows the result in place; **Use server default** removes the project's settings. |
| `#/members` | The organization's members and their roles; pending invitations; **Invite** returns a link to send (the server sends no email, and keeps only a hash of the code, so a link can be copied only right after it is made). |
| `#/settings` | Project name, default workspace (the console opens the ontology views in it), trace retention, content capture, the organization's name, its audit log (admins) and **Delete project** (owners, typing the name). |

The console keeps its session in `sessionStorage` like any other token and
renews it while the tab is open, once less than a quarter of its lifetime is
left. Sign-in through an identity provider binds the login to the tab: 32
random bytes in `sessionStorage` (`onto.login`), whose SHA-256 goes with the
start of the sign-in and the bytes themselves with the exchange, so a callback
link opened in another browser cannot be used. Embedded views, link tokens and
operator tokens keep the operator UI described above.

## Signing in

The UI calls the same HTTP API, under the same principal and token rules as
any other client. Credentials come from one of three places:

1. **An embedding page.** It posts
   `{ type: "onto:session", token, tenant, workspace }` with `postMessage`,
   from an origin listed in `[ui] embed_origins`. The UI announces itself
   with `{ type: "onto:ready" }`, and `{ type: "onto:focus", id }` moves the
   map to an entity.
2. **A link.** `/ui/#/map?token=…&tenant=…&workspace=…&focus=project:alpha`.
   The token is removed from the address as soon as the page reads it.
3. **The sign-in form**, for operators pasting a token.

The token is kept in `sessionStorage`, so it lasts only for the tab. In dev
mode (`onto serve --dev`) no token is needed.

### Sessions for browsers

A backend should never hand its service token to a page. Instead it mints a
short-lived session for the user:

```bash
curl -s https://onto.internal/v1/auth/session \
  -H "authorization: Bearer $NEXC_ONTO_TOKEN" -H 'content-type: application/json' \
  -d '{"act_as": "user:42", "roles": ["member", "approver"], "entity": "person:42",
       "tenant": "ws_3f2a", "ttl_secs": 3600}'
```

The response is `{token: "onto1.…", expires_at, principal, tenant}`.

- A session token carries exactly one principal and one tenant, signed with
  HMAC-SHA256.
- It cannot impersonate anyone, and requests to another tenant are refused.
- It expires after `ttl_secs`, at most `auth.session_max_ttl_secs` (12 hours
  by default).
- Roles are limited by the minting token's `impersonate_roles`. A token
  without `impersonate` can only mint a session for itself.
- Set `ONTO_SESSION_SECRET` (32 characters or more) to the same value on
  every replica, so a session minted on one is accepted by all. When it is
  unset, each process signs with a random key of its own.

`GET /v1/auth/whoami` returns the principal, tenant and capabilities (write,
apply schema, approve, propose). The UI hides or disables actions the caller
may not take; the server enforces them regardless.

## Embedding in nexc

```toml
[ui]
embed_origins = ["https://nexc.example.com"]   # CSP frame-ancestors + postMessage origins
title = "Ontology"
```

```html
<iframe id="onto" src="https://onto.internal/ui/#/map?workspace=main&focus=project:alpha"></iframe>
<script>
  const frame = document.getElementById("onto");
  window.addEventListener("message", (e) => {
    if (e.origin === "https://onto.internal" && e.data?.type === "onto:ready")
      frame.contentWindow.postMessage({ type: "onto:session", token: sessionFromBackend, tenant: "ws_3f2a" }, "https://onto.internal");
  });
</script>
```

You can also skip the iframe and link to `/ui/#/…` in a new tab, with the
session in the fragment.

**Deep links:**

- `#/overview?range=24h|30d`, `#/traces?status=error&q=…`, `#/traces/<id>`, `#/sessions/<id>`, `#/usage?month=2026-10`, `#/connect?tab=mcp` (console)
- `#/map?focus=<id>&mode=impact|path`
- `#/entities/<id>`
- `#/explore` (object types), `#/explore/<type>?q=…&sort=-prop&f=<filters as JSON>`
- `#/build`
- `#/schema/object_types/<name>`
- `#/proposals/<id>`

**Response headers.** `/ui` sends a strict CSP (`default-src 'self'`,
`frame-ancestors 'self'` plus the configured origins), `nosniff` and
`no-referrer`. Hashed assets are cached as immutable; `index.html` is
`no-cache`.

## Building

The built UI is committed in `crates/onto-grpc/ui-dist`, so
`cargo install` needs no Node. To change the UI:

```bash
cd ui && npm ci
```

```bash
npm run dev       # http://127.0.0.1:5173/ui/, proxying the API to ONTO_URL (default 127.0.0.1:50051)
```

```bash
npm run build     # type-check, then write crates/onto-grpc/ui-dist; rebuild the binary to embed it
```

The stack is React 19, TanStack Query, d3-force (map layout), Tailwind 4,
Geist and Lucide icons: about 145 KB of gzipped JavaScript for the operator
views. The console pages load on first use (about 35 KB more); their charts
are small SVG components (`src/components/charts.tsx`) coloured from the CSS
tokens, with no chart library.
