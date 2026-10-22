// Against a running server, only when ONTO_URL is set, e.g.
//   onto seed demo && onto serve --dev
//   ONTO_URL=http://127.0.0.1:50051 npm test
// ONTO_API_KEY and ONTO_TENANT are used when set.

import { test } from "node:test";
import assert from "node:assert/strict";

import { Onto, OntoError } from "../src/index.ts";

const live = Boolean(process.env.ONTO_URL);

test("live: health, ask, stream, search, traces", { skip: live ? false : "set ONTO_URL to run" }, async () => {
  const onto = new Onto({ sessionId: `sdk-ts-${Date.now()}` });
  const health = await onto.call("system.health");
  assert.equal(typeof health, "object");

  const r = await onto.ask("Who owns Project Alpha?", { userId: "sdk-test", tags: ["sdk"] });
  assert.equal(typeof r.answer, "string");
  assert.ok(r.answer.length > 0);

  const kinds: string[] = [];
  for await (const ev of onto.askStream({ question: "What is blocked?" })) kinds.push(ev.type);
  assert.ok(kinds.at(-1) === "done" || kinds.at(-1) === "error", kinds.join(","));

  const hits = await onto.search("payments", { limit: 3 });
  assert.ok(Array.isArray(hits.hits));

  await assert.rejects(onto.call("no.such.action"), (e: unknown) => e instanceof OntoError && e.status === 404);

  const who = await onto.auth.whoami();
  const capabilities = who.capabilities as { observe?: boolean } | undefined;
  if (capabilities?.observe) {
    const page = await onto.traces.list({ limit: 5 });
    assert.ok(Array.isArray(page.traces));
    if (r.trace_id) {
      const detail = await onto.traces.get(r.trace_id);
      assert.equal(detail.trace.id, r.trace_id);
    }
    const m = await onto.traces.metrics({ bucket: "day" });
    assert.equal(typeof m.requests, "number");
  }
});
