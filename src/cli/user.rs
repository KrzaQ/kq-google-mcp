//! `gmcp user list`: who has logged in, and which clock each of them keeps.
//! `gmcp user set-timezone` moves somebody to another zone, which is the same
//! change the person makes for themselves on the portal's home page.
//!
//! There is no `create`: a user row appears when somebody logs in through the
//! browser and never any other way, which is what makes `--user EMAIL`
//! elsewhere refuse a name nobody answers to.

use anyhow::{Result, anyhow};
use clap::Subcommand;

use crate::cli;
use crate::db::Db;
use crate::domain::zone;

#[derive(Subcommand)]
pub enum UserCommand {
    /// List everyone who has logged in
    List,
    /// Set the zone a person's times are shown in and read on
    SetTimezone {
        /// The email of somebody who has logged in
        email: String,
        /// An IANA zone name, e.g. Europe/Warsaw
        timezone: String,
    },
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
                    u.timezone.clone(),
                    db.list_connections(u.id).await?.len().to_string(),
                    cli::day(u.created_at),
                    cli::instant(u.last_login_at),
                ]);
            }
            cli::table(
                &[
                    "ID",
                    "NAME",
                    "EMAIL",
                    "TIMEZONE",
                    "CONNECTIONS",
                    "SINCE",
                    "LAST LOGIN",
                ],
                &rows,
                "nobody has logged in yet",
            );
        }
        // The name is read by the same rule the portal reads it by, so a typo
        // is refused here in the words it is refused there.
        UserCommand::SetTimezone { email, timezone } => {
            let user = cli::person(db, &email).await?;
            let zone = zone::parse(&timezone).map_err(|e| anyhow!(e))?;
            let user = db.set_user_timezone(user.id, zone).await?;
            println!("{} now keeps time in {}", user.display(), user.timezone);
        }
    }
    Ok(())
}
