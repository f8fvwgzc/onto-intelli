-- Outbox claims and backlog counts that read what they return, not the
-- whole backlog (review of phase 1 steps 8-12, CP-6). A claim takes due
-- `pending`/`retry` rows in (next_at, id) order and expired `inflight` rows
-- in (lease_until, id) order, each from its own index with its own LIMIT,
-- with the database clock read once; fan-out counts each subscribed
-- endpoint's open rows up to its pending cap from the open-rows index.
-- 0011's indexes stay (older builds use them).

CREATE INDEX IF NOT EXISTS outbox_ready ON {schema}.outbox (next_at, id)
    WHERE state IN ('pending', 'retry');
CREATE INDEX IF NOT EXISTS outbox_leased ON {schema}.outbox (lease_until, id)
    WHERE state = 'inflight';
CREATE INDEX IF NOT EXISTS outbox_open ON {schema}.outbox (target)
    WHERE state IN ('pending', 'retry', 'inflight');
