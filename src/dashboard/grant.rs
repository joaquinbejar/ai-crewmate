//! Short-lived, read-only grants for the dashboard.
//!
//! An agent token is a full-privilege credential: it can call every MCP tool.
//! Putting one in a URL leaks it into browser history, copied links, proxy
//! access logs and referrer headers. So the token is presented once, over a
//! header or a form POST, and exchanged for a signed grant that says only
//! "this browser proved it holds a token for team X, until this instant".
//!
//! The grant is stateless — no session table, no shared cache — so any replica
//! can verify one issued by another, as long as they share `BUS_DASHBOARD_SECRET`.

use sha2::{Digest, Sha256};
use uuid::Uuid;

const BLOCK: usize = 64;

/// HMAC-SHA256 (RFC 2104). Implemented here rather than pulling a dependency
/// for fifteen lines; `hmac_matches_rfc4231_vectors` pins it to the standard
/// test vectors.
fn hmac_sha256(key: &[u8], message: &[u8]) -> [u8; 32] {
    let mut block = [0u8; BLOCK];
    if key.len() > BLOCK {
        block[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        block[..key.len()].copy_from_slice(key);
    }

    let mut ipad = [0x36u8; BLOCK];
    let mut opad = [0x5cu8; BLOCK];
    for i in 0..BLOCK {
        ipad[i] ^= block[i];
        opad[i] ^= block[i];
    }

    let mut inner = Sha256::new();
    inner.update(ipad);
    inner.update(message);
    let inner = inner.finalize();

    let mut outer = Sha256::new();
    outer.update(opad);
    outer.update(inner);
    outer.finalize().into()
}

/// Constant-time comparison, so a wrong grant leaks nothing through timing.
fn eq_ct(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Mint a grant for `team_id`, valid for `ttl_secs` from `now_unix`.
pub fn issue(secret: &[u8], team_id: Uuid, now_unix: i64, ttl_secs: i64) -> String {
    let payload = format!("{team_id}.{}", now_unix + ttl_secs);
    let tag = hmac_sha256(secret, payload.as_bytes());
    format!("{payload}.{}", hex::encode(tag))
}

/// Verify a grant and return the team it was issued for.
pub fn verify(secret: &[u8], grant: &str, now_unix: i64) -> Option<Uuid> {
    // team_id . expiry . tag
    let (payload, tag_hex) = grant.rsplit_once('.')?;
    let (team, expiry) = payload.rsplit_once('.')?;

    let expected = hmac_sha256(secret, payload.as_bytes());
    let presented = hex::decode(tag_hex).ok()?;
    if !eq_ct(&expected, &presented) {
        return None;
    }
    if expiry.parse::<i64>().ok()? <= now_unix {
        return None;
    }
    Uuid::parse_str(team).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// RFC 4231 test cases 1 and 2 — if these pass, the construction is HMAC.
    #[test]
    fn hmac_matches_rfc4231_vectors() {
        let tag = hmac_sha256(&[0x0b; 20], b"Hi There");
        assert_eq!(
            hex::encode(tag),
            "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7"
        );

        let tag = hmac_sha256(b"Jefe", b"what do ya want for nothing?");
        assert_eq!(
            hex::encode(tag),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );

        // Case 3 exercises the >block-size key path.
        let tag = hmac_sha256(
            &[0xaa; 131],
            b"Test Using Larger Than Block-Size Key - Hash Key First",
        );
        assert_eq!(
            hex::encode(tag),
            "60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54"
        );
    }

    #[test]
    fn round_trips_within_its_lifetime() {
        let team = Uuid::new_v4();
        let g = issue(b"secret", team, 1_000, 60);
        assert_eq!(verify(b"secret", &g, 1_030), Some(team));
    }

    #[test]
    fn rejects_expired_grants() {
        let g = issue(b"secret", Uuid::new_v4(), 1_000, 60);
        assert_eq!(verify(b"secret", &g, 1_061), None, "past its expiry");
    }

    #[test]
    fn rejects_a_different_secret() {
        let g = issue(b"secret", Uuid::new_v4(), 1_000, 60);
        assert_eq!(verify(b"other", &g, 1_010), None);
    }

    #[test]
    fn rejects_tampering() {
        let team = Uuid::new_v4();
        let g = issue(b"secret", team, 1_000, 60);

        // Extending the expiry invalidates the tag.
        let (payload, tag) = g.rsplit_once('.').expect("shape");
        let (team_part, _) = payload.rsplit_once('.').expect("shape");
        let forged = format!("{team_part}.9999999999.{tag}");
        assert_eq!(verify(b"secret", &forged, 1_010), None);

        // So does swapping in another team.
        let other = Uuid::new_v4();
        let forged = format!("{other}.{}", g.split_once('.').expect("shape").1);
        assert_eq!(verify(b"secret", &forged, 1_010), None);

        assert_eq!(verify(b"secret", "garbage", 1_010), None);
        assert_eq!(verify(b"secret", "", 1_010), None);
    }
}
