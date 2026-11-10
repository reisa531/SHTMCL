//! Module-local error vocabulary for 06 (rooms, membership, leases, tunnel).
//!
//! Every variant maps to a stable code that matches the public error-code format
//! `^[a-z][a-z0-9._-]{2,63}$` and to one of the frozen [`ErrorCategory`] values.
//! The namespace belongs to this module (see `contracts/domain-v1.md` section 7):
//! the first segment is always `rooms`, so `scripts/check-codes.mjs` can verify
//! ownership without 00 having to register anything centrally.
//!
//! Every code declared here is reachable in this build; a code that only a future
//! adapter could produce is added together with that adapter, so the vocabulary never
//! advertises behaviour that does not exist yet.
//!
//! Envelopes carry no secret, invite digest, candidate address or token.

use std::fmt;

use shtmcl_domain::{
    ErrorCategory, ErrorCode, ErrorEnvelope, USER_ACTION_OPEN_DETAILS, USER_ACTION_RELOAD,
    USER_ACTION_RETRY,
};

/// Stable codes produced by this module.
pub mod codes {
    pub const COMMAND_UNKNOWN: &str = "rooms.command.unknown";
    pub const REQUEST_INVALID: &str = "rooms.request.invalid";
    pub const REQUEST_IDEMPOTENCY_CONFLICT: &str = "rooms.request.idempotency_conflict";
    pub const IDENTITY_UNAVAILABLE: &str = "rooms.identity.unavailable";
    pub const IDENTITY_UNVERIFIED: &str = "rooms.identity.unverified";
    pub const IDENTITY_MISMATCH: &str = "rooms.identity.mismatch";
    pub const OWNER_UNAVAILABLE: &str = "rooms.owner.unavailable";
    pub const LAUNCH_UNSUPPORTED: &str = "rooms.launch.unsupported";
    pub const SIGNALING_UNAVAILABLE: &str = "rooms.signaling.unavailable";
    pub const RELAY_UNAVAILABLE: &str = "rooms.relay.unavailable";
    pub const LOOPBACK_UNAVAILABLE: &str = "rooms.loopback.unavailable";
    pub const ROOM_NOT_FOUND: &str = "rooms.room.not_found";
    pub const ROOM_CONFLICT: &str = "rooms.room.conflict";
    pub const ROOM_TERMINAL: &str = "rooms.room.terminal";
    pub const ROOM_EXPIRED: &str = "rooms.room.expired";
    pub const ROOM_JOINS_CLOSED: &str = "rooms.room.joins_closed";
    pub const OWNER_REQUIRED: &str = "rooms.room.owner_required";
    pub const INVITE_NOT_FOUND: &str = "rooms.invite.not_found";
    pub const INVITE_INVALID: &str = "rooms.invite.invalid";
    pub const INVITE_EXPIRED: &str = "rooms.invite.expired";
    pub const INVITE_REVOKED: &str = "rooms.invite.revoked";
    pub const INVITE_EXHAUSTED: &str = "rooms.invite.exhausted";
    /// The OS entropy source is unavailable, so no secret could be created.
    pub const INVITE_ENTROPY_UNAVAILABLE: &str = "rooms.invite.entropy_unavailable";
    pub const MEMBERSHIP_NOT_FOUND: &str = "rooms.membership.not_found";
    pub const MEMBERSHIP_TERMINAL: &str = "rooms.membership.terminal";
    pub const MEMBERSHIP_CONFLICT: &str = "rooms.membership.conflict";
    pub const CONNECTION_NOT_FOUND: &str = "rooms.connection.not_found";
    pub const CONNECTION_REJECTED: &str = "rooms.connection.rejected";
    pub const CONNECTION_NO_PATH: &str = "rooms.connection.no_path";
    pub const AUTHORIZATION_STALE: &str = "rooms.authorization.stale_generation";
    pub const LEASE_EXPIRED: &str = "rooms.lease.expired";
    pub const LEASE_UNAUTHORIZED: &str = "rooms.lease.unauthorized";
    pub const STATE_INVALID_TRANSITION: &str = "rooms.state.invalid_transition";
    pub const TUNNEL_BINDING_INVALID: &str = "rooms.tunnel.binding_invalid";
    pub const TUNNEL_SEQUENCE_GAP: &str = "rooms.tunnel.sequence_gap";
    pub const TUNNEL_FRAME_INVALID: &str = "rooms.tunnel.frame_invalid";
    pub const TUNNEL_CLOSED: &str = "rooms.tunnel.closed";
    pub const INTERNAL: &str = "rooms.internal";
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RoomsError {
    /// `payload.command` is not a command this module owns.
    UnknownCommand,
    /// A payload field is missing, empty or of the wrong JSON type.
    InvalidRequest {
        detail: String,
    },
    /// The same idempotency key arrived with a different request body.
    IdempotencyConflict {
        key: String,
    },
    /// The owner of a consumed port (01, 02, 05, signalling, relay) is absent.
    OwnerUnavailable {
        owner: &'static str,
    },
    IdentityUnavailable,
    IdentityUnverified,
    /// The caller is not the member it claims to act for.
    IdentityMismatch {
        detail: String,
    },
    LaunchUnsupported {
        reason: String,
    },
    SignalingUnavailable,
    RelayUnavailable,
    LoopbackUnavailable,
    RoomNotFound {
        room_id: String,
    },
    RoomConflict {
        expected: u64,
        actual: u64,
    },
    RoomTerminal {
        state: String,
    },
    /// The room's own deadline passed; distinct from an owner action.
    RoomExpired,
    RoomJoinsClosed,
    /// Only the room owner may perform this action.
    OwnerRequired,
    InviteNotFound,
    InviteInvalid,
    InviteExpired,
    InviteRevoked,
    InviteExhausted,
    /// No OS entropy: no secret can be created, and nothing weaker is used.
    EntropyUnavailable,
    MembershipNotFound {
        membership_id: String,
    },
    /// The membership already reached a terminal approval state.
    MembershipTerminal {
        state: String,
    },
    MembershipConflict {
        expected: u64,
        actual: u64,
    },
    ConnectionNotFound {
        connection_id: String,
    },
    ConnectionRejected {
        detail: String,
    },
    /// The caller presented an authorization generation that is no longer current.
    ///
    /// Field direction follows `shtmcl_domain::TaskError::StaleGeneration`: `expected`
    /// carries the **system's current truth**, `actual` carries what the caller (or the
    /// recorded connection) presented. A stale caller must never be told that its own
    /// value is "expected" — the message and `detail_ref` say `current` / `presented`
    /// so nothing has to be inferred from the field names.
    StaleAuthorization {
        expected: u64,
        actual: u64,
    },
    /// Neither the direct nor the relay path produced a usable path.
    NoPathFound,
    LeaseExpired {
        expired_by_ms: u64,
    },
    /// No authoritative service time, so no lease can be authorized.
    LeaseUnauthorized,
    InvalidTransition {
        entity: &'static str,
        from: &'static str,
        to: &'static str,
    },
    TunnelBindingInvalid {
        detail: String,
    },
    /// A control frame arrived out of order: the stream cannot be trusted.
    TunnelSequenceGap {
        expected: u64,
        actual: u64,
    },
    TunnelFrameInvalid {
        detail: String,
    },
    TunnelClosed,
    Internal {
        detail: String,
    },
}

impl RoomsError {
    pub fn code(&self) -> &'static str {
        use codes::*;
        match self {
            Self::UnknownCommand => COMMAND_UNKNOWN,
            Self::InvalidRequest { .. } => REQUEST_INVALID,
            Self::IdempotencyConflict { .. } => REQUEST_IDEMPOTENCY_CONFLICT,
            Self::OwnerUnavailable { .. } => OWNER_UNAVAILABLE,
            Self::IdentityUnavailable => IDENTITY_UNAVAILABLE,
            Self::IdentityUnverified => IDENTITY_UNVERIFIED,
            Self::IdentityMismatch { .. } => IDENTITY_MISMATCH,
            Self::LaunchUnsupported { .. } => LAUNCH_UNSUPPORTED,
            Self::SignalingUnavailable => SIGNALING_UNAVAILABLE,
            Self::RelayUnavailable => RELAY_UNAVAILABLE,
            Self::LoopbackUnavailable => LOOPBACK_UNAVAILABLE,
            Self::RoomNotFound { .. } => ROOM_NOT_FOUND,
            Self::RoomConflict { .. } => ROOM_CONFLICT,
            Self::RoomTerminal { .. } => ROOM_TERMINAL,
            Self::RoomExpired => ROOM_EXPIRED,
            Self::RoomJoinsClosed => ROOM_JOINS_CLOSED,
            Self::OwnerRequired => OWNER_REQUIRED,
            Self::InviteNotFound => INVITE_NOT_FOUND,
            Self::InviteInvalid => INVITE_INVALID,
            Self::InviteExpired => INVITE_EXPIRED,
            Self::InviteRevoked => INVITE_REVOKED,
            Self::InviteExhausted => INVITE_EXHAUSTED,
            Self::EntropyUnavailable => INVITE_ENTROPY_UNAVAILABLE,
            Self::MembershipNotFound { .. } => MEMBERSHIP_NOT_FOUND,
            Self::MembershipTerminal { .. } => MEMBERSHIP_TERMINAL,
            Self::MembershipConflict { .. } => MEMBERSHIP_CONFLICT,
            Self::ConnectionNotFound { .. } => CONNECTION_NOT_FOUND,
            Self::ConnectionRejected { .. } => CONNECTION_REJECTED,
            Self::NoPathFound => CONNECTION_NO_PATH,
            Self::StaleAuthorization { .. } => AUTHORIZATION_STALE,
            Self::LeaseExpired { .. } => LEASE_EXPIRED,
            Self::LeaseUnauthorized => LEASE_UNAUTHORIZED,
            Self::InvalidTransition { .. } => STATE_INVALID_TRANSITION,
            Self::TunnelBindingInvalid { .. } => TUNNEL_BINDING_INVALID,
            Self::TunnelSequenceGap { .. } => TUNNEL_SEQUENCE_GAP,
            Self::TunnelFrameInvalid { .. } => TUNNEL_FRAME_INVALID,
            Self::TunnelClosed => TUNNEL_CLOSED,
            Self::Internal { .. } => INTERNAL,
        }
    }

    pub fn category(&self) -> ErrorCategory {
        match self {
            Self::OwnerUnavailable { .. }
            | Self::IdentityUnavailable
            | Self::SignalingUnavailable
            | Self::RelayUnavailable
            | Self::LoopbackUnavailable
            | Self::LaunchUnsupported { .. } => ErrorCategory::Unsupported,
            Self::IdentityUnverified | Self::IdentityMismatch { .. } | Self::OwnerRequired => {
                ErrorCategory::PermissionDenied
            }
            Self::RoomNotFound { .. }
            | Self::InviteNotFound
            | Self::MembershipNotFound { .. }
            | Self::ConnectionNotFound { .. } => ErrorCategory::NotFound,
            Self::InvalidRequest { .. }
            | Self::UnknownCommand
            | Self::TunnelFrameInvalid { .. }
            | Self::TunnelBindingInvalid { .. } => ErrorCategory::InvalidRequest,
            Self::RoomConflict { .. }
            | Self::MembershipConflict { .. }
            | Self::IdempotencyConflict { .. }
            | Self::NoPathFound
            | Self::InviteExhausted
            | Self::InviteInvalid
            | Self::InviteExpired
            | Self::InviteRevoked
            | Self::RoomTerminal { .. }
            | Self::RoomExpired
            | Self::RoomJoinsClosed
            | Self::MembershipTerminal { .. }
            | Self::StaleAuthorization { .. }
            | Self::LeaseExpired { .. }
            | Self::LeaseUnauthorized
            | Self::InvalidTransition { .. }
            | Self::ConnectionRejected { .. }
            | Self::TunnelSequenceGap { .. }
            | Self::TunnelClosed => ErrorCategory::Conflict,
            Self::EntropyUnavailable => ErrorCategory::Internal,
            Self::Internal { .. } => ErrorCategory::Internal,
        }
    }

    pub fn retryable(&self) -> bool {
        matches!(
            self,
            Self::SignalingUnavailable
                | Self::RelayUnavailable
                | Self::OwnerUnavailable { .. }
                | Self::NoPathFound
                // An entropy source can be momentarily unavailable; retrying is the
                // only honest recovery, and no weaker value is ever substituted.
                | Self::EntropyUnavailable
        )
    }

    pub fn user_action(&self) -> Option<&'static str> {
        match self {
            Self::InviteExpired
            | Self::InviteRevoked
            | Self::InviteExhausted
            | Self::RoomJoinsClosed
            | Self::RoomTerminal { .. }
            | Self::RoomExpired
            | Self::MembershipTerminal { .. }
            | Self::StaleAuthorization { .. }
            | Self::LeaseExpired { .. } => Some(USER_ACTION_RELOAD),
            Self::NoPathFound
            | Self::SignalingUnavailable
            | Self::RelayUnavailable
            | Self::EntropyUnavailable => Some(USER_ACTION_RETRY),
            Self::OwnerUnavailable { .. } | Self::LaunchUnsupported { .. } => {
                Some(USER_ACTION_OPEN_DETAILS)
            }
            _ => None,
        }
    }

    /// Opaque detail reference. Never contains a secret, digest, token or address.
    pub fn detail_ref(&self) -> Option<String> {
        match self {
            Self::RoomNotFound { room_id } => Some(format!("rooms:room:{room_id}")),
            Self::MembershipNotFound { membership_id } => {
                Some(format!("rooms:membership:{membership_id}"))
            }
            Self::ConnectionNotFound { connection_id } => {
                Some(format!("rooms:connection:{connection_id}"))
            }
            Self::RoomConflict { expected, actual } => {
                Some(format!("rooms:revision:{expected}->{actual}"))
            }
            Self::MembershipConflict { expected, actual } => {
                Some(format!("rooms:membership-revision:{expected}->{actual}"))
            }
            Self::StaleAuthorization { expected, actual } => Some(format!(
                "rooms:generation:current={expected}:presented={actual}"
            )),
            Self::LeaseExpired { expired_by_ms } => {
                Some(format!("rooms:lease:expired:{expired_by_ms}"))
            }
            Self::InvalidTransition { entity, from, to } => {
                Some(format!("rooms:transition:{entity}:{from}->{to}"))
            }
            Self::IdempotencyConflict { key } => Some(format!("rooms:idempotency:{key}")),
            Self::TunnelSequenceGap { expected, actual } => {
                Some(format!("rooms:tunnel:{expected}->{actual}"))
            }
            Self::OwnerUnavailable { owner } => Some(format!("rooms:owner:{owner}")),
            _ => None,
        }
    }

    /// Convert to the frozen error envelope. No stack trace, credential, digest or
    /// candidate address is ever attached.
    pub fn to_envelope(&self) -> ErrorEnvelope {
        let code = ErrorCode::new(self.code())
            .unwrap_or_else(|_| ErrorEnvelope::internal_unavailable().code);
        let mut envelope = ErrorEnvelope::new(code, self.category(), self.retryable());
        if let Some(action) = self.user_action() {
            envelope = envelope.with_user_action(action);
        }
        if let Some(detail) = self.detail_ref() {
            envelope = envelope.with_detail_ref(detail);
        }
        envelope
    }
}

impl fmt::Display for RoomsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownCommand => f.write_str("unknown rooms command"),
            Self::InvalidRequest { detail } => write!(f, "invalid rooms request: {detail}"),
            Self::IdempotencyConflict { key } => {
                write!(f, "idempotency key {key} was reused with a different body")
            }
            Self::OwnerUnavailable { owner } => write!(f, "port owner {owner} is not wired"),
            Self::IdentityUnavailable => f.write_str("no platform identity"),
            Self::IdentityUnverified => f.write_str("platform identity is not verified"),
            Self::IdentityMismatch { detail } => write!(f, "identity mismatch: {detail}"),
            Self::LaunchUnsupported { reason } => write!(f, "launch cannot join here: {reason}"),
            Self::SignalingUnavailable => f.write_str("signalling service unavailable"),
            Self::RelayUnavailable => f.write_str("relay service unavailable"),
            Self::LoopbackUnavailable => f.write_str("no loopback binding available"),
            Self::RoomNotFound { room_id } => write!(f, "unknown room {room_id}"),
            Self::RoomConflict { expected, actual } => {
                write!(
                    f,
                    "room revision changed: expected {expected}, actual {actual}"
                )
            }
            Self::RoomTerminal { state } => write!(f, "room is terminal ({state})"),
            Self::RoomExpired => f.write_str("room has expired"),
            Self::RoomJoinsClosed => f.write_str("room is not accepting new members"),
            Self::OwnerRequired => f.write_str("only the room owner may do this"),
            Self::InviteNotFound => f.write_str("unknown invite"),
            Self::InviteInvalid => f.write_str("invite secret does not match"),
            Self::InviteExpired => f.write_str("invite has expired"),
            Self::InviteRevoked => f.write_str("invite was revoked"),
            Self::InviteExhausted => f.write_str("invite has no uses left"),
            Self::EntropyUnavailable => f.write_str("no OS entropy available for an invite secret"),
            Self::MembershipNotFound { membership_id } => {
                write!(f, "unknown membership {membership_id}")
            }
            Self::MembershipTerminal { state } => {
                write!(f, "membership is already final ({state})")
            }
            Self::MembershipConflict { expected, actual } => write!(
                f,
                "membership revision changed: expected {expected}, actual {actual}"
            ),
            Self::ConnectionNotFound { connection_id } => {
                write!(f, "unknown connection {connection_id}")
            }
            Self::ConnectionRejected { detail } => write!(f, "connection rejected: {detail}"),
            Self::NoPathFound => f.write_str("no direct or relay path within budget"),
            Self::StaleAuthorization { expected, actual } => write!(
                f,
                "authorization generation is stale: current {expected}, presented {actual}"
            ),
            Self::LeaseExpired { expired_by_ms } => {
                write!(f, "lease expired {expired_by_ms} ms ago")
            }
            Self::LeaseUnauthorized => f.write_str("no authoritative service time for the lease"),
            Self::InvalidTransition { entity, from, to } => {
                write!(f, "invalid {entity} transition: {from} -> {to}")
            }
            Self::TunnelBindingInvalid { detail } => write!(f, "invalid tunnel binding: {detail}"),
            Self::TunnelSequenceGap { expected, actual } => {
                write!(
                    f,
                    "tunnel frame out of order: expected {expected}, got {actual}"
                )
            }
            Self::TunnelFrameInvalid { detail } => write!(f, "invalid tunnel frame: {detail}"),
            Self::TunnelClosed => f.write_str("tunnel is closed"),
            Self::Internal { detail } => write!(f, "internal rooms failure: {detail}"),
        }
    }
}

impl std::error::Error for RoomsError {}

/// Lets `?` convert a module error into the transport envelope at the command
/// boundary, so no call site has to remember `to_envelope()`.
impl From<RoomsError> for ErrorEnvelope {
    fn from(error: RoomsError) -> Self {
        error.to_envelope()
    }
}

/// Shared identifier validation failures become a `rooms.request.invalid` error,
/// keeping the module inside its own namespace even for malformed ids.
impl From<shtmcl_domain::ids::IdError> for RoomsError {
    fn from(error: shtmcl_domain::ids::IdError) -> Self {
        Self::InvalidRequest {
            detail: error.to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn every_code() -> Vec<&'static str> {
        use codes::*;
        vec![
            COMMAND_UNKNOWN,
            REQUEST_INVALID,
            REQUEST_IDEMPOTENCY_CONFLICT,
            IDENTITY_UNAVAILABLE,
            IDENTITY_UNVERIFIED,
            IDENTITY_MISMATCH,
            OWNER_UNAVAILABLE,
            LAUNCH_UNSUPPORTED,
            SIGNALING_UNAVAILABLE,
            RELAY_UNAVAILABLE,
            LOOPBACK_UNAVAILABLE,
            ROOM_NOT_FOUND,
            ROOM_CONFLICT,
            ROOM_TERMINAL,
            ROOM_EXPIRED,
            ROOM_JOINS_CLOSED,
            OWNER_REQUIRED,
            INVITE_NOT_FOUND,
            INVITE_INVALID,
            INVITE_EXPIRED,
            INVITE_REVOKED,
            INVITE_EXHAUSTED,
            INVITE_ENTROPY_UNAVAILABLE,
            MEMBERSHIP_NOT_FOUND,
            MEMBERSHIP_TERMINAL,
            MEMBERSHIP_CONFLICT,
            CONNECTION_NOT_FOUND,
            CONNECTION_REJECTED,
            CONNECTION_NO_PATH,
            AUTHORIZATION_STALE,
            LEASE_EXPIRED,
            LEASE_UNAUTHORIZED,
            STATE_INVALID_TRANSITION,
            TUNNEL_BINDING_INVALID,
            TUNNEL_SEQUENCE_GAP,
            TUNNEL_FRAME_INVALID,
            TUNNEL_CLOSED,
            INTERNAL,
        ]
    }

    #[test]
    fn every_module_code_matches_the_public_format_and_namespace() {
        for code in every_code() {
            assert!(
                ErrorCode::new(code).is_ok(),
                "code {code} does not match the public error-code format"
            );
            assert!(
                code.starts_with("rooms."),
                "code {code} must stay in the rooms namespace"
            );
        }
    }

    #[test]
    fn envelopes_carry_no_credentials_or_addresses() {
        let errors = [
            RoomsError::InviteExpired,
            RoomsError::StaleAuthorization {
                expected: 2,
                actual: 1,
            },
            RoomsError::SignalingUnavailable,
        ];
        for error in errors {
            let json = serde_json::to_string(&error.to_envelope()).expect("serialize");
            for forbidden in ["token", "secret", "credential", "192.168", "://"] {
                assert!(
                    !json.contains(forbidden),
                    "envelope leaked {forbidden}: {json}"
                );
            }
        }
    }

    #[test]
    fn distinct_failures_keep_distinct_codes() {
        assert_ne!(
            RoomsError::InviteExpired.code(),
            RoomsError::InviteRevoked.code()
        );
        assert_ne!(
            RoomsError::InviteRevoked.code(),
            RoomsError::InviteExhausted.code()
        );
        assert_ne!(
            RoomsError::MembershipTerminal {
                state: "left".to_string()
            }
            .code(),
            RoomsError::MembershipConflict {
                expected: 1,
                actual: 2
            }
            .code()
        );
        assert_eq!(RoomsError::UnknownCommand.code(), codes::COMMAND_UNKNOWN);
    }
}
