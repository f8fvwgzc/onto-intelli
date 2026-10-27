-- Leases, consumer cursors and timers (docs/design/EVENTS.md D12, section 9).
-- Every time here is the database clock, never a process clock: processes
-- skew, and a paused process can wake up believing it still holds a lease.

CREATE OR REPLACE FUNCTION {schema}.now_ms() RETURNS BIGINT
LANGUAGE sql VOLATILE AS $$
    SELECT (extract(epoch FROM clock_timestamp()) * 1000)::bigint
$$;

-- One row per lease name. `epoch` grows each time the lease changes holder;
-- a write fenced with (name, holder, epoch) commits only while that holder
-- still has the lease at that epoch.
CREATE TABLE IF NOT EXISTS {schema}.leases (
    name TEXT PRIMARY KEY,
    holder TEXT NOT NULL,
    epoch BIGINT NOT NULL,
    until BIGINT NOT NULL
);

-- The change cursor of a background consumer (a webhook endpoint's fan-out,
-- the automation consumer): it resumes after (seq, n).
CREATE TABLE IF NOT EXISTS {schema}.consumers (
    name TEXT PRIMARY KEY,
    seq BIGINT NOT NULL,
    n INT NOT NULL,
    updated_at BIGINT NOT NULL
);

-- Work due at a database-clock time (retries of runs and deliveries).
CREATE TABLE IF NOT EXISTS {schema}.timers (
    name TEXT PRIMARY KEY,
    due_at BIGINT NOT NULL,
    doc JSONB NOT NULL DEFAULT '{}'::jsonb
);
CREATE INDEX IF NOT EXISTS timers_due ON {schema}.timers (due_at);
