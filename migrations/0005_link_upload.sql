-- A download link can now point at a file the agent staged, which is a fifth
-- kind. `kind` names its kinds in a CHECK constraint, and SQLite cannot widen
-- one in place, so the table is rebuilt the way 0004 rebuilt it: new table,
-- copy, drop, rename.
--
-- The kind exists because the Docs API cannot take image bytes.
-- `insertInlineImage` takes a URI and Google fetches it, so a picture has to
-- be reachable from Google's own servers for the length of the call, and a
-- short-lived download link is exactly that. The bytes stay where every other
-- staged file lives — on disk in this process, never in the database — and
-- the target names the held file rather than carrying it.
--
-- Everything 0001 and 0004 gave the table is written out again — STRICT, the
-- text primary key, both foreign keys with ON DELETE CASCADE, the use
-- counter's check and its default, and the index on expires_at, which the drop
-- takes with it. Nothing references `links`, so nothing else can be left
-- pointing at a table that is gone.
--
-- The audit_log CHECK is deliberately left alone here.

CREATE TABLE links_new (
    id             TEXT NOT NULL PRIMARY KEY,  -- 22 chars, base64url of 16 random bytes
    connection_id  INTEGER NOT NULL REFERENCES connections(id) ON DELETE CASCADE,
    token_id       INTEGER NOT NULL REFERENCES api_tokens(id) ON DELETE CASCADE,
    kind           TEXT NOT NULL CHECK (kind IN ('gmail_attachment', 'drive_download',
                                                 'drive_export', 'docs_image', 'upload')),
    target         TEXT NOT NULL,              -- JSON: message_id+attachment_id | file_id |
                                               -- file_id+mime | doc_id+object_id | upload_id
    filename       TEXT NOT NULL,
    mime_type      TEXT NOT NULL,
    size           INTEGER,
    expires_at     TEXT NOT NULL,
    uses_left      INTEGER NOT NULL DEFAULT 3 CHECK (uses_left >= 0),
    created_at     TEXT NOT NULL
) STRICT;

INSERT INTO links_new (id, connection_id, token_id, kind, target, filename, mime_type, size,
                       expires_at, uses_left, created_at)
SELECT id, connection_id, token_id, kind, target, filename, mime_type, size,
       expires_at, uses_left, created_at
FROM links;

DROP TABLE links;

ALTER TABLE links_new RENAME TO links;

CREATE INDEX links_expires_at ON links (expires_at);
