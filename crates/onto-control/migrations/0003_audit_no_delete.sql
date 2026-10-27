-- Control store, version 3: the audit log refuses DELETE and TRUNCATE as
-- well as UPDATE (0001). It has no retention. `{schema}` is the quoted
-- control schema. Never edit this file once released; add the next migration.

CREATE OR REPLACE FUNCTION {schema}.refuse_delete() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN RAISE EXCEPTION '%.% is append-only (% refused)', TG_TABLE_SCHEMA, TG_TABLE_NAME, TG_OP; END $$;
CREATE TRIGGER audit_no_delete BEFORE DELETE ON {schema}.audit_log
    FOR EACH ROW EXECUTE FUNCTION {schema}.refuse_delete();
CREATE TRIGGER audit_no_truncate BEFORE TRUNCATE ON {schema}.audit_log
    FOR EACH STATEMENT EXECUTE FUNCTION {schema}.refuse_delete();
