//! `gmcp connection`: the Google account grants.
//!
//! Connecting is browser-only — the consent flow needs somebody to click
//! through Google's screens — so the CLI only lists what is there, drops a row
//! and flips the gateway flag.

use anyhow::Result;
use chrono::Utc;
use clap::{Subcommand, ValueEnum};

use crate::cli;
use crate::db::{AuditKind, AuditOutcome, ConnectionPatch, Db, NewAuditEntry};

#[derive(Subcommand)]
pub enum ConnectionCommand {
    /// List connections, everyone's or one person's
    List {
        /// Only this person's (email)
        #[arg(long)]
        user: Option<String>,
    },
    /// Delete a connection row; the grant at Google is not touched
    Remove { id: i64 },
    /// Whether delegate tokens may reach this connection
    SetGateway {
        id: i64,
        #[arg(value_enum)]
        state: Gateway,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Gateway {
    On,
    Off,
}

pub async fn run(db: &Db, cmd: ConnectionCommand) -> Result<()> {
    match cmd {
        ConnectionCommand::List { user } => {
            let only = match &user {
                Some(email) => Some(cli::person(db, email).await?),
                None => None,
            };
            let rows = cli::connections(db, only.as_ref())
                .await?
                .into_iter()
                .map(|(u, c)| {
                    vec![
                        c.id.to_string(),
                        u.display().to_string(),
                        c.label,
                        c.google_email,
                        cli::joined(&c.services),
                        c.status.to_string(),
                        if c.delegate_ok { "yes" } else { "no" }.to_string(),
                        cli::instant(c.last_used_at),
                    ]
                })
                .collect::<Vec<_>>();
            cli::table(
                &[
                    "ID",
                    "USER",
                    "LABEL",
                    "GOOGLE",
                    "SERVICES",
                    "STATUS",
                    "GATEWAY",
                    "LAST USED",
                ],
                &rows,
                "no connections",
            );
        }
        ConnectionCommand::Remove { id } => {
            let c = cli::found("connection", id, db.get_connection(id).await)?;
            let user = db.get_user(c.user_id).await?;
            db.delete_connection(id).await?;
            // The audit row outlives the connection, so it names it in words
            // rather than by an id that no longer resolves.
            let entry = NewAuditEntry {
                user_id: Some(user.id),
                detail: Some(format!(
                    "{} ({}) removed from the CLI",
                    c.label, c.google_email
                )),
                ..NewAuditEntry::new(Utc::now(), AuditKind::ConnectionRemoved, AuditOutcome::Ok)
            };
            if let Err(e) = db.insert_audit(entry).await {
                tracing::error!("could not record a connection_removed audit row: {e}");
            }
            println!(
                "removed connection {id} ({}, {}) of {}",
                c.label,
                c.google_email,
                user.display()
            );
            println!(
                "The grant at Google is untouched: the CLI does not revoke it. \
                 Remove it at https://myaccount.google.com/permissions if it should go."
            );
        }
        ConnectionCommand::SetGateway { id, state } => {
            let on = state == Gateway::On;
            let patch = ConnectionPatch {
                delegate_ok: Some(on),
                ..ConnectionPatch::default()
            };
            let c = cli::found("connection", id, db.update_connection(id, patch).await)?;
            println!(
                "connection {id} ({}, {}) is {} delegate tokens",
                c.label,
                c.google_email,
                if on {
                    "now reachable by"
                } else {
                    "no longer reachable by"
                }
            );
        }
    }
    Ok(())
}
