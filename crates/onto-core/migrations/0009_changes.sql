-- The change feed (docs/design/EVENTS.md, Part A): one append-only row per
-- changed entity, link or schema version, written in the commit that made
-- the change. The cursor is (seq, n). A row holds the record without the
-- entity document; create and update rows join the version that commit
-- wrote through entity_versions.change_seq.

CREATE TABLE IF NOT EXISTS {schema}.changes (
    seq BIGINT NOT NULL,
    n INT NOT NULL,
    ts BIGINT NOT NULL,
    workspace TEXT NOT NULL,
    kind TEXT NOT NULL,
    op TEXT NOT NULL,
    target TEXT NOT NULL,
    doc JSONB NOT NULL,
    PRIMARY KEY (seq, n)
);
CREATE INDEX IF NOT EXISTS changes_ts ON {schema}.changes (ts);

-- Append-only like the audit trail (0003, 0008): only retention deletes.
DROP TRIGGER IF EXISTS changes_append_only ON {schema}.changes;
CREATE TRIGGER changes_append_only BEFORE UPDATE ON {schema}.changes
    FOR EACH ROW EXECUTE FUNCTION {schema}.refuse_update();
DROP TRIGGER IF EXISTS changes_no_delete ON {schema}.changes;
CREATE TRIGGER changes_no_delete BEFORE DELETE ON {schema}.changes
    FOR EACH ROW EXECUTE FUNCTION {schema}.refuse_delete();
DROP TRIGGER IF EXISTS changes_no_truncate ON {schema}.changes;
CREATE TRIGGER changes_no_truncate BEFORE TRUNCATE ON {schema}.changes
    FOR EACH STATEMENT EXECUTE FUNCTION {schema}.refuse_delete();

-- Capture is a tenant property. A cursor whose seq is below `floor` is
-- expired: retention removed its rows, or nothing was recorded meanwhile.
CREATE TABLE IF NOT EXISTS {schema}.changes_meta (
    one BOOLEAN PRIMARY KEY DEFAULT true CHECK (one),
    capture BOOLEAN NOT NULL DEFAULT true,
    floor BIGINT NOT NULL DEFAULT 0
);
-- A tenant that already has commits recorded none of them: the floor starts
-- at its head, so a cursor from before this migration reads as expired
-- instead of as an empty feed. A new tenant starts at 0.
INSERT INTO {schema}.changes_meta (one, floor) SELECT true, seq FROM {schema}.head WHERE id = 1
    ON CONFLICT DO NOTHING;

-- Builds from this one on refuse to write when the tenant needs a newer writer.
ALTER TABLE {schema}.head ADD COLUMN IF NOT EXISTS min_writer INT NOT NULL DEFAULT 0;
ALTER TABLE {schema}.entity_versions ADD COLUMN IF NOT EXISTS change_seq BIGINT;
CREATE INDEX IF NOT EXISTS entity_versions_change ON {schema}.entity_versions (id, change_seq)
    WHERE change_seq IS NOT NULL;

-- Does a commit document write the live schema (KV namespaces "schema" and
-- "schema.version")? Its `kv` is a list of [namespace, key] pairs.
CREATE OR REPLACE FUNCTION {schema}.commit_touches_schema(doc JSONB) RETURNS BOOLEAN
LANGUAGE sql IMMUTABLE AS $$
    SELECT EXISTS (
        SELECT 1 FROM jsonb_array_elements(coalesce(doc->'kv', '[]'::jsonb)) AS k(pair)
        WHERE jsonb_typeof(k.pair) = 'array' AND k.pair->>0 IN ('schema', 'schema.version')
    )
$$;

-- A state-changing commit without captured change rows becomes a gap record
-- (older writers, bulk imports, anything around commit_locked). Deferred, so
-- it runs at COMMIT and sees the tenant's capture setting then.
CREATE OR REPLACE FUNCTION {schema}.changes_gap() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE
    reload BOOLEAN := coalesce((NEW.doc->>'reload')::boolean, false);
BEGIN
    IF coalesce((NEW.doc->>'captured')::boolean, false) THEN RETURN NULL; END IF;
    IF NOT coalesce((SELECT capture FROM {schema}.changes_meta), true) THEN RETURN NULL; END IF;
    IF jsonb_array_length(coalesce(NEW.doc->'entities', '[]'::jsonb)) = 0
       AND jsonb_array_length(coalesce(NEW.doc->'links', '[]'::jsonb)) = 0
       AND NOT reload
       AND NOT {schema}.commit_touches_schema(NEW.doc) THEN
        RETURN NULL;                                  -- KV-only commit: nothing for the feed
    END IF;
    INSERT INTO {schema}.changes (seq, n, ts, workspace, kind, op, target, doc)
    VALUES (NEW.seq, 0, NEW.ts, '', '', 'gap', '', jsonb_build_object(
        'cursor', NEW.seq::text || '.0',
        'ts', NEW.ts,
        'op', 'gap',
        'gap', jsonb_build_object(
            'reason', CASE WHEN reload THEN 'bulk_import' ELSE 'uncaptured_writer' END,
            'origin', NEW.origin,
            'entities', coalesce(NEW.doc->'entities', '[]'::jsonb),
            'links', coalesce(NEW.doc->'links', '[]'::jsonb))))
    ON CONFLICT DO NOTHING;
    RETURN NULL;
END $$;
DROP TRIGGER IF EXISTS commits_capture ON {schema}.commits;
CREATE CONSTRAINT TRIGGER commits_capture AFTER INSERT ON {schema}.commits
    DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION {schema}.changes_gap();
