-- Counters for system.stats, system.health and GET /v1/admin/tenants
-- (docs/design/EVENTS.md section 11) that read only what they count.
--
-- Gap records over 24 hours and the latest gap read the gap records alone
-- (a few per tenant) instead of every change record in the window.
CREATE INDEX IF NOT EXISTS changes_gaps ON {schema}.changes (ts) WHERE op = 'gap';

-- Outbox rows per target and state for the states stats report (pending,
-- inflight, retry, dead): delivered and cancelled rows, most of the table,
-- are never read, and each (target, state) count is an index range.
CREATE INDEX IF NOT EXISTS outbox_stats ON {schema}.outbox (target, state)
    WHERE state IN ('pending', 'inflight', 'retry', 'dead');
