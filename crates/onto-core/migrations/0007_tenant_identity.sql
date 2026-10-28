-- The tenant id this schema belongs to. Tenant ids that differ only in case
-- or in '-', '.' and '_' map to one schema name: the first id to open the
-- schema owns it, and every other id is refused (no tenant reads another's
-- data through a look-alike id).

CREATE TABLE IF NOT EXISTS {schema}.tenant_identity (
    one BOOLEAN PRIMARY KEY DEFAULT true CHECK (one),
    tenant TEXT NOT NULL,
    claimed_at BIGINT NOT NULL
);
