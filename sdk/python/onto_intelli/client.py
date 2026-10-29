"""The onto-intelli client (docs/CONTROL_PLANE.md section 12): the HTTP API
over ``urllib.request``. Results are plain ``dict``s whose keys are the
server's (the Rust serde shapes); the ``TypedDict``s below document them."""

from __future__ import annotations

import datetime as _dt
import email.utils
import json
import os
import socket
import time
import urllib.error
import urllib.parse
import urllib.request
from typing import Any, Dict, Iterator, List, Mapping, Optional, TypedDict, Union

from .changes import Changes
from .errors import OntoError
from .sse import parse_sse

VERSION = "0.2.0"

Json = Any
JsonObject = Dict[str, Any]
Time = Union[int, float, _dt.datetime]

# ── shapes (documentation; results are plain dicts) ─────────────────────────


class AskTrace(TypedDict, total=False):
    session_id: str
    user_id: str
    tags: List[str]
    metadata: JsonObject
    name: str


class AskRequest(TypedDict, total=False):
    question: str
    workspace: str
    conversation_id: str
    mode: str  # auto | deterministic | llm
    token_budget: int
    include_context: bool
    no_cache: bool
    confirm: bool
    trace: AskTrace


class Resolved(TypedDict):
    id: str
    kind: str
    title: str
    mention: str
    confidence: float
    position: int


class TokenUsage(TypedDict):
    prompt_tokens: int
    completion_tokens: int
    cached_tokens: int
    cost_usd: float
    estimated: bool


class AskResponse(TypedDict, total=False):
    answer: str
    # deterministic | llm | llm_cache | answer_cache | fallback | clarify | proposal | action
    mode: str
    intent: str
    why: bool
    entities: List[Resolved]
    facts: List[str]
    conversation_id: str
    usage: TokenUsage
    efficiency: JsonObject
    timings: JsonObject
    provider: str
    model: str
    request_id: str
    revision: int
    memories: List[str]
    context: JsonObject
    note: str
    options: List[Resolved]
    action: JsonObject
    trace_id: str


class AskEvent(TypedDict, total=False):
    type: str  # stage | entities | delta | done | error
    text: str
    data: Json  # done: the AskResponse; error: {"code": ...}


class Entity(TypedDict, total=False):
    id: str
    kind: str
    workspace: str
    title: str
    state: str
    props: JsonObject
    aliases: List[str]
    provenance: JsonObject
    created_at: int
    updated_at: int
    revision: int


class TraceRow(TypedDict, total=False):
    id: str
    ts: int
    duration_ms: float
    name: str
    kind: str  # ask | action | call | eval
    status: str  # ok | error
    error_code: Optional[str]
    error_message: Optional[str]
    workspace: str
    principal: str
    via: Optional[str]
    user_id: Optional[str]
    session_id: Optional[str]
    api_key: Optional[str]  # the key's prefix, never the key
    source: str
    request_id: Optional[str]
    intent: Optional[str]
    mode: Optional[str]
    deterministic: bool
    provider: Optional[str]
    model: Optional[str]
    llm_calls: int
    cache_hits: int
    input_tokens: int
    output_tokens: int
    cached_tokens: int
    context_tokens: int
    saved_tokens: int
    cost_usd: float
    input: Json
    output: Json
    metadata: Json
    tags: List[str]
    version: str
    preview: str


class SpanRow(TypedDict, total=False):
    trace_id: str
    id: str
    parent_id: Optional[str]
    name: str
    kind: str  # span | generation | retrieval | tool | event
    ts: int
    duration_ms: float
    status: str
    error: Optional[str]
    model: Optional[str]
    input_tokens: int
    output_tokens: int
    cached_tokens: int
    cost_usd: float
    attrs: Json
    input: Json
    output: Json


class ScoreRow(TypedDict, total=False):
    id: str
    trace_id: str
    span_id: Optional[str]
    ts: int
    name: str
    value: Optional[float]
    label: Optional[str]
    source: str
    author: Optional[str]
    comment: Optional[str]


class TraceDetail(TypedDict):
    trace: TraceRow
    spans: List[SpanRow]
    scores: List[ScoreRow]


class TracePage(TypedDict):
    traces: List[TraceRow]
    next: Optional[str]


class TraceMetrics(TypedDict, total=False):
    requests: int
    errors: int
    error_rate: float
    p50_ms: float
    p95_ms: float
    p99_ms: float
    input_tokens: int
    output_tokens: int
    cached_tokens: int
    cost_usd: float
    deterministic_share: float
    cache_hit_rate: float
    llm_calls: int
    series: List[JsonObject]
    by_model: List[JsonObject]
    by_name: List[JsonObject]
    scores: List[JsonObject]


class SessionRow(TypedDict, total=False):
    session_id: str
    user_id: Optional[str]
    first_ts: int
    last_ts: int
    traces: int
    errors: int
    input_tokens: int
    output_tokens: int
    cost_usd: float
    preview: Optional[str]


class SessionPage(TypedDict):
    sessions: List[SessionRow]
    next: Optional[str]


class MintedSession(TypedDict):
    token: str
    expires_at: int
    principal: JsonObject
    tenant: Optional[str]


# ── the client ──────────────────────────────────────────────────────────────

_CODES = {
    400: "invalid_argument",
    401: "unauthenticated",
    403: "permission_denied",
    404: "not_found",
    409: "conflict",
    413: "invalid_argument",
    429: "resource_exhausted",
    503: "unavailable",
}


def _env(name: str) -> Optional[str]:
    v = os.environ.get(name, "").strip()
    return v or None


def _drop_none(d: Mapping[str, Any]) -> Dict[str, Any]:
    return {k: v for k, v in d.items() if v is not None}


def _ms(v: Any) -> Any:
    if isinstance(v, _dt.datetime):
        if v.tzinfo is None:
            v = v.replace(tzinfo=_dt.timezone.utc)
        return int(v.timestamp() * 1000)
    return v


def _query(params: Optional[Mapping[str, Any]]) -> str:
    if not params:
        return ""
    out = []
    for k, v in params.items():
        if v is None:
            continue
        v = _ms(v)
        if isinstance(v, bool):
            v = "true" if v else "false"
        out.append((k, str(v)))
    return "?" + urllib.parse.urlencode(out) if out else ""


def _retry_after(value: Optional[str]) -> Optional[float]:
    if value is None:
        return None
    try:
        return max(0.0, float(value))
    except ValueError:
        pass
    try:
        at = email.utils.parsedate_to_datetime(value)
    except (TypeError, ValueError):
        return None
    return max(0.0, at.timestamp() - time.time())


def _error(status: int, headers: Any, raw: bytes) -> OntoError:
    trace_id = headers.get("x-onto-trace-id") if headers is not None else None
    retry = _retry_after(headers.get("retry-after")) if headers is not None else None
    text = raw.decode("utf-8", "replace")
    code = _CODES.get(status, "internal" if status >= 500 else "unknown")
    message = text[:500] or "HTTP {}".format(status)
    try:
        body = json.loads(text)
        err = body.get("error") if isinstance(body, dict) else None
        if isinstance(err, dict):
            if isinstance(err.get("code"), str):
                code = err["code"]
            if isinstance(err.get("message"), str):
                message = err["message"]
    except ValueError:
        pass
    return OntoError(status, code, message, trace_id, retry)


class Onto:
    """A client of one onto-intelli server and project.

    Every argument falls back to an environment variable: ``url`` to
    ``ONTO_URL``, ``api_key`` (an ``onto_sk_`` key or an ``onto1.`` session)
    to ``ONTO_API_KEY``, ``tenant`` (a project ref; optional with a key, which
    implies its project) to ``ONTO_TENANT``. ``session_id`` is sent as
    ``x-onto-session-id`` on every request. 429 and 503 answers are retried
    up to ``max_retries`` times, waiting what ``retry-after`` says (at most
    ``max_retry_delay`` seconds). The key is never logged or shown.
    """

    def __init__(
        self,
        url: Optional[str] = None,
        api_key: Optional[str] = None,
        tenant: Optional[str] = None,
        workspace: Optional[str] = None,
        timeout: float = 30.0,
        session_id: Optional[str] = None,
        headers: Optional[Mapping[str, str]] = None,
        max_retries: int = 2,
        max_retry_delay: float = 30.0,
    ) -> None:
        url = url or _env("ONTO_URL")
        if not url:
            raise OntoError(0, "invalid_argument", "onto-intelli: no server URL (pass url or set ONTO_URL)")
        self.url = url.rstrip("/")
        self.__api_key = api_key or _env("ONTO_API_KEY")
        self.tenant = tenant or _env("ONTO_TENANT")
        self.workspace = workspace
        self.timeout = timeout
        self.session_id = session_id
        self.max_retries = max(0, max_retries)
        self.max_retry_delay = max_retry_delay
        self._headers = dict(headers or {})
        self.entities = _Entities(self)
        self.links = _Links(self)
        self.actions = _Actions(self)
        self.objects = _Objects(self)
        self.traces = _Traces(self)
        self.sessions = _Sessions(self)
        self.auth = _Auth(self)
        #: The change feed: ``subscribe`` (a live stream), ``poll``, ``head``.
        self.changes = Changes(self)

    def __repr__(self) -> str:
        return "Onto(url={!r}, tenant={!r}, workspace={!r}, api_key={})".format(
            self.url, self.tenant, self.workspace, "'(set)'" if self.__api_key else "None"
        )

    # ── transport ────────────────────────────────────────────────────────

    def _open(
        self,
        method: str,
        path: str,
        body: Any = None,
        query: Optional[Mapping[str, Any]] = None,
        stream: bool = False,
        session_id: Optional[str] = None,
        workspace: Optional[str] = None,
        extra_headers: Optional[Mapping[str, str]] = None,
    ) -> Any:
        """One request with the retries of 429 and 503; the open response."""
        headers = {"accept": "text/event-stream" if stream else "application/json"}
        headers.update(self._headers)
        if self.__api_key:
            headers["authorization"] = "Bearer " + self.__api_key
        if self.tenant:
            headers["x-onto-tenant"] = self.tenant
        ws = workspace or self.workspace
        if ws:
            headers["x-onto-workspace"] = ws
        sid = session_id or self.session_id
        if sid:
            headers["x-onto-session-id"] = sid
        headers.update(extra_headers or {})
        data = None
        if body is not None:
            data = json.dumps(body).encode("utf-8")
            headers["content-type"] = "application/json"
        url = self.url + path + _query(query)
        attempt = 0
        while True:
            req = urllib.request.Request(url, data=data, headers=headers, method=method)
            try:
                return urllib.request.urlopen(req, timeout=self.timeout)
            except urllib.error.HTTPError as e:
                raw = e.read()
                if e.code in (429, 503) and attempt < self.max_retries:
                    after = _retry_after(e.headers.get("retry-after"))
                    wait = after if after is not None else 0.5 * (2 ** attempt)
                    time.sleep(min(wait, self.max_retry_delay))
                    attempt += 1
                    continue
                raise _error(e.code, e.headers, raw) from None
            except (socket.timeout, TimeoutError):
                raise OntoError(0, "deadline_exceeded", "no response within {} s".format(self.timeout)) from None
            except urllib.error.URLError as e:
                reason = e.reason
                if isinstance(reason, (socket.timeout, TimeoutError)):
                    raise OntoError(0, "deadline_exceeded", "no response within {} s".format(self.timeout)) from None
                raise OntoError(0, "unavailable", "cannot reach {}: {}".format(self.url, reason)) from None

    def _json(self, method: str, path: str, body: Any = None, **kw: Any) -> Any:
        with self._open(method, path, body, **kw) as res:
            raw = res.read()
            status = res.status
        if not raw:
            return {}
        try:
            return json.loads(raw.decode("utf-8"))
        except ValueError:
            raise OntoError(status, "internal", "the server's answer is not JSON: " + raw[:200].decode("utf-8", "replace")) from None

    # ── asking ───────────────────────────────────────────────────────────

    def ask(
        self,
        question: str,
        session_id: Optional[str] = None,
        user_id: Optional[str] = None,
        metadata: Optional[JsonObject] = None,
        mode: Optional[str] = None,
        tags: Optional[List[str]] = None,
        conversation_id: Optional[str] = None,
        workspace: Optional[str] = None,
        token_budget: Optional[int] = None,
        include_context: Optional[bool] = None,
        no_cache: Optional[bool] = None,
        confirm: Optional[bool] = None,
        name: Optional[str] = None,
    ) -> AskResponse:
        """Answer a question: deterministic from the graph when it can, the LLM when reasoning is needed."""
        body = _ask_body(question, locals())
        return self._json("POST", "/v1/ask", body, session_id=session_id, workspace=workspace)

    def ask_stream(
        self,
        question: str,
        conversation_id: Optional[str] = None,
        session_id: Optional[str] = None,
        user_id: Optional[str] = None,
        metadata: Optional[JsonObject] = None,
        mode: Optional[str] = None,
        tags: Optional[List[str]] = None,
        workspace: Optional[str] = None,
        token_budget: Optional[int] = None,
        no_cache: Optional[bool] = None,
        confirm: Optional[bool] = None,
        name: Optional[str] = None,
    ) -> Iterator[AskEvent]:
        """Answer as server-sent events: ``stage``, ``entities``, ``delta``..., then ``done`` (or ``error``)."""
        body = _ask_body(question, locals())
        res = self._open("POST", "/v1/ask/stream", body, stream=True, session_id=session_id, workspace=workspace)
        return self._events(res)

    @staticmethod
    def _events(res: Any) -> Iterator[AskEvent]:
        read = getattr(res, "read1", None) or res.read
        try:
            for msg in parse_sse(iter(lambda: read(65536), b"")):
                try:
                    ev = json.loads(msg["data"])
                except ValueError:
                    ev = {"type": msg["event"], "text": msg["data"]}
                if not isinstance(ev, dict):
                    ev = {"type": msg["event"], "text": msg["data"]}
                ev.setdefault("type", msg["event"])
                yield ev
                if ev["type"] in ("done", "error"):
                    return
        finally:
            res.close()

    # ── any action ───────────────────────────────────────────────────────

    def call(
        self,
        action: str,
        input: Optional[Mapping[str, Any]] = None,
        workspace: Optional[str] = None,
        session_id: Optional[str] = None,
    ) -> Any:
        """Run any action (``GET /v1/actions`` lists them): ``POST /v1/call/{action}``."""
        path = "/v1/call/" + urllib.parse.quote(action, safe="")
        return self._json("POST", path, dict(input or {}), workspace=workspace, session_id=session_id)

    def search(
        self,
        query: str,
        kinds: Optional[List[str]] = None,
        limit: Optional[int] = None,
        mode: Optional[str] = None,
        workspace: Optional[str] = None,
    ) -> JsonObject:
        """Full-text and vector search over entities (``entity.search``): ``{"hits": [...]}``."""
        body = _drop_none({"query": query, "kinds": kinds, "limit": limit, "mode": mode, "workspace": workspace})
        return self.call("entity.search", body, workspace=workspace)


def _ask_body(question: str, a: Mapping[str, Any]) -> AskRequest:
    trace = _drop_none(
        {"session_id": a.get("session_id"), "user_id": a.get("user_id"), "tags": a.get("tags"),
         "metadata": a.get("metadata"), "name": a.get("name")}
    )
    body = _drop_none(
        {
            "question": question,
            "workspace": a.get("workspace"),
            "conversation_id": a.get("conversation_id"),
            "mode": a.get("mode"),
            "token_budget": a.get("token_budget"),
            "include_context": a.get("include_context"),
            "no_cache": a.get("no_cache"),
            "confirm": a.get("confirm"),
        }
    )
    if trace:
        body["trace"] = trace
    return body  # type: ignore[return-value]


class _Part:
    def __init__(self, onto: Onto) -> None:
        self._onto = onto


class _Entities(_Part):
    def get(self, id: str, derived: bool = False) -> Entity:
        return self._onto.call("entity.get", _drop_none({"id": id, "derived": derived or None}))

    def find(self, **spec: Any) -> JsonObject:
        """``entity.find``: kind, workspace, state, open, props, related, within, limit, offset → ``{entities, total}``."""
        return self._onto.call("entity.find", _drop_none(spec))

    def upsert(self, entity: Optional[Mapping[str, Any]] = None, **fields: Any) -> Entity:
        """Create or update an entity (props merge): ``upsert({"id": "issue:1", "title": "…"})`` or keywords."""
        body = dict(entity or {})
        body.update(fields)
        return self._onto.call("entity.upsert", _drop_none(body))

    def patch(
        self,
        id: str,
        title: Optional[str] = None,
        state: Optional[str] = None,
        props: Optional[JsonObject] = None,
        unset: Optional[List[str]] = None,
    ) -> Entity:
        body = _drop_none({"id": id, "title": title, "state": state, "props": props, "unset": unset})
        return self._onto.call("entity.patch", body)

    def delete(self, id: str) -> JsonObject:
        return self._onto.call("entity.delete", {"id": id})

    def history(self, id: str, limit: Optional[int] = None) -> JsonObject:
        return self._onto.call("entity.history", _drop_none({"id": id, "limit": limit}))


class _Links(_Part):
    def create(self, from_id: str, rel: str, to_id: str, props: Optional[JsonObject] = None) -> JsonObject:
        return self._onto.call("link.create", _drop_none({"from": from_id, "rel": rel, "to": to_id, "props": props}))

    def delete(self, from_id: str, rel: str, to_id: str) -> JsonObject:
        return self._onto.call("link.delete", {"from": from_id, "rel": rel, "to": to_id})


class _Actions(_Part):
    def list(self) -> JsonObject:
        """Action types (governed edits) of the schema: ``{"action_types": [...]}``."""
        return self._onto.call("action.list")

    def describe(self, name: str) -> JsonObject:
        return self._onto.call("action.describe", {"name": name})

    def plan(self, action: str, params: Optional[JsonObject] = None, workspace: Optional[str] = None) -> JsonObject:
        """Validate an action and preview its edits, without applying it."""
        return self._onto.call("action.plan", _drop_none({"action": action, "params": params or {}, "workspace": workspace}))

    def apply(
        self,
        action: str,
        params: Optional[JsonObject] = None,
        confirm: bool = False,
        dry_run: bool = False,
        workspace: Optional[str] = None,
    ) -> JsonObject:
        """Apply an action; high-risk and irreversible ones need ``confirm=True``. Returns the receipt."""
        body = {"action": action, "params": params or {}, "confirm": confirm, "dry_run": dry_run}
        if workspace:
            body["workspace"] = workspace
        return self._onto.call("action.apply", body)

    def undo(self, receipt: str, force: bool = False) -> JsonObject:
        return self._onto.call("action.undo", {"receipt": receipt, "force": force})


class _Objects(_Part):
    def types(self, workspace: Optional[str] = None) -> JsonObject:
        return self._onto._json("GET", "/v1/objects/types", query={"workspace": workspace})

    def explore(self, query: Optional[Mapping[str, Any]] = None, **kw: Any) -> JsonObject:
        """Objects of one type: ``explore({"kind": "customer", "search": "ada", "limit": 20})``."""
        body = dict(query or {})
        body.update(kw)
        return self._onto._json("POST", "/v1/objects/explore", _drop_none(body))


class _Traces(_Part):
    """Traces need a credential that may observe (an admin-scope key, a console session)."""

    def list(self, **filters: Any) -> TracePage:
        """since, until (ms or datetime), name, kind, status, user_id, session_id, model, workspace, mode, tag,
        min_ms, q, limit, cursor."""
        return self._onto._json("GET", "/v1/traces", query=filters)

    def get(self, id: str) -> TraceDetail:
        return self._onto._json("GET", "/v1/traces/" + urllib.parse.quote(id, safe=""))

    def metrics(self, **query: Any) -> TraceMetrics:
        """since, until (ms or datetime), bucket (hour | day), name, workspace."""
        return self._onto._json("GET", "/v1/metrics", query=query)

    def score(
        self,
        trace_id: str,
        name: str,
        value: Optional[float] = None,
        label: Optional[str] = None,
        comment: Optional[str] = None,
        span_id: Optional[str] = None,
    ) -> ScoreRow:
        body = _drop_none({"name": name, "value": value, "label": label, "comment": comment, "span_id": span_id})
        return self._onto._json("POST", "/v1/traces/" + urllib.parse.quote(trace_id, safe="") + "/scores", body)


class _Sessions(_Part):
    def list(self, **filters: Any) -> SessionPage:
        """since, until, user_id, limit, cursor."""
        return self._onto._json("GET", "/v1/sessions", query=filters)

    def get(self, id: str, limit: Optional[int] = None) -> JsonObject:
        """A session's traces, oldest first: ``{"session_id", "traces"}``."""
        return self._onto._json("GET", "/v1/sessions/" + urllib.parse.quote(id, safe=""), query={"limit": limit})


class _Auth(_Part):
    def mint_session(
        self,
        act_as: Optional[str] = None,
        roles: Optional[List[str]] = None,
        workspaces: Optional[List[str]] = None,
        entity: Optional[str] = None,
        ttl_secs: Optional[int] = None,
        tenant: Optional[str] = None,
    ) -> MintedSession:
        """A short-lived ``onto1.`` session for a browser, acting for one of your users (backend → browser).
        With an API key the session is bound to the key and stops working when the key is revoked."""
        body = _drop_none(
            {"act_as": act_as, "roles": roles, "workspaces": workspaces, "entity": entity, "ttl_secs": ttl_secs,
             "tenant": tenant}
        )
        return self._onto._json("POST", "/v1/auth/session", body)

    def whoami(self) -> JsonObject:
        return self._onto._json("GET", "/v1/auth/whoami")
