// Traces, sessions and metrics of the selected project (`/v1/traces`,
// `/v1/sessions`, `/v1/metrics`; docs/CONTROL_PLANE.md section 9). These are
// data-plane routes: the request carries x-onto-tenant like every other.

import { get, post } from "./api";

export interface TraceRow {
  id: string;
  ts: number;
  duration_ms: number;
  name: string;
  kind: string;
  status: "ok" | "error" | string;
  error_code?: string | null;
  error_message?: string | null;
  workspace: string;
  principal: string;
  via?: string | null;
  user_id?: string | null;
  session_id?: string | null;
  api_key?: string | null;
  source: string;
  intent?: string | null;
  mode?: string | null;
  deterministic: boolean;
  provider?: string | null;
  model?: string | null;
  llm_calls: number;
  cache_hits: number;
  input_tokens: number;
  output_tokens: number;
  cached_tokens: number;
  context_tokens: number;
  saved_tokens: number;
  cost_usd: number;
  input?: { question?: string; [k: string]: unknown } | null;
  output?: { answer?: string; [k: string]: unknown } | null;
  metadata: Record<string, unknown>;
  tags: string[];
  version: string;
  preview?: string | null;
}

export interface SpanRow {
  trace_id: string;
  id: string;
  parent_id?: string | null;
  name: string;
  kind: "span" | "generation" | "retrieval" | "tool" | "event" | string;
  ts: number;
  duration_ms: number;
  status: string;
  error?: string | null;
  model?: string | null;
  input_tokens: number;
  output_tokens: number;
  cached_tokens: number;
  cost_usd: number;
  attrs: Record<string, unknown>;
  input?: unknown;
  output?: unknown;
}

export interface ScoreRow {
  id: string;
  trace_id: string;
  span_id?: string | null;
  ts: number;
  name: string;
  value?: number | null;
  label?: string | null;
  source: string;
  author?: string | null;
  comment?: string | null;
}

export interface TraceDetail {
  trace: TraceRow;
  spans: SpanRow[];
  scores: ScoreRow[];
}

export interface TracePage {
  traces: TraceRow[];
  next?: string | null;
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

export interface Metrics {
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

export interface SessionRow {
  session_id: string;
  user_id?: string | null;
  first_ts: number;
  last_ts: number;
  traces: number;
  errors: number;
  input_tokens: number;
  output_tokens: number;
  cost_usd: number;
  preview?: string | null;
}

export type TraceFilter = Partial<Record<"since" | "until" | "name" | "status" | "user_id" | "session_id" | "model" | "mode" | "tag" | "q" | "limit" | "cursor", string | number>>;

export const observe = {
  traces: (f: TraceFilter) => get<TracePage>("/v1/traces", f),
  trace: (id: string) => get<TraceDetail>(`/v1/traces/${encodeURIComponent(id)}`),
  score: (id: string, s: { name: string; value?: number; label?: string; comment?: string; span_id?: string }) => post<ScoreRow>(`/v1/traces/${encodeURIComponent(id)}/scores`, s),
  /** Metrics of the selected project, or of `tenant` (the projects page asks each project). */
  metrics: (q: { since?: number; until?: number; bucket?: "hour" | "day" }, tenant?: string) => get<Metrics>("/v1/metrics", q, tenant),
  sessions: (q: { since?: number; user_id?: string; cursor?: string; limit?: number }) => get<{ sessions: SessionRow[]; next?: string | null }>("/v1/sessions", q),
  session: (id: string) => get<{ session_id: string; traces: TraceRow[] }>(`/v1/sessions/${encodeURIComponent(id)}`, { limit: 200 }),
};

export const HOUR = 3_600_000;
export const DAY = 24 * HOUR;

/** The time ranges of the console's range pickers. */
export const RANGES = [
  { value: "1h", label: "Last hour", ms: HOUR },
  { value: "24h", label: "Last 24 hours", ms: DAY },
  { value: "7d", label: "Last 7 days", ms: 7 * DAY },
  { value: "30d", label: "Last 30 days", ms: 30 * DAY },
] as const;

export type RangeValue = (typeof RANGES)[number]["value"];

export function rangeMs(v: string | null | undefined, fallback: RangeValue = "7d"): number {
  return (RANGES.find((r) => r.value === v) ?? RANGES.find((r) => r.value === fallback)!).ms;
}

/** Every bucket label of a window (UTC), matching the server's labels. */
export function bucketLabels(since: number, until: number, bucket: "hour" | "day"): string[] {
  const step = bucket === "hour" ? HOUR : DAY;
  const out: string[] = [];
  for (let t = Math.floor(since / step) * step; t < until && out.length < 800; t += step) {
    const iso = new Date(t).toISOString();
    out.push(bucket === "hour" ? `${iso.slice(0, 13)}:00` : iso.slice(0, 10));
  }
  return out;
}

/** The series with empty buckets filled in, so charts keep a true time axis. */
export function fillSeries(series: SeriesPoint[], since: number, until: number, bucket: "hour" | "day"): SeriesPoint[] {
  const by = new Map(series.map((s) => [s.bucket, s]));
  return bucketLabels(since, until, bucket).map((b) => by.get(b) ?? { bucket: b, requests: 0, errors: 0, p50_ms: 0, p95_ms: 0, input_tokens: 0, output_tokens: 0, cost_usd: 0 });
}
