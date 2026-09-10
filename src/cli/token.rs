//! `gmcp token`: the bearer tokens MCP clients present.
//!
//! Creation applies [`crate::domain::token::validate`], the same rules the
//! token API applies, so a scope list the portal refuses is refused here too
//! and in the same words. What the CLI adds is the terminal's shape of them:
//! a person named by email rather than by session, connections named by their
//! labels rather than by id, and the ready-made client snippets printed with
//! the secret, which is shown once and never stored.

use anyhow::{Result, anyhow, bail};
use chrono::Utc;
use clap::Subcommand;

use crate::cli;
use crate::db::{AuditKind, AuditOutcome, ClientProfile, Db, NewAuditEntry, NewToken, User};
use crate::domain::token::{self, Owner, Request, RequestError};

/// Where the snippets point when `GMCP_PUBLIC_URL` is not set. It is the
/// address `make dev-backend` serves, and the line above them says so.
const FALLBACK_URL: &str = "http://localhost:8000";

#[derive(Subcommand)]
pub enum TokenCommand {
    /// Create a token; the secret is printed once and never stored
    Create {
        /// What the token is called in the portal and the log
        name: String,
        /// Comma-separated `service:level` scopes, e.g. gmail:read,gmail:draft
        #[arg(long, required = true, value_delimiter = ',')]
        scopes: Vec<String>,
        /// Client profile: generic, openwebui, claude-code or opencode
        #[arg(long, default_value = "generic")]
        client: String,
        /// Email of the person a personal token acts as
        #[arg(long)]
        user: Option<String>,
        /// Every current and future connection of that person
        #[arg(long, conflicts_with = "connections")]
        all_connections: bool,
        /// Comma-separated connection labels, e.g. work,personal
        #[arg(long, value_delimiter = ',')]
        connections: Vec<String>,
        /// A gateway token: no person, no allowlist, acts for whoever
        /// X-Gmcp-User names
        #[arg(long)]
        delegate: bool,
    },
    /// List every token, personal and delegate
    List,
    /// Revoke a token by id
    Revoke { id: i64 },
}

pub async fn run(db: &Db, public_url: Option<String>, cmd: TokenCommand) -> Result<()> {
    match cmd {
        TokenCommand::Create {
            name,
            scopes,
            client,
            user,
            all_connections,
            connections,
            delegate,
        } => {
            create(
                db,
                public_url,
                Created {
                    name,
                    scopes,
                    client,
                    user,
                    all_connections,
                    connections,
                    delegate,
                },
            )
            .await
        }
        TokenCommand::List => list(db).await,
        TokenCommand::Revoke { id } => revoke(db, id).await,
    }
}

/// The arguments of `token create`, kept together so the work is one function
/// rather than a seven-argument call.
pub struct Created {
    pub name: String,
    pub scopes: Vec<String>,
    pub client: String,
    pub user: Option<String>,
    pub all_connections: bool,
    pub connections: Vec<String>,
    pub delegate: bool,
}

async fn create(db: &Db, public_url: Option<String>, args: Created) -> Result<()> {
    let client: ClientProfile = args.client.parse().map_err(|e: String| anyhow!(e))?;
    // `--delegate` is the readable spelling of the scope; passing the scope
    // itself does the same thing, so the two cannot disagree.
    let mut scopes = args.scopes.clone();
    if args.delegate && !scopes.iter().any(|s| s.trim() == token::SCOPE_DELEGATE) {
        scopes.push(token::SCOPE_DELEGATE.to_string());
    }
    let valid = token::validate(Request {
        name: &args.name,
        scopes: &scopes,
        owner: match &args.user {
            Some(email) => Owner::Named(email),
            None => Owner::Unnamed,
        },
        all_connections: args.all_connections,
        connections: args.connections.len(),
    })
    // The rules are shared; the flags that satisfy them are the CLI's own.
    .map_err(|e| match e {
        RequestError::DelegateUser => anyhow!("{e}; drop --user"),
        RequestError::NoUser => anyhow!("{e}; name them with --user EMAIL"),
        RequestError::NoConnections => {
            anyhow!("{e}; pass --all-connections or --connections work,personal")
        }
        RequestError::DelegateConnections => {
            anyhow!("{e}; drop --all-connections and --connections")
        }
        other => anyhow!(other),
    })?;

    // A delegate token belongs to nobody, but `created_by` is NOT NULL: every
    // token has an author. From the CLI there is no session to be the author,
    // so the first person on record is, and the output says so.
    let (acts_as, author) = match valid.delegate {
        true => {
            let first = db.list_users().await?.into_iter().next().ok_or_else(|| {
                anyhow!("nobody has logged in yet, so there is no one to record as the author")
            })?;
            (None, first)
        }
        false => {
            let owner =
                cli::person(db, args.user.as_deref().expect("a personal token has one")).await?;
            (Some(owner.clone()), owner)
        }
    };

    let ids = allowlist(db, acts_as.as_ref(), &args.connections).await?;
    let secret = token::generate();
    let created = db
        .create_token(NewToken {
            name: valid.name.to_string(),
            token_hash: secret.hash,
            scopes: valid.scopes.iter().map(ToString::to_string).collect(),
            client,
            user_id: acts_as.as_ref().map(|u| u.id),
            all_connections: args.all_connections,
            created_by: author.id,
        })
        .await?;
    if !ids.is_empty() {
        db.set_token_connections(created.id, &ids).await?;
    }
    log(db, author.id, &created, AuditKind::TokenCreated).await;

    println!("token {} {:?}", created.id, created.name);
    println!("  client       {client}");
    println!(
        "  scopes       {}",
        valid
            .scopes
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(", ")
    );
    match &acts_as {
        Some(u) => {
            println!(
                "  acts as      {}",
                u.email.as_deref().unwrap_or(u.display())
            );
            println!("  connections  {}", reach(db, &created).await?);
        }
        None => {
            println!("  acts as      whoever X-Gmcp-User names, if they flagged the connection");
            println!(
                "  created by   {} (nobody was named, so the first person on record is the \
                 author; a delegate token belongs to none of them)",
                author.email.as_deref().unwrap_or(author.display())
            );
        }
    }
    println!();
    println!("  {}", secret.secret);
    println!();
    println!("The secret is not stored and cannot be shown again.");
    snippets(
        public_url,
        &secret.secret,
        acts_as.is_none() || client == ClientProfile::OpenWebUi,
    );
    Ok(())
}

/// The connection ids behind `--connections work,personal`. A label that is
/// not one of the person's is refused with the ones that are: a token that
/// silently reached nothing would look like a working token.
async fn allowlist(db: &Db, acts_as: Option<&User>, labels: &[String]) -> Result<Vec<i64>> {
    let Some(user) = acts_as.filter(|_| !labels.is_empty()) else {
        return Ok(Vec::new());
    };
    let theirs = db.list_connections(user.id).await?;
    let mut ids = Vec::new();
    for label in labels {
        let label = label.trim();
        let found = theirs
            .iter()
            .find(|c| c.label.eq_ignore_ascii_case(label))
            .ok_or_else(|| {
                anyhow!(
                    "{} has no connection labelled {label:?}; theirs are {}",
                    user.email.as_deref().unwrap_or(user.display()),
                    cli::joined(&theirs.iter().map(|c| c.label.clone()).collect::<Vec<_>>())
                )
            })?;
        if !ids.contains(&found.id) {
            ids.push(found.id);
        }
    }
    Ok(ids)
}

async fn list(db: &Db) -> Result<()> {
    let mut rows = Vec::new();
    for t in db.list_tokens(None).await? {
        let who = match t.user_id {
            Some(id) => db.get_user(id).await?.display().to_string(),
            None => "-".to_string(),
        };
        rows.push(vec![
            t.id.to_string(),
            t.name.clone(),
            t.client.to_string(),
            t.scopes.join(","),
            who,
            reach(db, &t).await?,
            cli::day(t.created_at),
            cli::instant(t.last_used_at),
            cli::instant(t.revoked_at),
        ]);
    }
    cli::table(
        &[
            "ID",
            "NAME",
            "CLIENT",
            "SCOPES",
            "USER",
            "CONNECTIONS",
            "CREATED",
            "LAST USED",
            "REVOKED",
        ],
        &rows,
        "no tokens",
    );
    Ok(())
}

/// What a token reaches, in one cell: the gateway's rule, every connection of
/// its person, or the labels on its allowlist.
async fn reach(db: &Db, t: &crate::db::ApiToken) -> Result<String> {
    if t.is_delegate() {
        return Ok("delegate".into());
    }
    if t.all_connections {
        return Ok("all".into());
    }
    let mut labels = Vec::new();
    for id in db.token_connections(t.id).await? {
        labels.push(db.get_connection(id).await?.label);
    }
    Ok(cli::joined(&labels))
}

async fn revoke(db: &Db, id: i64) -> Result<()> {
    let t = cli::found("token", id, db.get_token(id).await)?;
    match db.revoke_token(id).await {
        Ok(()) => {}
        // Revoking is final: a second call means the token was already gone,
        // which is worth saying rather than reporting as done again.
        Err(crate::db::DbError::NotFound) => bail!(
            "token {id} ({:?}) was already revoked, on {}",
            t.name,
            cli::instant(t.revoked_at)
        ),
        Err(e) => return Err(e.into()),
    }
    log(db, t.created_by, &t, AuditKind::TokenRevoked).await;
    println!("revoked token {id} ({:?})", t.name);
    Ok(())
}

/// The log answers "where did this token come from" for tokens minted at the
/// terminal too; the author is the person the row is recorded against.
async fn log(db: &Db, author: i64, t: &crate::db::ApiToken, kind: AuditKind) {
    let entry = NewAuditEntry {
        user_id: Some(author),
        token_id: Some(t.id),
        args: Some(serde_json::json!({
            "name": t.name,
            "client": t.client.as_str(),
            "scopes": t.scopes,
            "all_connections": t.all_connections,
            "via": "cli",
        })),
        detail: Some(t.name.clone()),
        ..NewAuditEntry::new(Utc::now(), kind, AuditOutcome::Ok)
    };
    if let Err(e) = db.insert_audit(entry).await {
        tracing::error!("could not record a {kind} audit row: {e}");
    }
}

/// The three clients, ready to paste. `X-Gmcp-User` is only ever set by a
/// gateway that speaks for several people, so it is printed as required for a
/// delegate token and as unnecessary for a personal one.
fn snippets(public_url: Option<String>, secret: &str, gateway: bool) {
    let base = public_url.unwrap_or_else(|| {
        println!();
        println!("GMCP_PUBLIC_URL is not set; the snippets below assume {FALLBACK_URL}.");
        FALLBACK_URL.to_string()
    });
    let url = format!("{}/mcp", base.trim_end_matches('/'));
    println!();
    println!("Claude Code:");
    println!(
        "  claude mcp add --transport http gmcp {url} \
         --header \"Authorization: Bearer {secret}\""
    );
    println!();
    println!("OpenCode, in opencode.json:");
    println!("  {{");
    println!("    \"$schema\": \"https://opencode.ai/config.json\",");
    println!("    \"mcp\": {{");
    println!("      \"gmcp\": {{");
    println!("        \"type\": \"remote\",");
    println!("        \"url\": \"{url}\",");
    println!("        \"enabled\": true,");
    println!("        \"headers\": {{ \"Authorization\": \"Bearer {secret}\" }}");
    println!("      }}");
    println!("    }}");
    println!("  }}");
    println!();
    println!("Open WebUI, as an MCP tool server (its header box is parsed as JSON):");
    println!("  URL            {url}");
    println!("  Auth           Bearer");
    println!("  Bearer token   {secret}");
    println!(
        "  Extra headers  {{\"X-Gmcp-User\": \"{{{{USER_EMAIL}}}}\"}}{}",
        if gateway {
            ""
        } else {
            "   (a personal token acts as its own person; this header is ignored)"
        }
    );
    println!("  Set the model's Function Calling to Native, or it never sees a picture.");
}
