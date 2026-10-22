-- The OpenTelemetry trace a call was part of. A `traceparent` header names
-- the caller's trace and span; several calls under one upstream trace each
-- get a trace of their own (a fresh id), and keep the upstream ids here so
-- they can be joined to OpenTelemetry.

ALTER TABLE {schema}.traces ADD COLUMN IF NOT EXISTS otel_trace_id TEXT;  -- 32 hex, from traceparent
ALTER TABLE {schema}.traces ADD COLUMN IF NOT EXISTS parent_span TEXT;    -- 16 hex, from traceparent
CREATE INDEX IF NOT EXISTS traces_otel ON {schema}.traces (otel_trace_id, ts) WHERE otel_trace_id IS NOT NULL;
