mod config;
mod domain;

use anyhow::Result;
use clap::{Parser, Subcommand};

use crate::config::Config;

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
        // Serve parses its configuration even so: a misconfigured deployment
        // should fail here rather than on the first request.
        Command::Serve => {
            let config = Config::from_env()?;
            tracing::info!("{}", config.summary());
            if !config.google.configured() {
                tracing::warn!(
                    "GMCP_GOOGLE_CLIENT_ID and GMCP_GOOGLE_CLIENT_SECRET are unset; \
                     no account can be connected until they are"
                );
            }
            not_implemented("serve")
        }
        Command::Migrate { .. } => not_implemented("migrate"),
        Command::Token => not_implemented("token"),
        Command::Connection => not_implemented("connection"),
        Command::User => not_implemented("user"),
        Command::Prune => not_implemented("prune"),
    }
}
