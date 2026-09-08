//! The terminal commands. Each one opens `GMCP_DATABASE` directly, applies
//! the same rules the API applies, and prints what it did.
//!
//! Nothing here reads the environment: `main` passes what it read, so a test
//! can run a subcommand against an in-memory database without an ambient
//! variable changing what comes out.

pub mod check_secret;
pub mod connection;
pub mod migrate;
pub mod prune;
pub mod token;
pub mod user;

#[cfg(test)]
mod tests;

use anyhow::{Result, anyhow};
use chrono::{DateTime, Utc};

use crate::db::{Connection, Db, DbError, DbResult, User};

/// A row that is not there is a mistyped id, not a failure of the database:
/// `not found` on its own is no help at a terminal, so this says which id of
/// what was looked for.
pub fn found<T>(what: &str, id: i64, result: DbResult<T>) -> Result<T> {
    result.map_err(|e| match e {
        DbError::NotFound => anyhow!("no {what} with id {id}"),
        other => other.into(),
    })
}

/// The person an `--user EMAIL` names. Only someone who has logged in through
/// the browser exists: the portal has no other way to make a user, and a
/// token for a person who is not there would act as nobody.
pub async fn person(db: &Db, email: &str) -> Result<User> {
    db.find_user_by_email(email)
        .await?
        .ok_or_else(|| anyhow!("{email} has never logged in"))
}

/// Every connection of one person, or of everybody, each with its owner. The
/// order is the portal's: by person, then by label.
pub async fn connections(db: &Db, only: Option<&User>) -> Result<Vec<(User, Connection)>> {
    let people = match only {
        Some(u) => vec![u.clone()],
        None => db.list_users().await?,
    };
    let mut out = Vec::new();
    for u in people {
        for c in db.list_connections(u.id).await? {
            out.push((u.clone(), c));
        }
    }
    Ok(out)
}

/// Minutes are as fine as any of these columns needs to be; a missing instant
/// is a dash rather than an empty cell, so a table stays readable.
pub fn instant(at: Option<DateTime<Utc>>) -> String {
    at.map(|t| t.format("%Y-%m-%d %H:%M").to_string())
        .unwrap_or_else(|| "-".into())
}

pub fn day(at: DateTime<Utc>) -> String {
    at.format("%Y-%m-%d").to_string()
}

/// A list, as columns wide enough for what is in them. Fixed widths would cut
/// a scope list or an email, and both are exactly what these tables are read
/// for; `empty` is what is printed when there is nothing at all, because a
/// blank answer looks like a broken one.
pub fn table(header: &[&str], rows: &[Vec<String>], empty: &str) {
    if rows.is_empty() {
        println!("{empty}");
        return;
    }
    let mut widths: Vec<usize> = header.iter().map(|h| h.chars().count()).collect();
    for row in rows {
        for (i, cell) in row.iter().enumerate() {
            widths[i] = widths[i].max(cell.chars().count());
        }
    }
    let line = |cells: &[String]| {
        let mut out = String::new();
        for (i, cell) in cells.iter().enumerate() {
            if i > 0 {
                out.push_str("  ");
            }
            out.push_str(cell);
            if i + 1 < cells.len() {
                for _ in cell.chars().count()..widths[i] {
                    out.push(' ');
                }
            }
        }
        println!("{}", out.trim_end());
    };
    line(&header.iter().map(|h| (*h).to_string()).collect::<Vec<_>>());
    for row in rows {
        line(row);
    }
}

/// What a list of names looks like in a cell, and what "nothing" looks like.
pub fn joined(items: &[String]) -> String {
    if items.is_empty() {
        "-".to_string()
    } else {
        items.join(", ")
    }
}
