// Shapes of the onto-intelli HTTP API. Field names are those the server
// sends (the Rust serde shapes), so they are snake_case; request option
// objects of the SDK are camelCase and converted.

/** Any JSON value. */
export type Json = null | boolean | number | string | Json[] | { [key: string]: Json };

export type JsonObject = { [key: string]: Json };

// ── asking ───────────────────────────────────────────────────────────────

/** `auto`: deterministic when possible, the LLM otherwise. */
export type AskMode = "auto" | "deterministic" | "llm";

/** How an answer was produced. */
export type AnswerMode =
  | "deterministic"
  | "llm"
  | "llm_cache"
  | "answer_cache"
  | "fallback"
  | "clarify"
  | "proposal"
  | "action";

/** `AskRequest.trace`: what the trace of a question records. */
export interface AskTrace {
  session_id?: string;
  user_id?: string;
  tags?: string[];
  metadata?: JsonObject;
  name?: string;
}

/** The body of `POST /v1/ask` and `POST /v1/ask/stream`. */
export interface AskRequest {
  question: string;
  workspace?: string;
  /** Continue a conversation; `"new"` starts one. */
  conversation_id?: string;
  mode?: AskMode;
  token_budget?: number;
  include_context?: boolean;
  no_cache?: boolean;
  /** Apply commands instead of only proposing them. */
  confirm?: boolean;
  trace?: AskTrace;
}

/** An entity a question mentions. */
export interface Resolved {
  id: string;
  kind: string;
  title: string;
  mention: string;
  confidence: number;
  position: number;
}

export interface TokenUsage {
  prompt_tokens: number;
  completion_tokens: number;
  cached_tokens: number;
  cost_usd: number;
  estimated: boolean;
}

export interface Efficiency {
  context_tokens: number;
  naive_tokens: number;
  saved_tokens: number;
  facts_used: number;
  facts_dropped: number;
}

export interface Timings {
  resolve_ms: number;
  retrieve_ms: number;
  llm_ms: number;
  total_ms: number;
}

/** The answer of `POST /v1/ask` (and the `data` of a `done` stream event). */
export interface AskResponse {
  answer: string;
  mode: AnswerMode;
  /** `owner`, `dependencies`, `impact`, `reasoning`, … */
  intent: string;
  why: boolean;
  entities: Resolved[];
  facts: string[];
  conversation_id?: string;
  usage: TokenUsage;
  efficiency: Efficiency;
  timings: Timings;
  provider: string;
  model: string;
  request_id: string;
  revision: number;
  memories?: string[];
  context?: JsonObject;
  note?: string;
  /** Candidates when `mode` is `clarify`. */
  options?: Resolved[];
  /** The receipt or plan of a command. */
  action?: JsonObject;
  /** The trace that recorded this answer. */
  trace_id?: string;
}

/** A server-sent event of `POST /v1/ask/stream`. */
export interface AskEvent {
  type: "stage" | "entities" | "delta" | "done" | "error" | (string & {});
  /** `delta`: answer text; `stage`: the stage name; `error`: the message. */
  text: string;
  /** `done`: the AskResponse; `entities`: the resolved entities; `error`: `{code}`. */
  data?: Json;
}

// ── the graph ────────────────────────────────────────────────────────────

export interface Provenance {
  source: string;
  derived_from?: string[];
  generated_by?: string;
  run?: string;
  external_ref?: string;
  at: number;
  connector?: string;
}

export interface Entity {
  id: string;
  kind: string;
  workspace: string;
  title: string;
  state?: string;
  props?: JsonObject;
  aliases?: string[];
  provenance?: Provenance;
  created_at: number;
  updated_at: number;
  revision: number;
  /** `entities.get(id, { derived: true })`. */
  derived?: JsonObject;
}

/** Create or update an entity (`entity.upsert`). */
export interface EntityInput {
  id?: string;
  kind?: string;
  key?: string;
  workspace?: string;
  title?: string;
  state?: string;
  clear_state?: boolean;
  props?: JsonObject;
  aliases?: string[];
  provenance?: Partial<Provenance>;
  /** Replace props instead of merging them. */
  replace?: boolean;
  fill_only?: boolean;
  links?: { rel: string; to: string; props?: JsonObject }[];
}

/** Change an existing entity (`entity.patch`). */
export interface EntityPatch {
  id: string;
  title?: string;
  state?: string;
  props?: JsonObject;
  unset?: string[];
}

/** `entity.find`. */
export interface FindSpec {
  kind?: string;
  workspace?: string;
  state?: string | string[];
  open?: boolean;
  props?: JsonObject;
  related?: { rel: string; to: string };
  within?: string;
  limit?: number;
  offset?: number;
}

export interface FindResult {
  entities: Entity[];
  total: number;
}

export interface SearchHit {
  entity: Entity;
  score: number;
  signals?: JsonObject;
}

export interface SearchResult {
  hits: SearchHit[];
}

export interface HistoryEntry {
  revision: number;
  ts: number;
  at: string;
  actor: string;
  request_id?: string;
  deleted: boolean;
  state?: string;
  title?: string;
  changed: string[];
}

export interface History {
  id: string;
  history: HistoryEntry[];
}

export interface Link {
  from: string;
  rel: string;
  to: string;
  props?: JsonObject;
}

export interface LinkCreated {
  from: string;
  rel: string;
  to: string;
  revision: number;
  changed: boolean;
}

export interface MutationResult {
  applied: number;
  entities: string[];
  links: number;
  events: string[];
  revision: number;
}

// ── governed actions ─────────────────────────────────────────────────────

export interface ActionPlan {
  action: string;
  params: JsonObject;
  valid: boolean;
  checks: JsonObject[];
  preview: string[];
  mutations: JsonObject[];
  inverse: JsonObject[][];
  risk: "low" | "medium" | "high" | (string & {});
  reversible: boolean;
  needs_confirmation: boolean;
  warnings: string[];
}

export interface ActionReceipt {
  id: string;
  action: string;
  /** `applied` | `proposed` | `rejected` | `undone` */
  status: string;
  actor: string;
  workspace: string;
  request_id: string;
  ts: number;
  plan: ActionPlan;
  result?: MutationResult;
  reason?: string;
  touched?: { [id: string]: number | null };
}

// ── objects ──────────────────────────────────────────────────────────────

/** `objects.explore`. */
export interface ExploreQuery {
  kind: string;
  workspace?: string;
  search?: string;
  filters?: { prop: string; op: string; value?: Json }[];
  sort?: string;
  limit?: number;
  offset?: number;
  facets?: string[];
  stats?: string[];
}

export interface ExploreResult {
  kind: string;
  label: string;
  total: number;
  objects: Entity[];
  columns: JsonObject[];
  facets: { [prop: string]: JsonObject[] };
  stats: { [prop: string]: JsonObject };
  links: JsonObject[];
  actions: string[];
}

// ── traces ───────────────────────────────────────────────────────────────

/** One trace. Lists leave out `input` and `output` and add `preview`. */
export interface TraceRow {
  id: string;
  ts: number;
  duration_ms: number;
  name: string;
  /** `ask` | `action` | `call` | `eval` */
  kind: string;
  /** `ok` | `error` */
  status: string;
  error_code: string | null;
  error_message: string | null;
  workspace: string;
  principal: string;
  via: string | null;
  user_id: string | null;
  session_id: string | null;
  /** The API key's prefix (never the key). */
  api_key: string | null;
  source: string;
  request_id: string | null;
  intent: string | null;
  mode: string | null;
  deterministic: boolean;
  provider: string | null;
  model: string | null;
  llm_calls: number;
  cache_hits: number;
  input_tokens: number;
  output_tokens: number;
  cached_tokens: number;
  context_tokens: number;
  saved_tokens: number;
  cost_usd: number;
  input: Json | null;
  output: Json | null;
  metadata: Json;
  tags: string[];
  version: string;
  preview?: string;
}

export interface SpanRow {
  trace_id: string;
  id: string;
  parent_id: string | null;
  name: string;
  /** `span` | `generation` | `retrieval` | `tool` | `event` */
  kind: string;
  ts: number;
  duration_ms: number;
  status: string;
  error: string | null;
  model: string | null;
  input_tokens: number;
  output_tokens: number;
  cached_tokens: number;
  cost_usd: number;
  attrs: Json;
  input: Json | null;
  output: Json | null;
}

export interface ScoreRow {
  id: string;
  trace_id: string;
  span_id: string | null;
  ts: number;
  name: string;
  value: number | null;
  label: string | null;
  /** `eval` | `api` | `console` */
  source: string;
  author: string | null;
  comment: string | null;
}

export interface TraceDetail {
  trace: TraceRow;
  spans: SpanRow[];
  scores: ScoreRow[];
}

export interface TracePage {
  traces: TraceRow[];
  /** The cursor of the next page, if any. */
  next: string | null;
}

/** Filters of `traces.list` (camelCase here; sent as snake_case). */
export interface TraceFilter {
  since?: number | Date;
  until?: number | Date;
  name?: string;
  kind?: string;
  status?: "ok" | "error" | (string & {});
  userId?: string;
  sessionId?: string;
  model?: string;
  workspace?: string;
  mode?: string;
  tag?: string;
  minMs?: number;
  /** Text search in questions and answers. */
  q?: string;
  limit?: number;
  cursor?: string;
}

export interface MetricsQuery {
  since?: number | Date;
  until?: number | Date;
  /** `hour` | `day` */
  bucket?: "hour" | "day" | (string & {});
  name?: string;
  workspace?: string;
}

export interface SeriesPoint {
  bucket: string;
  requests: number;
  errors: number;
  p50_ms: number;
  p95_ms: number;
  input_tokens: number;
  output_tokens: number;
  cost_usd: number;
}

export interface TraceMetrics {
  requests: number;
  errors: number;
  error_rate: number;
  p50_ms: number;
  p95_ms: number;
  p99_ms: number;
  input_tokens: number;
  output_tokens: number;
  cached_tokens: number;
  cost_usd: number;
  deterministic_share: number;
  cache_hit_rate: number;
  llm_calls: number;
  series: SeriesPoint[];
  by_model: { model: string; calls: number; input_tokens: number; output_tokens: number; cost_usd: number; p95_ms: number }[];
  by_name: { name: string; requests: number; errors: number; p95_ms: number }[];
  scores: { name: string; count: number; avg: number | null }[];
}

export interface ScoreInput {
  name: string;
  /** Numeric and boolean (0 or 1) scores. */
  value?: number;
  /** Categorical scores. */
  label?: string;
  comment?: string;
  spanId?: string;
}

export interface SessionFilter {
  since?: number | Date;
  until?: number | Date;
  userId?: string;
  limit?: number;
  cursor?: string;
}

export interface SessionRow {
  session_id: string;
  user_id: string | null;
  first_ts: number;
  last_ts: number;
  traces: number;
  errors: number;
  input_tokens: number;
  output_tokens: number;
  cost_usd: number;
  preview: string | null;
}

export interface SessionPage {
  sessions: SessionRow[];
  next: string | null;
}

export interface Session {
  session_id: string;
  /** Oldest first, with inputs and outputs. */
  traces: TraceRow[];
}

// ── sessions for browsers ────────────────────────────────────────────────

export interface MintSessionOptions {
  /** The end user the session acts for (`user:42`). */
  actAs?: string;
  roles?: string[];
  workspaces?: string[];
  /** The entity that stands for the user in the graph. */
  entity?: string;
  ttlSecs?: number;
  tenant?: string;
}

export interface MintedSession {
  token: string;
  expires_at: number;
  principal: JsonObject;
  tenant: string | null;
}

// ── change feed ──────────────────────────────────────────────────────────

export type ChangeOp =
  | "entity.create"
  | "entity.update"
  | "entity.delete"
  | "entity.hidden"
  | "link.create"
  | "link.update"
  | "link.delete"
  | "schema.change"
  | "gap";

/** Which records a stream or a poll wants; dimensions combine with AND. `gap` records always pass. */
export interface ChangeFilter {
  /** Entity kinds or interfaces (a link matches through either end). */
  kinds?: string[];
  ops?: ChangeOp[];
  workspaces?: string[];
  /** Entity ids (at most 1000; a link matches through either end). */
  ids?: string[];
  /** Relations (canonical, inverse or `^rel`): link records only. */
  rels?: string[];
  /** Updates that changed any of these fields. */
  props?: string[];
  /** Globs on the writer's source; `!x` excludes. */
  sources?: string[];
  /** `ids`: ids, kinds, ops and changed field names only. */
  fields?: "full" | "ids";
  /** Records written by schema migrations (default true). */
  includeMigrations?: boolean;
}

/** One change as this reader gets it. `cursor` is `"<seq>.<n>"`. */
export interface Change {
  cursor: string;
  ts: number;
  op: ChangeOp;
  workspace: string;
  kind: string;
  id: string;
  revision?: number;
  changed?: string[];
  state?: { from?: string | null; to?: string | null };
  moved?: JsonObject;
  /** Old values of the changed fields (only when you could see the entity before and after). */
  before?: JsonObject;
  /** The entity as the change left it (create, update). */
  entity?: Entity;
  entity_missing?: boolean;
  /** You could not see the entity before this change: treat it as an upsert. */
  became_visible?: boolean;
  link?: JsonObject;
  schema?: JsonObject;
  /** A commit written without records: re-read with `entities.find` (not pinned to a cursor). */
  gap?: { reason: string; origin?: string; entities: string[]; links: string[][]; hidden?: number };
  cause: JsonObject;
}

export interface ChangePage {
  changes: Change[];
  /** Resume after this cursor (the last one scanned). */
  next: string;
  head: number;
  floor: number;
  /** The cursor is older than the floor: records may be missing. */
  expired: boolean;
  message?: string;
}

export interface ChangeHead {
  cursor: string;
  head: number;
  floor: number;
  capture: boolean;
}

/** A stream's `reset`: the cursor expired. */
export interface ChangeReset {
  code: string;
  floor: number;
  head: number;
  message: string;
}
