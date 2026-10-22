// The client against a node:http mock server that records every request:
// headers, JSON bodies, SSE framing, error mapping and retries.

import { test, before, after, beforeEach } from "node:test";
import assert from "node:assert/strict";
import { createServer, type IncomingMessage, type ServerResponse } from "node:http";
import type { AddressInfo } from "node:net";

import { Onto, OntoError, parseSse } from "../src/index.ts";

interface Seen {
  method: string;
  path: string;
  query: URLSearchParams;
  headers: IncomingMessage["headers"];
  body: unknown;
}

type Handler = (req: Seen, res: ServerResponse, n: number) => void | Promise<void>;

const seen: Seen[] = [];
let handler: Handler = (_req, res) => json(res, 200, {});
let base = "";
const server = createServer(async (req, res) => {
  const chunks: Buffer[] = [];
  for await (const c of req) chunks.push(c as Buffer);
  const raw = Buffer.concat(chunks).toString("utf8");
  const url = new URL(req.url ?? "/", "http://x");
  const s: Seen = {
    method: req.method ?? "",
    path: url.pathname,
    query: url.searchParams,
    headers: req.headers,
    body: raw ? JSON.parse(raw) : undefined,
  };
  seen.push(s);
  await handler(s, res, seen.length);
});

function json(res: ServerResponse, status: number, body: unknown, headers: Record<string, string> = {}): void {
  res.writeHead(status, { "content-type": "application/json", ...headers });
  res.end(JSON.stringify(body));
}

before(async () => {
  await new Promise<void>((r) => server.listen(0, "127.0.0.1", r));
  base = `http://127.0.0.1:${(server.address() as AddressInfo).port}`;
});

after(async () => {
  await new Promise<void>((r) => server.close(() => r()));
});

beforeEach(() => {
  seen.length = 0;
  handler = (_req, res) => json(res, 200, {});
});

function client(extra: Partial<ConstructorParameters<typeof Onto>[0]> = {}): Onto {
  return new Onto({ url: `${base}/`, apiKey: "onto_sk_7f3k9q2m_secretsecretsecret", ...extra });
}

test("ask sends the key, tenant, session id and a snake_case body", async () => {
  handler = (_req, res) =>
    json(res, 200, { answer: "Payments Team", mode: "deterministic", trace_id: "a".repeat(32) }, { "x-onto-trace-id": "a".repeat(32) });
  const onto = client({ tenant: "k3v9q2m8x1p4t7w5r6y0", workspace: "acme", headers: { "x-extra": "1" } });
  const r = await onto.ask("Who owns payments?", {
    sessionId: "chat-42",
    userId: "user:7",
    tags: ["web"],
    metadata: { plan: "pro" },
    mode: "deterministic",
    conversationId: "new",
    noCache: true,
  });
  assert.equal(r.answer, "Payments Team");
  assert.equal(r.trace_id, "a".repeat(32));
  const [req] = seen;
  assert.equal(req.method, "POST");
  assert.equal(req.path, "/v1/ask");
  assert.equal(req.headers.authorization, "Bearer onto_sk_7f3k9q2m_secretsecretsecret");
  assert.equal(req.headers["x-onto-tenant"], "k3v9q2m8x1p4t7w5r6y0");
  assert.equal(req.headers["x-onto-workspace"], "acme");
  assert.equal(req.headers["x-onto-session-id"], "chat-42");
  assert.equal(req.headers["content-type"], "application/json");
  assert.equal(req.headers["x-extra"], "1");
  assert.deepEqual(req.body, {
    question: "Who owns payments?",
    conversation_id: "new",
    mode: "deterministic",
    no_cache: true,
    trace: { session_id: "chat-42", user_id: "user:7", tags: ["web"], metadata: { plan: "pro" } },
  });
});

test("environment defaults and no key", async () => {
  const saved = { ...process.env };
  try {
    process.env.ONTO_URL = base;
    process.env.ONTO_API_KEY = "onto_sk_fromenv0_x";
    process.env.ONTO_TENANT = "tenant1";
    await new Onto().call("system.health");
    assert.equal(seen[0].headers.authorization, "Bearer onto_sk_fromenv0_x");
    assert.equal(seen[0].headers["x-onto-tenant"], "tenant1");
    delete process.env.ONTO_API_KEY;
    delete process.env.ONTO_TENANT;
    await new Onto().call("system.health");
    assert.equal(seen[1].headers.authorization, undefined, "dev mode or a public route: no key, no header");
    assert.equal(seen[1].headers["x-onto-tenant"], undefined);
    delete process.env.ONTO_URL;
    assert.throws(() => new Onto(), (e: unknown) => e instanceof OntoError && e.code === "invalid_argument");
  } finally {
    process.env = saved;
  }
});

test("the key never shows when the client is printed or serialized", async () => {
  const onto = client();
  const { inspect } = await import("node:util");
  assert.ok(!inspect(onto, { depth: 5 }).includes("secretsecret"));
  assert.ok(!JSON.stringify(onto).includes("secretsecret"));
});

test("call, search and the entity, link and action helpers", async () => {
  handler = (_req, res) => json(res, 200, { ok: true });
  const onto = client();
  await onto.call("graph.impact", { id: "service:payments" });
  await onto.search("payments", { kinds: ["service"], limit: 10 });
  await onto.entities.get("service:payments", { derived: true });
  await onto.entities.find({ kind: "issue", state: ["open"], limit: 5 });
  await onto.entities.upsert({ id: "issue:1", kind: "issue", title: "Broken" });
  await onto.entities.patch({ id: "issue:1", state: "done" });
  await onto.entities.delete("issue:1");
  await onto.entities.history("issue:1", { limit: 3 });
  await onto.links.create({ from: "team:a", rel: "owns", to: "service:b" });
  await onto.links.delete({ from: "team:a", rel: "owns", to: "service:b" });
  await onto.actions.list();
  await onto.actions.plan("assign_issue", { issue: "issue:1" });
  await onto.actions.apply("assign_issue", { issue: "issue:1" }, { confirm: true });
  await onto.actions.undo("rcpt_1");
  const calls = seen.map((s) => [s.method, s.path, s.body]);
  assert.deepEqual(calls, [
    ["POST", "/v1/call/graph.impact", { id: "service:payments" }],
    ["POST", "/v1/call/entity.search", { query: "payments", kinds: ["service"], limit: 10 }],
    ["POST", "/v1/call/entity.get", { id: "service:payments", derived: true }],
    ["POST", "/v1/call/entity.find", { kind: "issue", state: ["open"], limit: 5 }],
    ["POST", "/v1/call/entity.upsert", { id: "issue:1", kind: "issue", title: "Broken" }],
    ["POST", "/v1/call/entity.patch", { id: "issue:1", state: "done" }],
    ["POST", "/v1/call/entity.delete", { id: "issue:1" }],
    ["POST", "/v1/call/entity.history", { id: "issue:1", limit: 3 }],
    ["POST", "/v1/call/link.create", { from: "team:a", rel: "owns", to: "service:b" }],
    ["POST", "/v1/call/link.delete", { from: "team:a", rel: "owns", to: "service:b" }],
    ["POST", "/v1/call/action.list", {}],
    ["POST", "/v1/call/action.plan", { action: "assign_issue", params: { issue: "issue:1" } }],
    ["POST", "/v1/call/action.apply", { action: "assign_issue", params: { issue: "issue:1" }, confirm: true, dry_run: false }],
    ["POST", "/v1/call/action.undo", { receipt: "rcpt_1", force: false }],
  ]);
});

test("objects, traces, sessions and sessions for browsers", async () => {
  handler = (_req, res) => json(res, 200, { traces: [], next: null });
  const onto = client();
  await onto.objects.types({ workspace: "acme" });
  await onto.objects.explore({ kind: "customer", search: "ada", limit: 5 });
  await onto.traces.list({ since: new Date(1700000000000), status: "error", userId: "u1", sessionId: "s 1", minMs: 2.5, q: "2024", limit: 10 });
  await onto.traces.get("abc/def");
  await onto.traces.metrics({ since: 5, bucket: "day" });
  await onto.traces.score("t1", { name: "user.feedback", value: 1, comment: "good", spanId: "s1" });
  await onto.sessions.list({ userId: "u1", limit: 2 });
  await onto.sessions.get("chat-42", { limit: 20 });
  await onto.auth.mintSession({ actAs: "user:7", roles: ["viewer"], workspaces: ["acme"], entity: "person:7", ttlSecs: 600 });
  assert.equal(seen[0].path, "/v1/objects/types");
  assert.equal(seen[0].method, "GET");
  assert.equal(seen[0].query.get("workspace"), "acme");
  assert.equal(seen[0].body, undefined);
  assert.equal(seen[0].headers["content-type"], undefined, "no body, no content type");
  assert.deepEqual(seen[1].body, { kind: "customer", search: "ada", limit: 5 });
  assert.equal(seen[2].path, "/v1/traces");
  assert.deepEqual(Object.fromEntries(seen[2].query), {
    since: "1700000000000",
    status: "error",
    user_id: "u1",
    session_id: "s 1",
    min_ms: "2.5",
    q: "2024",
    limit: "10",
  });
  assert.equal(seen[3].path, "/v1/traces/abc%2Fdef");
  assert.deepEqual(Object.fromEntries(seen[4].query), { since: "5", bucket: "day" });
  assert.equal(seen[4].path, "/v1/metrics");
  assert.equal(seen[5].path, "/v1/traces/t1/scores");
  assert.deepEqual(seen[5].body, { name: "user.feedback", value: 1, comment: "good", span_id: "s1" });
  assert.equal(seen[6].path, "/v1/sessions");
  assert.deepEqual(Object.fromEntries(seen[6].query), { user_id: "u1", limit: "2" });
  assert.equal(seen[7].path, "/v1/sessions/chat-42");
  assert.equal(seen[7].query.get("limit"), "20");
  assert.equal(seen[8].path, "/v1/auth/session");
  assert.deepEqual(seen[8].body, { act_as: "user:7", roles: ["viewer"], workspaces: ["acme"], entity: "person:7", ttl_secs: 600 });
});

test("error bodies become OntoError with status, code, message and trace id", async () => {
  handler = (_req, res) =>
    json(res, 403, { error: { code: "permission_denied", message: "key onto_sk_x belongs to project a, not b" } }, { "x-onto-trace-id": "f".repeat(32) });
  const onto = client();
  await assert.rejects(onto.call("entity.get", { id: "x" }), (e: unknown) => {
    assert.ok(e instanceof OntoError);
    assert.equal(e.status, 403);
    assert.equal(e.code, "permission_denied");
    assert.equal(e.message, "key onto_sk_x belongs to project a, not b");
    assert.equal(e.traceId, "f".repeat(32));
    assert.equal(e.trace_id, "f".repeat(32));
    assert.ok(!String(e.stack).includes("secretsecret"));
    return true;
  });
  // Not JSON: the status decides the code.
  handler = (_req, res) => {
    res.writeHead(404, { "content-type": "text/plain" });
    res.end("no such route");
  };
  await assert.rejects(onto.call("nope"), (e: unknown) => e instanceof OntoError && e.status === 404 && e.code === "not_found" && e.message === "no such route");
  handler = (_req, res) => json(res, 500, { error: { code: "internal", message: "boom" } });
  await assert.rejects(onto.ask("x"), (e: unknown) => e instanceof OntoError && e.status === 500 && e.code === "internal");
  // No server: status 0.
  const dead = new Onto({ url: "http://127.0.0.1:9", apiKey: "k" });
  await assert.rejects(dead.call("system.health"), (e: unknown) => e instanceof OntoError && e.status === 0 && e.code === "unavailable");
});

test("429 and 503 are retried up to twice, honouring retry-after", async () => {
  handler = (_req, res, n) => {
    if (n === 1) return json(res, 429, { error: { code: "resource_exhausted", message: "slow down" } }, { "retry-after": "0" });
    if (n === 2) return json(res, 503, { error: { code: "unavailable", message: "busy" } }, { "retry-after": "0.05" });
    return json(res, 200, { answer: "ok" });
  };
  const onto = client();
  const t0 = Date.now();
  const r = await onto.ask("q");
  assert.equal(r.answer, "ok");
  assert.equal(seen.length, 3);
  assert.ok(Date.now() - t0 >= 45, "waited for retry-after");
  assert.deepEqual(seen[2].body, { question: "q" }, "the same body each time");

  // A third refusal is the caller's.
  seen.length = 0;
  handler = (_req, res) => json(res, 429, { error: { code: "resource_exhausted", message: "budget" } }, { "retry-after": "0" });
  await assert.rejects(onto.ask("q"), (e: unknown) => e instanceof OntoError && e.status === 429 && e.retryAfter === 0);
  assert.equal(seen.length, 3);

  // Other errors are not retried; maxRetries and maxRetryDelayMs are options.
  seen.length = 0;
  handler = (_req, res) => json(res, 500, { error: { code: "internal", message: "x" } });
  await assert.rejects(onto.ask("q"));
  assert.equal(seen.length, 1);
  seen.length = 0;
  handler = (_req, res) => json(res, 503, {}, { "retry-after": "3600" });
  const quick = client({ maxRetries: 1, maxRetryDelayMs: 10 });
  await assert.rejects(quick.ask("q"), (e: unknown) => e instanceof OntoError && e.status === 503);
  assert.equal(seen.length, 2, "one retry, after at most 10 ms instead of an hour");
});

test("timeouts and aborts", async () => {
  handler = (_req, res) => {
    setTimeout(() => json(res, 200, {}), 300); // answers too late
  };
  const slow = client({ timeoutMs: 50 });
  await assert.rejects(slow.call("x"), (e: unknown) => e instanceof OntoError && e.code === "deadline_exceeded");
  const ctl = new AbortController();
  const p = client().call("x", {}, { signal: ctl.signal });
  setTimeout(() => ctl.abort(), 20);
  await assert.rejects(p, (e: unknown) => e instanceof OntoError && e.code === "cancelled");
});

test("a signal aborts the body too, not only the headers", { timeout: 10_000 }, async () => {
  // Headers at once, then a stream that never ends.
  let closed = false;
  handler = (_req, res) => {
    res.writeHead(200, { "content-type": "text/event-stream" });
    res.write(`event: delta\ndata: ${JSON.stringify({ type: "delta", text: "a" })}\n\n`);
    const t = setInterval(() => res.write(": keep-alive\n\n"), 5);
    res.on("close", () => {
      clearInterval(t);
      closed = true;
    });
  };
  const ctl = new AbortController();
  const events: string[] = [];
  await assert.rejects(
    (async () => {
      for await (const ev of client().askStream({ question: "q", signal: ctl.signal })) {
        events.push(ev.type);
        setTimeout(() => ctl.abort(), 20);
      }
    })(),
    (e: unknown) => e instanceof OntoError && e.code === "cancelled",
  );
  assert.deepEqual(events, ["delta"]);
  for (let i = 0; i < 100 && !closed; i++) await new Promise((r) => setTimeout(r, 10));
  assert.ok(closed, "the connection is closed");
  // A JSON body that is slow to arrive.
  handler = (_req, res) => {
    res.writeHead(200, { "content-type": "application/json" });
    res.write("{");
    setTimeout(() => res.end("}"), 500);
  };
  const ctl2 = new AbortController();
  const p = client().call("x", {}, { signal: ctl2.signal });
  setTimeout(() => ctl2.abort(), 50);
  await assert.rejects(p, (e: unknown) => e instanceof OntoError && e.code === "cancelled");
});

function sse(res: ServerResponse, chunks: string[]): void {
  res.writeHead(200, { "content-type": "text/event-stream", "x-onto-trace-id": "b".repeat(32) });
  let i = 0;
  const next = () => {
    if (i === chunks.length) return res.end();
    res.write(chunks[i++]);
    setTimeout(next, 2);
  };
  next();
}

test("askStream parses events split across chunks", async () => {
  const done = { answer: "Payments Team owns it.", mode: "llm", trace_id: "b".repeat(32) };
  const frames =
    `event: stage\ndata: ${JSON.stringify({ type: "stage", text: "intent", data: { intent: "owner" } })}\n\n` +
    `: keep-alive\n\n` +
    `event: delta\r\ndata: ${JSON.stringify({ type: "delta", text: "Payments Team " })}\r\n\r\n` +
    `event: delta\ndata: ${JSON.stringify({ type: "delta", text: "owns it ✓." })}\n\n` +
    `event: done\ndata: ${JSON.stringify({ type: "done", text: "", data: done })}\n\n`;
  // Split every 7 bytes: inside field names, JSON, a multi-byte character and \r\n.
  const bytes = Buffer.from(frames, "utf8");
  handler = (_req, res) => {
    res.writeHead(200, { "content-type": "text/event-stream" });
    let i = 0;
    const next = () => {
      if (i >= bytes.length) return res.end();
      res.write(bytes.subarray(i, i + 7));
      i += 7;
      setImmediate(next);
    };
    next();
  };
  const onto = client();
  const events = [];
  for await (const ev of onto.askStream({ question: "Who owns payments?", conversationId: "new", sessionId: "s1" })) {
    events.push(ev);
  }
  assert.deepEqual(
    events.map((e) => e.type),
    ["stage", "delta", "delta", "done"],
  );
  assert.equal(events.slice(1, 3).map((e) => e.text).join(""), "Payments Team owns it ✓.");
  assert.deepEqual(events[3].data, done);
  assert.equal(seen[0].path, "/v1/ask/stream");
  assert.equal(seen[0].headers.accept, "text/event-stream");
  assert.equal(seen[0].headers["x-onto-session-id"], "s1");
  assert.deepEqual(seen[0].body, { question: "Who owns payments?", conversation_id: "new", trace: { session_id: "s1" } });
});

test("askStream ends at an error event and maps a refused stream", async () => {
  handler = (_req, res) =>
    sse(res, [`event: error\ndata: {"type":"error","text":"budget reached","data":{"code":"resource_exhausted"}}\n\n`, `event: delta\ndata: {"type":"delta","text":"never"}\n\n`]);
  const events = [];
  for await (const ev of client().askStream("q")) events.push(ev);
  assert.deepEqual(events, [{ type: "error", text: "budget reached", data: { code: "resource_exhausted" } }]);
  handler = (_req, res) => json(res, 429, { error: { code: "resource_exhausted", message: "rate" } }, { "retry-after": "0" });
  await assert.rejects(
    (async () => {
      for await (const _ of client({ maxRetries: 0 }).askStream("q")) {
        // nothing
      }
    })(),
    (e: unknown) => e instanceof OntoError && e.status === 429,
  );
});

test("the SSE parser: multi-line data, ids, comments, a missing final blank line", async () => {
  const text = "id: 1\ndata: a\ndata: b\n\n:comment\nevent: x\ndata:c\n\ndata: tail";
  const stream = new ReadableStream<Uint8Array>({
    start(c) {
      const b = new TextEncoder().encode(text);
      for (let i = 0; i < b.length; i += 3) c.enqueue(b.subarray(i, i + 3));
      c.close();
    },
  });
  const out = [];
  for await (const m of parseSse(stream)) out.push(m);
  assert.deepEqual(out, [
    { event: "message", data: "a\nb", id: "1" },
    { event: "x", data: "c", id: "1" },
    { event: "message", data: "tail", id: "1" },
  ]);
});
