//! The trace tables through the `Store` trait: one fixture written to the
//! in-memory store and (with a test database, see `common`) to PostgreSQL,
//! the same queries on both, and the answers compared: lists with filters
//! and keyset paging, details, sessions, metrics (percentiles, buckets, by
//! model, deterministic share, scores) and retention. PostgreSQL also
//! refuses updates of the append-only tables.

mod common;

use std::time::Duration;

use serde_json::{Value, json};

use onto_core::store::{MemoryStore, Store};
use onto_core::trace::{
    MetricsQuery, ScoreRow, SessionQuery, SpanRow, TraceQuery, TraceRecord, TraceRow, decode_cursor,
};

/// 2026-10-20T00:00:00Z.
const BASE: i64 = 1_792_454_400_000;
/// Traces start 50 minutes apart.
const STEP: i64 = 3_000_000;
const N: i64 = 30;

fn fixture() -> (Vec<TraceRecord>, Vec<ScoreRow>) {
    let names = [("intelli.ask", "ask"), ("action.apply:assign", "action"), ("ontology.refine", "call")];
    let mut records = Vec::new();
    for i in 0..N {
        let (name, kind) = names[(i % 3) as usize];
        let id = format!("{:032x}", i + 1);
        let ts = BASE + i * STEP;
        let ask = kind == "ask";
        let model = if i % 2 == 0 { "m-a" } else { "m-b" };
        let trace = TraceRow {
            id: id.clone(),
            ts,
            duration_ms: ((i * 37) % 101) as f64 + 0.5,
            name: name.into(),
            kind: kind.into(),
            status: if i % 7 == 0 { "error".into() } else { "ok".into() },
            error_code: (i % 7 == 0).then(|| "internal".into()),
            error_message: (i % 7 == 0).then(|| "boom".into()),
            workspace: if i % 4 == 0 { "beta".into() } else { "acme".into() },
            principal: format!("user:{}", i % 3),
            via: None,
            user_id: Some(format!("u{}", i % 3)),
            session_id: (i % 2 == 0).then(|| format!("s{}", i % 5)),
            api_key: (i % 3 == 0).then(|| "onto_sk_abcd1234".into()),
            source: "http".into(),
            otel_trace_id: (i % 6 == 1).then(|| "4bf92f3577b34da6a3ce929d0e0e4736".into()),
            parent_span: (i % 6 == 1).then(|| format!("{:016x}", 0xabc0 + i)),
            request_id: Some(format!("ask_{i}")),
            intent: ask.then(|| "lookup".into()),
            mode: ask.then(|| if i % 2 == 0 { "deterministic".into() } else { "llm".into() }),
            deterministic: ask && i % 2 == 0,
            provider: Some("mock".into()),
            model: Some(model.into()),
            llm_calls: i64::from(i % 3 != 1),
            cache_hits: i64::from(i % 5 == 0),
            input_tokens: i * 10,
            output_tokens: i * 2,
            cached_tokens: i,
            context_tokens: i * 3,
            saved_tokens: i * 4,
            cost_usd: i as f64 * 0.001,
            input: ask.then(|| json!({ "question": format!("Question {i} about 100%_off deals, «unicode» ✓") })),
            output: ask.then(|| json!({ "answer": format!("Answer {i}") })),
            metadata: json!({ "i": i }),
            tags: vec![format!("t{}", i % 2)],
            version: "0.2.0".into(),
            preview: None,
            redacted: false,
        };
        let mut spans = vec![SpanRow {
            trace_id: id.clone(),
            id: format!("{:016x}", 1000 + i),
            parent_id: None,
            name: name.into(),
            kind: if ask { "span".into() } else { "tool".into() },
            ts,
            duration_ms: trace.duration_ms,
            status: trace.status.clone(),
            attrs: json!({ "workspace": trace.workspace }),
            ..Default::default()
        }];
        if i % 3 != 1 {
            spans.push(SpanRow {
                trace_id: id.clone(),
                id: format!("{:016x}", 2000 + i),
                parent_id: Some(format!("{:016x}", 1000 + i)),
                name: "llm".into(),
                kind: "generation".into(),
                ts: ts + 1,
                duration_ms: ((i * 13) % 50) as f64 + 0.25,
                status: "ok".into(),
                model: Some(model.into()),
                input_tokens: i * 10,
                output_tokens: i * 2,
                cached_tokens: i,
                cost_usd: i as f64 * 0.001,
                attrs: json!({ "provider": "mock", "cache": "miss" }),
                input: Some(json!({ "messages": [{ "role": "user", "content": "hi" }] })),
                output: Some(json!({ "text": "hello" })),
                ..Default::default()
            });
        }
        records.push(TraceRecord { trace, spans });
    }
    let mut scores = Vec::new();
    for i in (0..N).filter(|i| i % 4 == 0) {
        scores.push(ScoreRow {
            id: format!("sc_{i}"),
            trace_id: format!("{:032x}", i + 1),
            ts: BASE + i * STEP + 10,
            name: "eval.pass".into(),
            value: Some(f64::from(u8::from(i % 8 == 0))),
            source: "eval".into(),
            author: Some("user:0".into()),
            ..Default::default()
        });
    }
    scores.push(ScoreRow {
        id: "sc_label".into(),
        trace_id: format!("{:032x}", 2),
        ts: BASE + STEP + 20,
        name: "user.feedback".into(),
        label: Some("helpful".into()),
        source: "api".into(),
        comment: Some("nice".into()),
        ..Default::default()
    });
    (records, scores)
}

fn ids(page: &[TraceRow]) -> Vec<String> {
    page.iter().map(|t| t.id.clone()).collect()
}

fn all_pages(store: &dyn Store, mut q: TraceQuery) -> Vec<String> {
    let mut out = Vec::new();
    loop {
        let page = store.trace_list(&q).unwrap();
        out.extend(ids(&page.traces));
        match page.next {
            Some(next) => q.cursor = Some(next),
            None => return out,
        }
    }
}

/// Run every query on `store` and return the answers.
fn suite(store: &dyn Store) -> Value {
    let (records, scores) = fixture();
    store.trace_write(&records[..20]).unwrap();
    store.trace_write(&records[20..]).unwrap();
    store.trace_write(&records[..3]).unwrap(); // written again: skipped
    for s in &scores {
        store.score_write(s).unwrap();
    }
    store.score_write(&scores[0]).unwrap();

    let mut out = serde_json::Map::new();
    // Lists: newest first, no bodies, a preview of the question.
    let full = store.trace_list(&TraceQuery { limit: Some(200), ..Default::default() }).unwrap();
    assert_eq!(full.traces.len(), N as usize);
    assert!(full.next.is_none());
    let expected: Vec<String> = (0..N).rev().map(|i| format!("{:032x}", i + 1)).collect();
    assert_eq!(ids(&full.traces), expected);
    assert!(full.traces.iter().all(|t| t.input.is_none() && t.output.is_none()));
    let first_ask = &full.traces.iter().find(|t| t.kind == "ask").unwrap();
    assert!(first_ask.preview.as_deref().unwrap().starts_with("Question 27 about"));
    assert_eq!(store.trace_list(&TraceQuery::default()).unwrap().traces.len(), 30, "default limit 50");
    // Keyset paging gives the same order with no gaps or repeats.
    assert_eq!(all_pages(store, TraceQuery { limit: Some(7), ..Default::default() }), expected);
    let page = store.trace_list(&TraceQuery { limit: Some(7), ..Default::default() }).unwrap();
    let (ts, id) = decode_cursor(page.next.as_deref().unwrap()).unwrap();
    assert_eq!((ts, id.as_str()), (page.traces[6].ts, page.traces[6].id.as_str()));
    assert!(store.trace_list(&TraceQuery { cursor: Some("%%".into()), ..Default::default() }).is_err());

    let filters: Vec<(&str, TraceQuery)> = vec![
        ("name", TraceQuery { name: Some("ontology.refine".into()), ..Default::default() }),
        ("kind", TraceQuery { kind: Some("action".into()), ..Default::default() }),
        ("status", TraceQuery { status: Some("error".into()), ..Default::default() }),
        ("workspace", TraceQuery { workspace: Some("beta".into()), ..Default::default() }),
        ("user", TraceQuery { user_id: Some("u1".into()), ..Default::default() }),
        ("session", TraceQuery { session_id: Some("s2".into()), ..Default::default() }),
        ("model", TraceQuery { model: Some("m-b".into()), ..Default::default() }),
        ("mode", TraceQuery { mode: Some("llm".into()), ..Default::default() }),
        ("otel", TraceQuery { otel_trace_id: Some("4bf92f3577b34da6a3ce929d0e0e4736".into()), ..Default::default() }),
        ("tag", TraceQuery { tag: Some("t1".into()), ..Default::default() }),
        ("min_ms", TraceQuery { min_ms: Some(80.5), ..Default::default() }),
        ("window", TraceQuery { since: Some(BASE + 5 * STEP), until: Some(BASE + 9 * STEP), ..Default::default() }),
        ("q", TraceQuery { q: Some("QUESTION 1".into()), ..Default::default() }),
        ("q_unicode", TraceQuery { q: Some("«UNICODE» ✓".into()), ..Default::default() }),
        ("q_literal_percent", TraceQuery { q: Some("100%_off".into()), ..Default::default() }),
        ("q_no_wildcards", TraceQuery { q: Some("1_0".into()), ..Default::default() }),
        ("q_no_percent", TraceQuery { q: Some("Question%deals".into()), ..Default::default() }),
        (
            "combined",
            TraceQuery {
                kind: Some("ask".into()),
                status: Some("ok".into()),
                workspace: Some("acme".into()),
                ..Default::default()
            },
        ),
    ];
    let mut f = serde_json::Map::new();
    for (name, q) in filters {
        f.insert(name.into(), json!(all_pages(store, TraceQuery { limit: Some(4), ..q })));
    }
    // Calls under one OpenTelemetry trace are traces of their own.
    let otel: Vec<String> = [25, 19, 13, 7, 1].iter().map(|i| format!("{:032x}", i + 1)).collect();
    assert_eq!(f["otel"], json!(otel));
    assert_eq!(
        f["window"],
        json!([
            "00000000000000000000000000000009",
            "00000000000000000000000000000008",
            "00000000000000000000000000000007",
            "00000000000000000000000000000006"
        ])
    );
    assert_eq!(f["q_unicode"].as_array().unwrap().len(), 10, "every ask, case-insensitively");
    assert_eq!(f["q_literal_percent"].as_array().unwrap().len(), 10);
    assert_eq!(f["q_no_wildcards"], json!([]), "_ is not a wildcard");
    assert_eq!(f["q_no_percent"], json!([]), "% is not a wildcard");
    assert_eq!(f["q"].as_array().unwrap().len(), 3, "asks 12, 15 and 18");
    out.insert("filters".into(), Value::Object(f));

    // One trace with its spans and scores.
    let d = store.trace_get(&format!("{:032x}", 1)).unwrap().unwrap();
    assert_eq!(d.trace.input, records[0].trace.input);
    assert_eq!(d.trace.metadata, json!({ "i": 0 }));
    assert_eq!(d.spans.len(), 2);
    assert_eq!(d.spans[1].parent_id.as_deref(), Some(d.spans[0].id.as_str()));
    assert_eq!(d.scores.len(), 1, "written twice, stored once");
    assert_eq!(
        d,
        onto_core::trace::TraceDetail {
            trace: records[0].trace.clone(),
            spans: records[0].spans.clone(),
            scores: vec![scores[0].clone()]
        }
    );
    assert!(store.trace_get("ffffffffffffffffffffffffffffffff").unwrap().is_none());
    out.insert("detail".into(), serde_json::to_value(&d).unwrap());
    out.insert("labelled".into(), serde_json::to_value(store.trace_get(&format!("{:032x}", 2)).unwrap()).unwrap());

    // A session's traces, oldest first, with bodies.
    let s = store.trace_session("s0", 50).unwrap();
    assert_eq!(s.iter().map(|t| t.ts).collect::<Vec<_>>(), vec![BASE, BASE + 10 * STEP, BASE + 20 * STEP]);
    assert!(s[0].input.is_some());
    out.insert("session".into(), json!(ids(&s)));

    // Sessions: one row each, the most recently active first, paged.
    let mut pages = Vec::new();
    let mut q = SessionQuery { limit: Some(2), ..Default::default() };
    loop {
        let p = store.trace_sessions(&q).unwrap();
        pages.extend(p.sessions);
        match p.next {
            Some(n) => q.cursor = Some(n),
            None => break,
        }
    }
    assert_eq!(pages.iter().map(|s| s.session_id.as_str()).collect::<Vec<_>>(), vec!["s3", "s1", "s4", "s2", "s0"]);
    let s0 = pages.iter().find(|s| s.session_id == "s0").unwrap();
    assert_eq!((s0.traces, s0.errors, s0.first_ts), (3, 1, BASE));
    assert!(s0.preview.as_deref().unwrap().starts_with("Question 0"));
    out.insert("sessions".into(), serde_json::to_value(&pages).unwrap());
    let u = store
        .trace_sessions(&SessionQuery { user_id: Some("u0".into()), since: Some(BASE + STEP), ..Default::default() })
        .unwrap();
    out.insert("sessions_u0".into(), serde_json::to_value(&u).unwrap());
    // One workspace: only its traces count (s0 has traces 0 and 20 in beta, 10 in acme).
    let beta = store.trace_sessions(&SessionQuery { workspace: Some("beta".into()), ..Default::default() }).unwrap();
    let s0 = beta.sessions.iter().find(|s| s.session_id == "s0").unwrap();
    assert_eq!((s0.traces, s0.first_ts, s0.last_ts), (2, BASE, BASE + 20 * STEP));
    let acme = store.trace_sessions(&SessionQuery { workspace: Some("acme".into()), ..Default::default() }).unwrap();
    assert_eq!(acme.sessions.iter().find(|s| s.session_id == "s0").unwrap().traces, 1);
    let none = store.trace_sessions(&SessionQuery { workspace: Some("hr".into()), ..Default::default() }).unwrap();
    assert!(none.sessions.is_empty());
    out.insert("sessions_beta".into(), serde_json::to_value(&beta).unwrap());

    // Metrics: totals, percentiles, buckets by day and hour, by model, by name, scores.
    let all = MetricsQuery { since: Some(BASE), until: Some(BASE + N * STEP), ..Default::default() };
    let m = store.trace_metrics(&all).unwrap();
    assert_eq!((m.requests, m.errors), (30, 5));
    assert_eq!(m.series.len(), 2, "two days");
    assert_eq!(m.series[0].bucket, "2026-10-20");
    assert_eq!(m.by_name.len(), 3);
    assert_eq!(m.by_model.iter().map(|b| b.calls).sum::<u64>(), 20);
    assert_eq!(m.deterministic_share, 0.5);
    assert_eq!(m.cache_hit_rate, 0.2, "asks 0, 15 of 0, 3, …, 27");
    assert_eq!(m.scores[0].name, "eval.pass");
    assert_eq!((m.scores[0].count, m.scores[0].avg), (8, Some(0.5)));
    assert_eq!((m.scores[1].count, m.scores[1].avg), (1, None));
    out.insert("metrics".into(), serde_json::to_value(&m).unwrap());
    let hourly = MetricsQuery { bucket: Some("hour".into()), name: Some("intelli.ask".into()), ..all.clone() };
    let h = store.trace_metrics(&hourly).unwrap();
    assert_eq!(h.requests, 10);
    assert!(h.series.iter().all(|p| p.bucket.len() == 16 && p.bucket.ends_with(":00")), "{:?}", h.series);
    out.insert("metrics_hourly".into(), serde_json::to_value(&h).unwrap());
    let beta = store.trace_metrics(&MetricsQuery { workspace: Some("beta".into()), ..all.clone() }).unwrap();
    out.insert("metrics_beta".into(), serde_json::to_value(&beta).unwrap());
    let empty = store.trace_metrics(&MetricsQuery { since: Some(0), until: Some(1000), ..Default::default() }).unwrap();
    assert_eq!((empty.requests, empty.p95_ms, empty.error_rate), (0, 0.0, 0.0));
    assert!(store.trace_metrics(&MetricsQuery { bucket: Some("year".into()), ..Default::default() }).is_err());

    // Retention deletes what started before the cut and nothing else.
    let cut = BASE + 10 * STEP;
    let deleted = store.trace_vacuum(cut, Duration::from_secs(60)).unwrap();
    // 10 traces, their spans (10 roots, 7 generations), 3 eval scores and the label.
    assert_eq!(deleted, 10 + 17 + 4);
    let left = store.trace_list(&TraceQuery { limit: Some(200), ..Default::default() }).unwrap();
    assert_eq!(left.traces.len(), 20);
    assert!(left.traces.iter().all(|t| t.ts >= cut));
    assert!(store.trace_get(&format!("{:032x}", 1)).unwrap().is_none());
    assert_eq!(store.trace_get(&format!("{:032x}", 11)).unwrap().unwrap().spans.len(), 1);
    assert_eq!(store.trace_vacuum(cut, Duration::from_secs(60)).unwrap(), 0);
    out.insert("after_vacuum".into(), serde_json::to_value(store.trace_metrics(&all).unwrap()).unwrap());
    Value::Object(out)
}

#[test]
fn memory_and_postgres_answer_alike() {
    let memory = suite(&MemoryStore::default());
    let Some(pg) = common::TestTenant::new("traces") else { return };
    let postgres = suite(&pg.store());
    for (k, v) in memory.as_object().unwrap() {
        assert_eq!(&postgres[k], v, "{k} differs between memory and postgres");
    }
}

#[test]
fn trace_tables_are_append_only() {
    let Some(pg) = common::TestTenant::new("trace_append") else { return };
    let store = pg.store();
    let (records, scores) = fixture();
    store.trace_write(&records[..1]).unwrap();
    store.score_write(&scores[0]).unwrap();
    let schema = store.schema_name().to_string();
    let url = pg.url.clone();
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    for (table, set) in [("traces", "name = 'x'"), ("spans", "name = 'x'"), ("scores", "name = 'x'")] {
        let (url, schema) = (url.clone(), schema.clone());
        let res = rt.block_on(async move {
            let pool = onto_core::sqlx::PgPool::connect(&url).await.unwrap();
            let update = format!("UPDATE \"{schema}\".{table} SET {set}");
            onto_core::sqlx::raw_sql(onto_core::sqlx::AssertSqlSafe(update)).execute(&pool).await
        });
        let msg = res.unwrap_err().to_string();
        assert!(msg.contains("append-only"), "{table}: {msg}");
    }
}

#[test]
fn nul_characters_do_not_fail_a_batch() {
    let Some(pg) = common::TestTenant::new("trace_nul") else { return };
    let store = pg.store();
    let (mut records, mut scores) = fixture();
    let t = &mut records[0];
    t.trace.input = Some(json!({ "question": "a\u{0}b" }));
    t.trace.metadata = json!({ "k\u{0}": ["x\u{0}"] });
    t.trace.tags = vec!["t\u{0}".into()];
    t.trace.user_id = Some("u\u{0}".into());
    t.spans[0].attrs = json!({ "answer": "\u{0}" });
    scores[0].comment = Some("c\u{0}".into());
    // One bad record and the rest of the batch: all of them are stored.
    store.trace_write(&records[..5]).unwrap();
    store.score_write(&scores[0]).unwrap();
    let d = store.trace_get(&records[0].trace.id).unwrap().unwrap();
    assert_eq!(d.trace.input, Some(json!({ "question": "a\u{FFFD}b" })));
    assert_eq!(d.trace.metadata, json!({ "k\u{FFFD}": ["x\u{FFFD}"] }));
    assert_eq!((d.trace.tags.clone(), d.trace.user_id.as_deref()), (vec!["t\u{FFFD}".to_string()], Some("u\u{FFFD}")));
    assert_eq!(d.spans[0].attrs, json!({ "answer": "\u{FFFD}" }));
    assert_eq!(d.scores[0].comment.as_deref(), Some("c\u{FFFD}"));
    assert_eq!(store.trace_list(&TraceQuery::default()).unwrap().traces.len(), 5);
}
