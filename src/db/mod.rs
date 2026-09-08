//! SQLite access. Every query in the application lives here, so the API, the
//! CLI and the MCP server share one implementation and one set of tests.
//!
//! The `Db` knows nothing about scopes, sealing or HTTP principals: callers
//! pass resolved values, and `Reach` is the only thing it is told about who is
//! asking. Instants are always passed in or taken from `Utc::now()` here;
//! nothing has a SQL default, so a test can place a row wherever it likes.

// The query surface is written once, whole; its callers are the HTTP, MCP and
// CLI layers that come later. Until they land the binary itself uses only a
// few of these (the tests below use them all), and dead-code warnings would
// drown out real ones. Remove this when `http/` exists.
#![allow(dead_code)]

mod rows;
#[cfg(test)]
mod tests;

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use sqlx::SqlitePool;
use sqlx::sqlite::{
    SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteQueryResult,
    SqliteSynchronous,
};

pub use rows::*;

pub static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");

/// Long enough to outlast the one writer's slowest statement, short enough
/// that a wedged process is noticed.
const BUSY_TIMEOUT: Duration = Duration::from_millis(5000);

#[derive(Clone)]
pub struct Db {
    pool: SqlitePool,
}

#[derive(Debug, thiserror::Error)]
pub enum DbError {
    #[error("not found")]
    NotFound,
    #[error("{0}")]
    Conflict(String),
    #[error(transparent)]
    Sqlx(#[from] sqlx::Error),
}

pub type DbResult<T> = std::result::Result<T, DbError>;

fn not_found_or(e: sqlx::Error) -> DbError {
    match e {
        sqlx::Error::RowNotFound => DbError::NotFound,
        other => DbError::Sqlx(other),
    }
}

/// Constraint violations are the caller's mistake, not a database failure:
/// they become conflicts carrying a message a person can act on.
fn conflict_or(e: sqlx::Error, message: impl FnOnce() -> String) -> DbError {
    match &e {
        sqlx::Error::Database(d)
            if d.is_unique_violation()
                || d.is_check_violation()
                || d.is_foreign_key_violation() =>
        {
            DbError::Conflict(message())
        }
        _ => DbError::Sqlx(e),
    }
}

fn affected(res: SqliteQueryResult) -> DbResult<()> {
    if res.rows_affected() == 0 {
        Err(DbError::NotFound)
    } else {
        Ok(())
    }
}

/// The pragmas every connection gets, whatever the database is. `journal_mode`
/// is not among them: WAL is a property of a file, and an in-memory database
/// has no journal to speak of.
fn pragmas(options: SqliteConnectOptions) -> SqliteConnectOptions {
    options
        .busy_timeout(BUSY_TIMEOUT)
        .foreign_keys(true)
        .synchronous(SqliteSynchronous::Normal)
}

/// Newest first, then by id: the one ordering the log is ever read in.
const AUDIT_ORDER: &str = "ORDER BY at DESC, id DESC";

impl Db {
    /// The database at `path`, created along with its directory if either is
    /// missing. Migrations are not applied here; `serve` and `migrate` say
    /// when that happens.
    pub async fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        let options = pragmas(
            SqliteConnectOptions::new()
                .filename(path)
                .create_if_missing(true),
        )
        .journal_mode(SqliteJournalMode::Wal);
        let pool = SqlitePoolOptions::new()
            .max_connections(5)
            .connect_with(options)
            .await
            .with_context(|| format!("opening {}", path.display()))?;
        Ok(Self { pool })
    }

    /// A fresh, migrated database that never touches the disk: what every
    /// test runs against.
    ///
    /// An in-memory database belongs to the connection that created it, so the
    /// pool is pinned to exactly one connection which is never recycled — a
    /// second connection would be a second, empty database, and a reaped one
    /// would take the schema with it. The cost is that queries serialise,
    /// which is why nothing here holds a transaction open across a call that
    /// wants a connection of its own.
    pub async fn open_memory() -> Result<Self> {
        let options = pragmas(SqliteConnectOptions::new().in_memory(true));
        let pool = SqlitePoolOptions::new()
            .min_connections(1)
            .max_connections(1)
            .idle_timeout(None)
            .max_lifetime(None)
            .connect_with(options)
            .await
            .context("opening an in-memory database")?;
        let db = Self { pool };
        db.migrate().await?;
        Ok(db)
    }

    pub fn from_pool(pool: SqlitePool) -> Self {
        Self { pool }
    }

    /// For tests that need to age or corrupt rows behind the queries.
    #[cfg(test)]
    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    pub async fn migrate(&self) -> Result<()> {
        MIGRATOR
            .run(&self.pool)
            .await
            .context("running migrations")?;
        Ok(())
    }

    /// (version, description, applied) for every known migration.
    pub async fn migration_status(&self) -> Result<Vec<(i64, String, bool)>> {
        let applied: Vec<(i64,)> =
            sqlx::query_as("SELECT version FROM _sqlx_migrations ORDER BY version")
                .fetch_all(&self.pool)
                .await
                .unwrap_or_default();
        Ok(MIGRATOR
            .iter()
            .map(|m| {
                let done = applied.iter().any(|(v,)| *v == m.version);
                (m.version, m.description.to_string(), done)
            })
            .collect())
    }

    // ----- users -----------------------------------------------------------

    /// Login: the row is keyed by the OIDC subject, and email and name are
    /// refreshed from the token when the provider offers them.
    pub async fn upsert_user(
        &self,
        subject: &str,
        email: Option<&str>,
        name: Option<&str>,
    ) -> DbResult<User> {
        let now = Utc::now();
        sqlx::query_as(
            "INSERT INTO users (subject, email, name, created_at, last_login_at) \
             VALUES (?, ?, ?, ?, ?) \
             ON CONFLICT (subject) DO UPDATE SET \
               email = COALESCE(excluded.email, users.email), \
               name = COALESCE(excluded.name, users.name), \
               last_login_at = excluded.last_login_at \
             RETURNING *",
        )
        .bind(subject.trim())
        .bind(email.map(str::trim))
        .bind(name.map(str::trim))
        .bind(now)
        .bind(now)
        .fetch_one(&self.pool)
        .await
        .map_err(|e| conflict_or(e, || "that email already belongs to another account".into()))
    }

    pub async fn get_user(&self, id: i64) -> DbResult<User> {
        sqlx::query_as("SELECT * FROM users WHERE id = ?")
            .bind(id)
            .fetch_one(&self.pool)
            .await
            .map_err(not_found_or)
    }

    /// What `X-Gmcp-User` resolves against.
    pub async fn find_user_by_email(&self, email: &str) -> DbResult<Option<User>> {
        Ok(
            sqlx::query_as("SELECT * FROM users WHERE lower(email) = lower(?)")
                .bind(email.trim())
                .fetch_optional(&self.pool)
                .await?,
        )
    }

    pub async fn list_users(&self) -> DbResult<Vec<User>> {
        Ok(sqlx::query_as("SELECT * FROM users ORDER BY id")
            .fetch_all(&self.pool)
            .await?)
    }

    /// The browser login the delegate rule counts from.
    pub async fn touch_last_login(&self, id: i64) -> DbResult<()> {
        affected(
            sqlx::query("UPDATE users SET last_login_at = ? WHERE id = ?")
                .bind(Utc::now())
                .bind(id)
                .execute(&self.pool)
                .await?,
        )
    }

    // ----- connections -----------------------------------------------------

    /// A grant that has just come back from the Google callback. It starts
    /// healthy; only a refusal from Google moves it.
    pub async fn create_connection(&self, c: NewConnection) -> DbResult<Connection> {
        let now = Utc::now();
        sqlx::query_as(
            "INSERT INTO connections (user_id, label, google_email, services, granted_scopes, \
               refresh_token_sealed, status, delegate_ok, created_at, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?) RETURNING *",
        )
        .bind(c.user_id)
        .bind(c.label.trim())
        .bind(c.google_email.trim())
        .bind(sqlx::types::Json(&c.services))
        .bind(sqlx::types::Json(&c.granted_scopes))
        .bind(&c.refresh_token_sealed)
        .bind(ConnectionStatus::Ok)
        .bind(c.delegate_ok)
        .bind(now)
        .bind(now)
        .fetch_one(&self.pool)
        .await
        .map_err(|e| {
            conflict_or(e, || {
                format!(
                    "a connection labelled {:?} or for {} already exists",
                    c.label.trim(),
                    c.google_email.trim()
                )
            })
        })
    }

    pub async fn get_connection(&self, id: i64) -> DbResult<Connection> {
        sqlx::query_as("SELECT * FROM connections WHERE id = ?")
            .bind(id)
            .fetch_one(&self.pool)
            .await
            .map_err(not_found_or)
    }

    /// Every connection a person has, healthy or not; the portal shows them
    /// all, which is how a revoked one gets noticed.
    pub async fn list_connections(&self, user_id: i64) -> DbResult<Vec<Connection>> {
        Ok(
            sqlx::query_as("SELECT * FROM connections WHERE user_id = ? ORDER BY label")
                .bind(user_id)
                .fetch_all(&self.pool)
                .await?,
        )
    }

    /// By the label a tool's `account` argument names.
    pub async fn find_connection(&self, user_id: i64, label: &str) -> DbResult<Option<Connection>> {
        Ok(sqlx::query_as(
            "SELECT * FROM connections WHERE user_id = ? AND lower(label) = lower(?)",
        )
        .bind(user_id)
        .bind(label.trim())
        .fetch_optional(&self.pool)
        .await?)
    }

    /// Rename, or flag for the gateway. Everything else about a connection
    /// comes from Google.
    pub async fn update_connection(&self, id: i64, p: ConnectionPatch) -> DbResult<Connection> {
        let label = p.label.as_deref().map(str::trim);
        sqlx::query_as(
            "UPDATE connections SET label = COALESCE(?, label), \
               delegate_ok = COALESCE(?, delegate_ok), updated_at = ? \
             WHERE id = ? RETURNING *",
        )
        .bind(label)
        .bind(p.delegate_ok)
        .bind(Utc::now())
        .bind(id)
        .fetch_one(&self.pool)
        .await
        .map_err(|e| match e {
            sqlx::Error::RowNotFound => DbError::NotFound,
            e => conflict_or(e, || {
                format!(
                    "a connection labelled {:?} already exists",
                    label.unwrap_or_default()
                )
            }),
        })
    }

    /// What a Google refusal leaves behind: `needs_reauth` with the message
    /// the UI shows, or `ok` again once a refresh works.
    pub async fn set_connection_status(
        &self,
        id: i64,
        status: ConnectionStatus,
        detail: Option<&str>,
    ) -> DbResult<Connection> {
        sqlx::query_as(
            "UPDATE connections SET status = ?, status_detail = ?, updated_at = ? \
             WHERE id = ? RETURNING *",
        )
        .bind(status)
        .bind(detail)
        .bind(Utc::now())
        .bind(id)
        .fetch_one(&self.pool)
        .await
        .map_err(not_found_or)
    }

    /// A reconnect through the portal: a new refresh token, whatever Google
    /// granted this time, and a clean bill of health.
    pub async fn set_connection_grant(
        &self,
        id: i64,
        services: &[String],
        granted_scopes: &[String],
        refresh_token_sealed: &[u8],
    ) -> DbResult<Connection> {
        sqlx::query_as(
            "UPDATE connections SET services = ?, granted_scopes = ?, refresh_token_sealed = ?, \
               status = ?, status_detail = NULL, updated_at = ? \
             WHERE id = ? RETURNING *",
        )
        .bind(sqlx::types::Json(services))
        .bind(sqlx::types::Json(granted_scopes))
        .bind(refresh_token_sealed)
        .bind(ConnectionStatus::Ok)
        .bind(Utc::now())
        .bind(id)
        .fetch_one(&self.pool)
        .await
        .map_err(not_found_or)
    }

    pub async fn touch_connection_used(&self, id: i64) -> DbResult<()> {
        affected(
            sqlx::query("UPDATE connections SET last_used_at = ? WHERE id = ?")
                .bind(Utc::now())
                .bind(id)
                .execute(&self.pool)
                .await?,
        )
    }

    /// Removing a connection takes its allowlist entries and its outstanding
    /// download links with it; the audit rows stay, with a NULL reference.
    pub async fn delete_connection(&self, id: i64) -> DbResult<()> {
        affected(
            sqlx::query("DELETE FROM connections WHERE id = ?")
                .bind(id)
                .execute(&self.pool)
                .await?,
        )
    }

    // ----- api tokens ------------------------------------------------------

    pub async fn create_token(&self, t: NewToken) -> DbResult<ApiToken> {
        sqlx::query_as(
            "INSERT INTO api_tokens (name, token_hash, scopes, client, user_id, all_connections, \
               created_by, created_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?) RETURNING *",
        )
        .bind(t.name.trim())
        .bind(&t.token_hash)
        .bind(sqlx::types::Json(&t.scopes))
        .bind(t.client)
        .bind(t.user_id)
        .bind(t.all_connections)
        .bind(t.created_by)
        .bind(Utc::now())
        .fetch_one(&self.pool)
        .await
        .map_err(|e| {
            conflict_or(e, || {
                "a delegate token has no user; every other token needs one".into()
            })
        })
    }

    pub async fn get_token(&self, id: i64) -> DbResult<ApiToken> {
        sqlx::query_as("SELECT * FROM api_tokens WHERE id = ?")
            .bind(id)
            .fetch_one(&self.pool)
            .await
            .map_err(not_found_or)
    }

    /// What a bearer header resolves to. Revoked tokens are not tokens.
    pub async fn find_active_token(&self, token_hash: &str) -> DbResult<Option<ApiToken>> {
        Ok(
            sqlx::query_as("SELECT * FROM api_tokens WHERE token_hash = ? AND revoked_at IS NULL")
                .bind(token_hash)
                .fetch_optional(&self.pool)
                .await?,
        )
    }

    /// The tokens of one person, or every token when `user_id` is `None`
    /// (which is how the CLI lists them, delegate tokens included).
    pub async fn list_tokens(&self, user_id: Option<i64>) -> DbResult<Vec<ApiToken>> {
        Ok(sqlx::query_as(
            "SELECT * FROM api_tokens WHERE (?1 IS NULL OR user_id = ?1) \
             ORDER BY created_at, id",
        )
        .bind(user_id)
        .fetch_all(&self.pool)
        .await?)
    }

    /// Revoking is final and idempotent only once: a second call is a
    /// `NotFound`, so the caller can say the token was already gone.
    pub async fn revoke_token(&self, id: i64) -> DbResult<()> {
        affected(
            sqlx::query("UPDATE api_tokens SET revoked_at = ? WHERE id = ? AND revoked_at IS NULL")
                .bind(Utc::now())
                .bind(id)
                .execute(&self.pool)
                .await?,
        )
    }

    /// Bumped at most once a minute: the column is for "when was this last
    /// used", not a hit counter.
    pub async fn touch_token_used(&self, token: &ApiToken) -> DbResult<()> {
        let stale = token
            .last_used_at
            .is_none_or(|u| Utc::now() - u > chrono::Duration::minutes(1));
        if stale {
            sqlx::query("UPDATE api_tokens SET last_used_at = ? WHERE id = ?")
                .bind(Utc::now())
                .bind(token.id)
                .execute(&self.pool)
                .await?;
        }
        Ok(())
    }

    /// Replace a personal token's allowlist. Ignored while
    /// `all_connections` is set, but kept, so turning that off restores it.
    pub async fn set_token_connections(
        &self,
        token_id: i64,
        connection_ids: &[i64],
    ) -> DbResult<()> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("DELETE FROM token_connections WHERE token_id = ?")
            .bind(token_id)
            .execute(&mut *tx)
            .await?;
        for id in connection_ids {
            sqlx::query(
                "INSERT INTO token_connections (token_id, connection_id) VALUES (?, ?) \
                 ON CONFLICT DO NOTHING",
            )
            .bind(token_id)
            .bind(id)
            .execute(&mut *tx)
            .await
            .map_err(|e| conflict_or(e, || format!("no connection with id {id}")))?;
        }
        tx.commit().await?;
        Ok(())
    }

    pub async fn token_connections(&self, token_id: i64) -> DbResult<Vec<i64>> {
        Ok(sqlx::query_scalar(
            "SELECT connection_id FROM token_connections WHERE token_id = ? \
             ORDER BY connection_id",
        )
        .bind(token_id)
        .fetch_all(&self.pool)
        .await?)
    }

    // ----- what a principal can see ----------------------------------------

    /// The connections a principal may name: a session sees its own, a
    /// personal token its allowlist (or all of its user's), a delegate token
    /// the acting person's gateway connections. A `revoked` connection is not
    /// listed at all; a `needs_reauth` one is, so the model can say what to do
    /// about it.
    pub async fn visible_connections(&self, reach: Reach) -> DbResult<Vec<Connection>> {
        let sql = format!(
            "SELECT * FROM connections WHERE user_id = ?1 \
             AND status IN ('ok', 'needs_reauth'){} ORDER BY label",
            match reach {
                Reach::Session { .. } => "",
                Reach::Token { .. } =>
                    " AND (?2 = 1 OR EXISTS (SELECT 1 FROM token_connections t \
                     WHERE t.token_id = ?3 AND t.connection_id = connections.id))",
                Reach::Delegate { .. } => " AND delegate_ok = 1",
            }
        );
        let mut q = sqlx::query_as(&sql).bind(reach.user_id());
        if let Reach::Token {
            token_id,
            all_connections,
            ..
        } = reach
        {
            q = q.bind(all_connections).bind(token_id);
        }
        Ok(q.fetch_all(&self.pool).await?)
    }

    /// The connection a tool's `account` argument names, resolved against
    /// what the principal may see. A person has a handful of connections, so
    /// this filters the visible list rather than growing a second query.
    pub async fn find_visible_connection(
        &self,
        reach: Reach,
        label: &str,
    ) -> DbResult<Option<Connection>> {
        let label = label.trim();
        Ok(self
            .visible_connections(reach)
            .await?
            .into_iter()
            .find(|c| c.label.eq_ignore_ascii_case(label)))
    }

    // ----- download links --------------------------------------------------

    pub async fn create_link(&self, l: NewLink) -> DbResult<Link> {
        sqlx::query_as(
            "INSERT INTO links (id, connection_id, token_id, kind, target, filename, mime_type, \
               size, expires_at, uses_left, created_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?) RETURNING *",
        )
        .bind(&l.id)
        .bind(l.connection_id)
        .bind(l.token_id)
        .bind(l.kind)
        .bind(sqlx::types::Json(&l.target))
        .bind(&l.filename)
        .bind(&l.mime_type)
        .bind(l.size)
        .bind(l.expires_at)
        .bind(l.uses_left)
        .bind(Utc::now())
        .fetch_one(&self.pool)
        .await
        .map_err(|e| conflict_or(e, || format!("link {} already exists", l.id)))
    }

    /// Spend one use of a link. The check and the decrement are one statement,
    /// so two simultaneous hits on a link with one use left cannot both win;
    /// the second lookup only decides which of the three refusals to report.
    pub async fn take_link(
        &self,
        id: &str,
        now: DateTime<Utc>,
    ) -> DbResult<std::result::Result<Link, LinkRefusal>> {
        let taken: Option<Link> = sqlx::query_as(
            "UPDATE links SET uses_left = uses_left - 1 \
             WHERE id = ? AND expires_at > ? AND uses_left > 0 RETURNING *",
        )
        .bind(id)
        .bind(now)
        .fetch_optional(&self.pool)
        .await?;
        if let Some(link) = taken {
            return Ok(Ok(link));
        }
        let existing: Option<Link> = sqlx::query_as("SELECT * FROM links WHERE id = ?")
            .bind(id)
            .fetch_optional(&self.pool)
            .await?;
        Ok(Err(match existing {
            None => LinkRefusal::Unknown,
            Some(l) if l.expires_at <= now => LinkRefusal::Expired,
            Some(_) => LinkRefusal::Exhausted,
        }))
    }

    /// Expired rows are deleted by `prune` and opportunistically on mint.
    pub async fn delete_expired_links(&self, now: DateTime<Utc>) -> DbResult<u64> {
        Ok(sqlx::query("DELETE FROM links WHERE expires_at <= ?")
            .bind(now)
            .execute(&self.pool)
            .await?
            .rows_affected())
    }

    // ----- audit log -------------------------------------------------------

    pub async fn insert_audit(&self, e: NewAuditEntry) -> DbResult<AuditEntry> {
        Ok(sqlx::query_as(
            "INSERT INTO audit_log (at, kind, user_id, token_id, connection_id, tool, args, \
               outcome, detail, duration_ms, ip) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?) RETURNING *",
        )
        .bind(e.at)
        .bind(e.kind)
        .bind(e.user_id)
        .bind(e.token_id)
        .bind(e.connection_id)
        .bind(e.tool.as_deref())
        .bind(e.args.map(sqlx::types::Json))
        .bind(e.outcome)
        .bind(e.detail.as_deref())
        .bind(e.duration_ms)
        .bind(e.ip.as_deref())
        .fetch_one(&self.pool)
        .await?)
    }

    /// One page of the log, newest first. `before` continues where the
    /// previous page ended; the comparison is on `(at, id)` because two rows
    /// can share an instant.
    pub async fn list_audit(&self, f: AuditFilter) -> DbResult<Vec<AuditEntry>> {
        let limit = f.limit.unwrap_or(50).clamp(1, 500);
        Ok(sqlx::query_as(&format!(
            "SELECT * FROM audit_log WHERE \
               (?1 IS NULL OR user_id = ?1) AND \
               (?2 IS NULL OR token_id = ?2) AND \
               (?3 IS NULL OR connection_id = ?3) AND \
               (?4 IS NULL OR kind = ?4) AND \
               (?5 IS NULL OR tool = ?5) AND \
               (?6 IS NULL OR at >= ?6) AND \
               (?7 IS NULL OR at <= ?7) AND \
               (?8 IS NULL OR (at, id) < (?8, ?9)) \
             {AUDIT_ORDER} LIMIT ?10"
        ))
        .bind(f.user_id)
        .bind(f.token_id)
        .bind(f.connection_id)
        .bind(f.kind)
        .bind(f.tool.as_deref())
        .bind(f.from)
        .bind(f.to)
        .bind(f.before.map(|c| c.at))
        .bind(f.before.map(|c| c.id))
        .bind(limit)
        .fetch_all(&self.pool)
        .await?)
    }

    pub async fn delete_audit_before(&self, cutoff: DateTime<Utc>) -> DbResult<u64> {
        Ok(sqlx::query("DELETE FROM audit_log WHERE at < ?")
            .bind(cutoff)
            .execute(&self.pool)
            .await?
            .rows_affected())
    }

    /// What `gmcp prune` does: throw away links nobody can use any more and
    /// log rows older than the retention. Nothing else is ever deleted.
    pub async fn prune(
        &self,
        now: DateTime<Utc>,
        audit_retention: chrono::Duration,
    ) -> DbResult<Pruned> {
        Ok(Pruned {
            links: self.delete_expired_links(now).await?,
            audit: self.delete_audit_before(now - audit_retention).await?,
        })
    }
}
