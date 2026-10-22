# onto-intelli for Python

A client for an [onto-intelli](../../README.md) server: ask questions of
your ontology, run governed actions, read and write entities, and read the
traces the server records. Standard library only (`urllib.request`,
`json`), Python 3.9+, typed (`py.typed`).

## Install

From a checkout (the package is not on PyPI yet):

```sh
pip install /path/to/onto-intelli/sdk/python
```

Or copy the `onto_intelli/` directory into your project: it has no
dependencies.

## Connect

Create a project and an API key in the console (**API keys**), or ask your
operator for one. The **Connect** page shows this code with your project's
URL and key prefix filled in.

```python
import os
from onto_intelli import Onto

# With no arguments: ONTO_URL, ONTO_API_KEY and ONTO_TENANT.
onto = Onto(url="https://onto.example.com", api_key=os.environ["ONTO_API_KEY"])

res = onto.ask("Who owns the payments service?", session_id="chat-42", user_id="user:7")
print(res["answer"], res["mode"], res["trace_id"])
```

| Argument | Environment | Meaning |
|---|---|---|
| `url` | `ONTO_URL` | Base URL of the server. Required. |
| `api_key` | `ONTO_API_KEY` | An `onto_sk_…` API key or an `onto1.…` session. Sent as `authorization: Bearer …`. Not needed against `onto serve --dev`. |
| `tenant` | `ONTO_TENANT` | Project ref, sent as `x-onto-tenant`. Optional with an API key, which implies its project. |
| `workspace` | | Default workspace (`x-onto-workspace`). |
| `session_id` | | Sent as `x-onto-session-id` on every request; the console groups traces by it. |
| `headers` | | Extra headers on every request. |
| `timeout` | | Socket timeout in seconds. Default 30. |
| `max_retries` | | Retries of 429 and 503 answers. Default 2. |
| `max_retry_delay` | | The longest wait between retries in seconds, whatever `retry-after` says. Default 30. |

The key is never logged; `repr(onto)` shows only whether one is set.

## Calls

```python
onto.ask(question, session_id=None, user_id=None, metadata=None, mode=None, tags=None,
         conversation_id=None, no_cache=None, confirm=None)
for ev in onto.ask_stream("What is blocked?", conversation_id="new"):
    # ev["type"]: "stage" | "entities" | "delta" | "done" | "error"
    if ev["type"] == "delta":
        print(ev["text"], end="")
onto.call("graph.impact", {"id": "service:payments"})     # any action: GET /v1/actions lists them
onto.search("payments", kinds=["service"], limit=10)

onto.entities.get(id); onto.entities.find(kind="issue", state="open"); onto.entities.upsert({...})
onto.entities.patch(id, state="done"); onto.entities.delete(id); onto.entities.history(id)
onto.links.create("team:payments", "owns", "service:payments"); onto.links.delete(...)
onto.actions.list(); onto.actions.plan(action, params); onto.actions.apply(action, params, confirm=True)
onto.actions.undo(receipt)
onto.objects.types(); onto.objects.explore({"kind": "customer", "search": "ada", "limit": 20})

onto.traces.list(since=datetime.now(timezone.utc) - timedelta(days=1), status="error")
onto.traces.get(id); onto.traces.metrics(bucket="day")
onto.traces.score(trace_id, "user.feedback", value=1, comment="helpful")
onto.sessions.list(); onto.sessions.get(id)
onto.auth.mint_session(act_as="user:7", roles=["viewer"], ttl_secs=900)
```

Results are plain `dict`s with the server's keys (the Rust serde shapes);
the `TypedDict`s in `onto_intelli.client` document them (`AskResponse`,
`AskEvent`, `Entity`, `TraceRow`, `TraceDetail`, `TraceMetrics`, …). `since`
and `until` take Unix milliseconds or a `datetime` (naive ones are UTC).

Reading traces needs a credential that may observe: an `admin`-scope key or
a console session. Registering connectors needs an admin.

## Errors and retries

Every failure raises `OntoError` with `status` (HTTP status, or 0 when no
answer came), `code` (the server's code: `invalid_argument`,
`unauthenticated`, `permission_denied`, `not_found`, `conflict`,
`resource_exhausted`, `unavailable`, `internal`; without a response
`unavailable` or `deadline_exceeded`), `message`, `trace_id` (the
`x-onto-trace-id` of a traced call) and `retry_after`.

```python
from onto_intelli import OntoError

try:
    onto.ask("…")
except OntoError as e:
    if e.code == "resource_exhausted":
        print("budget reached, retry in", e.retry_after, "s")
    else:
        raise
```

429 (a rate limit or budget) and 503 answers are retried up to twice,
waiting what `retry-after` says (capped by `max_retry_delay`), else 0.5 s
then 1 s. `ask_stream` retries only before the stream starts.

## Tests

```sh
cd sdk/python
python3 -m unittest discover -s tests                                 # offline, against http.server
ONTO_URL=http://127.0.0.1:50051 python3 -m unittest discover -s tests # also the live test (onto serve --dev)
```
