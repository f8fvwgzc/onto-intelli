-- The outbox (docs/design/EVENTS.md B.1, D7): one row per webhook delivery
-- or writeback call. Rows from actions and automations are inserted in the
-- commit that made the change; change-sourced rows hold only the change
-- cursor and are rendered when they are sent. A row is enqueued once per
-- (target, dedupe) and delivered at least once: receivers dedupe on its id.
-- Every time here is the database clock (now_ms(), 0010).

CREATE TABLE IF NOT EXISTS {schema}.outbox (
    id TEXT PRIMARY KEY,
    created_at BIGINT NOT NULL,
    kind TEXT NOT NULL,
    target TEXT NOT NULL,
    dedupe TEXT NOT NULL,
    source TEXT NOT NULL,
    workspace TEXT NOT NULL,
    receipt TEXT,
    run TEXT,
    cursor_seq BIGINT,
    cursor_n INT,
    event_type TEXT NOT NULL,
    payload JSONB NOT NULL DEFAULT 'null'::jsonb,
    cause JSONB,
    state TEXT NOT NULL,
    attempts INT NOT NULL DEFAULT 0,
    next_at BIGINT NOT NULL,
    -- The claim token of the worker sending it: a worker whose lease expired
    -- cannot finish a row another worker claimed since.
    claim TEXT,
    lease_until BIGINT,
    last_status INT,
    last_error TEXT,
    delivered_at BIGINT,
    otel_trace_id TEXT,
    UNIQUE (target, dedupe)
);
CREATE INDEX IF NOT EXISTS outbox_due ON {schema}.outbox (next_at)
    WHERE state IN ('pending', 'retry', 'inflight');
CREATE INDEX IF NOT EXISTS outbox_target ON {schema}.outbox (target, created_at);
CREATE INDEX IF NOT EXISTS outbox_receipt ON {schema}.outbox (receipt) WHERE receipt IS NOT NULL;

-- One row per attempt to send an outbox row. Append-only like the audit
-- trail (0003, 0008): only retention deletes.
CREATE TABLE IF NOT EXISTS {schema}.delivery_attempts (
    outbox_id TEXT NOT NULL,
    -- The claim the attempt was made under (unique per claim, so a replayed
    -- row's attempts never collide with its earlier ones).
    claim TEXT NOT NULL,
    attempt INT NOT NULL,
    ts BIGINT NOT NULL,
    doc JSONB NOT NULL,
    PRIMARY KEY (outbox_id, claim)
);
CREATE INDEX IF NOT EXISTS delivery_attempts_ts ON {schema}.delivery_attempts (ts);
CREATE INDEX IF NOT EXISTS delivery_attempts_row ON {schema}.delivery_attempts (outbox_id, ts);
DROP TRIGGER IF EXISTS delivery_attempts_append_only ON {schema}.delivery_attempts;
CREATE TRIGGER delivery_attempts_append_only BEFORE UPDATE ON {schema}.delivery_attempts
    FOR EACH ROW EXECUTE FUNCTION {schema}.refuse_update();
DROP TRIGGER IF EXISTS delivery_attempts_no_delete ON {schema}.delivery_attempts;
CREATE TRIGGER delivery_attempts_no_delete BEFORE DELETE ON {schema}.delivery_attempts
    FOR EACH ROW EXECUTE FUNCTION {schema}.refuse_delete();
DROP TRIGGER IF EXISTS delivery_attempts_no_truncate ON {schema}.delivery_attempts;
CREATE TRIGGER delivery_attempts_no_truncate BEFORE TRUNCATE ON {schema}.delivery_attempts
    FOR EACH STATEMENT EXECUTE FUNCTION {schema}.refuse_delete();
