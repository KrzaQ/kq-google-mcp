//! `gmcp user list`: who has logged in.
//!
//! There is no `create`: a user row appears when somebody logs in through the
//! browser and never any other way, which is what makes `--user EMAIL`
//! elsewhere refuse a name nobody answers to.

use anyhow::Result;
use clap::Subcommand;

use crate::cli;
use crate::db::Db;

#[derive(Subcommand)]
pub enum UserCommand {
    /// List everyone who has logged in
    List,
}

pub async fn run(db: &Db, cmd: UserCommand) -> Result<()> {
    match cmd {
        UserCommand::List => {
            let mut rows = Vec::new();
            for u in db.list_users().await? {
                rows.push(vec![
                    u.id.to_string(),
                    u.display().to_string(),
                    u.email.clone().unwrap_or_else(|| "-".into()),
                    db.list_connections(u.id).await?.len().to_string(),
                    cli::day(u.created_at),
                    cli::instant(u.last_login_at),
                ]);
            }
            cli::table(
                &["ID", "NAME", "EMAIL", "CONNECTIONS", "SINCE", "LAST LOGIN"],
                &rows,
                "nobody has logged in yet",
            );
        }
    }
    Ok(())
}
