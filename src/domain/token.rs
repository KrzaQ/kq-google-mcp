//! Bearer tokens for MCP clients. Only the SHA-256 of the secret is stored;
//! the secret itself is shown once, at creation. Lookup hashes the presented
//! secret and compares hashes, so the comparison is over fixed-length hex and
//! never over the secret, which is what keeps it out of timing's way.

use base64::Engine;
use sha2::{Digest, Sha256};

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
}
