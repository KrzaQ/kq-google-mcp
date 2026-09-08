//! Bearer tokens for MCP clients. Only the SHA-256 of the secret is stored;
//! the secret itself is shown once, at creation. Lookup hashes the presented
//! secret and compares hashes, so the comparison is over fixed-length hex and
//! never over the secret, which is what keeps it out of timing's way.
//!
//! [`validate`] is the other half: the rules a request to mint a token has to
//! satisfy whoever is asking. The token API and `gmcp token create` both call
//! it, so a scope list the portal refuses is refused at the terminal too, in
//! the same words.

use base64::Engine;
use sha2::{Digest, Sha256};

use super::scope::{self, Scope, ScopeError};

pub const PREFIX: &str = "gg_";
/// The one scope that is not `service:level`: a trusted gateway (Open WebUI)
/// that names the acting user per request in the `X-Gmcp-User` header.
/// Without the header the token is useless. `domain::scope` parses it.
pub const SCOPE_DELEGATE: &str = "delegate";

pub struct NewToken {
    pub secret: String,
    pub hash: String,
}

pub fn generate() -> NewToken {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).expect("os randomness");
    let secret = format!(
        "{PREFIX}{}",
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
    );
    let hash = hash(&secret);
    NewToken { secret, hash }
}

pub fn hash(secret: &str) -> String {
    hex::encode(Sha256::digest(secret.as_bytes()))
}

/// Who a token is being minted for, as far as the rules care.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Owner<'a> {
    /// A person was named for the token to act as: the CLI's `--user`.
    Named(&'a str),
    /// Nobody was named.
    Unnamed,
    /// The person asking is the person the token acts as, so naming them
    /// would be redundant: a browser session mints its own tokens, and a
    /// delegate token made in one belongs to nobody all the same.
    Caller,
}

/// A request to mint a token, before anything is looked up in the database.
#[derive(Debug, Clone, Copy)]
pub struct Request<'a> {
    pub name: &'a str,
    /// The raw strings, `delegate` included; the registry validates them.
    pub scopes: &'a [String],
    pub owner: Owner<'a>,
    pub all_connections: bool,
    /// How many connections were named for the allowlist.
    pub connections: usize,
}

/// A request that satisfies the rules: the name trimmed, the scopes in
/// canonical order, and what kind of token this is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Valid<'a> {
    pub name: &'a str,
    pub scopes: Vec<Scope>,
    /// The token carries `delegate`: it belongs to nobody.
    pub delegate: bool,
}

/// Why a token was not minted. Every message is written to be shown to
/// whoever asked, whether that is a browser or a terminal.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum RequestError {
    #[error("a token needs a name")]
    NoName,
    #[error(transparent)]
    Scope(#[from] ScopeError),
    #[error(
        "a delegate token belongs to nobody and acts for whoever X-Gmcp-User names; \
         it takes no user of its own"
    )]
    DelegateUser,
    #[error(
        "a delegate token reaches whatever the acting person has flagged for the gateway; \
         it has no connection list of its own"
    )]
    DelegateConnections,
    #[error("a personal token needs the person it acts as")]
    NoUser,
    #[error("a personal token needs every connection of its person, or a list of connections")]
    NoConnections,
}

/// The rules of the plan, in one place: the registry validates the scopes and
/// refuses a write level without its read level; a delegate token takes
/// neither a person nor an allowlist; a personal token takes both.
pub fn validate<'a>(request: Request<'a>) -> Result<Valid<'a>, RequestError> {
    let name = request.name.trim();
    if name.is_empty() {
        return Err(RequestError::NoName);
    }
    let scopes = scope::parse_scopes(request.scopes)?;
    scope::check_requirements(&scopes)?;
    let delegate = scope::is_delegate(&scopes);
    let named = matches!(request.owner, Owner::Named(_));
    if delegate {
        if named {
            return Err(RequestError::DelegateUser);
        }
        if request.all_connections || request.connections > 0 {
            return Err(RequestError::DelegateConnections);
        }
    } else {
        if request.owner == Owner::Unnamed {
            return Err(RequestError::NoUser);
        }
        if !request.all_connections && request.connections == 0 {
            return Err(RequestError::NoConnections);
        }
    }
    Ok(Valid {
        name,
        scopes,
        delegate,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secrets_are_unique_and_hash_stably() {
        let a = generate();
        let b = generate();
        assert_ne!(a.secret, b.secret);
        assert!(a.secret.starts_with("gg_"));
        // 32 bytes of base64url without padding.
        assert_eq!(a.secret.len(), PREFIX.len() + 43);
        assert_eq!(a.hash, hash(&a.secret));
        assert_eq!(a.hash.len(), 64);
        assert!(a.hash.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn the_prefix_is_part_of_what_is_hashed() {
        let t = generate();
        assert_ne!(hash(t.secret.trim_start_matches(PREFIX)), t.hash);
    }

    fn request<'a>(scopes: &'a [String], owner: Owner<'a>) -> Request<'a> {
        Request {
            name: "a token",
            scopes,
            owner,
            all_connections: true,
            connections: 0,
        }
    }

    fn scopes(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn the_registry_is_the_authority_on_a_scope_list() {
        // An unknown scope comes back with every valid one, so whoever asked
        // can see what they meant.
        let bogus = scopes(&["bogus"]);
        let error = validate(request(&bogus, Owner::Caller)).unwrap_err();
        let message = error.to_string();
        assert!(message.contains("unknown scope"), "{message}");
        assert!(message.contains("gmail:read"), "{message}");
        assert!(message.contains("calendar:write"), "{message}");
        assert!(message.contains("delegate"), "{message}");

        // A write level without its read level names the one to add.
        let write = scopes(&["docs:write"]);
        let message = validate(request(&write, Owner::Caller))
            .unwrap_err()
            .to_string();
        assert_eq!(message, "docs:write is useless without docs:read; add it");

        // And a valid list comes back canonical, whatever order it went in.
        let ok = scopes(&["docs:write", "docs:read", "gmail:read"]);
        let valid = validate(request(&ok, Owner::Caller)).unwrap();
        assert_eq!(
            valid
                .scopes
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>(),
            ["gmail:read", "docs:read", "docs:write"]
        );
        assert!(!valid.delegate);
        assert_eq!(valid.name, "a token");
    }

    #[test]
    fn a_delegate_token_takes_neither_a_person_nor_an_allowlist() {
        let list = scopes(&["gmail:read", "delegate"]);
        let valid = validate(Request {
            all_connections: false,
            ..request(&list, Owner::Unnamed)
        })
        .unwrap();
        assert!(valid.delegate);

        assert_eq!(
            validate(Request {
                all_connections: false,
                ..request(&list, Owner::Named("anna@example.test"))
            }),
            Err(RequestError::DelegateUser)
        );
        assert_eq!(
            validate(request(&list, Owner::Unnamed)),
            Err(RequestError::DelegateConnections)
        );
        assert_eq!(
            validate(Request {
                all_connections: false,
                connections: 1,
                ..request(&list, Owner::Unnamed)
            }),
            Err(RequestError::DelegateConnections)
        );
        // A session mints a delegate token without naming anyone: the person
        // asking is the one who made it, not the one it acts as.
        assert!(
            validate(Request {
                all_connections: false,
                ..request(&list, Owner::Caller)
            })
            .is_ok()
        );
    }

    #[test]
    fn a_personal_token_needs_a_person_and_something_to_reach() {
        let list = scopes(&["gmail:read"]);
        assert_eq!(
            validate(request(&list, Owner::Unnamed)),
            Err(RequestError::NoUser)
        );
        assert_eq!(
            validate(Request {
                all_connections: false,
                ..request(&list, Owner::Named("anna@example.test"))
            }),
            Err(RequestError::NoConnections)
        );
        assert!(
            validate(Request {
                all_connections: false,
                connections: 2,
                ..request(&list, Owner::Named("anna@example.test"))
            })
            .is_ok()
        );
        assert!(validate(request(&list, Owner::Caller)).is_ok());
    }

    #[test]
    fn a_token_needs_a_name() {
        let list = scopes(&["gmail:read"]);
        assert_eq!(
            validate(Request {
                name: "  ",
                ..request(&list, Owner::Caller)
            }),
            Err(RequestError::NoName)
        );
        assert_eq!(
            validate(Request {
                name: "  claude code  ",
                ..request(&list, Owner::Caller)
            })
            .unwrap()
            .name,
            "claude code"
        );
    }
}
