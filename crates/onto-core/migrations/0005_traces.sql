-- Langfuse-style traces of the engine's work: a trace per ask, governed
-- action or traced call, its spans (intent, resolution, retrieval, LLM
-- generations, tools) and the scores evaluations and feedback attach to it.
-- Rows are only inserted; the retention task deletes old ones.

CREATE TABLE IF NOT EXISTS {schema}.traces (
    id TEXT PRIMARY KEY,                    -- 32 hex
    ts BIGINT NOT NULL,                     -- start, ms
    duration_ms DOUBLE PRECISION NOT NULL,
    name TEXT NOT NULL,                     -- intelli.ask, action.apply:assign, ontology.refine, …
    kind TEXT NOT NULL,                     -- ask | action | call | eval
    status TEXT NOT NULL,                   -- ok | error
    error_code TEXT,
    error_message TEXT,
    workspace TEXT NOT NULL,
    principal TEXT NOT NULL,
    via TEXT,
    user_id TEXT,
    session_id TEXT,
    api_key TEXT,                           -- prefix
    source TEXT NOT NULL,
    request_id TEXT,
    intent TEXT,
    mode TEXT,                              -- AnswerMode for asks
    deterministic BOOLEAN NOT NULL DEFAULT false,
    provider TEXT,
    model TEXT,
    llm_calls INT NOT NULL DEFAULT 0,
    cache_hits INT NOT NULL DEFAULT 0,      -- answer + completion cache
    input_tokens BIGINT NOT NULL DEFAULT 0,
    output_tokens BIGINT NOT NULL DEFAULT 0,
    cached_tokens BIGINT NOT NULL DEFAULT 0,
    context_tokens BIGINT NOT NULL DEFAULT 0,
    saved_tokens BIGINT NOT NULL DEFAULT 0,
    cost_usd DOUBLE PRECISION NOT NULL DEFAULT 0,
    input JSONB,                            -- {question} clipped, NULL without capture_content
    output JSONB,                           -- {answer} clipped
    metadata JSONB NOT NULL DEFAULT '{}',
    tags TEXT[] NOT NULL DEFAULT '{}',
    version TEXT NOT NULL                   -- onto-intelli version
);
CREATE INDEX IF NOT EXISTS traces_ts ON {schema}.traces (ts DESC, id DESC);
CREATE INDEX IF NOT EXISTS traces_session ON {schema}.traces (session_id, ts) WHERE session_id IS NOT NULL;
CREATE INDEX IF NOT EXISTS traces_user ON {schema}.traces (user_id, ts) WHERE user_id IS NOT NULL;
CREATE INDEX IF NOT EXISTS traces_name_ts ON {schema}.traces (name, ts);
CREATE INDEX IF NOT EXISTS traces_errors ON {schema}.traces (ts) WHERE status = 'error';

CREATE TABLE IF NOT EXISTS {schema}.spans (
    trace_id TEXT NOT NULL,
    id TEXT NOT NULL,                       -- 16 hex
    parent_id TEXT,
    name TEXT NOT NULL,
    kind TEXT NOT NULL,                     -- span | generation | retrieval | tool | event
    ts BIGINT NOT NULL,
    duration_ms DOUBLE PRECISION NOT NULL,
    status TEXT NOT NULL,
    error TEXT,
    model TEXT,
    input_tokens BIGINT NOT NULL DEFAULT 0,
    output_tokens BIGINT NOT NULL DEFAULT 0,
    cached_tokens BIGINT NOT NULL DEFAULT 0,
    cost_usd DOUBLE PRECISION NOT NULL DEFAULT 0,
    attrs JSONB NOT NULL DEFAULT '{}',
    input JSONB,
    output JSONB,
    PRIMARY KEY (trace_id, id)
);
CREATE INDEX IF NOT EXISTS spans_ts ON {schema}.spans (ts);
CREATE INDEX IF NOT EXISTS spans_model_ts ON {schema}.spans (model, ts) WHERE kind = 'generation';

CREATE TABLE IF NOT EXISTS {schema}.scores (
    id TEXT PRIMARY KEY,
    trace_id TEXT NOT NULL,
    span_id TEXT,
    ts BIGINT NOT NULL,
    name TEXT NOT NULL,                     -- eval.pass, user.feedback, accuracy, …
    value DOUBLE PRECISION,                 -- numeric and boolean (0/1) scores
    label TEXT,                             -- categorical scores
    source TEXT NOT NULL,                   -- eval | api | console
    author TEXT,
    comment TEXT
);
CREATE INDEX IF NOT EXISTS scores_trace ON {schema}.scores (trace_id);
CREATE INDEX IF NOT EXISTS scores_name_ts ON {schema}.scores (name, ts);

DROP TRIGGER IF EXISTS traces_append_only ON {schema}.traces;
CREATE TRIGGER traces_append_only BEFORE UPDATE ON {schema}.traces
    FOR EACH ROW EXECUTE FUNCTION {schema}.refuse_update();
DROP TRIGGER IF EXISTS spans_append_only ON {schema}.spans;
CREATE TRIGGER spans_append_only BEFORE UPDATE ON {schema}.spans
    FOR EACH ROW EXECUTE FUNCTION {schema}.refuse_update();
DROP TRIGGER IF EXISTS scores_append_only ON {schema}.scores;
CREATE TRIGGER scores_append_only BEFORE UPDATE ON {schema}.scores
    FOR EACH ROW EXECUTE FUNCTION {schema}.refuse_update();
