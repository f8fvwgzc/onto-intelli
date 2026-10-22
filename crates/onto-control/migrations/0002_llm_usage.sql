-- Control store, version 2: each project's own LLM provider (bring your
-- own LLM) and its usage per UTC day. `{schema}` is the quoted control
-- schema. Never edit this file once released; add the next migration.

CREATE TABLE {schema}.project_llm (
    project_id TEXT PRIMARY KEY REFERENCES {schema}.projects (id) ON DELETE CASCADE,
    provider TEXT NOT NULL,                 -- anthropic | openai | ollama | openai-compatible | none
    model TEXT NOT NULL DEFAULT '',
    base_url TEXT,
    api_key_ct BYTEA,                       -- vault envelope (AES-256-GCM), NULL = no key
    api_key_last4 TEXT,
    max_output_tokens INT NOT NULL DEFAULT 4096,
    temperature REAL,
    effort TEXT,
    timeout_secs INT NOT NULL DEFAULT 120,
    pricing JSONB NOT NULL DEFAULT '{}',    -- {input_per_mtok, output_per_mtok, cached_input_per_mtok}
    headers JSONB NOT NULL DEFAULT '{}',    -- non-secret extra headers
    monthly_budget_usd DOUBLE PRECISION,    -- NULL = no budget
    monthly_token_budget BIGINT,
    asks_per_minute INT,                    -- NULL = no limit
    updated_at BIGINT NOT NULL,
    updated_by TEXT NOT NULL
);

-- Usage per project, UTC day and model. Kept 400 days (budgets need the
-- whole month even when traces are kept 30 days).
CREATE TABLE {schema}.usage_daily (
    project_id TEXT NOT NULL REFERENCES {schema}.projects (id) ON DELETE CASCADE,
    day DATE NOT NULL,
    provider TEXT NOT NULL,
    model TEXT NOT NULL,
    asks BIGINT NOT NULL DEFAULT 0,
    llm_calls BIGINT NOT NULL DEFAULT 0,
    deterministic BIGINT NOT NULL DEFAULT 0,
    input_tokens BIGINT NOT NULL DEFAULT 0,
    output_tokens BIGINT NOT NULL DEFAULT 0,
    cached_tokens BIGINT NOT NULL DEFAULT 0,
    cost_usd DOUBLE PRECISION NOT NULL DEFAULT 0,
    errors BIGINT NOT NULL DEFAULT 0,
    PRIMARY KEY (project_id, day, provider, model)
);
