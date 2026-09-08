-- Initial schema. See CLAUDE.md for what the tables are for.
--
-- Conventions, enforced by STRICT on every table:
--   * instants are RFC 3339 UTC TEXT, written by the application (never a SQL
--     default), so the text sorts chronologically and `(at, id)` paging can
--     compare strings;
--   * lists are JSON TEXT arrays;
--   * booleans are 0/1 INTEGER;
--   * sealed secrets are BLOB.
-- Migrations are never edited after they are committed.

CREATE TABLE users (
    id             INTEGER PRIMARY KEY AUTOINCREMENT,
    subject        TEXT NOT NULL UNIQUE,    -- OIDC sub
    email          TEXT UNIQUE,             -- what X-Gmcp-User names
    name           TEXT,
    created_at     TEXT NOT NULL,
    last_login_at  TEXT                     -- the delegate rule reads this
) STRICT;

-- One Google account grant. `label` is what tools call `account`.
CREATE TABLE connections (
    id                    INTEGER PRIMARY KEY AUTOINCREMENT,
    user_id               INTEGER NOT NULL REFERENCES users(id),
    label                 TEXT NOT NULL,
    google_email          TEXT NOT NULL,    -- from userinfo at connect time
    services              TEXT NOT NULL,    -- JSON array, subset of the registry
    granted_scopes        TEXT NOT NULL,    -- JSON array, what Google actually granted
    refresh_token_sealed  BLOB NOT NULL,    -- AES-256-GCM, nonce || ciphertext
    status                TEXT NOT NULL CHECK (status IN ('ok', 'needs_reauth', 'revoked')),
    status_detail         TEXT,             -- last Google error, for the UI
    delegate_ok           INTEGER NOT NULL DEFAULT 0 CHECK (delegate_ok IN (0, 1)),
    created_at            TEXT NOT NULL,
    updated_at            TEXT NOT NULL,
    last_used_at          TEXT,
    UNIQUE (user_id, label),
    UNIQUE (user_id, google_email)
) STRICT;

-- A personal token acts as its user; a delegate token belongs to nobody and
-- names the acting person in X-Gmcp-User on every request.
CREATE TABLE api_tokens (
    id               INTEGER PRIMARY KEY AUTOINCREMENT,
    name             TEXT NOT NULL,
    token_hash       TEXT NOT NULL UNIQUE,  -- sha256 of the secret, prefix gg_
    scopes           TEXT NOT NULL,         -- JSON array of service:level, plus 'delegate'
    client           TEXT NOT NULL CHECK (client IN ('generic', 'openwebui', 'claude-code', 'opencode')),
    user_id          INTEGER REFERENCES users(id),
    all_connections  INTEGER NOT NULL DEFAULT 0 CHECK (all_connections IN (0, 1)),
    created_by       INTEGER NOT NULL REFERENCES users(id),
    created_at       TEXT NOT NULL,
    last_used_at     TEXT,
    revoked_at       TEXT,
    -- `scopes` is a JSON array of strings, so the element "delegate" is
    -- exactly the substring '"delegate"'. A delegate token has no user; every
    -- other token needs one.
    CHECK ((user_id IS NULL) = (scopes LIKE '%"delegate"%'))
) STRICT;

-- The allowlist of a personal token with all_connections = 0.
CREATE TABLE token_connections (
    token_id       INTEGER NOT NULL REFERENCES api_tokens(id) ON DELETE CASCADE,
    connection_id  INTEGER NOT NULL REFERENCES connections(id) ON DELETE CASCADE,
    PRIMARY KEY (token_id, connection_id)
) STRICT;

-- Short-lived download capabilities. A link is worthless once its connection
-- is gone, so it follows it out.
CREATE TABLE links (
    id             TEXT NOT NULL PRIMARY KEY,  -- 22 chars, base64url of 16 random bytes
    connection_id  INTEGER NOT NULL REFERENCES connections(id) ON DELETE CASCADE,
    token_id       INTEGER NOT NULL REFERENCES api_tokens(id) ON DELETE CASCADE,
    kind           TEXT NOT NULL CHECK (kind IN ('gmail_attachment', 'drive_download', 'drive_export')),
    target         TEXT NOT NULL,              -- JSON: message_id+attachment_id | file_id | file_id+mime
    filename       TEXT NOT NULL,
    mime_type      TEXT NOT NULL,
    size           INTEGER,
    expires_at     TEXT NOT NULL,
    uses_left      INTEGER NOT NULL DEFAULT 3 CHECK (uses_left >= 0),
    created_at     TEXT NOT NULL
) STRICT;
CREATE INDEX links_expires_at ON links (expires_at);

-- Why the model drafted that. The log outlives what it points at, so a
-- removed user or connection leaves its rows behind with a NULL reference.
CREATE TABLE audit_log (
    id             INTEGER PRIMARY KEY AUTOINCREMENT,
    at             TEXT NOT NULL,
    kind           TEXT NOT NULL CHECK (kind IN ('tool_call', 'link_created', 'link_used',
                                                 'link_refused', 'connect', 'reconnect',
                                                 'connection_removed', 'token_created',
                                                 'token_revoked')),
    user_id        INTEGER REFERENCES users(id) ON DELETE SET NULL,
    token_id       INTEGER REFERENCES api_tokens(id) ON DELETE SET NULL,
    connection_id  INTEGER REFERENCES connections(id) ON DELETE SET NULL,
    tool           TEXT,
    args           TEXT,                       -- JSON, secrets stripped, <= 4 KB
    outcome        TEXT NOT NULL CHECK (outcome IN ('ok', 'error', 'forbidden')),
    detail         TEXT,                       -- error message, link id, filename
    duration_ms    INTEGER,
    ip             TEXT
) STRICT;
CREATE INDEX audit_log_at ON audit_log (at);
CREATE INDEX audit_log_user_at ON audit_log (user_id, at);
CREATE INDEX audit_log_connection_at ON audit_log (connection_id, at);
