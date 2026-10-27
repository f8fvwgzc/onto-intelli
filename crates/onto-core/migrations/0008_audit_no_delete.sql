-- The audit trail and the history tables refuse DELETE and TRUNCATE as
-- well as UPDATE (0003). Retention is the one exception: `vacuum` sets
-- `onto.retention` to 'on' for its own transaction before it deletes old
-- events. This guards against application bugs and stray statements; a
-- database role that may alter the triggers is not stopped by them.

CREATE OR REPLACE FUNCTION {schema}.refuse_delete() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF TG_OP = 'DELETE' AND current_setting('onto.retention', true) = 'on' THEN
        RETURN OLD;
    END IF;
    RAISE EXCEPTION '%.% is append-only (% refused)', TG_TABLE_SCHEMA, TG_TABLE_NAME, TG_OP;
END
$$;

DROP TRIGGER IF EXISTS events_no_delete ON {schema}.events;
CREATE TRIGGER events_no_delete BEFORE DELETE ON {schema}.events
    FOR EACH ROW EXECUTE FUNCTION {schema}.refuse_delete();
DROP TRIGGER IF EXISTS events_no_truncate ON {schema}.events;
CREATE TRIGGER events_no_truncate BEFORE TRUNCATE ON {schema}.events
    FOR EACH STATEMENT EXECUTE FUNCTION {schema}.refuse_delete();
DROP TRIGGER IF EXISTS entity_versions_no_delete ON {schema}.entity_versions;
CREATE TRIGGER entity_versions_no_delete BEFORE DELETE ON {schema}.entity_versions
    FOR EACH ROW EXECUTE FUNCTION {schema}.refuse_delete();
DROP TRIGGER IF EXISTS entity_versions_no_truncate ON {schema}.entity_versions;
CREATE TRIGGER entity_versions_no_truncate BEFORE TRUNCATE ON {schema}.entity_versions
    FOR EACH STATEMENT EXECUTE FUNCTION {schema}.refuse_delete();
DROP TRIGGER IF EXISTS link_versions_no_delete ON {schema}.link_versions;
CREATE TRIGGER link_versions_no_delete BEFORE DELETE ON {schema}.link_versions
    FOR EACH ROW EXECUTE FUNCTION {schema}.refuse_delete();
DROP TRIGGER IF EXISTS link_versions_no_truncate ON {schema}.link_versions;
CREATE TRIGGER link_versions_no_truncate BEFORE TRUNCATE ON {schema}.link_versions
    FOR EACH STATEMENT EXECUTE FUNCTION {schema}.refuse_delete();
