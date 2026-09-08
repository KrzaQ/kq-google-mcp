//! Refresh tokens are sealed before they touch the database. A `.db` file is
//! too easy to copy around to leave a long-lived Google grant in the clear on
//! it, even on an encrypted dataset.
//!
//! AES-256-GCM under `HKDF-SHA256(GMCP_SECRET, info = "gmcp/refresh-token/v1")`
//! with a fresh 12-byte nonce stored in front of the ciphertext. The key is
//! derived rather than used directly so that the same `GMCP_SECRET` can sign
//! cookies without the two uses sharing key material. Rotating `GMCP_SECRET`
//! makes every stored token unopenable, which is what `gmcp check-secret`
//! reports.

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Nonce};
use hkdf::Hkdf;
use sha2::Sha256;

const INFO: &[u8] = b"gmcp/refresh-token/v1";
const NONCE_LEN: usize = 12;
/// AES-GCM appends a 16-byte tag, so anything shorter than a nonce plus a tag
/// cannot be something this module wrote.
const TAG_LEN: usize = 16;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum SealError {
    #[error("the sealed value is too short to be a sealed refresh token")]
    Malformed,
    #[error("the refresh token could not be opened; GMCP_SECRET may have changed")]
    Decrypt,
    #[error("the opened refresh token is not valid UTF-8")]
    Utf8,
}

fn cipher(secret: &[u8]) -> Aes256Gcm {
    let mut key = [0u8; 32];
    Hkdf::<Sha256>::new(None, secret)
        .expand(INFO, &mut key)
        .expect("32 bytes is a valid HKDF-SHA256 output length");
    Aes256Gcm::new_from_slice(&key).expect("a 32-byte AES-256 key")
}

/// `nonce || ciphertext+tag`, ready for the `refresh_token_sealed` BLOB.
pub fn seal(secret: &[u8], plaintext: &str) -> Vec<u8> {
    let mut nonce = [0u8; NONCE_LEN];
    getrandom::fill(&mut nonce).expect("os randomness");
    let ciphertext = cipher(secret)
        .encrypt(Nonce::from_slice(&nonce), plaintext.as_bytes())
        .expect("aes-gcm encryption of a refresh token cannot fail");
    let mut sealed = Vec::with_capacity(NONCE_LEN + ciphertext.len());
    sealed.extend_from_slice(&nonce);
    sealed.extend_from_slice(&ciphertext);
    sealed
}

/// The inverse. Every failure is the same to the caller in kind — the token
/// is unusable and the connection has to be made again — but the variants
/// tell a corrupt row from a rotated secret in the log.
pub fn open(secret: &[u8], sealed: &[u8]) -> Result<String, SealError> {
    if sealed.len() < NONCE_LEN + TAG_LEN {
        return Err(SealError::Malformed);
    }
    let (nonce, ciphertext) = sealed.split_at(NONCE_LEN);
    let plaintext = cipher(secret)
        .decrypt(Nonce::from_slice(nonce), ciphertext)
        .map_err(|_| SealError::Decrypt)?;
    String::from_utf8(plaintext).map_err(|_| SealError::Utf8)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET: &[u8] = b"a development secret of thirty-two bytes or more";
    const TOKEN: &str = "1//0eXaMpLe-refresh-token_value";

    fn flip(sealed: &[u8], at: usize) -> Vec<u8> {
        let mut tampered = sealed.to_vec();
        tampered[at] ^= 0x01;
        tampered
    }

    #[test]
    fn round_trip() {
        let sealed = seal(SECRET, TOKEN);
        assert_eq!(open(SECRET, &sealed).unwrap(), TOKEN);
        // The plaintext is nowhere in the blob, and the nonce is in front.
        assert!(!sealed.windows(TOKEN.len()).any(|w| w == TOKEN.as_bytes()));
        assert_eq!(sealed.len(), NONCE_LEN + TOKEN.len() + TAG_LEN);
    }

    #[test]
    fn every_sealing_uses_a_fresh_nonce() {
        let a = seal(SECRET, TOKEN);
        let b = seal(SECRET, TOKEN);
        assert_ne!(a, b);
        assert_ne!(a[..NONCE_LEN], b[..NONCE_LEN]);
        assert_eq!(open(SECRET, &b).unwrap(), TOKEN);
    }

    #[test]
    fn a_rotated_secret_cannot_open_anything() {
        let sealed = seal(SECRET, TOKEN);
        let other = b"a different development secret, also long enough";
        assert_eq!(open(other, &sealed), Err(SealError::Decrypt));
    }

    #[test]
    fn tampering_is_detected() {
        let sealed = seal(SECRET, TOKEN);
        assert_eq!(open(SECRET, &flip(&sealed, 0)), Err(SealError::Decrypt));
        assert_eq!(
            open(SECRET, &flip(&sealed, NONCE_LEN + 1)),
            Err(SealError::Decrypt)
        );
        let tag = sealed.len() - 1;
        assert_eq!(open(SECRET, &flip(&sealed, tag)), Err(SealError::Decrypt));
    }

    #[test]
    fn short_input_fails_cleanly() {
        assert_eq!(open(SECRET, &[]), Err(SealError::Malformed));
        assert_eq!(open(SECRET, &[0u8; NONCE_LEN]), Err(SealError::Malformed));
        assert_eq!(
            open(SECRET, &[0u8; NONCE_LEN + TAG_LEN - 1]),
            Err(SealError::Malformed)
        );
        // Long enough to try, still not ours.
        assert_eq!(
            open(SECRET, &[0u8; NONCE_LEN + TAG_LEN]),
            Err(SealError::Decrypt)
        );
    }

    #[test]
    fn the_empty_token_survives_the_trip() {
        let sealed = seal(SECRET, "");
        assert_eq!(open(SECRET, &sealed).unwrap(), "");
    }
}
