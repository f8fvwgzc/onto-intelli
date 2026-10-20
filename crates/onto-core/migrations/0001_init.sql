-- Graph, append-only event log, entity history, KV with expiry, commit log.
-- {schema} is the tenant's PostgreSQL schema. IF NOT EXISTS lets this adopt
-- tenants created before migrations were versioned.

CREATE TABLE IF NOT EXISTS {schema}.meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);

-- Commit head: writers lock this row and compare it with the sequence they
-- staged against (optimistic concurrency).
CREATE TABLE IF NOT EXISTS {schema}.head (id INT PRIMARY KEY CHECK (id = 1), seq BIGINT NOT NULL);
INSERT INTO {schema}.head (id, seq) VALUES (1, 0) ON CONFLICT DO NOTHING;

CREATE TABLE IF NOT EXISTS {schema}.entities (
    id TEXT PRIMARY KEY,
    kind TEXT NOT NULL,
    workspace TEXT NOT NULL,
    doc JSONB NOT NULL,
    updated_at BIGINT NOT NULL
);
CREATE INDEX IF NOT EXISTS entities_ws_kind ON {schema}.entities (workspace, kind);

CREATE TABLE IF NOT EXISTS {schema}.links (
    src TEXT NOT NULL,
    rel TEXT NOT NULL,
    dst TEXT NOT NULL,
    doc JSONB NOT NULL,
    PRIMARY KEY (src, rel, dst)
);
CREATE INDEX IF NOT EXISTS links_dst ON {schema}.links (dst, rel);

CREATE TABLE IF NOT EXISTS {schema}.events (
    id TEXT PRIMARY KEY,
    ts BIGINT NOT NULL,
    workspace TEXT NOT NULL,
    actor TEXT NOT NULL,
    action TEXT NOT NULL,
    target TEXT,
    doc JSONB NOT NULL
);
CREATE INDEX IF NOT EXISTS events_ws_ts ON {schema}.events (workspace, ts);
CREATE INDEX IF NOT EXISTS events_target ON {schema}.events (target, ts);
CREATE INDEX IF NOT EXISTS events_actor ON {schema}.events (actor, ts);
CREATE INDEX IF NOT EXISTS events_action ON {schema}.events (action, ts);

CREATE TABLE IF NOT EXISTS {schema}.kv (
    ns TEXT NOT NULL,
    key TEXT NOT NULL,
    value JSONB NOT NULL,
    expires_at BIGINT,
    PRIMARY KEY (ns, key)
);

CREATE TABLE IF NOT EXISTS {schema}.entity_versions (
    seq BIGSERIAL PRIMARY KEY,
    id TEXT NOT NULL,
    revision BIGINT NOT NULL,
    ts BIGINT NOT NULL,
    doc JSONB NOT NULL
);
CREATE INDEX IF NOT EXISTS entity_versions_id ON {schema}.entity_versions (id, seq);
CREATE INDEX IF NOT EXISTS entity_versions_ts ON {schema}.entity_versions (ts);

CREATE TABLE IF NOT EXISTS {schema}.commits (
    seq BIGINT PRIMARY KEY,
    ts BIGINT NOT NULL,
    origin TEXT NOT NULL,
    doc JSONB NOT NULL
);

CREATE TABLE IF NOT EXISTS {schema}.link_versions (
    seq BIGSERIAL PRIMARY KEY,
    src TEXT NOT NULL,
    rel TEXT NOT NULL,
    dst TEXT NOT NULL,
    ts BIGINT NOT NULL,
    doc JSONB NOT NULL
);
CREATE INDEX IF NOT EXISTS link_versions_src ON {schema}.link_versions (src, ts);
CREATE INDEX IF NOT EXISTS link_versions_dst ON {schema}.link_versions (dst, ts);
