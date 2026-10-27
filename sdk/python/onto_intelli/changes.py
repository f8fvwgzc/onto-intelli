"""The change feed: ``subscribe`` follows the SSE stream
(``POST /v1/changes/stream``) and reconnects after the last cursor it
received; ``poll`` and ``head`` are the unary calls. Delivery is at least
once across reconnects: a change may arrive again after a reconnect (dedupe
on ``cursor``), never be skipped silently (an expired cursor is a reset, a
commit written without records arrives as a ``gap`` change)."""

from __future__ import annotations

import json
import socket
import time
from typing import TYPE_CHECKING, Any, Callable, Dict, Iterator, List, Optional

from .errors import OntoError
from .sse import parse_sse

if TYPE_CHECKING:  # pragma: no cover
    from .client import Onto

JsonObject = Dict[str, Any]

#: The first and the longest wait before reconnecting (seconds); ``retry:`` replaces the first.
MIN_BACKOFF = 0.5
MAX_BACKOFF = 30.0

#: End codes after which the stream resumes at once from its cursor.
RESUME = ("slow_consumer", "tenant_closed", "principal_changed")
#: End codes that are final, with the HTTP status they stand for.
FINAL = {"unauthenticated": 401, "permission_denied": 403, "invalid_argument": 400}


def _filter(
    kinds: Optional[List[str]] = None,
    ops: Optional[List[str]] = None,
    workspaces: Optional[List[str]] = None,
    ids: Optional[List[str]] = None,
    rels: Optional[List[str]] = None,
    props: Optional[List[str]] = None,
    sources: Optional[List[str]] = None,
    fields: Optional[str] = None,
    include_migrations: Optional[bool] = None,
) -> JsonObject:
    body = {
        "kinds": kinds, "ops": ops, "workspaces": workspaces, "ids": ids, "rels": rels, "props": props,
        "sources": sources, "fields": fields, "include_migrations": include_migrations,
    }
    return {k: v for k, v in body.items() if v is not None}


def _seq(cursor: str) -> int:
    return int(cursor.split(".")[0])


class Changes:
    """``onto.changes``: the change feed of the client's project.

    Filter keywords (all optional): ``kinds``, ``ops``, ``workspaces``,
    ``ids`` (at most 1000), ``rels``, ``props``, ``sources`` (globs, ``!x``
    excludes), ``fields`` (``full`` or ``ids``), ``include_migrations``.
    """

    def __init__(self, onto: "Onto") -> None:
        self._onto = onto

    def head(self) -> JsonObject:
        """Where the feed is: ``{cursor, head, floor, capture}``."""
        return self._onto._json("GET", "/v1/changes/head")

    def poll(
        self,
        after: Optional[str] = None,
        from_: Optional[str] = None,
        limit: Optional[int] = None,
        **filter: Any,
    ) -> JsonObject:
        """One page after a cursor (``changes.poll``): ``{changes, next, head, floor, expired}``.
        Without ``after``, ``from_`` is ``earliest`` (default) or ``now``."""
        body = _filter(**filter)
        if after is not None:
            body["after"] = after
        if from_ is not None:
            body["from"] = from_
        if limit is not None:
            body["limit"] = limit
        return self._onto._json("POST", "/v1/call/changes.poll", body)

    def subscribe(
        self,
        after: Optional[str] = None,
        from_: str = "now",
        on_reset: Optional[Callable[[JsonObject], None]] = None,
        on_gap: Optional[Callable[[JsonObject], None]] = None,
        **filter: Any,
    ) -> Iterator[JsonObject]:
        """Follow the change feed: every change this caller may see that the
        filter wants, in cursor order.

        Reconnects with exponential backoff (0.5 s to 30 s, the server's
        ``retry:`` as the first wait) and ``Last-Event-ID``;
        ``slow_consumer``, ``tenant_closed`` and ``principal_changed``
        resume at once; ``too_far_behind`` pages with ``poll`` up to the
        head, then streams again. An expired cursor calls ``on_reset`` (then
        the stream continues from the head; re-read what you mirror with
        ``entity.find``, which is not pinned to a cursor) or, without it,
        raises ``OntoError`` with code ``cursor_expired``. A revoked
        credential raises ``unauthenticated``. Stop by leaving the loop.
        """
        body = _filter(**filter)
        cursor = after
        start = from_
        retry = None  # type: Optional[float]
        failures = 0
        while True:
            end = None  # type: Optional[JsonObject]
            reset = None  # type: Optional[JsonObject]
            try:
                req = dict(body)
                if cursor is None:
                    req["from"] = start
                headers = {"last-event-id": cursor} if cursor is not None else None
                res = self._onto._open("POST", "/v1/changes/stream", req, stream=True, extra_headers=headers)
                read = getattr(res, "read1", None) or res.read
                try:
                    for msg in parse_sse(iter(lambda: read(65536), b"")):
                        if "retry" in msg:
                            retry = msg["retry"] / 1000.0
                        failures = 0
                        data = json.loads(msg["data"]) if msg["data"] else {}
                        if msg["event"] == "change":
                            cursor = msg["id"] or data.get("cursor")
                            if data.get("op") == "gap" and on_gap is not None:
                                on_gap(data)
                            yield data
                        elif msg["event"] == "heartbeat":
                            cursor = msg["id"] or data.get("cursor")
                        elif msg["event"] == "reset":
                            reset = data
                            break
                        elif msg["event"] == "end":
                            end = data
                            break
                finally:
                    res.close()
            except OntoError as e:
                # An answer that retrying cannot change (bad filter, no access).
                if 400 <= e.status < 500 and e.status not in (408, 429):
                    raise
            except (OSError, socket.timeout, ValueError):
                pass  # a dropped connection: back off and reconnect
            if end is not None and end.get("code") == "too_far_behind" and (cursor or end.get("cursor")):
                cursor = cursor or end.get("cursor")
                while True:
                    page = self.poll(after=cursor, **filter)
                    if page.get("expired"):
                        reset = {"code": "cursor_expired", "floor": page.get("floor"), "head": page.get("head"),
                                 "message": page.get("message", "")}
                        break
                    for c in page.get("changes", []):
                        if c.get("op") == "gap" and on_gap is not None:
                            on_gap(c)
                        yield c
                    moved = page["next"] != cursor
                    cursor = page["next"]
                    if not moved or _seq(cursor) >= page["head"]:
                        break
                if reset is None:
                    continue
            if reset is not None:
                if on_reset is None:
                    raise OntoError(0, "cursor_expired", reset.get("message") or "the cursor expired")
                on_reset(reset)
                cursor, start = None, "now"
                continue
            if end is not None:
                code = end.get("code", "")
                if code in FINAL:
                    raise OntoError(FINAL[code], code, end.get("message") or code)
                if cursor is None:
                    cursor = end.get("cursor")
                if code in RESUME:
                    continue
            # A dropped connection or an unavailable server: back off.
            first = retry if retry is not None else MIN_BACKOFF
            time.sleep(min(first * (2 ** failures), MAX_BACKOFF))
            failures += 1
