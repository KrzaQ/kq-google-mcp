//! Configuration comes from `GMCP_*` environment variables only.
//!
//! The parsing itself takes a [`Vars`] lookup rather than reading the
//! environment: `from_env` passes the process environment and the tests pass a
//! map, so no test needs — or can be disturbed by — an ambient variable.

use std::net::SocketAddr;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};

/// Where the database lives when `GMCP_DATABASE` is unset: development runs
/// out of the checkout, the container mounts `/data`.
const DEFAULT_DATABASE: &str = "./data/gmcp.db";
const DEFAULT_API_BASE: &str = "https://www.googleapis.com";
const DEFAULT_OAUTH_BASE: &str = "https://oauth2.googleapis.com";
const DEFAULT_ACCOUNTS_BASE: &str = "https://accounts.google.com";
/// The zone a deployment works in when `GMCP_TIMEZONE` is unset: what a new
/// user gets, and what stands in for a user whose own zone no longer parses.
const DEFAULT_TIMEZONE: &str = "Europe/Warsaw";

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
    /// The house zone. Every user starts here, and a user whose own zone is
    /// missing falls back to it; nothing is ever rendered in UTC because it
    /// happened to be stored that way.
    pub timezone: chrono_tz::Tz,
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

    fn from_vars(vars: Vars) -> Result<Self> {
        Ok(Self {
            client_id: vars("GMCP_GOOGLE_CLIENT_ID"),
            client_secret: vars("GMCP_GOOGLE_CLIENT_SECRET"),
            api_base: base(vars, "GMCP_GOOGLE_API_BASE", DEFAULT_API_BASE)?,
            oauth_base: base(vars, "GMCP_GOOGLE_OAUTH_BASE", DEFAULT_OAUTH_BASE)?,
            accounts_base: base(vars, "GMCP_GOOGLE_ACCOUNTS_BASE", DEFAULT_ACCOUNTS_BASE)?,
        })
    }
}

/// Where a variable is read from. The one implementation that ships is the
/// process environment; a test passes a map of its own.
pub type Vars<'a> = &'a dyn Fn(&str) -> Option<String>;

pub fn var(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.trim().is_empty())
}

fn require(vars: Vars, name: &str) -> Result<String> {
    vars(name).with_context(|| format!("{name} is not set"))
}

fn base(vars: Vars, name: &str, default: &str) -> Result<url::Url> {
    vars(name)
        .unwrap_or_else(|| default.into())
        .parse()
        .with_context(|| format!("{name} must be an absolute URL"))
}

/// `GMCP_TIMEZONE`, an IANA name, `Europe/Warsaw` when unset. A typo stops
/// the boot rather than the first tool call: a server that renders times in
/// the wrong zone is worse than one that does not start.
fn timezone(vars: Vars) -> Result<chrono_tz::Tz> {
    let name = vars("GMCP_TIMEZONE").unwrap_or_else(|| DEFAULT_TIMEZONE.into());
    name.trim()
        .parse()
        .map_err(|e| anyhow::anyhow!("GMCP_TIMEZONE: {e}"))
}

/// `GMCP_DATABASE`, the path every subcommand opens.
pub fn database_from_env() -> PathBuf {
    database(&var)
}

fn database(vars: Vars) -> PathBuf {
    vars("GMCP_DATABASE")
        .unwrap_or_else(|| DEFAULT_DATABASE.into())
        .into()
}

impl Config {
    /// Everything `serve` needs, from the process environment.
    pub fn from_env() -> Result<Self> {
        Self::from_vars(&var)
    }

    /// The same, from wherever `vars` reads.
    pub fn from_vars(vars: Vars) -> Result<Self> {
        let bind = vars("GMCP_BIND")
            .unwrap_or_else(|| "0.0.0.0:8000".into())
            .parse()
            .context("GMCP_BIND must be host:port")?;
        let public_url: url::Url = require(vars, "GMCP_PUBLIC_URL")?
            .parse()
            .context("GMCP_PUBLIC_URL must be an absolute URL")?;
        let auth = match vars("GMCP_AUTH").as_deref().unwrap_or("oidc") {
            "dev" => {
                check_dev(&public_url, bind)?;
                AuthMode::Dev
            }
            "oidc" => AuthMode::Oidc(OidcConfig {
                issuer: require(vars, "GMCP_OIDC_ISSUER")?,
                client_id: require(vars, "GMCP_OIDC_CLIENT_ID")?,
                client_secret: require(vars, "GMCP_OIDC_CLIENT_SECRET")?,
                group: vars("GMCP_OIDC_GROUP"),
            }),
            other => bail!("GMCP_AUTH must be oidc or dev, got {other:?}"),
        };
        let secret = match (&auth, vars("GMCP_SECRET")) {
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
            database: database(vars),
            bind,
            public_url,
            secret,
            auth,
            google: GoogleConfig::from_vars(vars)?,
            auto_migrate: vars("GMCP_AUTO_MIGRATE").as_deref() != Some("0"),
            timezone: timezone(vars)?,
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
            "bind {}, public url {}, database {}, auth {auth}, {}, session secret {} bytes, \
             auto-migrate {}, timezone {}",
            self.bind,
            self.public_url,
            self.database.display(),
            self.google.describe(),
            self.secret.len(),
            self.auto_migrate,
            self.timezone,
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

    /// The variables a test hands the parser. Nothing here reads the process
    /// environment, so these tests say what they mean whatever the shell has
    /// exported.
    fn vars<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |name| {
            pairs
                .iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| (*value).to_string())
        }
    }

    #[test]
    fn google_bases_default_to_the_real_endpoints() {
        // Nothing configured is the shape a fresh clone gets: the real Google
        // endpoints, no credentials.
        let google = GoogleConfig::from_vars(&vars(&[])).unwrap();
        assert_eq!(google.api_base.as_str(), "https://www.googleapis.com/");
        assert_eq!(google.oauth_base.as_str(), "https://oauth2.googleapis.com/");
        assert_eq!(
            google.accounts_base.as_str(),
            "https://accounts.google.com/"
        );
        assert!(!google.configured());

        // And what a deployment sets is what is used.
        let google = GoogleConfig::from_vars(&vars(&[
            ("GMCP_GOOGLE_CLIENT_ID", "id.apps.googleusercontent.com"),
            ("GMCP_GOOGLE_CLIENT_SECRET", "secret"),
            ("GMCP_GOOGLE_API_BASE", "http://127.0.0.1:9/"),
        ]))
        .unwrap();
        assert!(google.configured());
        assert_eq!(google.api_base.as_str(), "http://127.0.0.1:9/");
        assert_eq!(google.oauth_base.as_str(), "https://oauth2.googleapis.com/");
    }

    #[test]
    fn the_house_zone_defaults_to_warsaw_and_a_typo_stops_the_boot() {
        assert_eq!(timezone(&vars(&[])).unwrap(), chrono_tz::Europe::Warsaw);
        assert_eq!(
            timezone(&vars(&[("GMCP_TIMEZONE", " America/New_York ")])).unwrap(),
            chrono_tz::America::New_York
        );
        // A name the tz database does not know would render every time in the
        // wrong place, so the process refuses to start rather than finding out
        // on the first tool call.
        let error = timezone(&vars(&[("GMCP_TIMEZONE", "Europe/Warszawa")]))
            .unwrap_err()
            .to_string();
        assert!(error.contains("GMCP_TIMEZONE"), "{error}");
    }

    #[test]
    fn dev_mode_will_not_start_on_the_default_bind() {
        let dev = [
            ("GMCP_AUTH", "dev"),
            ("GMCP_PUBLIC_URL", "http://localhost:8000"),
        ];
        // GMCP_BIND unset means 0.0.0.0, which dev auth must never answer on.
        let error = Config::from_vars(&vars(&dev)).unwrap_err();
        assert!(error.to_string().contains("GMCP_BIND"), "{error}");

        let mut with_bind = dev.to_vec();
        with_bind.push(("GMCP_BIND", "127.0.0.1:8000"));
        let config = Config::from_vars(&vars(&with_bind)).unwrap();
        assert!(matches!(config.auth, AuthMode::Dev));
        assert_eq!(config.database, PathBuf::from(DEFAULT_DATABASE));
        assert!(config.auto_migrate);
        // Dev mode with no GMCP_SECRET gets a random one, per process.
        assert_eq!(config.secret.len(), 64);
        // The startup line says which clock the deployment keeps.
        assert_eq!(config.timezone, chrono_tz::Europe::Warsaw);
        assert!(
            config.summary().contains("timezone Europe/Warsaw"),
            "{}",
            config.summary()
        );
    }
}
