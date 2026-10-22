-- Control store, version 1: users, identities, organizations, members,
-- invitations, projects, API keys, pending OIDC logins, login tickets and
-- the append-only audit log. `{schema}` is the quoted control schema.
-- Never edit this file once released; add the next migration.

CREATE TABLE {schema}.users (
    id TEXT PRIMARY KEY,                    -- usr_…
    email TEXT,                             -- from the IdP; may be NULL
    email_verified BOOLEAN NOT NULL DEFAULT false,
    name TEXT NOT NULL DEFAULT '',
    avatar_url TEXT,
    created_at BIGINT NOT NULL,
    last_login_at BIGINT,
    -- Sessions issued before this are refused (sign out everywhere).
    sessions_valid_after BIGINT NOT NULL DEFAULT 0,
    disabled BOOLEAN NOT NULL DEFAULT false
);
CREATE INDEX users_email ON {schema}.users (lower(email));

CREATE TABLE {schema}.identities (
    provider TEXT NOT NULL,                 -- console.oidc id, or 'local' (bootstrap), 'dev'
    subject TEXT NOT NULL,                  -- the IdP's `sub`
    user_id TEXT NOT NULL REFERENCES {schema}.users (id) ON DELETE CASCADE,
    email TEXT,
    claims JSONB NOT NULL DEFAULT '{}',     -- name, picture, email_verified at last login
    created_at BIGINT NOT NULL,
    last_login_at BIGINT NOT NULL,
    PRIMARY KEY (provider, subject)
);
CREATE INDEX identities_user ON {schema}.identities (user_id);

CREATE TABLE {schema}.orgs (
    id TEXT PRIMARY KEY,                    -- org_…
    slug TEXT NOT NULL UNIQUE,              -- [a-z0-9-], 3..40
    name TEXT NOT NULL,
    created_at BIGINT NOT NULL,
    created_by TEXT NOT NULL
);

CREATE TABLE {schema}.members (
    org_id TEXT NOT NULL REFERENCES {schema}.orgs (id) ON DELETE CASCADE,
    user_id TEXT NOT NULL REFERENCES {schema}.users (id) ON DELETE CASCADE,
    role TEXT NOT NULL CHECK (role IN ('owner', 'admin', 'developer', 'viewer')),
    created_at BIGINT NOT NULL,
    PRIMARY KEY (org_id, user_id)
);
CREATE INDEX members_user ON {schema}.members (user_id);

CREATE TABLE {schema}.invitations (
    id TEXT PRIMARY KEY,                    -- inv_…
    org_id TEXT NOT NULL REFERENCES {schema}.orgs (id) ON DELETE CASCADE,
    email TEXT NOT NULL,                    -- matched against a verified email
    role TEXT NOT NULL CHECK (role IN ('admin', 'developer', 'viewer')),
    code_hash TEXT NOT NULL UNIQUE,         -- SHA-256 of the invitation code in the link
    invited_by TEXT NOT NULL,
    created_at BIGINT NOT NULL,
    expires_at BIGINT NOT NULL,             -- 7 days
    accepted_at BIGINT,
    accepted_by TEXT
);

CREATE TABLE {schema}.projects (
    id TEXT PRIMARY KEY,                    -- prj_…
    org_id TEXT NOT NULL REFERENCES {schema}.orgs (id),
    ref TEXT NOT NULL UNIQUE,               -- the Hub tenant id
    name TEXT NOT NULL,
    status TEXT NOT NULL DEFAULT 'active' CHECK (status IN ('active', 'deleted')),
    -- {default_workspace, trace_retention_days, capture_content, asks_per_minute, …}
    settings JSONB NOT NULL DEFAULT '{}',
    settings_version BIGINT NOT NULL DEFAULT 1,   -- bumped on settings or LLM changes
    created_at BIGINT NOT NULL,
    created_by TEXT NOT NULL,
    deleted_at BIGINT
);
CREATE INDEX projects_org ON {schema}.projects (org_id);

CREATE TABLE {schema}.api_keys (
    id TEXT PRIMARY KEY,                    -- key_…
    project_id TEXT NOT NULL REFERENCES {schema}.projects (id) ON DELETE CASCADE,
    name TEXT NOT NULL,
    prefix TEXT NOT NULL UNIQUE,            -- onto_sk_ + 8 characters: shown in lists and logs
    hash TEXT NOT NULL,                     -- hex SHA-256(pepper || full key)
    scopes TEXT[] NOT NULL,                 -- read | write | agent | admin
    workspaces TEXT[] NOT NULL DEFAULT '{*}',
    impersonate BOOLEAN NOT NULL DEFAULT false,
    impersonate_roles TEXT[] NOT NULL DEFAULT '{}',
    created_at BIGINT NOT NULL,
    created_by TEXT NOT NULL,
    expires_at BIGINT,
    revoked_at BIGINT,
    revoked_by TEXT,
    last_used_at BIGINT,
    rotated_from TEXT                       -- the key this one replaced
);
CREATE INDEX api_keys_project ON {schema}.api_keys (project_id);

-- Pending OIDC logins (state → verifier, nonce). Single use, 10 minutes.
CREATE TABLE {schema}.oidc_logins (
    state_hash TEXT PRIMARY KEY,
    provider TEXT NOT NULL,
    nonce TEXT NOT NULL,
    code_verifier TEXT NOT NULL,
    binding_hash TEXT NOT NULL,
    invite_code_hash TEXT,
    return_to TEXT NOT NULL,
    created_at BIGINT NOT NULL,
    expires_at BIGINT NOT NULL
);

-- One-time tickets the callback hands to the console. Single use, 60 seconds.
CREATE TABLE {schema}.login_tickets (
    ticket_hash TEXT PRIMARY KEY,
    user_id TEXT NOT NULL REFERENCES {schema}.users (id) ON DELETE CASCADE,
    binding_hash TEXT NOT NULL,
    expires_at BIGINT NOT NULL
);

-- Who changed what in the control plane. Append-only.
CREATE TABLE {schema}.audit_log (
    seq BIGSERIAL PRIMARY KEY,
    ts BIGINT NOT NULL,
    org_id TEXT,
    project_id TEXT,
    actor TEXT NOT NULL,                    -- user id, or 'system'
    action TEXT NOT NULL,                   -- key.create, key.revoke, member.role, llm.update, …
    target TEXT,
    detail JSONB NOT NULL DEFAULT '{}'      -- never secrets
);
CREATE INDEX audit_org_ts ON {schema}.audit_log (org_id, ts);
CREATE OR REPLACE FUNCTION {schema}.refuse_update() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN RAISE EXCEPTION '%.% is append-only', TG_TABLE_SCHEMA, TG_TABLE_NAME; END $$;
CREATE TRIGGER audit_append_only BEFORE UPDATE ON {schema}.audit_log
    FOR EACH ROW EXECUTE FUNCTION {schema}.refuse_update();
