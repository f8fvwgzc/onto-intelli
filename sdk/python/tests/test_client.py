"""The client against an http.server mock in a thread that records every
request: headers, JSON bodies, SSE framing, error mapping and retries.
Run: python3 -m unittest discover -s tests"""

from __future__ import annotations

import datetime as dt
import json
import os
import sys
import threading
import time
import unittest
import urllib.parse
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from typing import Any, Callable, Dict, List

sys.path.insert(0, os.path.join(os.path.dirname(__file__), ".."))

from onto_intelli import Onto, OntoError, parse_sse  # noqa: E402

KEY = "onto_sk_7f3k9q2m_secretsecretsecret"


class Seen:
    def __init__(self, method: str, path: str, query: Dict[str, str], headers: Dict[str, str], body: Any) -> None:
        self.method = method
        self.path = path
        self.query = query
        self.headers = headers
        self.body = body


class Mock:
    """What the server answers: (status, headers, body bytes or a list of chunks)."""

    def __init__(self) -> None:
        self.seen = []  # type: List[Seen]
        self.respond = lambda req, n: (200, {}, b"{}")  # type: Callable[[Seen, int], Any]
        self.lock = threading.Lock()


MOCK = Mock()


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *args: Any) -> None:
        pass

    def _handle(self) -> None:
        n = int(self.headers.get("content-length") or 0)
        raw = self.rfile.read(n) if n else b""
        url = urllib.parse.urlsplit(self.path)
        seen = Seen(
            self.command,
            url.path,
            dict(urllib.parse.parse_qsl(url.query)),
            {k.lower(): v for k, v in self.headers.items()},
            json.loads(raw) if raw else None,
        )
        with MOCK.lock:
            MOCK.seen.append(seen)
            count = len(MOCK.seen)
        status, headers, body = MOCK.respond(seen, count)
        if isinstance(body, list):
            self.send_response(status)
            for k, v in headers.items():
                self.send_header(k, v)
            self.send_header("transfer-encoding", "chunked")
            self.end_headers()
            for chunk in body:
                self.wfile.write(b"%x\r\n%s\r\n" % (len(chunk), chunk))
                self.wfile.flush()
                time.sleep(0.002)
            self.wfile.write(b"0\r\n\r\n")
            return
        if status == 0:  # never answer in time
            time.sleep(0.6)
            status = 200
        self.send_response(status)
        headers = dict({"content-type": "application/json"}, **headers)
        for k, v in headers.items():
            self.send_header(k, v)
        self.send_header("content-length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def _safe(self) -> None:
        try:
            self._handle()
        except (BrokenPipeError, ConnectionResetError):
            pass  # the client stopped reading (an error event, a timeout)

    do_GET = _safe
    do_POST = _safe


def answer(status: int, body: Any, headers: Dict[str, str] = None) -> Any:
    return (status, headers or {}, json.dumps(body).encode())


class ClientTest(unittest.TestCase):
    server = None  # type: ThreadingHTTPServer
    base = ""

    @classmethod
    def setUpClass(cls) -> None:
        cls.server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        cls.server.daemon_threads = True
        threading.Thread(target=cls.server.serve_forever, daemon=True).start()
        cls.base = "http://127.0.0.1:{}".format(cls.server.server_address[1])

    @classmethod
    def tearDownClass(cls) -> None:
        cls.server.shutdown()
        cls.server.server_close()

    def setUp(self) -> None:
        MOCK.seen = []
        MOCK.respond = lambda req, n: answer(200, {})

    def client(self, **kw: Any) -> Onto:
        args = dict(url=self.base + "/", api_key=KEY)
        args.update(kw)
        return Onto(**args)

    def test_ask_headers_and_body(self) -> None:
        tid = "a" * 32
        MOCK.respond = lambda req, n: answer(200, {"answer": "Payments Team", "trace_id": tid}, {"x-onto-trace-id": tid})
        onto = self.client(tenant="k3v9q2m8x1p4t7w5r6y0", workspace="acme", headers={"x-extra": "1"})
        r = onto.ask(
            "Who owns payments?",
            session_id="chat-42",
            user_id="user:7",
            metadata={"plan": "pro"},
            mode="deterministic",
            tags=["web"],
            conversation_id="new",
            no_cache=True,
        )
        self.assertEqual(r["answer"], "Payments Team")
        self.assertEqual(r["trace_id"], tid)
        req = MOCK.seen[0]
        self.assertEqual((req.method, req.path), ("POST", "/v1/ask"))
        self.assertEqual(req.headers["authorization"], "Bearer " + KEY)
        self.assertEqual(req.headers["x-onto-tenant"], "k3v9q2m8x1p4t7w5r6y0")
        self.assertEqual(req.headers["x-onto-workspace"], "acme")
        self.assertEqual(req.headers["x-onto-session-id"], "chat-42")
        self.assertEqual(req.headers["content-type"], "application/json")
        self.assertEqual(req.headers["x-extra"], "1")
        self.assertEqual(
            req.body,
            {
                "question": "Who owns payments?",
                "conversation_id": "new",
                "mode": "deterministic",
                "no_cache": True,
                "trace": {"session_id": "chat-42", "user_id": "user:7", "tags": ["web"], "metadata": {"plan": "pro"}},
            },
        )

    def test_environment_defaults(self) -> None:
        saved = dict(os.environ)
        try:
            os.environ["ONTO_URL"] = self.base
            os.environ["ONTO_API_KEY"] = "onto_sk_fromenv0_x"
            os.environ["ONTO_TENANT"] = "tenant1"
            Onto().call("system.health")
            self.assertEqual(MOCK.seen[0].headers["authorization"], "Bearer onto_sk_fromenv0_x")
            self.assertEqual(MOCK.seen[0].headers["x-onto-tenant"], "tenant1")
            del os.environ["ONTO_API_KEY"]
            del os.environ["ONTO_TENANT"]
            Onto().call("system.health")
            self.assertNotIn("authorization", MOCK.seen[1].headers)
            self.assertNotIn("x-onto-tenant", MOCK.seen[1].headers)
            del os.environ["ONTO_URL"]
            with self.assertRaises(OntoError) as cm:
                Onto()
            self.assertEqual(cm.exception.code, "invalid_argument")
        finally:
            os.environ.clear()
            os.environ.update(saved)

    def test_key_is_never_shown(self) -> None:
        onto = self.client()
        self.assertNotIn("secretsecret", repr(onto))
        self.assertNotIn("secretsecret", str(onto))

    def test_helpers(self) -> None:
        MOCK.respond = lambda req, n: answer(200, {"ok": True})
        onto = self.client()
        onto.call("graph.impact", {"id": "service:payments"})
        onto.search("payments", kinds=["service"], limit=10)
        onto.entities.get("service:payments", derived=True)
        onto.entities.find(kind="issue", state=["open"], limit=5)
        onto.entities.upsert({"id": "issue:1", "kind": "issue"}, title="Broken")
        onto.entities.patch("issue:1", state="done")
        onto.entities.delete("issue:1")
        onto.entities.history("issue:1", limit=3)
        onto.links.create("team:a", "owns", "service:b")
        onto.links.delete("team:a", "owns", "service:b")
        onto.actions.list()
        onto.actions.plan("assign_issue", {"issue": "issue:1"})
        onto.actions.apply("assign_issue", {"issue": "issue:1"}, confirm=True)
        onto.actions.undo("rcpt_1")
        calls = [(s.method, s.path, s.body) for s in MOCK.seen]
        self.assertEqual(
            calls,
            [
                ("POST", "/v1/call/graph.impact", {"id": "service:payments"}),
                ("POST", "/v1/call/entity.search", {"query": "payments", "kinds": ["service"], "limit": 10}),
                ("POST", "/v1/call/entity.get", {"id": "service:payments", "derived": True}),
                ("POST", "/v1/call/entity.find", {"kind": "issue", "state": ["open"], "limit": 5}),
                ("POST", "/v1/call/entity.upsert", {"id": "issue:1", "kind": "issue", "title": "Broken"}),
                ("POST", "/v1/call/entity.patch", {"id": "issue:1", "state": "done"}),
                ("POST", "/v1/call/entity.delete", {"id": "issue:1"}),
                ("POST", "/v1/call/entity.history", {"id": "issue:1", "limit": 3}),
                ("POST", "/v1/call/link.create", {"from": "team:a", "rel": "owns", "to": "service:b"}),
                ("POST", "/v1/call/link.delete", {"from": "team:a", "rel": "owns", "to": "service:b"}),
                ("POST", "/v1/call/action.list", {}),
                ("POST", "/v1/call/action.plan", {"action": "assign_issue", "params": {"issue": "issue:1"}}),
                (
                    "POST",
                    "/v1/call/action.apply",
                    {"action": "assign_issue", "params": {"issue": "issue:1"}, "confirm": True, "dry_run": False},
                ),
                ("POST", "/v1/call/action.undo", {"receipt": "rcpt_1", "force": False}),
            ],
        )

    def test_objects_traces_sessions(self) -> None:
        MOCK.respond = lambda req, n: answer(200, {"traces": [], "next": None})
        onto = self.client()
        onto.objects.types(workspace="acme")
        onto.objects.explore({"kind": "customer", "search": "ada"}, limit=5)
        since = dt.datetime(2023, 11, 14, 22, 13, 20, tzinfo=dt.timezone.utc)
        onto.traces.list(since=since, status="error", user_id="u1", session_id="s 1", min_ms=2.5, q="2024", limit=10)
        onto.traces.get("abc/def")
        onto.traces.metrics(since=5, bucket="day")
        onto.traces.score("t1", "user.feedback", value=1, comment="good", span_id="s1")
        onto.sessions.list(user_id="u1", limit=2)
        onto.sessions.get("chat-42", limit=20)
        onto.auth.mint_session(act_as="user:7", roles=["viewer"], workspaces=["acme"], entity="person:7", ttl_secs=600)
        s = MOCK.seen
        self.assertEqual((s[0].method, s[0].path, s[0].query), ("GET", "/v1/objects/types", {"workspace": "acme"}))
        self.assertIsNone(s[0].body)
        self.assertNotIn("content-type", s[0].headers)
        self.assertEqual(s[1].body, {"kind": "customer", "search": "ada", "limit": 5})
        self.assertEqual(s[2].path, "/v1/traces")
        self.assertEqual(
            s[2].query,
            {"since": "1700000000000", "status": "error", "user_id": "u1", "session_id": "s 1", "min_ms": "2.5",
             "q": "2024", "limit": "10"},
        )
        self.assertEqual(s[3].path, "/v1/traces/abc%2Fdef")
        self.assertEqual((s[4].path, s[4].query), ("/v1/metrics", {"since": "5", "bucket": "day"}))
        self.assertEqual(s[5].path, "/v1/traces/t1/scores")
        self.assertEqual(s[5].body, {"name": "user.feedback", "value": 1, "comment": "good", "span_id": "s1"})
        self.assertEqual((s[6].path, s[6].query), ("/v1/sessions", {"user_id": "u1", "limit": "2"}))
        self.assertEqual((s[7].path, s[7].query), ("/v1/sessions/chat-42", {"limit": "20"}))
        self.assertEqual(s[8].path, "/v1/auth/session")
        self.assertEqual(
            s[8].body,
            {"act_as": "user:7", "roles": ["viewer"], "workspaces": ["acme"], "entity": "person:7", "ttl_secs": 600},
        )

    def test_errors(self) -> None:
        tid = "f" * 32
        MOCK.respond = lambda req, n: answer(
            403, {"error": {"code": "permission_denied", "message": "key belongs to project a"}}, {"x-onto-trace-id": tid}
        )
        onto = self.client()
        with self.assertRaises(OntoError) as cm:
            onto.call("entity.get", {"id": "x"})
        e = cm.exception
        self.assertEqual((e.status, e.code, e.message, e.trace_id), (403, "permission_denied", "key belongs to project a", tid))
        self.assertEqual(str(e), "key belongs to project a")
        self.assertNotIn("secretsecret", repr(e))
        MOCK.respond = lambda req, n: (404, {"content-type": "text/plain"}, b"no such route")
        with self.assertRaises(OntoError) as cm:
            onto.call("nope")
        self.assertEqual((cm.exception.status, cm.exception.code, cm.exception.message), (404, "not_found", "no such route"))
        MOCK.respond = lambda req, n: answer(500, {"error": {"code": "internal", "message": "boom"}})
        with self.assertRaises(OntoError) as cm:
            onto.ask("x")
        self.assertEqual((cm.exception.status, cm.exception.code), (500, "internal"))
        dead = Onto(url="http://127.0.0.1:9", api_key="k", timeout=2)
        with self.assertRaises(OntoError) as cm:
            dead.call("system.health")
        self.assertEqual((cm.exception.status, cm.exception.code), (0, "unavailable"))

    def test_retries(self) -> None:
        def respond(req: Seen, n: int) -> Any:
            if n == 1:
                return answer(429, {"error": {"code": "resource_exhausted", "message": "slow"}}, {"retry-after": "0"})
            if n == 2:
                return answer(503, {"error": {"code": "unavailable", "message": "busy"}}, {"retry-after": "0.05"})
            return answer(200, {"answer": "ok"})

        MOCK.respond = respond
        onto = self.client()
        t0 = time.monotonic()
        self.assertEqual(onto.ask("q")["answer"], "ok")
        self.assertGreaterEqual(time.monotonic() - t0, 0.045)
        self.assertEqual(len(MOCK.seen), 3)
        self.assertEqual(MOCK.seen[2].body, {"question": "q"})

        MOCK.seen = []
        MOCK.respond = lambda req, n: answer(429, {"error": {"code": "resource_exhausted", "message": "budget"}}, {"retry-after": "0"})
        with self.assertRaises(OntoError) as cm:
            onto.ask("q")
        self.assertEqual((cm.exception.status, cm.exception.retry_after), (429, 0.0))
        self.assertEqual(len(MOCK.seen), 3)

        MOCK.seen = []
        MOCK.respond = lambda req, n: answer(500, {"error": {"code": "internal", "message": "x"}})
        with self.assertRaises(OntoError):
            onto.ask("q")
        self.assertEqual(len(MOCK.seen), 1, "other errors are not retried")

        MOCK.seen = []
        MOCK.respond = lambda req, n: answer(503, {}, {"retry-after": "3600"})
        quick = self.client(max_retries=1, max_retry_delay=0.01)
        with self.assertRaises(OntoError):
            quick.ask("q")
        self.assertEqual(len(MOCK.seen), 2, "one retry, after at most 10 ms instead of an hour")

    def test_timeout(self) -> None:
        MOCK.respond = lambda req, n: (0, {}, b"{}")
        with self.assertRaises(OntoError) as cm:
            self.client(timeout=0.1).call("x")
        self.assertEqual(cm.exception.code, "deadline_exceeded")

    def test_ask_stream_split_chunks(self) -> None:
        done = {"answer": "Payments Team owns it.", "mode": "llm", "trace_id": "b" * 32}
        frames = (
            "event: stage\ndata: " + json.dumps({"type": "stage", "text": "intent", "data": {"intent": "owner"}}) + "\n\n"
            ": keep-alive\n\n"
            "event: delta\r\ndata: " + json.dumps({"type": "delta", "text": "Payments Team "}) + "\r\n\r\n"
            "event: delta\ndata: " + json.dumps({"type": "delta", "text": "owns it ✓."}, ensure_ascii=False) + "\n\n"
            "event: done\ndata: " + json.dumps({"type": "done", "text": "", "data": done}) + "\n\n"
        ).encode("utf-8")
        chunks = [frames[i : i + 7] for i in range(0, len(frames), 7)]
        MOCK.respond = lambda req, n: (200, {"content-type": "text/event-stream"}, chunks)
        onto = self.client()
        events = list(onto.ask_stream("Who owns payments?", conversation_id="new", session_id="s1"))
        self.assertEqual([e["type"] for e in events], ["stage", "delta", "delta", "done"])
        self.assertEqual("".join(e["text"] for e in events[1:3]), "Payments Team owns it ✓.")
        self.assertEqual(events[3]["data"], done)
        req = MOCK.seen[0]
        self.assertEqual((req.path, req.headers["accept"], req.headers["x-onto-session-id"]), ("/v1/ask/stream", "text/event-stream", "s1"))
        self.assertEqual(req.body, {"question": "Who owns payments?", "conversation_id": "new", "trace": {"session_id": "s1"}})

    def test_ask_stream_error_event_and_refusal(self) -> None:
        chunks = [
            b'event: error\ndata: {"type":"error","text":"budget reached","data":{"code":"resource_exhausted"}}\n\n',
            b'event: delta\ndata: {"type":"delta","text":"never"}\n\n',
        ]
        MOCK.respond = lambda req, n: (200, {"content-type": "text/event-stream"}, chunks)
        events = list(self.client().ask_stream("q"))
        self.assertEqual(events, [{"type": "error", "text": "budget reached", "data": {"code": "resource_exhausted"}}])
        MOCK.respond = lambda req, n: answer(429, {"error": {"code": "resource_exhausted", "message": "rate"}}, {"retry-after": "0"})
        with self.assertRaises(OntoError) as cm:
            self.client(max_retries=0).ask_stream("q")
        self.assertEqual(cm.exception.status, 429)


class SseTest(unittest.TestCase):
    def test_framing(self) -> None:
        text = "id: 1\ndata: a\ndata: b\n\n:comment\nevent: x\ndata:c\n\ndata: tail".encode()
        out = list(parse_sse(text[i : i + 3] for i in range(0, len(text), 3)))
        self.assertEqual(
            out,
            [
                {"event": "message", "data": "a\nb", "id": "1"},
                {"event": "x", "data": "c", "id": "1"},
                {"event": "message", "data": "tail", "id": "1"},
            ],
        )

    def test_multibyte_and_cr_split(self) -> None:
        raw = "data: é✓\r\n\r\ndata: z\r\r".encode("utf-8")
        out = list(parse_sse([raw[i : i + 1] for i in range(len(raw))]))
        self.assertEqual([m["data"] for m in out], ["é✓", "z"])


@unittest.skipUnless(os.environ.get("ONTO_URL"), "set ONTO_URL to run against a server (onto serve --dev)")
class LiveTest(unittest.TestCase):
    def test_live(self) -> None:
        onto = Onto(session_id="sdk-py-{}".format(int(time.time())))
        self.assertIsInstance(onto.call("system.health"), dict)
        r = onto.ask("Who owns Project Alpha?", user_id="sdk-test", tags=["sdk"])
        self.assertTrue(r["answer"])
        kinds = [e["type"] for e in onto.ask_stream("What is blocked?")]
        self.assertIn(kinds[-1], ("done", "error"))
        self.assertIsInstance(onto.search("payments", limit=3)["hits"], list)
        with self.assertRaises(OntoError) as cm:
            onto.call("no.such.action")
        self.assertEqual(cm.exception.status, 404)
        who = onto.auth.whoami()
        if who.get("capabilities", {}).get("observe"):
            self.assertIsInstance(onto.traces.list(limit=5)["traces"], list)
            if r.get("trace_id"):
                self.assertEqual(onto.traces.get(r["trace_id"])["trace"]["id"], r["trace_id"])
            self.assertIsInstance(onto.traces.metrics(bucket="day")["requests"], int)


if __name__ == "__main__":
    unittest.main()
