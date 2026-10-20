# Web UI

`onto serve` serves an operator UI at `/ui`, embedded in the binary. It has
four views:

| View | What you do there |
|---|---|
| **Map** | Explore the workspace graph: entities coloured by kind, kind filters, search (⌘K or `/`), click to inspect, double-click or `e` to expand a neighbourhood. **Path** traces the shortest connection between two entities; **Impact** shows what depends on an entity and its likely root cause; **History** replays the graph around the selection at any past time. |
| **Entities** | Browse by kind or search. Each entity page shows its properties, computed values, relations and full version history, with as-of views. |
| **Schema** | Edit object types and typed properties, link types (from/to, cardinality, inverse), action types (parameters, preconditions, effects, roles, risk), rules and interfaces. Edits collect in a change set. Preview its impact and migrations, then **apply** it (apply roles) or **propose** it for review. Versions lists every schema version, with undo and restore. |
| **Proposals** | The review inbox: rationale, impact against today's data (re-planned when stale), diff, reviews. Approvers approve or reject; the proposer can withdraw. |

It works at phone width (bottom tab bar, bottom-sheet panels), honours
`prefers-reduced-motion` and the system light/dark preference, and every
control is keyboard reachable.

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

- `#/map?focus=<id>&mode=impact|path`
- `#/entities/<id>`
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
Geist and Lucide icons: about 115 KB of gzipped JavaScript.
