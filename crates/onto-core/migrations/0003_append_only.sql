-- The audit trail and the history tables are append-only: rows are added,
-- and only retention (vacuum) deletes old ones. Updates are refused.

CREATE OR REPLACE FUNCTION {schema}.refuse_update() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    RAISE EXCEPTION '%.% is append-only', TG_TABLE_SCHEMA, TG_TABLE_NAME;
END
$$;

DROP TRIGGER IF EXISTS events_append_only ON {schema}.events;
CREATE TRIGGER events_append_only BEFORE UPDATE ON {schema}.events
    FOR EACH ROW EXECUTE FUNCTION {schema}.refuse_update();
DROP TRIGGER IF EXISTS entity_versions_append_only ON {schema}.entity_versions;
CREATE TRIGGER entity_versions_append_only BEFORE UPDATE ON {schema}.entity_versions
    FOR EACH ROW EXECUTE FUNCTION {schema}.refuse_update();
DROP TRIGGER IF EXISTS link_versions_append_only ON {schema}.link_versions;
CREATE TRIGGER link_versions_append_only BEFORE UPDATE ON {schema}.link_versions
    FOR EACH ROW EXECUTE FUNCTION {schema}.refuse_update();
