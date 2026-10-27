// The change feed: `subscribe` follows the SSE stream
// (`POST /v1/changes/stream`) and reconnects after the last cursor it
// received; `poll` and `head` are the unary calls. Delivery is at least once
// across reconnects: a record may arrive again after a reconnect, never be
// skipped silently (an expired cursor is a `reset`, a commit written without
// records arrives as a `gap` change).

import type { Change, ChangeFilter, ChangeHead, ChangePage, ChangeReset, JsonObject } from "./types.ts";
import { parseSse } from "./sse.ts";

/** What the client gives the change feed: unary JSON calls and one stream request. */
export interface ChangesTransport {
  json<T>(method: string, path: string, body?: unknown, signal?: AbortSignal): Promise<T>;
  /** Open `POST /v1/changes/stream` with `body` and `last-event-id`. */
  stream(
    body: JsonObject,
    lastEventId: string | undefined,
    signal: AbortSignal | undefined,
  ): Promise<{ body: ReadableStream<Uint8Array>; release: () => void }>;
  /** An error the client reports (status 0 without a response). */
  error(status: number, code: string, message: string): Error;
  /** Whether `e` is an error answer of the server with this HTTP status. */
  status(e: unknown): number | undefined;
}

export interface SubscribeOptions {
  /** Resume after this cursor (a previous change's `cursor`). Wins over `from`. */
  after?: string;
  /** Without `after`: `now` (default) or `earliest`. */
  from?: "now" | "earliest";
  signal?: AbortSignal;
  /**
   * The cursor expired (retention, or capture was off). Records may be
   * missing: re-read what you mirror with `entities.find` (not pinned to a
   * cursor); the stream then continues from the head. Without it, the
   * generator throws `OntoError("cursor_expired")`.
   */
  onReset?: (reset: ChangeReset) => void | Promise<void>;
  /** A `gap` change arrived (it is also yielded). */
  onGap?: (gap: Change) => void | Promise<void>;
}

export interface PollOptions extends ChangeFilter {
  after?: string;
  from?: "now" | "earliest";
  limit?: number;
  signal?: AbortSignal;
}

/** The first and the longest wait before reconnecting (ms); `retry:` replaces the first. */
const MIN_BACKOFF_MS = 500;
const MAX_BACKOFF_MS = 30_000;

/** End codes after which the stream resumes at once from its cursor. */
const RESUME = new Set(["slow_consumer", "tenant_closed", "principal_changed"]);
/** End codes that are final. */
const FINAL: Record<string, number> = { unauthenticated: 401, permission_denied: 403, invalid_argument: 400 };

function filterBody(f: ChangeFilter): JsonObject {
  const body: JsonObject = {};
  for (const k of ["kinds", "ops", "workspaces", "ids", "rels", "props", "sources", "fields"] as const) {
    const v = f[k];
    if (v !== undefined) body[k] = v as string | string[];
  }
  if (f.includeMigrations !== undefined) body.include_migrations = f.includeMigrations;
  return body;
}

function seq(cursor: string): number {
  return Number(cursor.split(".")[0]);
}

function wait(ms: number, signal?: AbortSignal): Promise<void> {
  return new Promise((resolve) => {
    if (ms <= 0 || signal?.aborted) return resolve();
    const t = setTimeout(done, ms);
    function done() {
      clearTimeout(t);
      signal?.removeEventListener("abort", done);
      resolve();
    }
    signal?.addEventListener("abort", done, { once: true });
  });
}

/** `onto.changes`: the change feed of the client's project. */
export class Changes {
  readonly #t: ChangesTransport;

  constructor(transport: ChangesTransport) {
    this.#t = transport;
  }

  /** One page after a cursor (`changes.poll`); `from` defaults to `earliest`. */
  poll(options: PollOptions = {}): Promise<ChangePage> {
    const { after, from, limit, signal, ...filter } = options;
    const body: JsonObject = filterBody(filter);
    if (after !== undefined) body.after = after;
    if (from !== undefined) body.from = from;
    if (limit !== undefined) body.limit = limit;
    return this.#t.json<ChangePage>("POST", "/v1/call/changes.poll", body, signal);
  }

  /** Where the feed is: `{cursor, head, floor, capture}`. */
  head(options: { signal?: AbortSignal } = {}): Promise<ChangeHead> {
    return this.#t.json<ChangeHead>("GET", "/v1/changes/head", undefined, options.signal);
  }

  /**
   * Follow the change feed: every change this caller may see that `filter`
   * wants, in cursor order. Reconnects with exponential backoff (0.5 s to
   * 30 s, the server's `retry:` as the first wait) and `Last-Event-ID`;
   * `slow_consumer`, `tenant_closed` and `principal_changed` resume at once;
   * `too_far_behind` pages with `poll` up to the head, then streams again.
   * A change may arrive twice across reconnects: dedupe on `cursor`.
   * Ends when `signal` aborts (`OntoError("cancelled")`) or on a final error
   * (a revoked credential: `unauthenticated`).
   */
  async *subscribe(filter: ChangeFilter = {}, options: SubscribeOptions = {}): AsyncGenerator<Change> {
    const t = this.#t;
    const signal = options.signal;
    let cursor = options.after;
    let from = options.from ?? "now";
    let retry: number | undefined;
    let failures = 0;
    for (;;) {
      if (signal?.aborted) throw t.error(0, "cancelled", "the subscription was aborted");
      let end: { code: string; cursor?: string; message?: string } | undefined;
      let reset: ChangeReset | undefined;
      try {
        const body = filterBody(filter);
        if (cursor === undefined) body.from = from;
        const { body: stream, release } = await t.stream(body, cursor, signal);
        try {
          for await (const msg of parseSse(stream)) {
            if (msg.retry !== undefined) retry = msg.retry;
            failures = 0;
            if (msg.event === "change") {
              const c = JSON.parse(msg.data) as Change;
              cursor = msg.id ?? c.cursor;
              if (c.op === "gap") await options.onGap?.(c);
              yield c;
            } else if (msg.event === "heartbeat") {
              const h = JSON.parse(msg.data) as { cursor: string };
              cursor = msg.id ?? h.cursor;
            } else if (msg.event === "reset") {
              reset = JSON.parse(msg.data) as ChangeReset;
              break;
            } else if (msg.event === "end") {
              end = JSON.parse(msg.data) as { code: string; cursor?: string; message?: string };
              break;
            }
          }
        } finally {
          release();
        }
      } catch (e) {
        if (signal?.aborted) throw t.error(0, "cancelled", "the subscription was aborted");
        const status = t.status(e);
        // An answer that retrying cannot change (bad filter, no access).
        if (status !== undefined && status >= 400 && status < 500 && status !== 408 && status !== 429) throw e;
      }
      if (reset) {
        if (!options.onReset) throw t.error(0, "cursor_expired", reset.message);
        await options.onReset(reset);
        cursor = undefined;
        from = "now";
        continue;
      }
      if (end) {
        if (end.code in FINAL) throw t.error(FINAL[end.code], end.code, end.message ?? end.code);
        cursor ??= end.cursor;
        if (RESUME.has(end.code)) continue;
        if (end.code === "too_far_behind" && cursor !== undefined) {
          // Page up to the head, then stream again from there.
          for (;;) {
            const page = await this.poll({ ...filter, after: cursor, signal });
            if (page.expired) {
              reset = { code: "cursor_expired", floor: page.floor, head: page.head, message: page.message ?? "" };
              break;
            }
            for (const c of page.changes) {
              if (c.op === "gap") await options.onGap?.(c);
              yield c;
            }
            const moved = page.next !== cursor;
            cursor = page.next;
            if (!moved || seq(cursor) >= page.head) break;
          }
          if (reset) {
            if (!options.onReset) throw t.error(0, "cursor_expired", reset.message);
            await options.onReset(reset);
            cursor = undefined;
            from = "now";
          }
          continue;
        }
      }
      // A dropped connection or an unavailable server: back off.
      const first = retry ?? MIN_BACKOFF_MS;
      await wait(Math.min(first * 2 ** failures, MAX_BACKOFF_MS), signal);
      failures++;
    }
  }
}
