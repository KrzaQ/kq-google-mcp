mod cli;
mod config;
mod db;
mod domain;
mod google;
mod http;

use anyhow::Result;
use clap::{Parser, Subcommand};

use crate::config::Config;
use crate::db::Db;

#[derive(Parser)]
#[command(
    name = "gmcp",
    version,
    about = "Scoped Google portal: API, web UI, CLI and MCP server"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the HTTP server (API, web UI, MCP)
    Serve,
    /// Apply pending migrations, or show their status
    Migrate {
        /// Only list migrations, do not apply anything
        #[arg(long)]
        status: bool,
    },
    /// Manage bearer tokens for MCP clients
    Token,
    /// Google account connections
    Connection,
    /// Users who have logged in
    User,
    /// Delete expired links and old audit rows
    Prune,
}

/// Until the surface it names exists, a subcommand says so and exits cleanly;
/// nothing here pretends to have done anything.
fn not_implemented(what: &str) -> Result<()> {
    println!("{what}: not implemented");
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .init();
    let cli = Cli::parse();
    match cli.command {
        // Serve parses its configuration first: a misconfigured deployment
        // should fail here rather than on the first request.
        Command::Serve => http::serve(Config::from_env()?).await,
        // The one subcommand that needs nothing but the database path.
        Command::Migrate { status } => {
            let db = Db::open(config::database_from_env()).await?;
            cli::migrate::run(&db, status).await
        }
        Command::Token => not_implemented("token"),
        Command::Connection => not_implemented("connection"),
        Command::User => not_implemented("user"),
        Command::Prune => not_implemented("prune"),
    }
}
