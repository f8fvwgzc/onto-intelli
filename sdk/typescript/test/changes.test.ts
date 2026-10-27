// The change feed against a scripted SSE server: `retry:` parsing,
// reconnects with Last-Event-ID, transparent resumes, the reset callback
// and its default error, the too_far_behind fallback to poll, and final
// errors.

import { test, before, after, beforeEach } from "node:test";
import assert from "node:assert/strict";
import { createServer, type IncomingMessage, type ServerResponse } from "node:http";
import type { AddressInfo } from "node:net";

import { Onto, OntoError, parseSse, type Change } from "../src/index.ts";

interface Seen {
  method: string;
  path: string;
  headers: IncomingMessage["headers"];
  body: Record<string, unknown> | undefined;
}

type Handler = (req: Seen, res: ServerResponse, n: number) => void;

const seen: Seen[] = [];
let handler: Handler = (_req, res) => res.end();
let base = "";
const server = createServer(async (req, res) => {
  const chunks: Buffer[] = [];
  for await (const c of req) chunks.push(c as Buffer);
  const raw = Buffer.concat(chunks).toString("utf8");
  const s: Seen = {
    method: req.method ?? "",
    path: new URL(req.url ?? "/", "http://x").pathname,
    headers: req.headers,
    body: raw ? (JSON.parse(raw) as Record<string, unknown>) : undefined,
  };
  seen.push(s);
  handler(s, res, seen.length);
});

before(async () => {
  await new Promise<void>((r) => server.listen(0, "127.0.0.1", r));
  base = `http://127.0.0.1:${(server.address() as AddressInfo).port}`;
});

after(async () => {
  server.closeAllConnections();
  await new Promise<void>((r) => server.close(() => r()));
});

beforeEach(() => {
  seen.length = 0;
});

function client(): Onto {
  return new Onto({ url: base, apiKey: "onto_sk_7f3k9q2m_secretsecretsecret", tenant: "p1" });
}

function change(cursor: string, id: string, op = "entity.update"): string {
  return `event: change\nid: ${cursor}\ndata: ${JSON.stringify({ cursor, op, id, kind: "issue", workspace: "acme", ts: 1, cause: {} })}\n\n`;
}

function sse(res: ServerResponse, body: string): void {
  res.writeHead(200, { "content-type": "text/event-stream" });
  res.end(body);
}

function json(res: ServerResponse, status: number, body: unknown): void {
  res.writeHead(status, { "content-type": "application/json" });
  res.end(JSON.stringify(body));
}

async function take(it: AsyncGenerator<Change>, n: number): Promise<Change[]> {
  const out: Change[] = [];
  for await (const c of it) {
    out.push(c);
    if (out.length === n) break;
  }
  return out;
}

test("parseSse exposes retry on the messages after it", async () => {
  const bytes = new TextEncoder().encode("retry: 1500\n\nevent: a\ndata: 1\n\nretry: x\nevent: b\ndata: 2\n\n");
  const stream = new ReadableStream<Uint8Array>({
    start(c) {
      c.enqueue(bytes.slice(0, 7));
      c.enqueue(bytes.slice(7));
      c.close();
    },
  });
  const got = [];
  for await (const m of parseSse(stream)) got.push(m);
  assert.deepEqual(got, [
    { event: "a", data: "1", id: undefined, retry: 1500 },
    { event: "b", data: "2", id: undefined, retry: 1500 },
  ]);
});

test("subscribe reconnects with Last-Event-ID and resumes transparently", async () => {
  handler = (req, res, n) => {
    if (n === 1) {
      sse(res, `retry: 5\n\n${change("3.0", "issue:1")}event: heartbeat\nid: 4.2\ndata: {"cursor":"4.2","head":4}\n\nevent: end\ndata: {"code":"slow_consumer","cursor":"3.0","message":"cut"}\n\n`);
    } else if (n === 2) {
      // The connection drops without an end event: back off (retry: 5 ms) and reconnect.
      sse(res, change("5.0", "issue:2"));
    } else {
      sse(res, change("6.1", "issue:3", "entity.create"));
    }
  };
  const got = await take(client().changes.subscribe({ kinds: ["issue"], ops: ["entity.update", "entity.create"] }, { from: "earliest" }), 3);
  assert.deepEqual(got.map((c) => c.cursor), ["3.0", "5.0", "6.1"]);
  assert.equal(seen[0].method, "POST");
  assert.equal(seen[0].path, "/v1/changes/stream");
  assert.equal(seen[0].headers.accept, "text/event-stream");
  assert.equal(seen[0].headers["x-onto-tenant"], "p1");
  assert.deepEqual(seen[0].body, { kinds: ["issue"], ops: ["entity.update", "entity.create"], from: "earliest" });
  assert.equal(seen[0].headers["last-event-id"], undefined);
  // After slow_consumer: from the heartbeat's cursor, which passed 3.0.
  assert.equal(seen[1].headers["last-event-id"], "4.2");
  assert.deepEqual(seen[1].body, { kinds: ["issue"], ops: ["entity.update", "entity.create"] });
  assert.equal(seen[2].headers["last-event-id"], "5.0");
});

test("a reset throws cursor_expired unless onReset handles it, then streams from now", async () => {
  const reset = `event: reset\ndata: {"code":"cursor_expired","floor":10,"head":12,"message":"re-read with entity.find"}\n\n`;
  handler = (_req, res) => sse(res, reset);
  await assert.rejects(take(client().changes.subscribe({}, { after: "1.0" }), 1), (e: unknown) => {
    assert.ok(e instanceof OntoError);
    assert.equal(e.code, "cursor_expired");
    assert.match(e.message, /entity\.find/);
    return true;
  });
  seen.length = 0;
  handler = (_req, res, n) => (n === 1 ? sse(res, reset) : sse(res, change("13.0", "issue:9")));
  const resets: unknown[] = [];
  const got = await take(client().changes.subscribe({}, { after: "1.0", onReset: (r) => void resets.push(r) }), 1);
  assert.equal(got[0].cursor, "13.0");
  assert.deepEqual(resets, [{ code: "cursor_expired", floor: 10, head: 12, message: "re-read with entity.find" }]);
  assert.equal(seen[0].headers["last-event-id"], "1.0");
  assert.equal(seen[1].headers["last-event-id"], undefined);
  assert.deepEqual(seen[1].body, { from: "now" });
});

test("too_far_behind pages with poll up to the head, then streams again", async () => {
  handler = (req, res, n) => {
    if (req.path === "/v1/call/changes.poll") {
      const after = req.body?.after;
      if (after === "2.0") return json(res, 200, { changes: [JSON.parse(change("3.0", "issue:p").split("data: ")[1])], next: "50.0", head: 90, floor: 0, expired: false });
      return json(res, 200, { changes: [], next: "90.0", head: 90, floor: 0, expired: false });
    }
    if (n === 1) return sse(res, `event: end\ndata: {"code":"too_far_behind","cursor":"2.0","message":"page"}\n\n`);
    return sse(res, change("91.0", "issue:s"));
  };
  const got = await take(client().changes.subscribe({ ids: ["issue:p", "issue:s"] }, { after: "2.0" }), 2);
  assert.deepEqual(got.map((c) => c.cursor), ["3.0", "91.0"]);
  const polls = seen.filter((s) => s.path === "/v1/call/changes.poll");
  assert.deepEqual(polls.map((p) => p.body?.after), ["2.0", "50.0"]);
  assert.deepEqual(polls[0].body, { ids: ["issue:p", "issue:s"], after: "2.0" });
  assert.equal(seen.at(-1)?.headers["last-event-id"], "90.0");
});

test("a revoked credential is final; gaps reach onGap", async () => {
  handler = (_req, res) =>
    sse(res, `${change("7.0", "", "gap")}event: end\ndata: {"code":"unauthenticated","cursor":"7.0","message":"revoked"}\n\n`);
  const gaps: Change[] = [];
  const it = client().changes.subscribe({}, { onGap: (g) => void gaps.push(g) });
  const first = await it.next();
  assert.equal((first.value as Change).op, "gap");
  await assert.rejects(it.next(), (e: unknown) => e instanceof OntoError && e.code === "unauthenticated" && e.status === 401);
  assert.equal(gaps.length, 1);
  // A refused request (bad filter) is not retried.
  seen.length = 0;
  handler = (_req, res) => json(res, 400, { error: { code: "invalid_argument", message: "changes.stream: bad op" } });
  await assert.rejects(take(client().changes.subscribe({ ops: ["entity.update"] }), 1), (e: unknown) => e instanceof OntoError && e.status === 400);
  assert.equal(seen.length, 1);
});

test("poll and head are the unary calls", async () => {
  handler = (req, res) =>
    req.path === "/v1/changes/head"
      ? json(res, 200, { cursor: "4.1", head: 4, floor: 0, capture: true })
      : json(res, 200, { changes: [], next: "4.1", head: 4, floor: 0, expired: false });
  const onto = client();
  assert.equal((await onto.changes.head()).cursor, "4.1");
  const page = await onto.changes.poll({ kinds: ["issue"], from: "now", limit: 10, includeMigrations: false });
  assert.equal(page.next, "4.1");
  assert.deepEqual(seen[1].body, { kinds: ["issue"], include_migrations: false, from: "now", limit: 10 });
  assert.equal(seen[0].method, "GET");
});

test("abort ends a subscription", async () => {
  handler = (_req, res) => {
    res.writeHead(200, { "content-type": "text/event-stream" });
    res.write(change("1.0", "issue:1"));
  };
  const ctl = new AbortController();
  const it = client().changes.subscribe({}, { signal: ctl.signal });
  assert.equal(((await it.next()).value as Change).cursor, "1.0");
  ctl.abort();
  await assert.rejects(it.next(), (e: unknown) => e instanceof OntoError && e.code === "cancelled");
});
