//! `gmcp check-secret`: can this `GMCP_SECRET` still open what is stored?
//!
//! Refresh tokens are sealed under a key derived from the secret, so rotating
//! it makes every connection unusable — and nothing says so until the next
//! tool call fails. This opens each one and reports which are readable. The
//! token itself is discarded the moment it comes back; nothing here prints it,
//! and there is no flag that would.

use anyhow::{Result, bail};

use crate::cli;
use crate::db::Db;
use crate::domain::seal;

pub async fn run(db: &Db, secret: Option<String>) -> Result<()> {
    let Some(secret) = secret else {
        bail!(
            "GMCP_SECRET is not set, and it is the key every refresh token is sealed under; \
             set it to what the deployment runs with"
        );
    };
    let all = cli::connections(db, None).await?;
    let mut rows = Vec::new();
    let mut failed = 0;
    for (user, c) in &all {
        let detail = match seal::open(secret.as_bytes(), &c.refresh_token_sealed) {
            // The opened token is dropped here and goes no further.
            Ok(_) => "ok".to_string(),
            Err(e) => {
                failed += 1;
                e.to_string()
            }
        };
        rows.push(vec![
            c.id.to_string(),
            user.display().to_string(),
            c.label.clone(),
            c.google_email.clone(),
            detail,
        ]);
    }
    cli::table(
        &["ID", "USER", "LABEL", "GOOGLE", "REFRESH TOKEN"],
        &rows,
        "no connections to check",
    );
    if failed > 0 {
        bail!(
            "{failed} of {} connections cannot be opened with this GMCP_SECRET; \
             each one has to be connected again in the portal",
            all.len()
        );
    }
    if !all.is_empty() {
        println!("all {} connections open with this GMCP_SECRET", all.len());
    }
    Ok(())
}
