//! Configuration comes from `GMCP_*` environment variables only.

use std::net::SocketAddr;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};

/// Where the database lives when `GMCP_DATABASE` is unset: development runs
/// out of the checkout, the container mounts `/data`.
const DEFAULT_DATABASE: &str = "./data/gmcp.db";
const DEFAULT_API_BASE: &str = "https://www.googleapis.com";
const DEFAULT_OAUTH_BASE: &str = "https://oauth2.googleapis.com";
const DEFAULT_ACCOUNTS_BASE: &str = "https://accounts.google.com";

#[derive(Debug, Clone)]
pub struct Config {
    /// Path of the SQLite file; the directory is created at startup.
    pub database: PathBuf,
    pub bind: SocketAddr,
    /// Public origin of the deployment. Both redirect URIs and every download
    /// link derive from it; it is never taken from a request header.
    pub public_url: url::Url,
    pub secret: Vec<u8>,
    pub auth: AuthMode,
    pub google: GoogleConfig,
    pub auto_migrate: bool,
}

#[derive(Debug, Clone)]
pub enum AuthMode {
    Oidc(OidcConfig),
    /// Everyone is one fixed user. Loopback only.
    Dev,
}

#[derive(Debug, Clone)]
pub struct OidcConfig {
    pub issuer: String,
    pub client_id: String,
    pub client_secret: String,
    /// Only members of this authentik group may log in; None lets any account
    /// of the issuer in.
    pub group: Option<String>,
}

/// The Google web client and the endpoints it talks to. The credentials are
/// optional so the portal starts, reports itself unconfigured and serves
/// everything that does not touch Google; the bases are configurable so tests
/// can point them at wiremock.
#[derive(Debug, Clone)]
pub struct GoogleConfig {
    pub client_id: Option<String>,
    pub client_secret: Option<String>,
    pub api_base: url::Url,
    pub oauth_base: url::Url,
    pub accounts_base: url::Url,
}

impl GoogleConfig {
    pub fn configured(&self) -> bool {
        self.client_id.is_some() && self.client_secret.is_some()
    }

    fn describe(&self) -> String {
        let client = match (&self.client_id, &self.client_secret) {
            (Some(id), Some(_)) => format!("client {id}"),
            (Some(id), None) => format!("client {id}, secret missing"),
            (None, _) => "unconfigured".to_string(),
        };
        format!(
            "google {client}, api {}, oauth {}, accounts {}",
            self.api_base, self.oauth_base, self.accounts_base
        )
    }

    fn from_env() -> Result<Self> {
        Ok(Self {
            client_id: var("GMCP_GOOGLE_CLIENT_ID"),
            client_secret: var("GMCP_GOOGLE_CLIENT_SECRET"),
            api_base: base("GMCP_GOOGLE_API_BASE", DEFAULT_API_BASE)?,
            oauth_base: base("GMCP_GOOGLE_OAUTH_BASE", DEFAULT_OAUTH_BASE)?,
            accounts_base: base("GMCP_GOOGLE_ACCOUNTS_BASE", DEFAULT_ACCOUNTS_BASE)?,
        })
    }
}

pub fn var(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.trim().is_empty())
}

pub fn require(name: &str) -> Result<String> {
    var(name).with_context(|| format!("{name} is not set"))
}

fn base(name: &str, default: &str) -> Result<url::Url> {
    var(name)
        .unwrap_or_else(|| default.into())
        .parse()
        .with_context(|| format!("{name} must be an absolute URL"))
}

/// `GMCP_DATABASE`, the path every subcommand opens.
pub fn database_from_env() -> PathBuf {
    var("GMCP_DATABASE")
        .unwrap_or_else(|| DEFAULT_DATABASE.into())
        .into()
}

impl Config {
    /// Everything `serve` needs.
    pub fn from_env() -> Result<Self> {
        let bind = var("GMCP_BIND")
            .unwrap_or_else(|| "0.0.0.0:8000".into())
            .parse()
            .context("GMCP_BIND must be host:port")?;
        let public_url: url::Url = require("GMCP_PUBLIC_URL")?
            .parse()
            .context("GMCP_PUBLIC_URL must be an absolute URL")?;
        let auth = match var("GMCP_AUTH").as_deref().unwrap_or("oidc") {
            "dev" => {
                check_dev(&public_url, bind)?;
                AuthMode::Dev
            }
            "oidc" => AuthMode::Oidc(OidcConfig {
                issuer: require("GMCP_OIDC_ISSUER")?,
                client_id: require("GMCP_OIDC_CLIENT_ID")?,
                client_secret: require("GMCP_OIDC_CLIENT_SECRET")?,
                group: var("GMCP_OIDC_GROUP"),
            }),
            other => bail!("GMCP_AUTH must be oidc or dev, got {other:?}"),
        };
        let secret = match (&auth, var("GMCP_SECRET")) {
            (_, Some(s)) if s.len() >= 32 => s.into_bytes(),
            (_, Some(_)) => bail!("GMCP_SECRET must be at least 32 bytes"),
            // Dev mode is loopback-only, a random per-process key is fine. It
            // also reseals nothing: connections made under it stay readable
            // only for the life of the process.
            (AuthMode::Dev, None) => {
                let mut buf = vec![0u8; 64];
                getrandom::fill(&mut buf).map_err(|e| anyhow::anyhow!("random secret: {e}"))?;
                buf
            }
            (AuthMode::Oidc(_), None) => bail!("GMCP_SECRET is not set"),
        };
        Ok(Self {
            database: database_from_env(),
            bind,
            public_url,
            secret,
            auth,
            google: GoogleConfig::from_env()?,
            auto_migrate: var("GMCP_AUTO_MIGRATE").as_deref() != Some("0"),
        })
    }

    /// What the process is configured to do, for the startup log. Secrets are
    /// reported as present or absent and never printed.
    pub fn summary(&self) -> String {
        let auth = match &self.auth {
            AuthMode::Dev => "dev (loopback only)".to_string(),
            AuthMode::Oidc(o) => format!(
                "oidc {} as {} (secret {}), group {}",
                o.issuer,
                o.client_id,
                if o.client_secret.is_empty() {
                    "missing"
                } else {
                    "set"
                },
                o.group.as_deref().unwrap_or("any"),
            ),
        };
        format!(
            "bind {}, public url {}, database {}, auth {auth}, {}, session secret {} bytes, auto-migrate {}",
            self.bind,
            self.public_url,
            self.database.display(),
            self.google.describe(),
            self.secret.len(),
            self.auto_migrate,
        )
    }
}

/// Dev auth logs everyone in as the same person, so it must never be
/// reachable from anywhere but the developer's own machine. Both halves are
/// checked: a loopback public URL says nothing about which addresses the
/// server answers on, and `GMCP_BIND` defaults to `0.0.0.0`, which would log
/// the whole network in as the dev user.
pub fn check_dev(public_url: &url::Url, bind: SocketAddr) -> Result<()> {
    let host = public_url.host_str().unwrap_or("");
    if public_url.scheme() != "http" || (host != "localhost" && host != "127.0.0.1") {
        bail!(
            "GMCP_AUTH=dev is only allowed with GMCP_PUBLIC_URL on http://localhost or http://127.0.0.1, got {public_url}"
        )
    }
    if !bind.ip().is_loopback() {
        bail!(
            "GMCP_AUTH=dev logs every request in as the dev user, so it is only allowed with \
             GMCP_BIND on a loopback address; set GMCP_BIND=127.0.0.1:8000, got {bind}"
        )
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dev_guard() {
        let local: SocketAddr = "127.0.0.1:8000".parse().unwrap();
        let any: SocketAddr = "0.0.0.0:8000".parse().unwrap();
        let url = |u: &str| u.parse::<url::Url>().unwrap();
        assert!(check_dev(&url("http://localhost:8000"), local).is_ok());
        assert!(check_dev(&url("http://127.0.0.1:1234"), local).is_ok());
        assert!(check_dev(&url("https://localhost:8000"), local).is_err());
        assert!(check_dev(&url("http://google-mcp.int.krzaq.cc"), local).is_err());
        assert!(check_dev(&url("http://0.0.0.0:8000"), local).is_err());
        // A loopback public URL says nothing about what the socket answers on:
        // the default bind is 0.0.0.0, and that would log the LAN in as dev.
        let error = check_dev(&url("http://localhost:8000"), any).unwrap_err();
        assert!(error.to_string().contains("GMCP_BIND"), "{error}");
        assert!(check_dev(&url("http://localhost:8000"), "[::1]:8000".parse().unwrap()).is_ok());
    }

    #[test]
    fn google_bases_default_to_the_real_endpoints() {
        // The test process configures nothing, so this is the shape a fresh
        // clone gets: the real Google endpoints, no credentials.
        let google = GoogleConfig::from_env().unwrap();
        assert_eq!(google.api_base.as_str(), "https://www.googleapis.com/");
        assert_eq!(google.oauth_base.as_str(), "https://oauth2.googleapis.com/");
        assert_eq!(
            google.accounts_base.as_str(),
            "https://accounts.google.com/"
        );
    }
}
