"""The change feed against a scripted SSE server: ``retry:`` parsing,
reconnects with Last-Event-ID, transparent resumes, the reset callback and
its default error, the too_far_behind fallback to poll, and final errors.
Run: python3 -m unittest discover -s tests"""

from __future__ import annotations

import json
import os
import sys
import threading
import unittest
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from typing import Any, Callable, Dict, List, Optional

sys.path.insert(0, os.path.join(os.path.dirname(__file__), ".."))

from onto_intelli import Onto, OntoError, parse_sse  # noqa: E402


class Seen:
    def __init__(self, method: str, path: str, headers: Dict[str, str], body: Any) -> None:
        self.method, self.path, self.headers, self.body = method, path, headers, body


SEEN = []  # type: List[Seen]
SCRIPT = [None]  # type: List[Optional[Callable[[Seen, int], Any]]]


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *args: Any) -> None:
        pass

    def _handle(self) -> None:
        n = int(self.headers.get("content-length") or 0)
        raw = self.rfile.read(n) if n else b""
        seen = Seen(self.command, self.path.split("?")[0], {k.lower(): v for k, v in self.headers.items()},
                    json.loads(raw) if raw else None)
        SEEN.append(seen)
        kind, status, body = SCRIPT[0](seen, len(SEEN))  # type: ignore[misc]
        payload = (body if isinstance(body, str) else json.dumps(body)).encode("utf-8")
        self.send_response(status)
        self.send_header("content-type", "text/event-stream" if kind == "sse" else "application/json")
        self.send_header("content-length", str(len(payload)))
        self.send_header("connection", "close")
        self.end_headers()
        self.wfile.write(payload)

    do_GET = _handle
    do_POST = _handle


def change(cursor: str, id: str, op: str = "entity.update") -> str:
    data = {"cursor": cursor, "op": op, "id": id, "kind": "issue", "workspace": "acme", "ts": 1, "cause": {}}
    return "event: change\nid: {}\ndata: {}\n\n".format(cursor, json.dumps(data))


class ChangesTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        cls.thread = threading.Thread(target=cls.server.serve_forever, daemon=True)
        cls.thread.start()
        cls.base = "http://127.0.0.1:{}".format(cls.server.server_address[1])

    @classmethod
    def tearDownClass(cls) -> None:
        cls.server.shutdown()
        cls.server.server_close()

    def setUp(self) -> None:
        del SEEN[:]

    def client(self) -> Onto:
        return Onto(url=self.base, api_key="onto_sk_7f3k9q2m_secretsecretsecret", tenant="p1", timeout=5)

    def take(self, it: Any, n: int) -> List[Dict[str, Any]]:
        out = []
        for c in it:
            out.append(c)
            if len(out) == n:
                break
        it.close()
        return out

    def test_parse_sse_exposes_retry(self) -> None:
        raw = b"retry: 1500\n\nevent: a\ndata: 1\n\nretry: x\nevent: b\ndata: 2\n\n"
        got = list(parse_sse([raw[:7], raw[7:]]))
        self.assertEqual(
            got,
            [{"event": "a", "data": "1", "id": None, "retry": 1500}, {"event": "b", "data": "2", "id": None, "retry": 1500}],
        )
        self.assertNotIn("retry", list(parse_sse([b"data: x\n\n"]))[0])

    def test_reconnects_with_last_event_id_and_resumes(self) -> None:
        def script(req: Seen, n: int) -> Any:
            if n == 1:
                return ("sse", 200, "retry: 5\n\n" + change("3.0", "issue:1")
                        + 'event: heartbeat\nid: 4.2\ndata: {"cursor":"4.2","head":4}\n\n'
                        + 'event: end\ndata: {"code":"slow_consumer","cursor":"3.0","message":"cut"}\n\n')
            if n == 2:
                return ("sse", 200, change("5.0", "issue:2"))  # dropped without end: back off, reconnect
            return ("sse", 200, change("6.1", "issue:3", "entity.create"))

        SCRIPT[0] = script
        got = self.take(self.client().changes.subscribe(kinds=["issue"], from_="earliest"), 3)
        self.assertEqual([c["cursor"] for c in got], ["3.0", "5.0", "6.1"])
        self.assertEqual((SEEN[0].method, SEEN[0].path), ("POST", "/v1/changes/stream"))
        self.assertEqual(SEEN[0].headers["accept"], "text/event-stream")
        self.assertEqual(SEEN[0].body, {"kinds": ["issue"], "from": "earliest"})
        self.assertNotIn("last-event-id", SEEN[0].headers)
        self.assertEqual(SEEN[1].headers["last-event-id"], "4.2")
        self.assertEqual(SEEN[1].body, {"kinds": ["issue"]})
        self.assertEqual(SEEN[2].headers["last-event-id"], "5.0")

    def test_reset_raises_unless_handled(self) -> None:
        reset = 'event: reset\ndata: {"code":"cursor_expired","floor":10,"head":12,"message":"re-read with entity.find"}\n\n'
        SCRIPT[0] = lambda req, n: ("sse", 200, reset)
        with self.assertRaises(OntoError) as cm:
            self.take(self.client().changes.subscribe(after="1.0"), 1)
        self.assertEqual(cm.exception.code, "cursor_expired")
        self.assertIn("entity.find", cm.exception.message)
        del SEEN[:]
        SCRIPT[0] = lambda req, n: ("sse", 200, reset if n == 1 else change("13.0", "issue:9"))
        resets = []  # type: List[Dict[str, Any]]
        got = self.take(self.client().changes.subscribe(after="1.0", on_reset=resets.append), 1)
        self.assertEqual(got[0]["cursor"], "13.0")
        self.assertEqual(resets[0]["floor"], 10)
        self.assertEqual(SEEN[0].headers["last-event-id"], "1.0")
        self.assertNotIn("last-event-id", SEEN[1].headers)
        self.assertEqual(SEEN[1].body, {"from": "now"})

    def test_too_far_behind_pages_with_poll(self) -> None:
        def script(req: Seen, n: int) -> Any:
            if req.path == "/v1/call/changes.poll":
                if req.body.get("after") == "2.0":
                    first = json.loads(change("3.0", "issue:p").split("data: ")[1])
                    return ("json", 200, {"changes": [first], "next": "50.0", "head": 90, "floor": 0, "expired": False})
                return ("json", 200, {"changes": [], "next": "90.0", "head": 90, "floor": 0, "expired": False})
            if n == 1:
                return ("sse", 200, 'event: end\ndata: {"code":"too_far_behind","cursor":"2.0","message":"page"}\n\n')
            return ("sse", 200, change("91.0", "issue:s"))

        SCRIPT[0] = script
        got = self.take(self.client().changes.subscribe(after="2.0", ids=["issue:p", "issue:s"]), 2)
        self.assertEqual([c["cursor"] for c in got], ["3.0", "91.0"])
        polls = [s for s in SEEN if s.path == "/v1/call/changes.poll"]
        self.assertEqual([p.body["after"] for p in polls], ["2.0", "50.0"])
        self.assertEqual(polls[0].body, {"ids": ["issue:p", "issue:s"], "after": "2.0"})
        self.assertEqual(SEEN[-1].headers["last-event-id"], "90.0")

    def test_final_errors_and_gaps(self) -> None:
        SCRIPT[0] = lambda req, n: ("sse", 200, change("7.0", "", "gap")
                                    + 'event: end\ndata: {"code":"unauthenticated","cursor":"7.0","message":"revoked"}\n\n')
        gaps = []  # type: List[Dict[str, Any]]
        it = self.client().changes.subscribe(on_gap=gaps.append)
        self.assertEqual(next(it)["op"], "gap")
        with self.assertRaises(OntoError) as cm:
            next(it)
        self.assertEqual((cm.exception.code, cm.exception.status), ("unauthenticated", 401))
        self.assertEqual(len(gaps), 1)
        del SEEN[:]
        SCRIPT[0] = lambda req, n: ("json", 400, {"error": {"code": "invalid_argument", "message": "bad op"}})
        with self.assertRaises(OntoError) as cm:
            self.take(self.client().changes.subscribe(ops=["entity.update"]), 1)
        self.assertEqual(cm.exception.status, 400)
        self.assertEqual(len(SEEN), 1, "a refused request is not retried")

    def test_poll_and_head(self) -> None:
        def script(req: Seen, n: int) -> Any:
            if req.path == "/v1/changes/head":
                return ("json", 200, {"cursor": "4.1", "head": 4, "floor": 0, "capture": True})
            return ("json", 200, {"changes": [], "next": "4.1", "head": 4, "floor": 0, "expired": False})

        SCRIPT[0] = script
        onto = self.client()
        self.assertEqual(onto.changes.head()["cursor"], "4.1")
        page = onto.changes.poll(from_="now", limit=10, kinds=["issue"], include_migrations=False)
        self.assertEqual(page["next"], "4.1")
        self.assertEqual(SEEN[1].body, {"kinds": ["issue"], "include_migrations": False, "from": "now", "limit": 10})
        self.assertEqual(SEEN[0].method, "GET")


if __name__ == "__main__":
    unittest.main()
