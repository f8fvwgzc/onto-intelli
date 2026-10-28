# Change feed, webhooks, Automate and writeback

Phase 1 of docs/design/EVENTS.md is built: a change feed, signed webhooks
delivered from an outbox, change-triggered automations, and asynchronous HTTP
writeback. This page shows each one working, with output from real runs
(trimmed; ids and times are the ones the run produced), says what it promises
and what it does not, and gives the benchmark numbers.

## What it promises

These sentences are the contract. Outputs, the CLI and the configuration
template use the same words.

- Each change is enqueued once per endpoint and delivered at least once; dedupe on the event id.
- Change records are computed by the build that wrote them; a commit from a build that did not
  record changes appears as a gap.
- Consistent resync is not offered in this version.
- Writeback is not reversible; action.undo does not undo the external write.
- Automation definitions are visible to everyone who can read the schema.
- Delivery concurrency limits are per process.

Also not offered in phase 1: per-endpoint ordering (the run below shows a
create arriving after a later update), WebSocket and gRPC streaming, the UI
live tail, triggers other than `change`, schedules, LLM or agent steps,
synchronous external effects, SQL writeback and `changes.snapshot`.

## How the examples were run

- 2026-10-28, an Apple M2 Pro (10 cores, 16 GB, macOS 26.6.2), PostgreSQL
  16.15 with pgvector 0.8.7 in Docker on the same machine (`127.0.0.1:5441`).
- A debug build of the CLI (`cargo build -p onto-intelli-cli`), `onto serve`
  on `127.0.0.1:50071` with token auth, tenant `docsrun`, schema
  `builtin:enterprise`, `[llm] provider = "none"`.
- A local receiver: a 20-line Python `http.server` on `127.0.0.1:18091` that
  records each request's path, headers and raw body and answers 200.
- Two config tokens: `operator` (admin in every workspace) and `ops-bot`
  (an agent in `acme`). The CLI talks to the server with `--remote` /
  `ONTO_REMOTE` and `ONTO_TOKEN`.
- Signing secrets are `env:` references resolved by the server
  (`env:DOCS_HOOK_SECRET`, `env:DOCS_DESK_SECRET`, each a `whsec_` secret of
  32 random bytes). A server without `ONTO_MASTER_KEY` has no sealer, so
  plaintext and generated secrets are refused there; the operator registers
  references instead.

The parts of the scratch configuration that matter here:

```toml
[engine.postgres]
tenant = "docsrun"

[auth]
mode = "token"
[[auth.tokens]]
token_env = "ONTO_TOKEN"
principal = { id = "operator", roles = ["admin"], workspaces = ["*"] }
[[auth.tokens]]
token_env = "ONTO_TOKEN_AGENT"
principal = { id = "ops-bot", roles = ["agent"], workspaces = ["acme"] }

[webhooks]
poll_secs = 2
backoff_base_secs = 2
backoff_max_secs = 10

# The local receiver: an operator rule lets this one host reach loopback.
[[webhooks.allow]]
host = "127.0.0.1"
cidrs = ["127.0.0.1/32"]

[writeback]
enabled = true
```

Without the `[[webhooks.allow]]` rule the receiver is refused, as any
loopback or private address is (see [Refused](#refused)).

## 1. The change feed

Every commit that changes entities, links or the schema writes change
records in the same transaction. A cursor is `<commit seq>.<n>`; a reader
resumes after the last cursor it saw.

```text
$ onto watch --once --after 13.0
14.0 schema.change  schema 3  version 3
15.0 entity.update  issue issue:pay-1  in_progress → resolved
17.0 entity.create  issue issue:pay-3
18.0 entity.update  issue issue:pay-3  in_progress → blocked
next: 18.0  (head 19, floor 0; resume with --after)
```

The same over HTTP as a server-sent event stream (`GET` or `POST
/v1/changes/stream`; `retry:` and `id:` let a client resume):

```text
$ curl -N -H "authorization: Bearer $ONTO_TOKEN" \
    "http://127.0.0.1:50071/v1/changes/stream?after=16.0&kinds=issue"
retry: 2000

event: change
data: {"cause":{"actor":"operator","request_id":"req_1a181de46ef0000048964","source":"api"},"cursor":"17.0","entity":{…,"id":"issue:pay-3","state":"in_progress","title":"Refund stuck",…},"id":"issue:pay-3","kind":"issue","op":"entity.create","revision":1,…}
id: 17.0

event: change
data: {"before":{"state":"in_progress"},…,"changed":["state"],"cursor":"18.0",…,"op":"entity.update",…}
id: 18.0
```

- Records are filtered per reader: a record whose entity the reader cannot
  see is not shown, `before` is only present when the reader could see the
  entity before and after, and an entity that leaves the reader's view
  arrives as `entity.hidden`.
- Change records are computed by the build that wrote them; a commit from a
  build that did not record changes appears as a gap. A bulk import is a
  `gap` record too (`reason: bulk_import`).
- Retention (`[changes] retain_days`, `retain_max_rows`) removes whole
  commits and raises the tenant's `floor` in the same transaction. A cursor
  below the floor is expired: poll and stream answer `reset`. Consistent
  resync is not offered in this version: re-read with `entity.find`, whose
  result is not pinned to a cursor.
- Capture is a tenant property (`changes.capture`, admins of every
  workspace), not process configuration, so replicas never disagree.

## 2. Webhooks

### Register an endpoint

An endpoint is a URL, a signing secret, an optional change filter and the
principal the receiver acts as (`webhook:<name>`): it receives only what
that principal may see.

```text
$ onto webhook add ops http://127.0.0.1:18091/hooks/ops \
    --secret env:DOCS_HOOK_SECRET --kind issue --sees acme
Added ops (active), secret "env:DOCS_HOOK_SECRET"
Each change is enqueued once per endpoint and delivered at least once; dedupe on the event id.

$ onto webhook list
ops  http://127.0.0.1:18091/hooks/ops  active  as viewer in acme  changes of issue  v1793179877333
```

A new subscription starts at the head of the feed (`"cursor": "3.0"` in
`onto webhook list --json`); it does not replay older changes.

### A change and its signed CloudEvent

```text
$ onto do set_state item=issue:pay-1 state=in_progress
applied set_state act_1a181d9add30000099dea
  • update issue:pay-1: state triaged → in_progress
  • record event state.changed on issue:pay-1
  risk low · reversible
```

What the receiver got (headers, then the body):

```text
POST /hooks/ops
content-type: application/cloudevents+json
user-agent: onto-intelli/0.2.0
webhook-id: obx_1a181d9adee00000d6e50
webhook-signature: v1,Xwaxhb2VXhODSa9ro9vlrWEYBLFRDEDPOSZyJkTGJGs=
webhook-timestamp: 1793179889
```

```json
{
  "specversion": "1.0",
  "id": "obx_1a181d9adee00000d6e50",
  "source": "onto-intelli://docsrun",
  "type": "onto.entity.update",
  "subject": "issue:pay-1",
  "time": "2026-10-28T09:31:29Z",
  "datacontenttype": "application/json",
  "tenant": "docsrun",
  "workspace": "acme",
  "cursor": "4.0",
  "data": {
    "cursor": "4.0",
    "op": "entity.update",
    "id": "issue:pay-1",
    "kind": "issue",
    "workspace": "acme",
    "revision": 2,
    "changed": ["state"],
    "before": { "state": "triaged" },
    "state": { "from": "triaged", "to": "in_progress" },
    "cause": { "actor": "operator", "intent": "action:set_state",
               "request_id": "req_1a181d9add300000894d0", "source": "api" },
    "entity": { "id": "issue:pay-1", "kind": "issue", "title": "Payment timeout on checkout",
                "state": "in_progress", "props": { "priority": "high" }, "revision": 2, … },
    "ts": 1793179889107
  }
}
```

The envelope is CloudEvents 1.0 (structured JSON); the signature is Standard
Webhooks: `v1,` and base64 of HMAC-SHA256 over `<webhook-id>.<webhook-timestamp>.<body>`
(two space-separated values while a secret is rotated). A `whsec_` secret is
base64 after its prefix; any other secret is used as its bytes. The `id` is
signed, so it is safe to dedupe on.

`data` is rendered when the row is sent, from the change record and the
entity version that change wrote, as the endpoint principal sees it at that
moment: a row whose entity the endpoint can no longer see is cancelled
(`not_visible`), and a row whose change retention removed is dead
(`expired`).

### Verify it

With the Python SDK (`sdk/python`, standard library only):

```python
from onto_intelli.webhooks import verify_webhook

event = verify_webhook(os.environ["DOCS_HOOK_SECRET"], headers, raw_body)
```

```text
verify_webhook: ok, id obx_1a181d9adee00000d6e50
wrong secret: refused (no webhook-signature matches)
```

With the TypeScript SDK (`verifyWebhook` in `sdk/typescript/src/webhooks.ts`,
run with `node --experimental-strip-types` on Node 22.12) over every request
the receiver recorded in this run. The run took longer than the 300 s
timestamp tolerance, so this check passed each delivery's own
`webhook-timestamp` as `now`:

```text
/hooks/ops  verifyWebhook: ok  obx_1a181d9adee00000d6e50  onto.entity.update
/hooks/pager  verifyWebhook: ok  obx_1a181dacdc500001d2331  onto.issue.escalated
/hooks/ops  verifyWebhook: ok  obx_1a181db14a8000024cc9b  onto.schema.change
/hooks/ops  verifyWebhook: ok  obx_1a181db14a80000257f31  onto.entity.update
/hooks/ops  verifyWebhook: ok  obx_1a181db14a80000234c60  onto.entity.create
/hooks/ops  verifyWebhook: ok  obx_1a181dc394700002e8310  onto.schema.change
/desk/tickets/close  verifyWebhook: ok  obx_1a181dc7201000033d442  (plain JSON body)
/hooks/ops  verifyWebhook: ok  obx_1a181dc7220000036fa2e  onto.entity.update
/hooks/ops  verifyWebhook: ok  obx_1a181deccb200000c4c61  onto.entity.update
/hooks/ops  verifyWebhook: ok  obx_1a181deccb200000b5b7c  onto.entity.create
```

Two things this list shows. The `ops` endpoint filters on `kind = issue` and
still receives `onto.schema.change` records (schema changes reach every
subscription, so a mirror can follow renames). And deliveries are not
ordered per endpoint: rows `…234c60` (the create of `issue:pay-2`, cursor
6.0) and `…257f31` (its update, cursor 11.0) were claimed together and
reached the receiver in the opposite order. Order by `cursor` if it matters.

### Deliveries

```text
$ curl -H "authorization: Bearer $ONTO_TOKEN" http://127.0.0.1:50071/v1/webhooks/ops/deliveries
{
  "items": [
    { "id": "obx_1a181d9adee00000d6e50", "kind": "webhook", "source": "change", "target": "ops",
      "cursor": "4.0", "dedupe": "4.0", "event_type": "onto.entity.update",
      "state": "delivered", "attempts": 1, "last_status": 200,
      "created_at": 1793179889137, "delivered_at": 1793179889156, … }
  ],
  "name": "ops",
  "note": "Each change is enqueued once per endpoint and delivered at least once; dedupe on the event id."
}
```

The commit was at `ts` 1793179889107: fan-out enqueued the row 30 ms later
and the receiver had it 49 ms after the commit (debug build, one
observation, not a benchmark).

How delivery behaves (configured in `[webhooks]`; tested in
`crates/onto-intelli/tests/webhooks*.rs`, not exercised in this run):

- 2xx is delivered. 408, 409, 425, 429, 5xx, timeouts and connection errors
  retry at `min(base * 2^(attempt-1), max)` with jitter, honouring
  `Retry-After` up to the maximum; other 4xx are dead; 410 also disables the
  endpoint; after `max_attempts` (15) a row is dead. Every dead row writes a
  `webhook.dead` event.
- `pause_after_failures` (20) consecutive failures pause the endpoint for
  `pause_secs` (300), then one probe row is sent; a failed probe doubles the
  pause, up to an hour.
- An endpoint with more than `pending_cap` undelivered rows is paused
  (`backlogged`) and keeps its cursor; if its cursor expires meanwhile it
  gets one `onto.changes.gap` delivery and moves to the floor.
- Delivery concurrency limits are per process: `max_in_flight_per_endpoint`
  (4) and `max_in_flight_per_tenant` (8) apply in each replica that holds the
  tenant.
- `webhook.replay`, `webhook.resume`, `webhook.rotate_secret`, `webhook.test`
  and the tenant switch `webhook.kill` are admin actions; `[webhooks] enabled
  = false` stops sending in one process (rows wait in the outbox).
- `traceparent` is not sent in this version. The worker adds it when a row
  carries a trace id, but no path that enqueues rows records one yet.

## 3. Automations

An automation is schema: `[automations.<name>]`, added with `put_automation`
through `schema.apply` or a proposal. Running it is operational state: a
person enables it, the enablement binds to everything it reaches, and it
expires. Automation definitions are visible to everyone who can read the
schema; they name endpoints and action types, never URLs or secrets
(validation refuses `env:`, `file:` and `sealed:` strings).

### Define

`escalate.toml`, a change set:

```toml
[[changes]]
op = "put_automation"
name = "escalate_blocked"

[changes.def]
description = "When a high-priority issue becomes blocked, open an unblock task and page the on-call."
run_as = { roles = ["member"], workspaces = ["acme"] }   # runs as automation:escalate_blocked
max_depth = 2
rate = { per_minute = 30, per_subject_per_hour = 10 }

[changes.def.trigger]
on = "change"
kinds = ["issue"]
ops = ["entity.update"]
state = { to = ["blocked"] }

[[changes.def.conditions]]
check = "prop_eq"
param = "subject"
prop = "priority"
value = "high"

[[changes.def.effects]]      # at most one action effect
action = { type = "create_task", params = { title = "Unblock: {subject.title}", project = "project:checkout", priority = "high" } }

[[changes.def.effects]]      # any number of webhook effects
webhook = { endpoint = "pager", event = "issue.escalated", payload = { id = "{trigger.id}", title = "{subject.title}", from = "{trigger.state.from}" } }
```

The webhook effect sends to `pager`, an endpoint without a filter (effect
deliveries only) whose principal is `member` in `acme`: an endpoint must
dominate the automation's `run_as`, since the payload may hold anything the
run could read. `ops` (a viewer) would be refused at enable time.

```text
$ onto webhook add pager http://127.0.0.1:18091/hooks/pager \
    --secret env:DOCS_HOOK_SECRET --role member --sees acme
Added pager (active), secret "env:DOCS_HOOK_SECRET"

$ onto schema plan escalate.toml
non-breaking on schema v1 · 0 entities · 0 links in scope
  · add automation escalate_blocked (on changes to issue: action create_task, webhook pager; not enabled by this change) (0 entities, 0 links)
  risk low
```

### Propose (agent) and approve (person)

```text
$ ONTO_TOKEN=$ONTO_TOKEN_AGENT onto schema apply escalate.toml
error: permission denied: … ops-bot is an agent: agents may propose schema changes but not apply them

$ ONTO_TOKEN=$ONTO_TOKEN_AGENT onto schema propose escalate.toml --title "Escalate blocked high-priority issues"
proposed prop_1a181daa0cb00000c774b Escalate blocked high-priority issues

$ onto schema approve prop_1a181daa0cb00000c774b
applied prop_1a181daa0cb00000c774b
applied as schema v2

$ onto automation list
escalate_blocked  inactive (not_enabled)  action create_task, webhook pager
```

Installing a definition (by a change set, a proposal or a pack) never
enables it.

### Enable

```text
$ ONTO_TOKEN=$ONTO_TOKEN_AGENT onto automation enable escalate_blocked
error: permission denied: … ops-bot is an agent: automation.enable is not open to agents

$ onto automation enable escalate_blocked
Enabled escalate_blocked
runs start with changes after 9.0; the enablement expires after 30 days (a person enables it again),
and any change to what it reaches stops it until then. A run's graph edits, receipt, outbox rows and
run row commit together. Webhook effects are enqueued once and delivered at least once; external
effects are not reversible.

$ onto automation list
escalate_blocked  active  action create_task, webhook pager  expires 2026-11-27T09:32:38Z
```

Who may enable: a principal that may approve schema changes (or an admin),
never an agent, that dominates `run_as`. The enablement stores a closure
hash over the automation, every action type it reaches (effects, roles,
risk, `unattended`, preconditions) and the version, principal and
registration of every endpoint and writeback target it reaches; a change to
any of them makes it inactive (`stale_definition`, naming the item) until a
person enables it again.

Phase 1 declares its service principals locally (`automation:<name>`,
`webhook:<name>`) until Tutela issues them. Because onto cannot tell when
the person who enabled an automation loses the right to, every enablement
expires after `[automation] enable_ttl_days` (30).

### Trigger it

```text
$ onto do set_state item=issue:pay-2 state=blocked
applied set_state act_1a181dacd9a0000147ca6
  • update issue:pay-2: state in_progress → blocked

$ onto automation runs
run_1a181dacdbe0000183311  escalate_blocked  succeeded  entity.update issue:pay-2  depth 1
```

The run (`GET /v1/automations/escalate_blocked/runs/run_1a181dacdbe0000183311`):

```json
{
  "run": {
    "id": "run_1a181dacdbe0000183311",
    "automation": "escalate_blocked",
    "principal": "automation:escalate_blocked",
    "state": "succeeded",
    "mode": "live",
    "depth": 1,
    "trigger_key": "11.0",
    "trigger": { "on": "change", "cursor": "11.0", "op": "entity.update", "id": "issue:pay-2",
                 "subject": "issue:pay-2", "state": { "from": "in_progress", "to": "blocked" },
                 "actor": "operator", "source": "api", "depth": 0, … },
    "effects": [
      { "index": 0, "kind": "action", "target": "create_task", "ok": true, "receipt": "act_1a181dacdbe0000183311" },
      { "index": 1, "kind": "webhook", "target": "pager", "ok": true, "outbox": "obx_1a181dacdc500001d2331" }
    ],
    "closure_hash": "ee2da53b99d571ffb6ed77ae3bd3202d01dc66ba3e3a9df017053f6365a0f82d",
    "schema_version": 2,
    "attempts": 1,
    "epoch": 1,
    "started_at": 1793179962816,
    "finished_at": 1793179962823,
    "means": "The run's graph edits, receipt, outbox rows and this row committed together. Webhook effects are enqueued once and delivered at least once (receivers dedupe on the event id). External effects are not reversible."
  }
}
```

Its receipt (`onto call action.receipt '{"id":"act_1a181dacdbe0000183311"}'`,
plan left out) is an ordinary action receipt whose actor is the automation:

```json
{
  "id": "act_1a181dacdbe0000183311",
  "action": "create_task",
  "actor": "automation:escalate_blocked",
  "request_id": "run_1a181dacdbe0000183311",
  "status": "applied",
  "result": { "applied": 3, "entities": ["task:1a181dacdbe0000183311"], "links": 1, "revision": 6,
              "events": ["evt_1a181dacdc500001f693e", "…"] },
  "workspace": "acme"
}
```

And the webhook effect at the receiver, signed like any delivery, with the
cause of the run:

```json
{
  "specversion": "1.0",
  "id": "obx_1a181dacdc500001d2331",
  "type": "onto.issue.escalated",
  "source": "onto-intelli://docsrun",
  "time": "2026-10-28T09:32:42Z",
  "tenant": "docsrun",
  "workspace": "acme",
  "causeautomation": "escalate_blocked",
  "causerun": "run_1a181dacdbe0000183311",
  "causedepth": 1,
  "datacontenttype": "application/json",
  "data": { "from": "in_progress", "id": "issue:pay-2", "title": "Card declined twice" }
}
```

From the trigger's commit (`ts` 1793179962778) the run started 38 ms later,
committed in 7 ms, and the receiver had the webhook effect 22 ms after that (one
observation, debug build).

How a run works: the consumer (one per tenant, under the lease
`automation:consumer`) claims the run with `INSERT … ON CONFLICT (automation,
trigger_key) DO NOTHING`, so a duplicate trigger is settled by the claim and
never retried. Then one transaction re-checks the conditions and the action's
preconditions on the locked graph and commits the graph edits, the receipt,
the outbox rows, the finished run row and the `automation.run` event
together, fenced by the lease epoch. A crash in between leaves a `running`
row that recovery executes again.

### Dry run

`automation.test` evaluates a change as the run principal and the caller
together, and writes nothing:

```text
$ curl -X POST -H "authorization: Bearer $ONTO_TOKEN" -H 'content-type: application/json' \
    http://127.0.0.1:50071/v1/automations/escalate_blocked/test -d '{"cursor":"18.0"}'
{
  "verdict": "would_run",
  "matched": true,
  "depth": 1, "depth_limit": 2,
  "effects": [
    { "index": 0, "kind": "action", "target": "create_task", "ok": true,
      "plan": { "preview": ["create task:1a181dfa8360000118695 \"Unblock: Refund stuck\"", "record event task.created on task:1a181dfa8360000118695"],
                "risk": "low", "reversible": true, "external": [] } },
    { "index": 1, "kind": "webhook", "target": "pager", "ok": true,
      "plan": { "event": "onto.issue.escalated", "payload": { "from": "in_progress", "id": "issue:pay-3", "title": "Refund stuck" } } }
  ],
  "note": "A dry run as the run principal and you together (an entity either may not see has no fields): nothing was written.",
  …
}
```

Pass the change's `cursor`. The other form, `{"id": "issue:pay-3"}`, tests
the entity as if it was just updated, without a state transition, so a
trigger with a `state` guard does not admit it and `{trigger.state.from}`
has no value: it answered `verdict: error` with `invalid_template` here.

### An expired enablement

The TTL is at least one day, so the run below shortens it by hand: with the
server stopped, the stored `expires_at` of `automation.state/escalate_blocked`
was set an hour in the past (`UPDATE onto_docsrun.kv SET value =
jsonb_set(value, '{expires_at}', …)`), then the server was started again.
Nothing else was changed.

```text
$ onto automation list
escalate_blocked  inactive (paused: enablement_expired)  action create_task, webhook pager  expires 2026-10-28T08:36:13Z

$ onto do set_state item=issue:pay-3 state=blocked        # a high-priority issue
applied set_state act_1a181de47130000073789

$ onto automation runs                                     # 70 s later: no new run
run_1a181dacdbe0000183311  escalate_blocked  succeeded  entity.update issue:pay-2  depth 1

$ onto call event.list '{"action":"automation.expired"}'
{ "events": [ { "action": "automation.expired", "actor": "system", "intent": "automation.consumer",
                "target": "automation:escalate_blocked", "payload": { "reason": "enablement_expired" }, … } ] }

$ onto automation enable escalate_blocked
Enabled escalate_blocked
runs start with changes after 18.0; …
```

Changes made while an automation is not enabled never start runs: enabling
again starts after the head (18.0 here), so `issue:pay-3` was not escalated.
`automation.rerun` reruns skipped or failed runs only.

### Guards (tested, not exercised in this run)

- Depth: a run's changes carry its depth; a change at depth `d` starts runs
  at `d + 1`, refused over `min(max_depth, [automation] max_depth)` (3).
  `allow_self = false` (the default) skips the automation's own changes.
  Inbound `x-onto-cause-depth` / `x-onto-cause-run` are read from config
  tokens only, and only to raise the depth.
- Rate: `rate.per_minute` per automation, `max_runs_per_minute` per tenant,
  `per_subject_per_hour` per (automation, subject) kept in the consumer's
  memory (approximate across lease handoffs). Over a limit the trigger
  becomes a `skipped` run that can be rerun.
- Plans that need confirmation (risk medium or higher, or not reversible
  without `unattended`) are refused, not run: delegated approval is not part
  of phase 1.
- `pause_after_failures` (5) consecutive failed runs pause the automation;
  `automation.disable`, the tenant switch `automation.kill` and `[automation]
  enabled = false` stop it, and a kill cancels pending retries.
- Change-triggered automations do not see bulk imports or uncaptured writes
  (`gap` records never start runs; lint `automation_gap_blind`).

## 4. Writeback

An action type may post to a registered HTTP target after it commits. The
request goes through the outbox like a webhook, with `Idempotency-Key:
<receipt>:<effect>`; the receiver must honour it, because delivery is at
least once. Writeback is not reversible; action.undo does not undo the
external write. `[writeback] enabled` is off by default; while off, plans
with a writeback effect are refused.

### Register a target

```text
$ curl -H "authorization: Bearer $ONTO_TOKEN" -H 'content-type: application/json' \
    http://127.0.0.1:50071/v1/writeback/targets -d '{"name":"helpdesk",
      "base_url":"http://127.0.0.1:18091/desk/","secret":"env:DOCS_DESK_SECRET",
      "routes":{"close":"tickets/close"},"workspaces":["acme"]}'
{
  "enabled": true,
  "note": "Writeback is not reversible; action.undo does not undo the external write. Each call is enqueued once and delivered at least once with Idempotency-Key: <receipt>:<effect>; the receiver must honour it.",
  "target": { "name": "helpdesk", "base_url": "http://127.0.0.1:18091/desk/", "routes": { "close": "tickets/close" },
              "secret": "env:DOCS_DESK_SECRET", "signed": true, "restricted": false, "workspaces": ["acme"], … }
}

$ curl -X POST -H "authorization: Bearer $ONTO_TOKEN" http://127.0.0.1:50071/v1/writeback/targets/helpdesk/test
{
  "ok": true, "base": "http://127.0.0.1:18091", "signed": true, "problems": [],
  "routes": [ { "route": "close", "path": "tickets/close", "ok": true } ],
  "note": "nothing was sent: writeback.test checks the configuration only (references resolve, the URL and every route pass the outbound rules), since a request could write to the target; host names are resolved and judged at every request"
}
```

### An action type with a writeback effect

```toml
[[changes]]
op = "put_action_type"
name = "close_ticket"

[changes.def]
description = "Resolve an issue and close its helpdesk ticket."
risk = "low"
parameters.issue = { type = "entity", kinds = ["issue"], required = true }

[[changes.def.preconditions]]
check = "open"
param = "issue"

[[changes.def.effects]]
patch = { id = "{issue}", state = "resolved" }

[[changes.def.effects]]
writeback = { target = "helpdesk", route = "close", body = { issue = "{issue}", title = "{issue.title}" } }
```

The declared risk is `low`, but the external write makes the plan not
reversible and at least medium, so it needs confirmation:

```text
$ onto do close_ticket issue=issue:pay-1
proposed close_ticket act_1a181dc46fc000030a850
  • update issue:pay-1: state in_progress → resolved
  • writeback helpdesk.close {"issue":"issue:pay-1","title":"Payment timeout on checkout"} [async]
  ! Writeback is not reversible; action.undo does not undo the external write.
  risk medium · not reversible
  risk Medium, not reversible (an external write: action.undo does not undo it): pass confirm=true to apply

$ onto do close_ticket issue=issue:pay-1 --confirm
applied close_ticket act_1a181dc71fc000032f37e
  • update issue:pay-1: state in_progress → resolved
  • writeback helpdesk.close {"issue":"issue:pay-1","title":"Payment timeout on checkout"} [async]
  ! Writeback is not reversible; action.undo does not undo the external write.
  risk medium · not reversible
```

At the receiver, 66 ms after the commit:

```text
POST /desk/tickets/close
content-type: application/json
idempotency-key: act_1a181dc71fc000032f37e:1
user-agent: onto-intelli/0.2.0
webhook-id: obx_1a181dc7201000033d442
webhook-signature: v1,a4Kwe+58XW39IS1CPSuRHyaZxGvYZ8rPxniz32lWueM=
webhook-timestamp: 1793180070

{"issue":"issue:pay-1","title":"Payment timeout on checkout"}

verify_webhook: ok, id obx_1a181dc7201000033d442
```

The body is posted as rendered (not a CloudEvent); the Standard Webhooks
headers are sent because the target has a secret. The key is the receipt and
the effect's index in the action type, so a retry of the same row, or a
replay, carries the same key.

The receipt lists the external write and its delivery state; undo refuses
without `force`, and with `force` undoes the graph edits only:

```text
$ onto call action.receipt '{"id":"act_1a181dc71fc000032f37e"}'
{
  "id": "act_1a181dc71fc000032f37e", "action": "close_ticket", "actor": "operator", "status": "applied",
  "external": [ { "effect": 1, "target": "helpdesk", "route": "close", "outbox": "obx_1a181dc7201000033d442",
                  "idempotency_key": "act_1a181dc71fc000032f37e:1", "state": "delivered", "attempts": 1 } ],
  "note": "not reversible: the external write is not undone by action.undo",
  …
}

$ onto undo act_1a181dc71fc000032f37e
error: invalid: … action close_ticket made an external write, which is not reversible: action.undo does
not undo it. Pass force=true to undo the graph edits only
```

`unattended = ["automation"]` (or `"agent"`) on an action type waives the
confirmation the external write added, for that caller kind only; it is
schema, so it is approved like any change and is part of every automation
closure that reaches it. That path was not exercised in this run.

## Refused

Endpoints and targets are judged when they are written and again at every
request (names are resolved and checked right before connecting). Link-local
and metadata addresses are never reachable, allow-listed or not:

```text
$ curl … /v1/webhooks -d '{"name":"meta","url":"https://169.254.169.254/latest/meta-data/",…}'
{"error":{"code":"invalid_argument","message":"invalid: url: refused: 169.254.169.254 resolves to 169.254.169.254, a link-local, metadata or reserved address"}} HTTP 400

$ curl … /v1/webhooks -d '{"name":"meta6","url":"https://[::ffff:169.254.169.254]/x",…}'
{"error":{"code":"invalid_argument","message":"invalid: url: refused: [::ffff:a9fe:a9fe] resolves to ::ffff:169.254.169.254, a link-local, metadata or reserved address"}} HTTP 400

$ curl … /v1/writeback/targets -d '{"name":"imds","base_url":"https://169.254.169.254/",…}'
{"error":{"code":"invalid_argument","message":"invalid: url: refused: 169.254.169.254 resolves to 169.254.169.254, a link-local, metadata or reserved address"}} HTTP 400

$ curl … /v1/webhooks -d '{"name":"meta","url":"http://169.254.169.254/latest/meta-data/",…}'
{"error":{"code":"invalid_argument","message":"invalid: url must be an https URL"}} HTTP 400

$ curl … /v1/webhooks -d '{"name":"loop","url":"https://127.0.0.2:9/x",…}'
{"error":{"code":"invalid_argument","message":"invalid: url: refused: 127.0.0.2 resolves to 127.0.0.2, not a public address (allow it with [[webhooks.allow]])"}} HTTP 400
```

An operator rule cannot open the metadata range either; the server refuses
to start:

```text
# [[webhooks.allow]] host = "metadata.internal", cidrs = ["169.254.169.254/32"]
error: config: webhooks.allow host 'metadata.internal': 169.254.169.254/32 is link-local, metadata or reserved and is never reachable
```

Service principal ids are reserved (`automation:`, `webhook:`, `connector:`,
`system`): no credential may be or act as one.

```text
# an impersonating config token with x-onto-act-as: automation:escalate_blocked
{"error":{"code":"permission_denied","message":"automation:escalate_blocked is a reserved service principal id (automation:, webhook:, connector:, system); no credential may act as it"}} HTTP 403

# [[auth.tokens]] principal = { id = "automation:escalate_blocked", … }
error: config: auth token principal automation:escalate_blocked is a reserved service principal id (automation:, webhook:, connector:, system): choose another id
```

Also enforced, and covered by tests rather than this run: `env:`/`file:`
references only from the operator credential (a config token or dev mode,
without act-as), with errors that name the reference and never its value;
endpoints and targets registered by project credentials (API keys, console
sessions) are `restricted` and ignore `[[webhooks.allow]]`; mutating
outbound configuration needs a caller who dominates the principal it acts
as.

## Operating notes

- **Restarts.** Fan-out (`webhook:fanout`) and the automation consumer
  (`automation:consumer`) run under database leases of 60 s. `onto serve`
  shuts down gracefully on SIGTERM (Unix; what `docker stop` and Kubernetes
  send) and Ctrl-C: it stops accepting connections, ends open change
  streams with their final `end` frame (`tenant_closed`, "the server is
  shutting down", with the cursor to resume after), lets workers finish the
  round they are in, then releases its leases, the outbox rows it claimed
  and did not settle, and the wake scanner lock, all within
  `[grpc] shutdown_grace_secs` (20) plus a few seconds, and exits 0. A
  release is fenced: only the holder at the epoch it took the lease at
  gives it back, so a process that lost its lease meanwhile releases
  nothing. The replacement process takes over at once; nothing is skipped,
  because each endpoint and the consumer resume from their cursors, and an
  unsettled row is sent again (delivered at least once). A second signal
  stops at once. A process that is killed (or crashes) still leaves its
  leases to expire: changes are then enqueued up to 60 s later (18 to 49 s
  in the run this page was written from, before graceful shutdown existed).
  One-off `onto` commands run no fan-out, delivery or automation workers
  (`[webhooks] workers`, `[automation] workers`), so they never hold a lease.
  Change streams and delivery of rows already in the outbox are otherwise
  not affected.
- **Wake-ups.** A commit wakes fan-out while the tenant has an enabled
  subscription and the consumer while it has an enabled automation;
  otherwise they poll (`[webhooks] poll_secs`). The deployment's wake
  scanner opens closed tenants whose background work is due.
- **Retention.** One task per open tenant (5 minutes after it opens, then
  hourly) trims the change feed (`[changes]`), finished outbox rows and their
  attempt log (`[webhooks] retain_days`, 14) and final run rows
  (`[automation] retain_days`, 30). The `automation.run` events stay with
  the audit trail.
- **Where the state lives.** docs/STORAGE.md lists the tables and
  migrations (0009 to 0013) and how to roll back past them;
  docs/CONTROL_PLANE.md lists the routes, the outbound address rules and the
  reserved ids.
- **Surfaces.** Actions `changes.*`, `webhook.*`, `outbox.list`,
  `automation.*` and `writeback.*` on every transport (`onto actions` lists
  them); gRPC `ChangeService`, `WebhookService`, `AutomateService`,
  `WritebackService`; MCP read tools `poll_changes`, `list_automations`,
  `automation_runs`, `test_automation`; CLI `onto watch`, `onto webhook
  list|add|rm`, `onto automation list|runs|enable|disable`; the UI's
  Automate page (Automations and Webhooks tabs). This run used the CLI and
  HTTP only.

## Benchmarks

Measured 2026-10-28 with `onto-bench` (release build) on the machine above,
PostgreSQL 16.15 with pgvector 0.8.7 in Docker on the same machine, a fresh
tenant per scenario.

### Change capture and live streams

```text
cargo build --release -p onto-bench
./target/release/onto-bench --url postgres://onto:onto@127.0.0.1:5441/onto --changes --writes 500
```

Each commit is one entity upsert through the engine (`Ontology::upsert`, one
PostgreSQL transaction). Subscribers are live streams in the same process,
each with its own principal, so no two share an evaluation (the worst case
for filtering); they drain their frames as fast as they arrive.

| Operation | What | Runs | p50 ms | p99 ms | mean ms | max ms |
|---|---|---:|---:|---:|---:|---:|
| commit, capture off | one entity upsert through the engine | 500 | 3.910 | 6.713 | 4.013 | 8.258 |
| commit, capture on | one entity upsert through the engine | 500 | 4.319 | 7.346 | 4.463 | 11.785 |
| commit, 0 streams | capture on, no subscribers | 500 | 4.080 | 7.155 | 4.256 | 13.473 |
| commit, 50 streams | capture on, 50 subscribers draining | 500 | 5.133 | 7.849 | 5.222 | 10.106 |
| commit, 200 streams | capture on, 200 subscribers draining | 500 | 6.711 | 11.316 | 6.935 | 15.639 |
| delivery lag | start of the commit until its frame reaches one subscriber (50 others open) | 100 | 6.451 | 9.004 | 6.603 | 16.042 |

- Capture adds about 0.4 ms at p50 (about 10%) to a single-entity commit
  here. Batches with many entities were not measured.
- Commit latency rises with streams open in the same process (about +1.0 ms
  p50 at 50 subscribers, +2.6 ms at 200). The likely cause, not profiled, is
  filtering sharing the CPU and the graph read lock with the writer: a writer
  waits at most for one slice of filtering (64 records), never for a
  subscriber's channel. Streams served by other replicas were not measured.
- The ring held at most 600 records (about 340 KB) during these runs.
- The delivery lag includes the commit itself (about 4 ms of it at p50).
- An earlier run of the same benchmark (build step 6, same machine) was
  within 0.4 ms at p50 on every commit row; its delivery lag p50 was 7.1 ms.
- Not measured: the per-page filtering time against
  `changes.eval_budget_ms` (50 ms stays the default) and commits on a large
  graph.

### Webhook fan-out during a crawl

```text
ONTO_BENCH_HOOK_SECRET=unused ./target/release/onto-bench --url postgres://onto:onto@127.0.0.1:5441/onto --fanout --crawl 100000
```

A crawl writes 100,000 new entities in commits of 500 (`Ontology::apply`,
five kinds in turn) on a fresh tenant with no endpoints, then on one with 50
endpoints, each subscribed to `entity.create` of one of the five kinds, so
every entity matches ten endpoints. Deliveries are off in this run: fan-out
enqueues the outbox rows and nothing is sent.

| Operation | What | Runs | p50 ms | p99 ms | mean ms | max ms |
|---|---|---:|---:|---:|---:|---:|
| crawl commit, 0 endpoints | 500 new entities per commit | 200 | 47.725 | 77.772 | 50.677 | 306.836 |
| crawl commit, 50 endpoints | 500 new entities per commit, fan-out running | 200 | 42.722 | 67.660 | 43.281 | 77.223 |

- The crawl took 10.2 s without endpoints and 8.7 s with 50: fan-out running
  beside it did not slow the commits measurably. The run without endpoints
  came first and had one 307 ms outlier; the order of the two scenarios was
  not varied.
- Fan-out enqueued 1,000,000 rows (ten per entity) and reached the head of
  the change feed 96.5 s after the crawl began: about 10,400 rows, or 1,040
  entities, per second. It runs behind a crawl this fast; each endpoint's
  cursor keeps its place, so nothing is lost while it catches up, and an
  endpoint whose backlog passes `pending_cap` pauses until it drains.

### Not measured

- Delivery throughput (rows sent per second to a receiver) and delivery
  latency under load: onto-bench has no delivery scenario yet. The single
  observations above (49 ms from commit to receiver for a change, 66 ms for
  a writeback, about 65 ms from a trigger's commit to its webhook effect)
  come from a debug build with one receiver and are not benchmarks.
- Automation run throughput.
