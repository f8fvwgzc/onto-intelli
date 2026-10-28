// Verifying onto-intelli webhook deliveries (docs/design/EVENTS.md B.3):
// CloudEvents 1.0 structured JSON bodies signed with Standard Webhooks.
//
// The signed content is `<webhook-id>.<webhook-timestamp>.<body>`, signed with
// HMAC-SHA256; `webhook-signature` holds one or more `v1,<base64>` values
// separated by spaces (two during a secret rotation). A `whsec_` secret is
// base64 after its prefix; any other secret is used as its UTF-8 bytes.
//
// Each change is enqueued once per endpoint and delivered at least once:
// dedupe on the event id (`webhook-id`, the envelope's `id`).

/** The Standard Webhooks default: timestamps further than this from now are refused. */
export const WEBHOOK_TOLERANCE_SECS = 300;

/** A CloudEvents 1.0 envelope as onto-intelli sends it. */
export interface CloudEvent {
  specversion: string;
  id: string;
  source: string;
  type: string;
  time: string;
  subject?: string;
  datacontenttype?: string;
  tenant?: string;
  workspace?: string;
  cursor?: string;
  causeautomation?: string;
  causerun?: string;
  causedepth?: number;
  data: unknown;
  [extension: string]: unknown;
}

export class WebhookVerificationError extends Error {
  constructor(message: string) {
    super(message);
    this.name = "WebhookVerificationError";
  }
}

export type WebhookHeaders =
  | { get(name: string): string | null }
  | Record<string, string | string[] | undefined>;

export interface VerifyOptions {
  /** Seconds the timestamp may differ from now (default 300). */
  toleranceSecs?: number;
  /** Now, in Unix seconds (default: the clock). */
  now?: number;
}

function header(h: WebhookHeaders, name: string): string {
  if (typeof (h as { get?: unknown }).get === "function") {
    return ((h as { get(n: string): string | null }).get(name) ?? "").trim();
  }
  const rec = h as Record<string, string | string[] | undefined>;
  const key = Object.keys(rec).find((k) => k.toLowerCase() === name);
  const v = key === undefined ? undefined : rec[key];
  return (Array.isArray(v) ? v[0] ?? "" : v ?? "").trim();
}

function fromBase64(b64: string): Uint8Array {
  const bin = atob(b64);
  const out = new Uint8Array(bin.length);
  for (let i = 0; i < bin.length; i++) out[i] = bin.charCodeAt(i);
  return out;
}

function toBase64(bytes: Uint8Array): string {
  let bin = "";
  for (const b of bytes) bin += String.fromCharCode(b);
  return btoa(bin);
}

/** The HMAC key of a secret: base64 after `whsec_`, else the UTF-8 bytes. */
export function signingKey(secret: string): Uint8Array {
  const s = secret.trim();
  if (s.startsWith("whsec_")) {
    try {
      return fromBase64(s.slice("whsec_".length));
    } catch {
      throw new WebhookVerificationError("a whsec_ secret is base64 after its prefix");
    }
  }
  if (!s) throw new WebhookVerificationError("an empty signing secret");
  return new TextEncoder().encode(s);
}

async function subtle(): Promise<SubtleCrypto> {
  const c = (globalThis as { crypto?: Crypto }).crypto;
  if (c?.subtle) return c.subtle;
  // Node 18 without the global: its Web Crypto implementation (a computed
  // specifier, so bundlers for browsers leave it alone).
  const specifier = "node:crypto";
  const nodeCrypto = (await import(/* @vite-ignore */ specifier)) as { webcrypto: Crypto };
  return nodeCrypto.webcrypto.subtle;
}

function bodyBytes(body: string | Uint8Array): Uint8Array {
  return typeof body === "string" ? new TextEncoder().encode(body) : body;
}

/** One Standard Webhooks signature: `v1,` and base64 of HMAC-SHA256(key, `<id>.<timestamp>.<body>`). */
export async function signWebhook(
  secret: string,
  id: string,
  timestamp: number,
  body: string | Uint8Array,
): Promise<string> {
  const s = await subtle();
  const key = await s.importKey("raw", signingKey(secret) as BufferSource, { name: "HMAC", hash: "SHA-256" }, false, ["sign"]);
  const prefix = new TextEncoder().encode(`${id}.${timestamp}.`);
  const b = bodyBytes(body);
  const msg = new Uint8Array(prefix.length + b.length);
  msg.set(prefix, 0);
  msg.set(b, prefix.length);
  const mac = new Uint8Array(await s.sign("HMAC", key, msg as BufferSource));
  return `v1,${toBase64(mac)}`;
}

function sameText(a: string, b: string): boolean {
  if (a.length !== b.length) return false;
  let diff = 0;
  for (let i = 0; i < a.length; i++) diff |= a.charCodeAt(i) ^ b.charCodeAt(i);
  return diff === 0;
}

/**
 * Verify a delivery and return its parsed body (the CloudEvent). Pass the
 * raw body exactly as received, and every secret that may sign it (the
 * current and, during a rotation, the previous). Throws
 * `WebhookVerificationError` when the timestamp is outside the tolerance, no
 * signature matches, or the envelope's id is not the `webhook-id`. Dedupe
 * on the returned `id`: deliveries are at least once.
 */
export async function verifyWebhook(
  secrets: string | string[],
  headers: WebhookHeaders,
  body: string | Uint8Array,
  opts: VerifyOptions = {},
): Promise<CloudEvent> {
  const id = header(headers, "webhook-id");
  const ts = header(headers, "webhook-timestamp");
  const sig = header(headers, "webhook-signature");
  if (!id || !ts || !sig) throw new WebhookVerificationError("missing webhook-id, webhook-timestamp or webhook-signature");
  const t = Number(ts);
  if (!/^-?\d+$/.test(ts) || !Number.isSafeInteger(t)) throw new WebhookVerificationError("webhook-timestamp is not a number");
  const now = opts.now ?? Math.floor(Date.now() / 1000);
  if (Math.abs(now - t) > (opts.toleranceSecs ?? WEBHOOK_TOLERANCE_SECS)) {
    throw new WebhookVerificationError("webhook-timestamp is outside the tolerance");
  }
  const given = sig.split(" ").filter((s) => s.startsWith("v1,")).map((s) => s.slice(3));
  let ok = false;
  for (const secret of Array.isArray(secrets) ? secrets : [secrets]) {
    const expected = (await signWebhook(secret, id, t, body)).slice(3);
    for (const g of given) ok = sameText(expected, g) || ok;
  }
  if (!ok) throw new WebhookVerificationError("no webhook-signature matches");
  let event: CloudEvent;
  try {
    event = JSON.parse(typeof body === "string" ? body : new TextDecoder().decode(body)) as CloudEvent;
  } catch {
    throw new WebhookVerificationError("the body is not JSON");
  }
  if (event && typeof event === "object" && "id" in event && event.id !== id) {
    throw new WebhookVerificationError("the event id is not the webhook-id");
  }
  return event;
}
