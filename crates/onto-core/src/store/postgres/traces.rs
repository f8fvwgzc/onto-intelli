//! The trace tables of a tenant (`0005_traces.sql`): writes, lists,
//! metrics, sessions, scores and retention. Aggregates run in SQL
//! (`percentile_cont`, `date_trunc` in UTC); the results are shaped by the
//! same code as the in-memory store's ([`TraceMetrics::from_parts`],
//! [`SessionRow::rounded`]), so both answer alike.

use std::time::{Duration, Instant};

use serde_json::{Value, json};
use sqlx::Row;
use sqlx::postgres::PgRow;

use super::{CHUNK, PgStore, parse, sql};
use crate::Result;
use crate::sqlx;
use crate::trace::{
    MetricTotals, MetricsQuery, MetricsWindow, ModelStat, NameStat, ScoreRow, ScoreStat, SeriesPoint, SessionPage,
    SessionQuery, SessionRow, SpanRow, TraceDetail, TraceMetrics, TracePage, TraceQuery, TraceRecord, TraceRow,
    VACUUM_BATCH, encode_cursor, nul_free_json,
};

const TRACE_COLS: &str = "id, ts, duration_ms, name, kind, status, error_code, error_message, workspace, principal, \
     via, user_id, session_id, api_key, source, request_id, intent, mode, deterministic, provider, model, llm_calls, \
     cache_hits, input_tokens, output_tokens, cached_tokens, context_tokens, saved_tokens, cost_usd, otel_trace_id, \
     parent_span";

const TRACE_RECORD: &str = "id text, ts bigint, duration_ms float8, name text, kind text, status text, \
     error_code text, error_message text, workspace text, principal text, via text, user_id text, session_id text, \
     api_key text, source text, request_id text, intent text, mode text, deterministic boolean, provider text, \
     model text, llm_calls int, cache_hits int, input_tokens bigint, output_tokens bigint, cached_tokens bigint, \
     context_tokens bigint, saved_tokens bigint, cost_usd float8, otel_trace_id text, parent_span text, \
     input jsonb, output jsonb, metadata jsonb, tags text[], version text";

const SPAN_COLS: &str = "trace_id, id, parent_id, name, kind, ts, duration_ms, status, error, model, input_tokens, \
     output_tokens, cached_tokens, cost_usd";

const SPAN_RECORD: &str = "trace_id text, id text, parent_id text, name text, kind text, ts bigint, \
     duration_ms float8, status text, error text, model text, input_tokens bigint, output_tokens bigint, \
     cached_tokens bigint, cost_usd float8, attrs jsonb, input jsonb, output jsonb";

/// A bound parameter.
enum Arg {
    I(i64),
    F(f64),
    S(String),
}

/// Bind `args` in order to a query built with `sqlx::query*`.
macro_rules! bind {
    ($q:expr, $args:expr) => {{
        let mut q = $q;
        for a in $args {
            q = match a {
                Arg::I(v) => q.bind(v),
                Arg::F(v) => q.bind(v),
                Arg::S(v) => q.bind(v),
            };
        }
        q
    }};
}

/// Collects `AND …` clauses with their parameters.
#[derive(Default)]
struct Where {
    sql: String,
    args: Vec<Arg>,
}

impl Where {
    fn push(&mut self, clause: &str, a: Arg) {
        self.args.push(a);
        self.sql.push_str(&format!(" AND {}", clause.replace('$', &format!("${}", self.args.len()))));
    }

    /// A clause with two parameters (`$` then `#`).
    fn push2(&mut self, clause: &str, a: Arg, b: Arg) {
        self.args.push(a);
        let n = self.args.len();
        self.args.push(b);
        self.sql
            .push_str(&format!(" AND {}", clause.replace('$', &format!("${n}")).replace('#', &format!("${}", n + 1))));
    }
}

/// `%…%` for ILIKE, with `\`, `%` and `_` escaped (backslash is LIKE's escape).
fn like_pattern(q: &str) -> String {
    let mut p = String::from("%");
    for c in q.chars() {
        if matches!(c, '\\' | '%' | '_') {
            p.push('\\');
        }
        p.push(c);
    }
    p.push('%');
    p
}

fn trace_where(q: &TraceQuery) -> Result<Where> {
    let mut w = Where::default();
    if let Some(v) = q.since {
        w.push("ts >= $", Arg::I(v));
    }
    if let Some(v) = q.until {
        w.push("ts < $", Arg::I(v));
    }
    for (col, v) in [
        ("name", &q.name),
        ("kind", &q.kind),
        ("status", &q.status),
        ("workspace", &q.workspace),
        ("user_id", &q.user_id),
        ("session_id", &q.session_id),
        ("model", &q.model),
        ("otel_trace_id", &q.otel_trace_id),
        ("mode", &q.mode),
    ] {
        if let Some(v) = v {
            w.push(&format!("{col} = $"), Arg::S(v.clone()));
        }
    }
    if let Some(t) = &q.tag {
        w.push("$ = ANY(tags)", Arg::S(t.clone()));
    }
    if let Some(m) = q.min_ms {
        w.push("duration_ms >= $", Arg::F(m));
    }
    if let Some(text) = q.q.as_deref().filter(|t| !t.is_empty()) {
        w.push("input->>'question' ILIKE $", Arg::S(like_pattern(text)));
    }
    if let Some((ts, id)) = q.after()? {
        w.push2("(ts < $ OR (ts = $ AND id COLLATE \"C\" < #))", Arg::I(ts), Arg::S(id));
    }
    Ok(w)
}

fn metrics_where(w: &MetricsWindow, prefix: &str) -> Where {
    let mut out = Where::default();
    out.push(&format!("{prefix}ts >= $"), Arg::I(w.since));
    out.push(&format!("{prefix}ts < $"), Arg::I(w.until));
    if let Some(n) = &w.name {
        out.push(&format!("{prefix}name = $"), Arg::S(n.clone()));
    }
    if let Some(ws) = &w.workspace {
        out.push(&format!("{prefix}workspace = $"), Arg::S(ws.clone()));
    }
    out
}

fn json_col(r: &PgRow, col: &str) -> Result<Option<Value>> {
    let s: Option<String> = r.try_get(col)?;
    s.map(|s| parse(&s)).transpose()
}

/// A trace from a row of `SELECT {TRACE_COLS}, metadata::text AS metadata,
/// tags, version` plus `input`/`output` (`bodies`) or `preview`.
fn trace_row(r: &PgRow, bodies: bool) -> Result<TraceRow> {
    Ok(TraceRow {
        id: r.try_get("id")?,
        ts: r.try_get("ts")?,
        duration_ms: r.try_get("duration_ms")?,
        name: r.try_get("name")?,
        kind: r.try_get("kind")?,
        status: r.try_get("status")?,
        error_code: r.try_get("error_code")?,
        error_message: r.try_get("error_message")?,
        workspace: r.try_get("workspace")?,
        principal: r.try_get("principal")?,
        via: r.try_get("via")?,
        user_id: r.try_get("user_id")?,
        session_id: r.try_get("session_id")?,
        api_key: r.try_get("api_key")?,
        source: r.try_get("source")?,
        request_id: r.try_get("request_id")?,
        intent: r.try_get("intent")?,
        mode: r.try_get("mode")?,
        deterministic: r.try_get("deterministic")?,
        provider: r.try_get("provider")?,
        model: r.try_get("model")?,
        llm_calls: i64::from(r.try_get::<i32, _>("llm_calls")?),
        cache_hits: i64::from(r.try_get::<i32, _>("cache_hits")?),
        input_tokens: r.try_get("input_tokens")?,
        output_tokens: r.try_get("output_tokens")?,
        cached_tokens: r.try_get("cached_tokens")?,
        context_tokens: r.try_get("context_tokens")?,
        saved_tokens: r.try_get("saved_tokens")?,
        cost_usd: r.try_get("cost_usd")?,
        otel_trace_id: r.try_get("otel_trace_id")?,
        parent_span: r.try_get("parent_span")?,
        input: if bodies { json_col(r, "input")? } else { None },
        output: if bodies { json_col(r, "output")? } else { None },
        metadata: json_col(r, "metadata")?.unwrap_or_else(|| json!({})),
        tags: r.try_get("tags")?,
        version: r.try_get("version")?,
        preview: if bodies { None } else { r.try_get("preview")? },
        redacted: false,
    })
}

fn span_row(r: &PgRow) -> Result<SpanRow> {
    Ok(SpanRow {
        trace_id: r.try_get("trace_id")?,
        id: r.try_get("id")?,
        parent_id: r.try_get("parent_id")?,
        name: r.try_get("name")?,
        kind: r.try_get("kind")?,
        ts: r.try_get("ts")?,
        duration_ms: r.try_get("duration_ms")?,
        status: r.try_get("status")?,
        error: r.try_get("error")?,
        model: r.try_get("model")?,
        input_tokens: r.try_get("input_tokens")?,
        output_tokens: r.try_get("output_tokens")?,
        cached_tokens: r.try_get("cached_tokens")?,
        cost_usd: r.try_get("cost_usd")?,
        attrs: json_col(r, "attrs")?.unwrap_or_else(|| json!({})),
        input: json_col(r, "input")?,
        output: json_col(r, "output")?,
    })
}

fn score_row(r: &PgRow) -> Result<ScoreRow> {
    Ok(ScoreRow {
        id: r.try_get("id")?,
        trace_id: r.try_get("trace_id")?,
        span_id: r.try_get("span_id")?,
        ts: r.try_get("ts")?,
        name: r.try_get("name")?,
        value: r.try_get("value")?,
        label: r.try_get("label")?,
        source: r.try_get("source")?,
        author: r.try_get("author")?,
        comment: r.try_get("comment")?,
    })
}

fn full_select(table: &str) -> String {
    format!(
        "SELECT {TRACE_COLS}, input::text AS input, output::text AS output, metadata::text AS metadata, tags, version \
         FROM {table}"
    )
}

/// count, errors, p50, p95, p99, input, output, cached, cost, llm calls, asks, deterministic asks, cached asks.
type TotalsRow = (i64, i64, Option<f64>, Option<f64>, Option<f64>, i64, i64, i64, f64, i64, i64, i64, i64);
/// bucket, count, errors, p50, p95, input, output, cost.
type SeriesRow = (String, i64, i64, Option<f64>, Option<f64>, i64, i64, f64);

fn count(v: i64) -> u64 {
    v.max(0) as u64
}

impl PgStore {
    pub(super) fn pg_trace_write(&self, batch: &[TraceRecord]) -> Result<()> {
        if batch.is_empty() {
            return Ok(());
        }
        let pool = self.pool.clone();
        let (tt, ts) = (self.t("traces"), self.t("spans"));
        // Serialize on the caller's thread: one JSON array per statement,
        // without NUL characters (jsonb refuses `\u0000`).
        let traces: Vec<String> = batch
            .chunks(CHUNK)
            .map(|c| nul_free_json(&c.iter().map(|r| &r.trace).collect::<Vec<_>>()))
            .collect::<Result<_>>()?;
        let spans: Vec<&SpanRow> = batch.iter().flat_map(|r| r.spans.iter()).collect();
        let spans: Vec<String> = spans.chunks(CHUNK).map(nul_free_json).collect::<Result<_>>()?;
        self.block(async move {
            let mut tx = pool.begin().await?;
            for doc in traces {
                sqlx::query(sql(format!(
                    "INSERT INTO {tt} ({TRACE_COLS}, input, output, metadata, tags, version) \
                     SELECT {TRACE_COLS}, input, output, COALESCE(metadata, '{{}}'), COALESCE(tags, '{{}}'), version \
                     FROM jsonb_to_recordset($1::jsonb) AS r({TRACE_RECORD}) ON CONFLICT (id) DO NOTHING"
                )))
                .bind(doc)
                .execute(&mut *tx)
                .await?;
            }
            for doc in spans {
                sqlx::query(sql(format!(
                    "INSERT INTO {ts} ({SPAN_COLS}, attrs, input, output) \
                     SELECT {SPAN_COLS}, COALESCE(attrs, '{{}}'), input, output \
                     FROM jsonb_to_recordset($1::jsonb) AS r({SPAN_RECORD}) ON CONFLICT (trace_id, id) DO NOTHING"
                )))
                .bind(doc)
                .execute(&mut *tx)
                .await?;
            }
            tx.commit().await?;
            Ok(())
        })
    }

    pub(super) fn pg_trace_list(&self, q: &TraceQuery) -> Result<TracePage> {
        let pool = self.pool.clone();
        let tt = self.t("traces");
        let w = trace_where(q)?;
        let size = q.page_size();
        self.block(async move {
            let stmt = format!(
                "SELECT {TRACE_COLS}, metadata::text AS metadata, tags, version, \
                 left(input->>'question', {}) AS preview FROM {tt} WHERE TRUE{} \
                 ORDER BY ts DESC, id COLLATE \"C\" DESC LIMIT {}",
                crate::trace::PREVIEW_CHARS,
                w.sql,
                size + 1
            );
            let rows = bind!(sqlx::query(sql(stmt)), w.args).fetch_all(&pool).await?;
            let mut traces = rows.iter().map(|r| trace_row(r, false)).collect::<Result<Vec<_>>>()?;
            let next = (traces.len() > size).then(|| encode_cursor(traces[size - 1].ts, &traces[size - 1].id));
            traces.truncate(size);
            Ok(TracePage { traces, next })
        })
    }

    pub(super) fn pg_trace_get(&self, id: &str) -> Result<Option<TraceDetail>> {
        let pool = self.pool.clone();
        let (tt, ts, sc) = (self.t("traces"), self.t("spans"), self.t("scores"));
        let id = id.to_string();
        self.block(async move {
            let Some(row) =
                sqlx::query(sql(format!("{} WHERE id = $1", full_select(&tt)))).bind(&id).fetch_optional(&pool).await?
            else {
                return Ok(None);
            };
            let trace = trace_row(&row, true)?;
            let spans = sqlx::query(sql(format!(
                "SELECT {SPAN_COLS}, attrs::text AS attrs, input::text AS input, output::text AS output \
                 FROM {ts} WHERE trace_id = $1 ORDER BY ts, id COLLATE \"C\""
            )))
            .bind(&id)
            .fetch_all(&pool)
            .await?
            .iter()
            .map(span_row)
            .collect::<Result<Vec<_>>>()?;
            let scores = sqlx::query(sql(format!(
                "SELECT id, trace_id, span_id, ts, name, value, label, source, author, comment \
                 FROM {sc} WHERE trace_id = $1 ORDER BY ts, id COLLATE \"C\""
            )))
            .bind(&id)
            .fetch_all(&pool)
            .await?
            .iter()
            .map(score_row)
            .collect::<Result<Vec<_>>>()?;
            Ok(Some(TraceDetail { trace, spans, scores }))
        })
    }

    pub(super) fn pg_trace_session(&self, id: &str, limit: usize) -> Result<Vec<TraceRow>> {
        let pool = self.pool.clone();
        let tt = self.t("traces");
        let id = id.to_string();
        let limit = limit.clamp(1, crate::trace::MAX_LIST);
        self.block(async move {
            let stmt =
                format!("{} WHERE session_id = $1 ORDER BY ts, id COLLATE \"C\" LIMIT {limit}", full_select(&tt));
            let rows = sqlx::query(sql(stmt)).bind(&id).fetch_all(&pool).await?;
            rows.iter().map(|r| trace_row(r, true)).collect()
        })
    }

    pub(super) fn pg_trace_metrics(&self, q: &MetricsQuery) -> Result<TraceMetrics> {
        let w = q.window()?;
        let pool = self.pool.clone();
        let (tt, ts, sc) = (self.t("traces"), self.t("spans"), self.t("scores"));
        let fmt = if w.bucket == "hour" { "YYYY-MM-DD\"T\"HH24:00" } else { "YYYY-MM-DD" };
        let bucket = w.bucket;
        self.block(async move {
            let plain = metrics_where(&w, "");
            let totals: TotalsRow = bind!(
                sqlx::query_as(sql(format!(
                    "SELECT count(*)::bigint, (count(*) FILTER (WHERE status = 'error'))::bigint, \
                     percentile_cont(0.5) WITHIN GROUP (ORDER BY duration_ms), \
                     percentile_cont(0.95) WITHIN GROUP (ORDER BY duration_ms), \
                     percentile_cont(0.99) WITHIN GROUP (ORDER BY duration_ms), \
                     COALESCE(sum(input_tokens), 0)::bigint, COALESCE(sum(output_tokens), 0)::bigint, \
                     COALESCE(sum(cached_tokens), 0)::bigint, COALESCE(sum(cost_usd), 0)::float8, \
                     COALESCE(sum(llm_calls), 0)::bigint, (count(*) FILTER (WHERE kind = 'ask'))::bigint, \
                     (count(*) FILTER (WHERE kind = 'ask' AND deterministic))::bigint, \
                     (count(*) FILTER (WHERE kind = 'ask' AND cache_hits > 0))::bigint \
                     FROM {tt} WHERE TRUE{}",
                    plain.sql
                ))),
                plain.args
            )
            .fetch_one(&pool)
            .await?;
            let t = MetricTotals {
                requests: count(totals.0),
                errors: count(totals.1),
                p50_ms: totals.2.unwrap_or(0.0),
                p95_ms: totals.3.unwrap_or(0.0),
                p99_ms: totals.4.unwrap_or(0.0),
                input_tokens: totals.5,
                output_tokens: totals.6,
                cached_tokens: totals.7,
                cost_usd: totals.8,
                llm_calls: totals.9,
                asks: count(totals.10),
                deterministic_asks: count(totals.11),
                cached_asks: count(totals.12),
            };

            let plain = metrics_where(&w, "");
            let series: Vec<SeriesRow> = bind!(
                sqlx::query_as(sql(format!(
                    "SELECT to_char(date_trunc('{bucket}', to_timestamp(ts / 1000.0) AT TIME ZONE 'UTC'), '{fmt}'), \
                     count(*)::bigint, (count(*) FILTER (WHERE status = 'error'))::bigint, \
                     percentile_cont(0.5) WITHIN GROUP (ORDER BY duration_ms), \
                     percentile_cont(0.95) WITHIN GROUP (ORDER BY duration_ms), \
                     COALESCE(sum(input_tokens), 0)::bigint, COALESCE(sum(output_tokens), 0)::bigint, \
                     COALESCE(sum(cost_usd), 0)::float8 FROM {tt} WHERE TRUE{} GROUP BY 1",
                    plain.sql
                ))),
                plain.args
            )
            .fetch_all(&pool)
            .await?;
            let series = series
                .into_iter()
                .map(|(bucket, requests, errors, p50, p95, input, output, cost)| SeriesPoint {
                    bucket,
                    requests: count(requests),
                    errors: count(errors),
                    p50_ms: p50.unwrap_or(0.0),
                    p95_ms: p95.unwrap_or(0.0),
                    input_tokens: input,
                    output_tokens: output,
                    cost_usd: cost,
                })
                .collect();

            let joined = metrics_where(&w, "t.");
            let models: Vec<(String, i64, i64, i64, f64, Option<f64>)> = bind!(
                sqlx::query_as(sql(format!(
                    "SELECT COALESCE(s.model, ''), count(*)::bigint, COALESCE(sum(s.input_tokens), 0)::bigint, \
                     COALESCE(sum(s.output_tokens), 0)::bigint, COALESCE(sum(s.cost_usd), 0)::float8, \
                     percentile_cont(0.95) WITHIN GROUP (ORDER BY s.duration_ms) \
                     FROM {ts} s JOIN {tt} t ON t.id = s.trace_id WHERE s.kind = 'generation'{} GROUP BY 1",
                    joined.sql
                ))),
                joined.args
            )
            .fetch_all(&pool)
            .await?;
            let by_model = models
                .into_iter()
                .map(|(model, calls, input, output, cost, p95)| ModelStat {
                    model,
                    calls: count(calls),
                    input_tokens: input,
                    output_tokens: output,
                    cost_usd: cost,
                    p95_ms: p95.unwrap_or(0.0),
                })
                .collect();

            let plain = metrics_where(&w, "");
            let names: Vec<(String, i64, i64, Option<f64>)> = bind!(
                sqlx::query_as(sql(format!(
                    "SELECT name, count(*)::bigint, (count(*) FILTER (WHERE status = 'error'))::bigint, \
                     percentile_cont(0.95) WITHIN GROUP (ORDER BY duration_ms) FROM {tt} WHERE TRUE{} GROUP BY name",
                    plain.sql
                ))),
                plain.args
            )
            .fetch_all(&pool)
            .await?;
            let by_name = names
                .into_iter()
                .map(|(name, requests, errors, p95)| NameStat {
                    name,
                    requests: count(requests),
                    errors: count(errors),
                    p95_ms: p95.unwrap_or(0.0),
                })
                .collect();

            let joined = metrics_where(&w, "t.");
            let scores: Vec<(String, i64, Option<f64>)> = bind!(
                sqlx::query_as(sql(format!(
                    "SELECT sc.name, count(*)::bigint, avg(sc.value) \
                     FROM {sc} sc JOIN {tt} t ON t.id = sc.trace_id WHERE TRUE{} GROUP BY sc.name",
                    joined.sql
                ))),
                joined.args
            )
            .fetch_all(&pool)
            .await?;
            let scores = scores.into_iter().map(|(name, n, avg)| ScoreStat { name, count: count(n), avg }).collect();
            Ok(TraceMetrics::from_parts(t, series, by_model, by_name, scores))
        })
    }

    pub(super) fn pg_trace_sessions(&self, q: &SessionQuery) -> Result<SessionPage> {
        let pool = self.pool.clone();
        let tt = self.t("traces");
        let size = q.page_size();
        let mut w = Where::default();
        if let Some(v) = q.since {
            w.push("ts >= $", Arg::I(v));
        }
        if let Some(v) = q.until {
            w.push("ts < $", Arg::I(v));
        }
        if let Some(u) = &q.user_id {
            w.push("user_id = $", Arg::S(u.clone()));
        }
        if let Some(ws) = &q.workspace {
            w.push("workspace = $", Arg::S(ws.clone()));
        }
        // Parameters of HAVING follow those of WHERE.
        let mut having = Where { args: std::mem::take(&mut w.args), ..Default::default() };
        if let Some((ts, id)) = q.after()? {
            having.push2("(max(ts) < $ OR (max(ts) = $ AND session_id COLLATE \"C\" < #))", Arg::I(ts), Arg::S(id));
        }
        self.block(async move {
            let stmt = format!(
                "SELECT session_id, (array_agg(user_id ORDER BY ts, id COLLATE \"C\"))[1] AS user_id, \
                 min(ts) AS first_ts, max(ts) AS last_ts, count(*)::bigint AS traces, \
                 (count(*) FILTER (WHERE status = 'error'))::bigint AS errors, \
                 COALESCE(sum(input_tokens), 0)::bigint AS input_tokens, \
                 COALESCE(sum(output_tokens), 0)::bigint AS output_tokens, \
                 COALESCE(sum(cost_usd), 0)::float8 AS cost_usd, \
                 (array_agg(left(input->>'question', {}) ORDER BY ts, id COLLATE \"C\") \
                    FILTER (WHERE input->>'question' IS NOT NULL))[1] AS preview \
                 FROM {tt} WHERE session_id IS NOT NULL{} GROUP BY session_id HAVING TRUE{} \
                 ORDER BY max(ts) DESC, session_id COLLATE \"C\" DESC LIMIT {}",
                crate::trace::PREVIEW_CHARS,
                w.sql,
                having.sql,
                size + 1
            );
            let rows = bind!(sqlx::query(sql(stmt)), having.args).fetch_all(&pool).await?;
            let mut sessions = rows
                .iter()
                .map(|r| {
                    Ok(SessionRow {
                        session_id: r.try_get("session_id")?,
                        user_id: r.try_get("user_id")?,
                        first_ts: r.try_get("first_ts")?,
                        last_ts: r.try_get("last_ts")?,
                        traces: count(r.try_get("traces")?),
                        errors: count(r.try_get("errors")?),
                        input_tokens: r.try_get("input_tokens")?,
                        output_tokens: r.try_get("output_tokens")?,
                        cost_usd: r.try_get("cost_usd")?,
                        preview: r.try_get("preview")?,
                    }
                    .rounded())
                })
                .collect::<Result<Vec<_>>>()?;
            let next = (sessions.len() > size)
                .then(|| encode_cursor(sessions[size - 1].last_ts, &sessions[size - 1].session_id));
            sessions.truncate(size);
            Ok(SessionPage { sessions, next })
        })
    }

    pub(super) fn pg_score_write(&self, s: &ScoreRow) -> Result<()> {
        let pool = self.pool.clone();
        let sc = self.t("scores");
        // Text columns cannot hold NUL either.
        let mut v = serde_json::to_value(s)?;
        crate::trace::scrub_nul(&mut v);
        let s: ScoreRow = serde_json::from_value(v)?;
        self.block(async move {
            sqlx::query(sql(format!(
                "INSERT INTO {sc} (id, trace_id, span_id, ts, name, value, label, source, author, comment) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10) ON CONFLICT (id) DO NOTHING"
            )))
            .bind(s.id)
            .bind(s.trace_id)
            .bind(s.span_id)
            .bind(s.ts)
            .bind(s.name)
            .bind(s.value)
            .bind(s.label)
            .bind(s.source)
            .bind(s.author)
            .bind(s.comment)
            .execute(&pool)
            .await?;
            Ok(())
        })
    }

    pub(super) fn pg_trace_vacuum(&self, before_ms: i64, budget: Duration) -> Result<u64> {
        let pool = self.pool.clone();
        let tables = [self.t("spans"), self.t("scores"), self.t("traces")];
        self.block(async move {
            let start = Instant::now();
            let mut deleted = 0u64;
            'tables: for t in tables {
                loop {
                    let n = sqlx::query(sql(format!(
                        "DELETE FROM {t} WHERE ctid IN (SELECT ctid FROM {t} WHERE ts < $1 LIMIT {VACUUM_BATCH})"
                    )))
                    .bind(before_ms)
                    .execute(&pool)
                    .await?
                    .rows_affected();
                    deleted += n;
                    if start.elapsed() >= budget {
                        break 'tables;
                    }
                    if n < VACUUM_BATCH as u64 {
                        break;
                    }
                }
            }
            Ok(deleted)
        })
    }
}
