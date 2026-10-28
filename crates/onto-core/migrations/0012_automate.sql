-- Automation runs (docs/design/EVENTS.md C.7, C.8). One row per trigger an
-- automation acted on: claimed with INSERT ... ON CONFLICT (automation,
-- trigger_key) DO NOTHING RETURNING id, so a duplicate trigger is settled by
-- the claim and never retried. A `running` row means nothing of the run's
-- commit happened yet; recovery runs it again. Over-limit triggers are
-- `skipped` rows that can be rerun. Leases, consumer cursors and retry timers
-- are the tables of 0010; every time here is the database clock (now_ms()).

CREATE TABLE IF NOT EXISTS {schema}.automation_runs (
    id TEXT PRIMARY KEY,
    automation TEXT NOT NULL,
    -- The change cursor that triggered the run.
    trigger_key TEXT NOT NULL,
    closure_hash TEXT NOT NULL,
    -- running | succeeded | failed | retry | skipped | refused | would_run | cancelled
    state TEXT NOT NULL,
    depth INT NOT NULL DEFAULT 0,
    -- The consumer lease epoch the run was claimed under.
    epoch BIGINT NOT NULL,
    started_at BIGINT NOT NULL,
    finished_at BIGINT,
    doc JSONB NOT NULL DEFAULT '{}'::jsonb,
    UNIQUE (automation, trigger_key)
);
CREATE INDEX IF NOT EXISTS automation_runs_recent ON {schema}.automation_runs (automation, started_at DESC);
CREATE INDEX IF NOT EXISTS automation_runs_started ON {schema}.automation_runs (started_at DESC, id DESC);
CREATE INDEX IF NOT EXISTS automation_runs_running ON {schema}.automation_runs (epoch)
    WHERE state = 'running';
