// onto-intelli for TypeScript and JavaScript: a client of the HTTP API
// (docs/CONTROL_PLANE.md section 12). Zero dependencies: `fetch` only
// (Node 18+, Deno, Bun; in browsers only with a short-lived session minted
// by your backend, never with an API key).

import type {
  ActionPlan,
  ActionReceipt,
  AskEvent,
  AskMode,
  AskRequest,
  AskResponse,
  Entity,
  EntityInput,
  EntityPatch,
  ExploreQuery,
  ExploreResult,
  FindResult,
  FindSpec,
  History,
  Json,
  JsonObject,
  Link,
  LinkCreated,
  MetricsQuery,
  MintSessionOptions,
  MintedSession,
  ScoreInput,
  ScoreRow,
  SearchResult,
  Session,
  SessionFilter,
  SessionPage,
  TraceDetail,
  TraceFilter,
  TraceMetrics,
  TracePage,
} from "./types.ts";
import { parseSse } from "./sse.ts";
import { Changes } from "./changes.ts";

export type * from "./types.ts";
export { parseSse } from "./sse.ts";
export type { SseMessage } from "./sse.ts";
export { Changes } from "./changes.ts";
export type { ChangesTransport, PollOptions, SubscribeOptions } from "./changes.ts";
export { WEBHOOK_TOLERANCE_SECS, WebhookVerificationError, signWebhook, signingKey, verifyWebhook } from "./webhooks.ts";
export type { CloudEvent, VerifyOptions, WebhookHeaders } from "./webhooks.ts";

export const VERSION = "0.2.0";

/** Options of {@link Onto}. Each falls back to an environment variable. */
export interface OntoOptions {
  /** Base URL of the server (`ONTO_URL`), e.g. `https://onto.example.com`. */
  url?: string;
  /** An `onto_sk_…` API key or an `onto1.…` session (`ONTO_API_KEY`). */
  apiKey?: string;
  /** Project ref (`ONTO_TENANT`); optional with an API key, which implies its project. */
  tenant?: string;
  /** Default workspace (`x-onto-workspace`). */
  workspace?: string;
  /** Sent as `x-onto-session-id` on every request (traces group by it). */
  sessionId?: string;
  /** A `fetch` implementation (default: the global one). */
  fetch?: typeof fetch;
  /** Extra headers on every request. */
  headers?: Record<string, string>;
  /** Time to the response headers, per attempt (default 30 000 ms). */
  timeoutMs?: number;
  /** Retries of 429 and 503 responses (default 2). */
  maxRetries?: number;
  /** Longest wait between retries, whatever `retry-after` says (default 30 000 ms). */
  maxRetryDelayMs?: number;
}

/** Per-request options. */
export interface RequestOptions {
  signal?: AbortSignal;
  /** Overrides the client's `x-onto-session-id`. */
  sessionId?: string;
  /** Overrides the client's workspace. */
  workspace?: string;
  headers?: Record<string, string>;
}

/** Options of {@link Onto.ask} (camelCase; sent as the snake_case `AskRequest`). */
export interface AskOptions extends RequestOptions {
  /** Continue a conversation; `"new"` starts one. */
  conversationId?: string;
  mode?: AskMode;
  tokenBudget?: number;
  includeContext?: boolean;
  noCache?: boolean;
  /** Apply commands instead of only proposing them. */
  confirm?: boolean;
  /** Recorded on the trace. */
  userId?: string;
  tags?: string[];
  metadata?: JsonObject;
  /** The trace's name (default `intelli.ask`). */
  name?: string;
}

export interface AskStreamRequest extends AskOptions {
  question: string;
}

/**
 * An error answer of the server, or a request that got none (`status` 0:
 * network failure, timeout, abort, bad options).
 */
export class OntoError extends Error {
  /** HTTP status, or 0 without a response. */
  readonly status: number;
  /** `invalid_argument`, `unauthenticated`, `permission_denied`, `not_found`, `resource_exhausted`, … */
  readonly code: string;
  /** The `x-onto-trace-id` of the failed call, when it was traced. */
  readonly traceId: string | undefined;
  /** Seconds the server asked to wait (`retry-after`), if any. */
  readonly retryAfter: number | undefined;

  constructor(status: number, code: string, message: string, traceId?: string, retryAfter?: number) {
    super(message);
    this.name = "OntoError";
    this.status = status;
    this.code = code;
    this.traceId = traceId;
    this.retryAfter = retryAfter;
  }

  /** The same as `traceId`, under the server's field name. */
  get trace_id(): string | undefined {
    return this.traceId;
  }
}

const CODES: Record<number, string> = {
  400: "invalid_argument",
  401: "unauthenticated",
  403: "permission_denied",
  404: "not_found",
  409: "conflict",
  413: "invalid_argument",
  429: "resource_exhausted",
  503: "unavailable",
};

function env(name: string): string | undefined {
  const p = (globalThis as { process?: { env?: Record<string, string | undefined> } }).process;
  const v = p?.env?.[name];
  return v && v.trim() !== "" ? v.trim() : undefined;
}

function snake(key: string): string {
  return key.replace(/[A-Z]/g, (c) => `_${c.toLowerCase()}`);
}

/** A camelCase filter as a query string: snake_case keys, dates as Unix ms. */
function queryString(filter: object | undefined): string {
  if (!filter) return "";
  const params = new URLSearchParams();
  for (const [k, v] of Object.entries(filter)) {
    if (v === undefined || v === null) continue;
    params.set(snake(k), v instanceof Date ? String(v.getTime()) : String(v));
  }
  const s = params.toString();
  return s ? `?${s}` : "";
}

function retryAfterSecs(h: string | null): number | undefined {
  if (h === null) return undefined;
  const n = Number(h);
  if (h.trim() !== "" && Number.isFinite(n)) return Math.max(0, n);
  const at = Date.parse(h);
  return Number.isNaN(at) ? undefined : Math.max(0, (at - Date.now()) / 1000);
}

function sleep(ms: number, signal?: AbortSignal): Promise<void> {
  return new Promise((resolve, reject) => {
    if (signal?.aborted) return reject(abortError(signal));
    const t = setTimeout(() => {
      signal?.removeEventListener("abort", onAbort);
      resolve();
    }, ms);
    const onAbort = () => {
      clearTimeout(t);
      reject(abortError(signal));
    };
    signal?.addEventListener("abort", onAbort, { once: true });
  });
}

function abortError(signal?: AbortSignal): OntoError {
  const reason = signal?.reason instanceof Error ? signal.reason.message : "the request was aborted";
  return new OntoError(0, "cancelled", reason);
}

async function errorOf(res: Response): Promise<OntoError> {
  const traceId = res.headers.get("x-onto-trace-id") ?? undefined;
  const retryAfter = retryAfterSecs(res.headers.get("retry-after"));
  const text = await res.text().catch(() => "");
  let code = CODES[res.status] ?? (res.status >= 500 ? "internal" : "unknown");
  let message = text.slice(0, 500) || res.statusText || `HTTP ${res.status}`;
  try {
    const body = JSON.parse(text) as { error?: { code?: unknown; message?: unknown } };
    if (body && typeof body.error === "object" && body.error !== null) {
      if (typeof body.error.code === "string") code = body.error.code;
      if (typeof body.error.message === "string") message = body.error.message;
    }
  } catch {
    // Not JSON: keep the status's code and the text.
  }
  return new OntoError(res.status, code, message, traceId, retryAfter);
}

interface Send {
  method: string;
  path: string;
  body?: unknown;
  query?: object;
  stream?: boolean;
  opts?: RequestOptions;
  extraHeaders?: Record<string, string>;
}

/** A client of one onto-intelli server and project. */
export class Onto {
  readonly url: string;
  readonly tenant: string | undefined;
  readonly workspace: string | undefined;
  readonly sessionId: string | undefined;
  readonly timeoutMs: number;
  readonly maxRetries: number;
  readonly maxRetryDelayMs: number;
  // Private fields: the key never shows in logs, `console.log(onto)` or JSON.
  #apiKey: string | undefined;
  #fetch: typeof fetch;
  #headers: Record<string, string>;

  constructor(options: OntoOptions = {}) {
    const url = options.url ?? env("ONTO_URL");
    if (!url) throw new OntoError(0, "invalid_argument", "onto-intelli: no server URL (pass url or set ONTO_URL)");
    this.url = url.replace(/\/+$/, "");
    this.#apiKey = options.apiKey ?? env("ONTO_API_KEY");
    this.tenant = options.tenant ?? env("ONTO_TENANT");
    this.workspace = options.workspace;
    this.sessionId = options.sessionId;
    this.timeoutMs = options.timeoutMs ?? 30_000;
    this.maxRetries = Math.max(0, options.maxRetries ?? 2);
    this.maxRetryDelayMs = options.maxRetryDelayMs ?? 30_000;
    const f = options.fetch ?? globalThis.fetch;
    if (typeof f !== "function") throw new OntoError(0, "invalid_argument", "onto-intelli: no fetch (Node 18+ or pass fetch)");
    this.#fetch = f;
    this.#headers = { ...(options.headers ?? {}) };
    this.changes = new Changes({
      json: <T>(method: string, path: string, body?: unknown, signal?: AbortSignal) =>
        this.#json<T>({ method, path, body, opts: { signal } }),
      stream: async (body, lastEventId, signal) => {
        const { res, release } = await this.#send({
          method: "POST",
          path: "/v1/changes/stream",
          body,
          stream: true,
          opts: { signal },
          extraHeaders: lastEventId === undefined ? undefined : { "last-event-id": lastEventId },
        });
        if (!res.body) {
          release();
          throw new OntoError(res.status, "internal", "the stream has no body");
        }
        return { body: res.body, release };
      },
      error: (status, code, message) => new OntoError(status, code, message),
      status: (e) => (e instanceof OntoError && e.status > 0 ? e.status : undefined),
    });
  }

  /**
   * The change feed: `subscribe` (a live stream that reconnects after the
   * last cursor), `poll` and `head`.
   */
  readonly changes: Changes;

  /** What `console.log` shows: never the key. */
  toJSON(): object {
    return { url: this.url, tenant: this.tenant, workspace: this.workspace, apiKey: this.#apiKey ? "(set)" : undefined };
  }

  #headersFor(s: Send): Record<string, string> {
    const h: Record<string, string> = { accept: s.stream ? "text/event-stream" : "application/json", ...this.#headers };
    if (this.#apiKey) h.authorization = `Bearer ${this.#apiKey}`;
    if (this.tenant) h["x-onto-tenant"] = this.tenant;
    const ws = s.opts?.workspace ?? this.workspace;
    if (ws) h["x-onto-workspace"] = ws;
    const session = s.opts?.sessionId ?? this.sessionId;
    if (session) h["x-onto-session-id"] = session;
    if (s.body !== undefined) h["content-type"] = "application/json";
    return { ...h, ...(s.extraHeaders ?? {}), ...(s.opts?.headers ?? {}) };
  }

  /**
   * One request with the retries of 429 and 503. Returns the successful
   * response and `release`: the caller's signal keeps aborting the request
   * while the body is read, until `release` is called (once the body is
   * read or cancelled). The timeout covers the response headers only.
   */
  async #send(s: Send): Promise<{ res: Response; release: () => void }> {
    const url = `${this.url}${s.path}${queryString(s.query)}`;
    const body = s.body === undefined ? undefined : JSON.stringify(s.body);
    const headers = this.#headersFor(s);
    const signal = s.opts?.signal;
    for (let attempt = 0; ; attempt++) {
      if (signal?.aborted) throw abortError(signal);
      const ctl = new AbortController();
      let timedOut = false;
      const timer = setTimeout(() => {
        timedOut = true;
        ctl.abort();
      }, this.timeoutMs);
      const onAbort = () => ctl.abort(signal?.reason);
      signal?.addEventListener("abort", onAbort, { once: true });
      const release = () => signal?.removeEventListener("abort", onAbort);
      let res: Response;
      try {
        res = await this.#fetch(url, { method: s.method, headers, body, signal: ctl.signal });
      } catch (e) {
        release();
        if (timedOut) throw new OntoError(0, "deadline_exceeded", `no response within ${this.timeoutMs} ms`);
        if (signal?.aborted) throw abortError(signal);
        throw new OntoError(0, "unavailable", `cannot reach ${this.url}: ${e instanceof Error ? e.message : String(e)}`);
      } finally {
        clearTimeout(timer);
      }
      if ((res.status === 429 || res.status === 503) && attempt < this.maxRetries) {
        release();
        const after = retryAfterSecs(res.headers.get("retry-after"));
        await res.body?.cancel().catch(() => undefined);
        const wait = after !== undefined ? after * 1000 : 500 * 2 ** attempt;
        await sleep(Math.min(wait, this.maxRetryDelayMs), signal);
        continue;
      }
      if (!res.ok) {
        try {
          throw await errorOf(res);
        } finally {
          release();
        }
      }
      return { res, release };
    }
  }

  async #json<T>(s: Send): Promise<T> {
    const { res, release } = await this.#send(s);
    let text: string;
    try {
      text = await res.text();
    } catch (e) {
      const signal = s.opts?.signal;
      if (signal?.aborted) throw abortError(signal);
      throw new OntoError(res.status, "unavailable", `reading the answer: ${e instanceof Error ? e.message : String(e)}`);
    } finally {
      release();
    }
    try {
      return (text ? JSON.parse(text) : {}) as T;
    } catch {
      throw new OntoError(res.status, "internal", `the server's answer is not JSON: ${text.slice(0, 200)}`);
    }
  }

  // ── asking ──────────────────────────────────────────────────────────────

  /** Answer a question: deterministic from the graph when it can, the LLM when reasoning is needed. */
  ask(question: string, options: AskOptions = {}): Promise<AskResponse> {
    return this.#json<AskResponse>({ method: "POST", path: "/v1/ask", body: askBody(question, options), opts: options });
  }

  /** Answer a question as server-sent events: `stage`, `entities`, `delta`…, then `done` (or `error`). */
  async *askStream(request: AskStreamRequest | string): AsyncGenerator<AskEvent> {
    const req: AskStreamRequest = typeof request === "string" ? { question: request } : request;
    const { res, release } = await this.#send({
      method: "POST",
      path: "/v1/ask/stream",
      body: askBody(req.question, req),
      stream: true,
      opts: req,
    });
    // `signal` aborts the stream until it ends, not only until it starts.
    try {
      if (!res.body) throw new OntoError(res.status, "internal", "the stream has no body");
      for await (const msg of parseSse(res.body)) {
        let ev: AskEvent;
        try {
          ev = JSON.parse(msg.data) as AskEvent;
        } catch {
          ev = { type: msg.event, text: msg.data };
        }
        if (!ev.type) ev.type = msg.event;
        yield ev;
        if (ev.type === "done" || ev.type === "error") return;
      }
    } catch (e) {
      if (req.signal?.aborted) throw abortError(req.signal);
      throw e;
    } finally {
      release();
    }
  }

  // ── any action ──────────────────────────────────────────────────────────

  /** Run any action (`GET /v1/actions` lists them): `POST /v1/call/{action}`. */
  call<T = JsonObject>(action: string, input: object = {}, options: RequestOptions = {}): Promise<T> {
    return this.#json<T>({ method: "POST", path: `/v1/call/${encodeURIComponent(action)}`, body: input, opts: options });
  }

  /** Full-text and vector search over entities (`entity.search`). */
  search(
    query: string,
    options: RequestOptions & { kinds?: string[]; limit?: number; mode?: "hybrid" | "lexical" | "vector" } = {},
  ): Promise<SearchResult> {
    const { kinds, limit, mode, workspace } = options;
    return this.call<SearchResult>("entity.search", { query, kinds, limit, mode, workspace }, options);
  }

  readonly entities = {
    get: (id: string, options: RequestOptions & { derived?: boolean } = {}): Promise<Entity> =>
      this.call<Entity>("entity.get", { id, derived: options.derived }, options),
    find: (spec: FindSpec = {}, options: RequestOptions = {}): Promise<FindResult> =>
      this.call<FindResult>("entity.find", spec, options),
    upsert: (input: EntityInput, options: RequestOptions = {}): Promise<Entity> =>
      this.call<Entity>("entity.upsert", input, options),
    patch: (patch: EntityPatch, options: RequestOptions = {}): Promise<Entity> =>
      this.call<Entity>("entity.patch", patch, options),
    delete: (id: string, options: RequestOptions = {}): Promise<{ id: string; deleted: boolean; revision: number }> =>
      this.call("entity.delete", { id }, options),
    history: (id: string, options: RequestOptions & { limit?: number } = {}): Promise<History> =>
      this.call<History>("entity.history", { id, limit: options.limit }, options),
  };

  readonly links = {
    create: (link: Link, options: RequestOptions = {}): Promise<LinkCreated> =>
      this.call<LinkCreated>("link.create", link, options),
    delete: (link: Omit<Link, "props">, options: RequestOptions = {}): Promise<{ deleted: boolean; revision: number }> =>
      this.call("link.delete", link, options),
  };

  readonly actions = {
    /** Action types (governed edits) of the schema. */
    list: (options: RequestOptions = {}): Promise<{ action_types: JsonObject[] }> =>
      this.call("action.list", {}, options),
    describe: (name: string, options: RequestOptions = {}): Promise<JsonObject> =>
      this.call("action.describe", { name }, options),
    /** Validate an action and preview its edits, without applying it. */
    plan: (action: string, params: JsonObject = {}, options: RequestOptions = {}): Promise<ActionPlan> =>
      this.call<ActionPlan>("action.plan", { action, params, workspace: options.workspace }, options),
    /** Apply an action; high-risk and irreversible ones need `confirm: true`. */
    apply: (
      action: string,
      params: JsonObject = {},
      options: RequestOptions & { confirm?: boolean; dryRun?: boolean } = {},
    ): Promise<ActionReceipt> =>
      this.call<ActionReceipt>(
        "action.apply",
        { action, params, confirm: options.confirm ?? false, dry_run: options.dryRun ?? false, workspace: options.workspace },
        options,
      ),
    /** Undo an applied action from its receipt id. */
    undo: (receipt: string, options: RequestOptions & { force?: boolean } = {}): Promise<ActionReceipt> =>
      this.call<ActionReceipt>("action.undo", { receipt, force: options.force ?? false, workspace: options.workspace }, options),
  };

  readonly objects = {
    /** Object types with counts, backing tables, properties, links and actions. */
    types: (options: RequestOptions = {}): Promise<{ types: JsonObject[] }> =>
      this.#json({ method: "GET", path: "/v1/objects/types", query: { workspace: options.workspace }, opts: options }),
    /** Objects of one type, filtered, searched, sorted and paged, with facets and statistics. */
    explore: (query: ExploreQuery, options: RequestOptions = {}): Promise<ExploreResult> =>
      this.#json<ExploreResult>({ method: "POST", path: "/v1/objects/explore", body: query, opts: options }),
  };

  /** Traces need a credential that may observe (an admin-scope key, a console session). */
  readonly traces = {
    list: (filter: TraceFilter = {}, options: RequestOptions = {}): Promise<TracePage> =>
      this.#json<TracePage>({ method: "GET", path: "/v1/traces", query: filter, opts: options }),
    get: (id: string, options: RequestOptions = {}): Promise<TraceDetail> =>
      this.#json<TraceDetail>({ method: "GET", path: `/v1/traces/${encodeURIComponent(id)}`, opts: options }),
    metrics: (query: MetricsQuery = {}, options: RequestOptions = {}): Promise<TraceMetrics> =>
      this.#json<TraceMetrics>({ method: "GET", path: "/v1/metrics", query, opts: options }),
    /** Score a trace (user feedback, an evaluation). */
    score: (traceId: string, score: ScoreInput, options: RequestOptions = {}): Promise<ScoreRow> => {
      const { spanId, ...rest } = score;
      const body: Record<string, Json | undefined> = { ...rest, span_id: spanId };
      return this.#json<ScoreRow>({
        method: "POST",
        path: `/v1/traces/${encodeURIComponent(traceId)}/scores`,
        body,
        opts: options,
      });
    },
  };

  readonly sessions = {
    list: (filter: SessionFilter = {}, options: RequestOptions = {}): Promise<SessionPage> =>
      this.#json<SessionPage>({ method: "GET", path: "/v1/sessions", query: filter, opts: options }),
    get: (id: string, options: RequestOptions & { limit?: number } = {}): Promise<Session> =>
      this.#json<Session>({
        method: "GET",
        path: `/v1/sessions/${encodeURIComponent(id)}`,
        query: { limit: options.limit },
        opts: options,
      }),
  };

  readonly auth = {
    /**
     * Mint a short-lived `onto1.` session for a browser, acting for one of
     * your users (backend → browser). With an API key the session is bound
     * to the key and stops working when the key is revoked.
     */
    mintSession: (session: MintSessionOptions = {}, options: RequestOptions = {}): Promise<MintedSession> =>
      this.#json<MintedSession>({
        method: "POST",
        path: "/v1/auth/session",
        body: {
          act_as: session.actAs,
          roles: session.roles,
          workspaces: session.workspaces,
          entity: session.entity,
          ttl_secs: session.ttlSecs,
          tenant: session.tenant,
        },
        opts: options,
      }),
    /** The caller's principal, project and capabilities. */
    whoami: (options: RequestOptions = {}): Promise<JsonObject> =>
      this.#json({ method: "GET", path: "/v1/auth/whoami", opts: options }),
  };
}

function askBody(question: string, o: AskOptions): AskRequest {
  const trace =
    o.sessionId !== undefined || o.userId !== undefined || o.tags !== undefined || o.metadata !== undefined || o.name !== undefined
      ? { session_id: o.sessionId, user_id: o.userId, tags: o.tags, metadata: o.metadata, name: o.name }
      : undefined;
  return {
    question,
    workspace: o.workspace,
    conversation_id: o.conversationId,
    mode: o.mode,
    token_budget: o.tokenBudget,
    include_context: o.includeContext,
    no_cache: o.noCache,
    confirm: o.confirm,
    trace,
  };
}

export default Onto;
