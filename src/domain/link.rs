//! Download links. A file never leaves through MCP content: a tool mints a
//! link, the model or the person fetches it, and the server streams from
//! Google on each hit and stores nothing. The id is the whole capability, so
//! it is random and short-lived; the numbers behind that live in
//! [`super::limits`].

use base64::Engine;
use chrono::{DateTime, TimeDelta, Utc};

use super::limits::{DOWNLOAD_MAX_BYTES, LINK_TTL_MINUTES, LINK_USES};

/// 16 random bytes, which is 22 characters of base64url without padding.
const ID_BYTES: usize = 16;
// Read by the CLI and the token grid of the later steps.
#[allow(dead_code)]
pub const ID_LEN: usize = 22;

/// A fresh link id. Unguessable is the point: the route is unauthenticated,
/// so knowing the id is the permission.
pub fn new_id() -> String {
    let mut bytes = [0u8; ID_BYTES];
    getrandom::fill(&mut bytes).expect("os randomness");
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// How long a link lives, as a duration.
pub fn ttl() -> TimeDelta {
    TimeDelta::minutes(LINK_TTL_MINUTES)
}

/// The `expires_at` of a link minted now. The clock is passed in so callers
/// and tests share one.
pub fn expires_at(now: DateTime<Utc>) -> DateTime<Utc> {
    now + ttl()
}

/// What a fresh link's `uses_left` starts at: enough for a retry and a second
/// reader, not enough to be a hosting service.
pub fn uses() -> i64 {
    LINK_USES
}

/// A file larger than this is refused rather than streamed.
#[allow(dead_code)]
pub fn too_large(size: u64) -> bool {
    size > DOWNLOAD_MAX_BYTES
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_unguessable_and_url_safe() {
        let a = new_id();
        let b = new_id();
        assert_ne!(a, b);
        assert_eq!(a.len(), ID_LEN);
        assert!(
            a.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
            "{a}"
        );
    }

    #[test]
    fn a_link_lives_fifteen_minutes_and_is_used_three_times() {
        let now: DateTime<Utc> = "2026-09-08T12:00:00Z".parse().unwrap();
        assert_eq!(
            expires_at(now),
            "2026-09-08T12:15:00Z".parse::<DateTime<Utc>>().unwrap()
        );
        assert_eq!(ttl().num_minutes(), 15);
        assert_eq!(uses(), 3);
    }

    #[test]
    fn the_download_cap_is_fifty_megabytes() {
        assert!(!too_large(50 * 1024 * 1024));
        assert!(too_large(50 * 1024 * 1024 + 1));
    }
}
