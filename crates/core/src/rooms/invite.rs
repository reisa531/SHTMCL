//! Invite secrets, digests and the redemption rules.
//!
//! A secret is displayed **once**, at creation time. Only its SHA-256 digest is
//! stored, so a snapshot, log line or event can never replay it. Usage is
//! consumed inside the same transaction that creates the membership (see
//! [`super::store`]), so a retry of the same request cannot burn a second use.
//!
//! Entropy: the secret comes from the OS CSPRNG through
//! [`shtmcl_platform::random_hex`] (32 bytes = 64 lowercase hex characters). It is
//! never derived from a timestamp, an invite id or a hasher seed, and a failure of
//! the entropy source is **fail-closed**: the command returns
//! `rooms.invite.entropy_unavailable` instead of falling back to something weaker.

use std::fmt;

use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use shtmcl_domain::RoomId;

use super::error::RoomsError;
use super::state::InviteId;

/// Bytes drawn from the OS entropy source for one secret.
pub const SECRET_BYTES: usize = 32;
/// Characters in a displayed secret (`2 * SECRET_BYTES` lowercase hex).
pub const SECRET_LEN: usize = SECRET_BYTES * 2;
pub const DEFAULT_TTL_MS: u64 = 15 * 60 * 1_000;
pub const DEFAULT_USAGE_LIMIT: u32 = 1;
/// Upper bound so a caller cannot create an unbounded-use invite by accident.
pub const MAX_USAGE_LIMIT: u32 = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InviteSpec {
    pub ttl_ms: u64,
    pub usage_limit: u32,
}

impl InviteSpec {
    pub fn new(ttl_ms: u64, usage_limit: u32) -> Result<Self, RoomsError> {
        if ttl_ms == 0 {
            return Err(RoomsError::InvalidRequest {
                detail: "ttlMs must be positive".to_string(),
            });
        }
        if usage_limit == 0 || usage_limit > MAX_USAGE_LIMIT {
            return Err(RoomsError::InvalidRequest {
                detail: format!("usageLimit must be 1-{MAX_USAGE_LIMIT}"),
            });
        }
        Ok(Self {
            ttl_ms,
            usage_limit,
        })
    }

    pub fn from_value(value: &Value) -> Result<Self, RoomsError> {
        let ttl_ms = value
            .get("ttlMs")
            .and_then(Value::as_u64)
            .unwrap_or(DEFAULT_TTL_MS);
        let usage_limit = value
            .get("usageLimit")
            .and_then(Value::as_u64)
            .map(|limit| limit as u32)
            .unwrap_or(DEFAULT_USAGE_LIMIT);
        Self::new(ttl_ms, usage_limit)
    }
}

fn to_hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(DIGITS[(byte >> 4) as usize] as char);
        out.push(DIGITS[(byte & 0x0f) as usize] as char);
    }
    out
}

/// A secret shown exactly once to the creator.
///
/// Fail-closed: when the OS entropy source is unavailable there is no weaker
/// fallback, the caller gets `rooms.invite.entropy_unavailable` and no invite.
pub fn generate_secret() -> Result<String, RoomsError> {
    let secret =
        shtmcl_platform::random_hex(SECRET_BYTES).map_err(|_| RoomsError::EntropyUnavailable)?;
    // Defensive: a platform adapter that returns the wrong shape is a failure, not
    // something to shorten or pad.
    if secret.len() != SECRET_LEN
        || !secret
            .chars()
            .all(|character| character.is_ascii_hexdigit() && !character.is_ascii_uppercase())
    {
        return Err(RoomsError::EntropyUnavailable);
    }
    Ok(secret)
}

/// SHA-256 of the presented secret, hex encoded. Only this value is stored.
pub fn digest_secret(secret: &str) -> String {
    let mut digest = Sha256::new();
    digest.update(secret.as_bytes());
    to_hex(&digest.finalize())
}

/// Compare without an early exit on the first differing byte.
pub fn secrets_match(left: &str, right: &str) -> bool {
    let left = left.as_bytes();
    let right = right.as_bytes();
    if left.len() != right.len() {
        return false;
    }
    let mut difference = 0u8;
    for index in 0..left.len() {
        difference |= left[index] ^ right[index];
    }
    difference == 0
}

/// Stored invite record. Never carries the secret itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Invite {
    pub invite_id: InviteId,
    pub room_id: RoomId,
    pub secret_digest: String,
    pub remaining_uses: u32,
    pub usage_limit: u32,
    pub expires_at_ms: u64,
    pub revoked_at_ms: Option<u64>,
    pub created_at_ms: u64,
    pub revision: shtmcl_domain::Revision,
}

impl Invite {
    pub fn new(
        invite_id: InviteId,
        room_id: RoomId,
        secret_digest: impl Into<String>,
        spec: InviteSpec,
        created_at_ms: u64,
        expires_at_ms: u64,
    ) -> Self {
        Self {
            invite_id,
            room_id,
            secret_digest: secret_digest.into(),
            remaining_uses: spec.usage_limit,
            usage_limit: spec.usage_limit,
            expires_at_ms,
            revoked_at_ms: None,
            created_at_ms,
            revision: shtmcl_domain::Revision::new(1),
        }
    }

    pub fn is_revoked(&self) -> bool {
        self.revoked_at_ms.is_some()
    }

    pub fn is_expired(&self, server_now_ms: u64) -> bool {
        server_now_ms >= self.expires_at_ms
    }

    /// Three distinguishable failures: revoked, expired, exhausted.
    pub fn check_redeemable(&self, server_now_ms: u64) -> Result<(), RoomsError> {
        if self.is_revoked() {
            return Err(RoomsError::InviteRevoked);
        }
        if self.is_expired(server_now_ms) {
            return Err(RoomsError::InviteExpired);
        }
        if self.remaining_uses == 0 {
            return Err(RoomsError::InviteExhausted);
        }
        Ok(())
    }

    /// Consume exactly one use. Called inside the membership transaction.
    pub fn consume(&mut self) -> Result<(), RoomsError> {
        if self.remaining_uses == 0 {
            return Err(RoomsError::InviteExhausted);
        }
        self.remaining_uses -= 1;
        self.revision = self
            .revision
            .checked_next()
            .ok_or_else(|| RoomsError::Internal {
                detail: "invite revision exhausted".to_string(),
            })?;
        Ok(())
    }

    pub fn revoke(&mut self, at_ms: u64) -> Result<bool, RoomsError> {
        if self.revoked_at_ms.is_some() {
            return Ok(false);
        }
        self.revoked_at_ms = Some(at_ms);
        self.revision = self
            .revision
            .checked_next()
            .ok_or_else(|| RoomsError::Internal {
                detail: "invite revision exhausted".to_string(),
            })?;
        Ok(true)
    }

    /// Projection safe for snapshots and events: no digest, no secret.
    pub fn to_value(&self) -> Value {
        json!({
            "inviteId": self.invite_id.as_str(),
            "roomId": self.room_id.as_str(),
            "remainingUses": self.remaining_uses,
            "usageLimit": self.usage_limit,
            "expiresAt": self.expires_at_ms,
            "revokedAt": self.revoked_at_ms,
            "createdAt": self.created_at_ms,
            "revision": self.revision.get(),
            // Signals that a digest exists without exposing it.
            "secretStoredAsDigest": self.secret_digest.len() == 64,
        })
    }
}

impl fmt::Display for Invite {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "invite {} ({} uses left, expires {})",
            self.invite_id, self.remaining_uses, self.expires_at_ms
        )
    }
}

/// Verify a presented secret against the stored digest.
pub fn verify_secret(invite: &Invite, presented: &str) -> Result<(), RoomsError> {
    let presented = presented.trim();
    if presented.is_empty() {
        return Err(RoomsError::InviteInvalid);
    }
    if secrets_match(&digest_secret(presented), &invite.secret_digest) {
        Ok(())
    } else {
        Err(RoomsError::InviteInvalid)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn invite(now: u64, spec: InviteSpec) -> Invite {
        let secret = generate_secret().expect("os entropy");
        Invite::new(
            InviteId::new("invite-1").expect("id"),
            RoomId::new("room-1").expect("id"),
            digest_secret(&secret),
            spec,
            now,
            now + spec.ttl_ms,
        )
    }

    #[test]
    fn secrets_come_from_os_entropy_and_only_the_digest_is_kept() {
        // 32 OS-random bytes rendered as 64 lowercase hex characters.
        assert_eq!(SECRET_LEN, 64);
        let first = generate_secret().expect("os entropy");
        let second = generate_secret().expect("os entropy");
        assert_eq!(first.len(), SECRET_LEN);
        assert_ne!(first, second, "two invites must never share a secret");
        assert!(first
            .chars()
            .all(|character| character.is_ascii_hexdigit() && !character.is_ascii_uppercase()));

        // Nothing in the stored projection or the digest equals the plaintext.
        let stored = invite(1_000, InviteSpec::new(1_000, 1).expect("spec"));
        let encoded = stored.to_value().to_string();
        assert!(!encoded.contains(&first));
        assert!(!encoded.contains(&stored.secret_digest));
        assert_eq!(stored.secret_digest.len(), 64, "sha256 hex digest");
    }

    #[test]
    fn digest_verification_is_exact() {
        let secret = generate_secret().expect("os entropy");
        let stored = Invite::new(
            InviteId::new("invite-1").expect("id"),
            RoomId::new("room-1").expect("id"),
            digest_secret(&secret),
            InviteSpec::new(1_000, 1).expect("spec"),
            1_000,
            2_000,
        );
        verify_secret(&stored, &secret).expect("match");
        assert_eq!(
            verify_secret(&stored, "not-the-secret")
                .expect_err("mismatch")
                .code(),
            super::super::error::codes::INVITE_INVALID
        );
        assert_eq!(
            verify_secret(&stored, "   ").expect_err("empty").code(),
            super::super::error::codes::INVITE_INVALID
        );
    }

    #[test]
    fn expired_revoked_and_exhausted_are_distinguishable() {
        let spec = InviteSpec::new(1_000, 1).expect("spec");
        let mut stored = invite(1_000, spec);
        assert_eq!(stored.check_redeemable(1_500).expect("still valid"), (),);
        assert_eq!(
            stored.check_redeemable(2_500).expect_err("expired").code(),
            super::super::error::codes::INVITE_EXPIRED
        );
        let mut fresh = invite(1_000, spec);
        fresh.revoke(1_200).expect("revoke");
        assert_eq!(
            fresh.check_redeemable(1_100).expect_err("revoked").code(),
            super::super::error::codes::INVITE_REVOKED
        );
        stored.consume().expect("consume");
        assert_eq!(
            stored
                .check_redeemable(1_500)
                .expect_err("exhausted")
                .code(),
            super::super::error::codes::INVITE_EXHAUSTED
        );
        assert_eq!(stored.remaining_uses, 0);
    }

    #[test]
    fn usage_limits_are_bounded() {
        assert!(InviteSpec::new(0, 1).is_err());
        assert!(InviteSpec::new(1_000, 0).is_err());
        assert!(InviteSpec::new(1_000, MAX_USAGE_LIMIT + 1).is_err());
        assert!(InviteSpec::new(1_000, MAX_USAGE_LIMIT).is_ok());
    }
}
