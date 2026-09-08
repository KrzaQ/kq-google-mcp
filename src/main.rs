mod cli;
mod config;
mod db;
mod domain;
mod google;
mod http;
mod mcp;

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
    Token {
        #[command(subcommand)]
        command: cli::token::TokenCommand,
    },
    /// Google account connections; connecting one is browser-only
    Connection {
        #[command(subcommand)]
        command: cli::connection::ConnectionCommand,
    },
    /// Users who have logged in
    User {
        #[command(subcommand)]
        command: cli::user::UserCommand,
    },
    /// Delete expired links and old audit rows
    Prune {
        /// How far back the log is kept: 180d, 12w, 6m
        #[arg(long, default_value = "180d", value_parser = cli::prune::parse_retention)]
        audit_older_than: chrono::Duration,
    },
    /// Open every stored refresh token with GMCP_SECRET and report which fail
    CheckSecret,
}

/// Every subcommand but `serve` needs nothing more than the database path.
async fn open() -> Result<Db> {
    Db::open(config::database_from_env()).await
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
        Command::Migrate { status } => cli::migrate::run(&open().await?, status).await,
        // The environment is read here and nowhere below: what the snippets
        // point at and what opens a refresh token are arguments, so a test can
        // run the same code without either.
        Command::Token { command } => {
            cli::token::run(&open().await?, config::var("GMCP_PUBLIC_URL"), command).await
        }
        Command::Connection { command } => cli::connection::run(&open().await?, command).await,
        Command::User { command } => cli::user::run(&open().await?, command).await,
        Command::Prune { audit_older_than } => {
            cli::prune::run(&open().await?, audit_older_than).await
        }
        Command::CheckSecret => {
            cli::check_secret::run(&open().await?, config::var("GMCP_SECRET")).await
        }
    }
}
