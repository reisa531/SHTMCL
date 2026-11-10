//! Leases and connection budgets.
//!
//! Every deadline here is a **service-side** deadline: the only time source this
//! module accepts is [`super::ports::ServiceClock`], which returns the room
//! service's own clock. A local wall clock is never an authorization source, and
//! `None` (no authority) is a refusal, not "probably fine".
//!
//! Numbers follow `docs/development/06-rooms-and-p2p.md` §5/§6: heartbeat 20 s,
//! member/connection lease 60 s, direct probe budget 3 s, relay adds 5 s, tunnel
//! buffer 2 MiB, and an online removal must take effect within ~2 s while a broken
//! control link may live until the lease deadline.

use serde_json::{json, Value};

use super::error::RoomsError;

pub const HEARTBEAT_INTERVAL_MS: u64 = 20_000;
pub const MEMBER_LEASE_MS: u64 = 60_000;
pub const CONNECTION_LEASE_MS: u64 = 60_000;
pub const DIRECT_BUDGET_MS: u64 = 3_000;
pub const RELAY_EXTRA_BUDGET_MS: u64 = 5_000;
pub const ONLINE_DISCONNECT_GRACE_MS: u64 = 2_000;
pub const CONNECTION_BUFFER_LIMIT: usize = 2 * 1024 * 1024;
/// The tunnel authorization may never outlive the member lease by more than this.
pub const RELAY_GRANT_LIMIT_BYTES: u64 = 512 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LeaseCheck {
    /// Still valid: the deadline and how much service time is left.
    Valid {
        expires_at_ms: u64,
        remaining_ms: u64,
    },
    Expired {
        expires_at_ms: u64,
        by_ms: u64,
    },
    /// No authoritative service time, or no lease at all.
    Unavailable,
}

/// Compare a service-side deadline with the service clock.
pub fn check(server_now_ms: Option<u64>, lease_until_ms: Option<u64>) -> LeaseCheck {
    match (server_now_ms, lease_until_ms) {
        (Some(now), Some(expires_at_ms)) => {
            if now >= expires_at_ms {
                LeaseCheck::Expired {
                    expires_at_ms,
                    by_ms: now - expires_at_ms,
                }
            } else {
                LeaseCheck::Valid {
                    expires_at_ms,
                    remaining_ms: expires_at_ms - now,
                }
            }
        }
        _ => LeaseCheck::Unavailable,
    }
}

/// Lease gate used by every command that needs current authorization.
///
/// Returns the deadline when the lease is valid; otherwise a stable error:
/// `rooms.lease.unauthorized` when no authority exists and `rooms.lease.expired`
/// when the deadline has passed.
pub fn require_valid(
    server_now_ms: Option<u64>,
    lease_until_ms: Option<u64>,
) -> Result<u64, RoomsError> {
    match check(server_now_ms, lease_until_ms) {
        LeaseCheck::Valid { expires_at_ms, .. } => Ok(expires_at_ms),
        LeaseCheck::Expired { by_ms, .. } => Err(RoomsError::LeaseExpired {
            expired_by_ms: by_ms,
        }),
        LeaseCheck::Unavailable => Err(RoomsError::LeaseUnauthorized),
    }
}

/// A fresh deadline from the service clock. Refuses to invent time.
pub fn deadline(server_now_ms: Option<u64>, ttl_ms: u64) -> Result<u64, RoomsError> {
    let now = server_now_ms.ok_or(RoomsError::LeaseUnauthorized)?;
    Ok(now.saturating_add(ttl_ms))
}

/// The earlier of two deadlines: a tunnel authorization must never outlive the
/// member lease that justified it.
pub fn earliest(first: u64, second: Option<u64>) -> u64 {
    match second {
        Some(second) => first.min(second),
        None => first,
    }
}

/// Direct-then-relay attempt budget. The clock values come from the service, so a
/// test can drive a timeout without sleeping.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConnectBudget {
    pub start_ms: u64,
    pub direct_ms: u64,
    pub relay_extra_ms: u64,
}

impl ConnectBudget {
    pub fn new(start_ms: u64) -> Self {
        Self {
            start_ms,
            direct_ms: DIRECT_BUDGET_MS,
            relay_extra_ms: RELAY_EXTRA_BUDGET_MS,
        }
    }

    pub fn with_direct(start_ms: u64, direct_ms: u64) -> Self {
        Self {
            start_ms,
            direct_ms,
            relay_extra_ms: RELAY_EXTRA_BUDGET_MS,
        }
    }

    pub fn direct_deadline_ms(&self) -> u64 {
        self.start_ms.saturating_add(self.direct_ms)
    }

    pub fn relay_deadline_ms(&self) -> u64 {
        self.direct_deadline_ms()
            .saturating_add(self.relay_extra_ms)
    }

    /// Service time if the direct attempt is still inside its budget.
    pub fn direct_remaining_ms(&self, at_ms: u64) -> u64 {
        self.direct_deadline_ms().saturating_sub(at_ms)
    }

    pub fn relay_remaining_ms(&self, at_ms: u64) -> u64 {
        self.relay_deadline_ms().saturating_sub(at_ms)
    }

    pub fn direct_allows(&self, at_ms: u64) -> bool {
        at_ms <= self.direct_deadline_ms()
    }

    pub fn relay_allows(&self, at_ms: u64) -> bool {
        at_ms <= self.relay_deadline_ms()
    }

    pub fn to_value(&self) -> Value {
        json!({
            "startedAt": self.start_ms,
            "directBudgetMs": self.direct_ms,
            "relayExtraMs": self.relay_extra_ms,
            "directDeadline": self.direct_deadline_ms(),
            "relayDeadline": self.relay_deadline_ms(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_authority_is_a_refusal_not_a_guess() {
        assert_eq!(
            require_valid(None, Some(5_000))
                .expect_err("no clock")
                .code(),
            super::super::error::codes::LEASE_UNAUTHORIZED
        );
        assert_eq!(
            require_valid(Some(1_000), None)
                .expect_err("no lease")
                .code(),
            super::super::error::codes::LEASE_UNAUTHORIZED
        );
        assert_eq!(
            deadline(None, MEMBER_LEASE_MS)
                .expect_err("no clock")
                .code(),
            super::super::error::codes::LEASE_UNAUTHORIZED
        );
    }

    #[test]
    fn expired_leases_report_how_late_they_are() {
        let error = require_valid(Some(6_500), Some(5_000)).expect_err("expired");
        assert_eq!(error.code(), super::super::error::codes::LEASE_EXPIRED);
        assert_eq!(
            check(Some(6_500), Some(5_000)),
            LeaseCheck::Expired {
                expires_at_ms: 5_000,
                by_ms: 1_500
            }
        );
        assert_eq!(
            require_valid(Some(1_000), Some(5_000)).expect("valid"),
            5_000
        );
    }

    #[test]
    fn leases_never_outlive_the_member_lease() {
        assert_eq!(earliest(60_000, Some(30_000)), 30_000);
        assert_eq!(earliest(60_000, None), 60_000);
        assert_eq!(
            deadline(Some(1_000), MEMBER_LEASE_MS).expect("deadline"),
            61_000
        );
    }

    #[test]
    fn direct_budget_precedes_the_relay_budget() {
        let budget = ConnectBudget::new(10_000);
        assert_eq!(budget.direct_deadline_ms(), 13_000);
        assert_eq!(budget.relay_deadline_ms(), 18_000);
        assert!(budget.direct_allows(12_999));
        assert!(!budget.direct_allows(13_500));
        assert!(budget.relay_allows(15_000));
        assert!(!budget.relay_allows(18_500));
    }
}
