// Webhook verification against the Standard Webhooks reference vector (the
// same vector as the Rust and Python tests): secret
// whsec_MfKQ9r8GKYqrTwjUPD8ILPZIo2LaLaSw, id msg_p5jXN8AQM9LWM0D4loKWxJek,
// timestamp 1614265330, body {"test": 2432232314}. The expected signature
// v1,g0hM9SsE+OTPJTGt/tmIKtSyZlE3uFJELVlNIOLJ1OE= is the reference libraries'
// test value, recomputed independently with Python's hmac and base64.

import { test } from "node:test";
import assert from "node:assert/strict";

import { WebhookVerificationError, signWebhook, verifyWebhook } from "../src/index.ts";

const SECRET = "whsec_MfKQ9r8GKYqrTwjUPD8ILPZIo2LaLaSw";
const ID = "msg_p5jXN8AQM9LWM0D4loKWxJek";
const TS = 1614265330;
const BODY = '{"test": 2432232314}';
const EXPECTED = "v1,g0hM9SsE+OTPJTGt/tmIKtSyZlE3uFJELVlNIOLJ1OE=";
const OTHER = "whsec_dGVzdC1zZWNyZXQtdHdvLXR3by10d28tdHdvLXR3bw==";

const headers = (sig: string, id = ID, ts = TS) => ({
  "webhook-id": id,
  "webhook-timestamp": String(ts),
  "webhook-signature": sig,
});

test("the reference vector signs byte for byte", async () => {
  assert.equal(await signWebhook(SECRET, ID, TS, BODY), EXPECTED);
  assert.equal(await signWebhook(SECRET, ID, TS, new TextEncoder().encode(BODY)), EXPECTED);
});

test("verification accepts the vector and returns the parsed body", async () => {
  const ev = await verifyWebhook(SECRET, headers(EXPECTED), BODY, { now: TS });
  assert.deepEqual(ev, { test: 2432232314 });
  // Headers objects (fetch) and any header case work too.
  const h = new Headers(headers(EXPECTED));
  assert.deepEqual(await verifyWebhook([SECRET], h, BODY, { now: TS + 299 }), { test: 2432232314 });
  const upper = { "Webhook-Id": ID, "Webhook-Timestamp": String(TS), "Webhook-Signature": EXPECTED };
  assert.deepEqual(await verifyWebhook(SECRET, upper, BODY, { now: TS }), { test: 2432232314 });
});

test("rotation: either secret verifies a header with both signatures", async () => {
  const both = `${await signWebhook(SECRET, ID, TS, BODY)} ${await signWebhook(OTHER, ID, TS, BODY)}`;
  assert.ok(both.startsWith(`${EXPECTED} v1,`));
  await verifyWebhook(SECRET, headers(both), BODY, { now: TS });
  await verifyWebhook(OTHER, headers(both), BODY, { now: TS });
  await verifyWebhook([OTHER, SECRET], headers(EXPECTED), BODY, { now: TS });
});

test("refusals", async () => {
  const refused = async (p: Promise<unknown>, msg: RegExp) =>
    assert.rejects(p, (e: unknown) => e instanceof WebhookVerificationError && msg.test((e as Error).message));
  await refused(verifyWebhook(SECRET, headers(EXPECTED), BODY, { now: TS + 301 }), /tolerance/);
  await refused(verifyWebhook(SECRET, headers(EXPECTED), BODY.replace("2432", "2433"), { now: TS }), /no webhook-signature/);
  await refused(verifyWebhook(SECRET, headers(EXPECTED, "msg_other"), BODY, { now: TS }), /no webhook-signature/);
  await refused(verifyWebhook(OTHER, headers(EXPECTED), BODY, { now: TS }), /no webhook-signature/);
  await refused(verifyWebhook(SECRET, headers("v2,abc"), BODY, { now: TS }), /no webhook-signature/);
  await refused(verifyWebhook(SECRET, { "webhook-id": ID }, BODY, { now: TS }), /missing/);
  // A CloudEvent whose id is not the webhook-id is refused even when signed.
  const ev = JSON.stringify({ specversion: "1.0", id: "obx_other", type: "onto.webhook.test", data: {} });
  const sig = await signWebhook(SECRET, "obx_1", TS, ev);
  await refused(verifyWebhook(SECRET, headers(sig, "obx_1"), ev, { now: TS }), /event id/);
  const good = JSON.stringify({ specversion: "1.0", id: "obx_1", type: "onto.webhook.test", data: {} });
  const ok = await verifyWebhook(SECRET, headers(await signWebhook(SECRET, "obx_1", TS, good), "obx_1"), good, { now: TS });
  assert.equal(ok.id, "obx_1");
});
