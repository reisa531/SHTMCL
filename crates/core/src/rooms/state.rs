//! Room, membership and connection state machines (module 06's own DTOs).
//!
//! These types are **not** in `crates/domain`: the shared contract only freezes
//! the envelope, the task/event/cancel shapes and the `membership.changed` /
//! `connection.changed` payload fields. Room-scoped DTOs live here so 00's crate
//! never has to learn module internals.
//!
//! Three counters stay distinct, exactly as `contracts/domain-v1.md` §2 demands:
//! - `revision` versions one entity (room / membership / connection),
//! - `authorization_generation` is the **permission** generation of one membership;
//!   removing a member or cancelling an approval raises it, so an old connection
//!   or an old retry can never be re-authorized,
//! - `attempt_generation` belongs to the shared task port and never appears here.

use std::fmt;

use serde_json::{json, Value};
use shtmcl_domain::{Revision, RoomId};

use super::error::RoomsError;

/// Identifiers owned by this module. The contract keeps IDs as opaque strings, so
/// these are thin validated wrappers rather than new public types in `domain`.
macro_rules! local_id {
    ($name:ident, $label:expr) => {
        #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name(String);

        impl $name {
            pub fn new(value: impl Into<String>) -> Result<Self, RoomsError> {
                let value = value.into();
                if value.is_empty() || value.len() > 128 {
                    return Err(RoomsError::InvalidRequest {
                        detail: format!("{} must be 1-128 bytes", $label),
                    });
                }
                if value
                    .chars()
                    .any(|character| character.is_control() || character.is_whitespace())
                {
                    return Err(RoomsError::InvalidRequest {
                        detail: format!("{} must not contain whitespace", $label),
                    });
                }
                Ok(Self(value))
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }
    };
}

local_id!(InviteId, "inviteId");
local_id!(MembershipId, "membershipId");
local_id!(ConnectionId, "connectionId");
local_id!(GrantId, "grantId");

/// What a joining player must match. Fields only; no private file is described.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoomRequirements {
    pub game_version: String,
    pub loader: String,
    pub content_lock_digest: String,
    pub auth_provider: String,
}

impl RoomRequirements {
    pub fn new(
        game_version: impl Into<String>,
        loader: impl Into<String>,
        content_lock_digest: impl Into<String>,
        auth_provider: impl Into<String>,
    ) -> Self {
        Self {
            game_version: game_version.into(),
            loader: loader.into(),
            content_lock_digest: content_lock_digest.into(),
            auth_provider: auth_provider.into(),
        }
    }

    pub fn to_value(&self) -> Value {
        json!({
            "gameVersion": self.game_version,
            "loader": self.loader,
            "contentLockDigest": self.content_lock_digest,
            "authProvider": self.auth_provider,
        })
    }

    /// The digest of this requirement set is what a room pins as
    /// `requirements_revision` provenance.
    pub fn matches(&self, other: &Self) -> bool {
        self == other
    }
}

/// Who can see the room in a directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoomVisibility {
    Private,
    Unlisted,
    Public,
}

impl RoomVisibility {
    pub fn parse(value: &str) -> Result<Self, RoomsError> {
        match value {
            "private" => Ok(Self::Private),
            "unlisted" => Ok(Self::Unlisted),
            "public" => Ok(Self::Public),
            other => Err(RoomsError::InvalidRequest {
                detail: format!("unknown visibility {other}"),
            }),
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Private => "private",
            Self::Unlisted => "unlisted",
            Self::Public => "public",
        }
    }
}

/// How a new member is admitted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JoinPolicy {
    /// A join request waits for the owner's decision.
    Approve,
    /// An approved membership is created immediately (still needs a valid invite).
    Open,
    /// Only an owner-issued invite can be redeemed; no other entry exists.
    InviteOnly,
}

impl JoinPolicy {
    pub fn parse(value: &str) -> Result<Self, RoomsError> {
        match value {
            "approve" => Ok(Self::Approve),
            "open" => Ok(Self::Open),
            "invite_only" => Ok(Self::InviteOnly),
            other => Err(RoomsError::InvalidRequest {
                detail: format!("unknown joinPolicy {other}"),
            }),
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Approve => "approve",
            Self::Open => "open",
            Self::InviteOnly => "invite_only",
        }
    }
}

/// `active / closing / ended / expired`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoomState {
    Active,
    /// New joins are refused; existing tunnels keep running until ended.
    Closing,
    Ended,
    Expired,
}

impl RoomState {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Closing => "closing",
            Self::Ended => "ended",
            Self::Expired => "expired",
        }
    }

    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Ended | Self::Expired)
    }

    pub const fn can_transition_to(self, next: Self) -> bool {
        match (self, next) {
            // `closing` is "stop new joins", which the owner may undo.
            (Self::Active, Self::Closing | Self::Ended | Self::Expired) => true,
            (Self::Closing, Self::Active | Self::Ended | Self::Expired) => true,
            _ => false,
        }
    }

    /// Terminal rooms never accept a new member again.
    pub const fn allows_joins(self) -> bool {
        matches!(self, Self::Active)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Room {
    pub room_id: RoomId,
    pub owner_id: String,
    pub name: String,
    pub visibility: RoomVisibility,
    pub join_policy: JoinPolicy,
    pub requirements: RoomRequirements,
    pub requirements_revision: Revision,
    /// The single game port this room will forward. It is the only target a
    /// loopback binding may point at.
    pub game_port: u16,
    pub state: RoomState,
    pub revision: Revision,
    pub created_at_ms: u64,
    /// Service-side deadline. `None` means the room never expires on its own.
    pub expires_at_ms: Option<u64>,
}

impl Room {
    pub fn to_value(&self, member_count: usize, invite_count: usize) -> Value {
        json!({
            "roomId": self.room_id.as_str(),
            "ownerId": self.owner_id,
            "name": self.name,
            "visibility": self.visibility.as_str(),
            "joinPolicy": self.join_policy.as_str(),
            "requirements": self.requirements.to_value(),
            "requirementsRevision": self.requirements_revision.get(),
            "gamePort": self.game_port,
            "state": self.state.as_str(),
            "revision": self.revision.get(),
            "createdAt": self.created_at_ms,
            "expiresAt": self.expires_at_ms,
            "memberCount": member_count,
            "activeInvites": invite_count,
        })
    }

    pub fn transition(&mut self, next: RoomState) -> Result<(), RoomsError> {
        if !self.state.can_transition_to(next) {
            return Err(RoomsError::InvalidTransition {
                entity: "room",
                from: self.state.as_str(),
                to: next.as_str(),
            });
        }
        self.state = next;
        self.bump_revision()?;
        Ok(())
    }

    pub fn bump_revision(&mut self) -> Result<(), RoomsError> {
        self.revision = self
            .revision
            .checked_next()
            .ok_or_else(|| RoomsError::Internal {
                detail: "room revision exhausted".to_string(),
            })?;
        Ok(())
    }

    /// Expiry is decided by the service clock, never by a caller's local time.
    pub fn expired(&self, server_now_ms: u64) -> bool {
        matches!(self.expires_at_ms, Some(deadline) if server_now_ms >= deadline)
    }
}

/// `requested / approved / denied / removed / left`. Terminal states are final:
/// re-joining creates a **new** membership id.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MembershipState {
    Requested,
    Approved,
    Denied,
    Removed,
    Left,
}

impl MembershipState {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Requested => "requested",
            Self::Approved => "approved",
            Self::Denied => "denied",
            Self::Removed => "removed",
            Self::Left => "left",
        }
    }

    /// Value published in `membership.changed.approvalState` (contract §12).
    pub const fn approval_state(self) -> &'static str {
        self.as_str()
    }

    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Denied | Self::Removed | Self::Left)
    }

    pub const fn can_transition_to(self, next: Self) -> bool {
        match (self, next) {
            (Self::Requested, Self::Approved | Self::Denied | Self::Left) => true,
            (Self::Approved, Self::Removed | Self::Left) => true,
            _ => false,
        }
    }

    /// Only an approved membership may hold a connection.
    pub const fn allows_connection(self) -> bool {
        matches!(self, Self::Approved)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MembershipRole {
    Owner,
    Member,
}

impl MembershipRole {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Owner => "owner",
            Self::Member => "member",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Membership {
    pub membership_id: MembershipId,
    pub room_id: RoomId,
    pub user_id: String,
    pub role: MembershipRole,
    pub state: MembershipState,
    /// Permission generation. Raised on removal and on approval cancellation; an
    /// old connection or an old retry presenting the previous value is refused.
    pub authorization_generation: u64,
    pub revision: Revision,
    /// Service-side member lease deadline. `None` means "not renewing".
    pub lease_until_ms: Option<u64>,
    pub requested_at_ms: u64,
    pub decided_at_ms: Option<u64>,
}

impl Membership {
    pub fn to_value(&self) -> Value {
        json!({
            "membershipId": self.membership_id.as_str(),
            "roomId": self.room_id.as_str(),
            "userId": self.user_id,
            "role": self.role.as_str(),
            "approvalState": self.state.approval_state(),
            "authorizationGeneration": self.authorization_generation,
            "revision": self.revision.get(),
            "leaseUntil": self.lease_until_ms,
            "requestedAt": self.requested_at_ms,
            "decidedAt": self.decided_at_ms,
        })
    }

    pub fn transition(&mut self, next: MembershipState, at_ms: u64) -> Result<(), RoomsError> {
        if !self.state.can_transition_to(next) {
            return Err(RoomsError::InvalidTransition {
                entity: "membership",
                from: self.state.as_str(),
                to: next.as_str(),
            });
        }
        self.state = next;
        self.decided_at_ms = Some(at_ms);
        self.bump_revision()?;
        Ok(())
    }

    pub fn bump_revision(&mut self) -> Result<(), RoomsError> {
        self.revision = self
            .revision
            .checked_next()
            .ok_or_else(|| RoomsError::Internal {
                detail: "membership revision exhausted".to_string(),
            })?;
        Ok(())
    }

    /// Raise the permission generation and stop renewing the lease: the two
    /// effects that make every previously granted permission unusable.
    pub fn revoke_authorization(&mut self) -> Result<(), RoomsError> {
        self.authorization_generation =
            self.authorization_generation
                .checked_add(1)
                .ok_or_else(|| RoomsError::Internal {
                    detail: "authorization generation exhausted".to_string(),
                })?;
        self.lease_until_ms = None;
        self.bump_revision()
    }
}

/// Which path the game stream actually uses. `None` until a connection is
/// established: `gathering` / `connecting` must never predict direct or relay.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportKind {
    Direct,
    Relay,
}

impl TransportKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Direct => "direct",
            Self::Relay => "relay",
        }
    }

    pub const fn evidence_marker(self) -> &'static str {
        match self {
            Self::Direct => EVIDENCE_TRANSPORT_DIRECT,
            Self::Relay => EVIDENCE_TRANSPORT_RELAY,
        }
    }
}

/// Evidence markers this module may attach to a task. The UI may only claim the
/// matching outcome when the marker is present.
pub const EVIDENCE_CONNECTION_CONNECTED: &str = "connection_connected";
pub const EVIDENCE_TRANSPORT_DIRECT: &str = "transport_direct";
pub const EVIDENCE_TRANSPORT_RELAY: &str = "transport_relay";
pub const EVIDENCE_MEMBERSHIP_LEFT: &str = "membership_left";
/// The member was removed by the owner; distinct from leaving voluntarily.
pub const EVIDENCE_MEMBERSHIP_REMOVED: &str = "membership_removed";
/// A cancellation arrived after the operation passed its point of no return, so the
/// work finished instead of being rolled back (the room/connection really exists).
pub const EVIDENCE_CANCEL_TOO_LATE: &str = "cancel_too_late";

/// `gathering / connecting / connected / reconnecting / disconnected / failed`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionState {
    Gathering,
    Connecting,
    Connected,
    Reconnecting,
    Disconnected,
    Failed,
}

impl ConnectionState {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Gathering => "gathering",
            Self::Connecting => "connecting",
            Self::Connected => "connected",
            Self::Reconnecting => "reconnecting",
            Self::Disconnected => "disconnected",
            Self::Failed => "failed",
        }
    }

    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Disconnected | Self::Failed)
    }

    pub const fn can_transition_to(self, next: Self) -> bool {
        match (self, next) {
            (Self::Gathering, Self::Connecting | Self::Failed | Self::Disconnected) => true,
            (
                Self::Connecting,
                Self::Connected | Self::Reconnecting | Self::Failed | Self::Disconnected,
            ) => true,
            (Self::Connected, Self::Reconnecting | Self::Failed | Self::Disconnected) => true,
            (Self::Reconnecting, Self::Connected | Self::Failed | Self::Disconnected) => true,
            _ => false,
        }
    }

    /// `transport` may only appear once the path is real.
    pub const fn transport_allowed(self) -> bool {
        matches!(self, Self::Connected)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectionSession {
    pub connection_id: ConnectionId,
    pub membership_id: MembershipId,
    pub room_id: RoomId,
    pub state: ConnectionState,
    pub transport: Option<TransportKind>,
    /// Authorization generation of the membership this connection was opened for.
    pub generation: u64,
    /// Service-side deadline; never later than the member lease.
    pub lease_until_ms: Option<u64>,
    pub revision: Revision,
    pub opened_at_ms: u64,
}

impl ConnectionSession {
    pub fn to_value(&self) -> Value {
        json!({
            "connectionId": self.connection_id.as_str(),
            "membershipId": self.membership_id.as_str(),
            "roomId": self.room_id.as_str(),
            "state": self.state.as_str(),
            "transport": self.transport.map(TransportKind::as_str),
            "generation": self.generation,
            "leaseUntil": self.lease_until_ms,
            "revision": self.revision.get(),
            "openedAt": self.opened_at_ms,
        })
    }

    pub fn transition(&mut self, next: ConnectionState) -> Result<(), RoomsError> {
        if !self.state.can_transition_to(next) {
            return Err(RoomsError::InvalidTransition {
                entity: "connection",
                from: self.state.as_str(),
                to: next.as_str(),
            });
        }
        self.state = next;
        self.revision = self
            .revision
            .checked_next()
            .ok_or_else(|| RoomsError::Internal {
                detail: "connection revision exhausted".to_string(),
            })?;
        if next.is_terminal() {
            self.lease_until_ms = None;
        }
        Ok(())
    }

    pub fn is_connected(&self) -> bool {
        self.state == ConnectionState::Connected
    }
}

/// Local loopback entry for the tunnel process.
///
/// The only address this module ever opens is the loopback interface, and the only
/// target it forwards to is the room's single declared game port. There is no
/// arbitrary LAN forwarding and no virtual network adapter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TunnelBinding {
    pub connection_id: ConnectionId,
    pub loopback_port: u16,
    pub target_port: u16,
    pub process_identity: String,
    pub local_revision: Revision,
}

/// Host part of every binding. Kept here so a test can assert it is loopback.
pub const LOOPBACK_HOST: &str = "127.0.0.1";

impl TunnelBinding {
    pub fn new(
        connection_id: ConnectionId,
        loopback_port: u16,
        target_port: u16,
        process_identity: impl Into<String>,
        local_revision: Revision,
    ) -> Result<Self, RoomsError> {
        let process_identity = process_identity.into();
        if loopback_port < 1024 {
            return Err(RoomsError::TunnelBindingInvalid {
                detail: "loopback port must be an ephemeral, non-privileged port".to_string(),
            });
        }
        if target_port == 0 {
            return Err(RoomsError::TunnelBindingInvalid {
                detail: "target port is required".to_string(),
            });
        }
        if target_port == loopback_port {
            return Err(RoomsError::TunnelBindingInvalid {
                detail: "target port must differ from the loopback port".to_string(),
            });
        }
        if process_identity.trim().is_empty() {
            return Err(RoomsError::TunnelBindingInvalid {
                detail: "binding must be associated with a tunnel process identity".to_string(),
            });
        }
        Ok(Self {
            connection_id,
            loopback_port,
            target_port,
            process_identity,
            local_revision,
        })
    }

    /// A binding is only valid for the room's declared port.
    pub fn matches_room_port(&self, room_port: u16) -> bool {
        self.target_port == room_port
    }

    pub fn to_value(&self) -> Value {
        json!({
            "connectionId": self.connection_id.as_str(),
            "loopbackHost": LOOPBACK_HOST,
            "loopbackPort": self.loopback_port,
            "targetPort": self.target_port,
            "processIdentity": self.process_identity,
            "localRevision": self.local_revision.get(),
        })
    }
}

/// Short-lived relay authorization. Bounded by bytes and a deadline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelayGrant {
    pub grant_id: GrantId,
    pub room_id: RoomId,
    pub region: String,
    pub bytes_limit: u64,
    pub expires_at_ms: u64,
    /// Opaque peer identifiers, never addresses.
    pub peer_pair: (String, String),
}

impl RelayGrant {
    pub fn to_value(&self) -> Value {
        json!({
            "grantId": self.grant_id.as_str(),
            "roomId": self.room_id.as_str(),
            "region": self.region,
            "bytesLimit": self.bytes_limit,
            "expiresAt": self.expires_at_ms,
            "peerPair": [self.peer_pair.0, self.peer_pair.1],
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn room() -> Room {
        Room {
            room_id: RoomId::new("room-1").expect("room id"),
            owner_id: "user-owner".to_string(),
            name: "fixture".to_string(),
            visibility: RoomVisibility::Private,
            join_policy: JoinPolicy::Approve,
            requirements: RoomRequirements::new("1.20.1", "fabric", "digest", "microsoft"),
            requirements_revision: Revision::new(3),
            game_port: 25565,
            state: RoomState::Active,
            revision: Revision::new(1),
            created_at_ms: 1_000,
            expires_at_ms: Some(10_000),
        }
    }

    #[test]
    fn room_terminal_states_are_irreversible() {
        let mut room = room();
        room.transition(RoomState::Ended).expect("end");
        assert!(room.state.is_terminal());
        assert!(!room.state.allows_joins());
        assert_eq!(room.revision, Revision::new(2));
        let error = room
            .transition(RoomState::Active)
            .expect_err("ended is final");
        assert_eq!(
            error.code(),
            super::super::error::codes::STATE_INVALID_TRANSITION
        );
    }

    #[test]
    fn closing_only_stops_new_joins_and_can_be_reopened() {
        let mut room = room();
        room.transition(RoomState::Closing).expect("close joins");
        assert!(!room.state.allows_joins());
        room.transition(RoomState::Active).expect("reopen");
        assert!(room.state.allows_joins());
    }

    #[test]
    fn membership_terminal_states_never_revive() {
        for terminal in [
            MembershipState::Denied,
            MembershipState::Removed,
            MembershipState::Left,
        ] {
            assert!(terminal.is_terminal());
            assert!(!terminal.can_transition_to(MembershipState::Approved));
            assert!(!terminal.can_transition_to(MembershipState::Requested));
        }
        assert!(MembershipState::Requested.can_transition_to(MembershipState::Left));
        assert!(MembershipState::Approved.can_transition_to(MembershipState::Left));
        assert!(!MembershipState::Approved.can_transition_to(MembershipState::Requested));
    }

    #[test]
    fn revoking_authorization_raises_generation_and_drops_the_lease() {
        let mut membership = Membership {
            membership_id: MembershipId::new("membership-1").expect("id"),
            room_id: RoomId::new("room-1").expect("id"),
            user_id: "user-1".to_string(),
            role: MembershipRole::Member,
            state: MembershipState::Approved,
            authorization_generation: 1,
            revision: Revision::new(2),
            lease_until_ms: Some(5_000),
            requested_at_ms: 1_000,
            decided_at_ms: Some(1_100),
        };
        membership.revoke_authorization().expect("revoke");
        assert_eq!(membership.authorization_generation, 2);
        assert_eq!(membership.lease_until_ms, None);
        assert_eq!(membership.revision, Revision::new(3));
    }

    #[test]
    fn connection_transport_is_only_published_once_connected() {
        assert!(!ConnectionState::Gathering.transport_allowed());
        assert!(!ConnectionState::Connecting.transport_allowed());
        assert!(ConnectionState::Connected.transport_allowed());
        assert!(!ConnectionState::Reconnecting.transport_allowed());
        assert!(ConnectionState::Connected.can_transition_to(ConnectionState::Reconnecting));
        assert!(!ConnectionState::Failed.can_transition_to(ConnectionState::Connecting));
    }

    #[test]
    fn bindings_stay_on_loopback_and_the_rooms_own_port() {
        let connection = ConnectionId::new("connection-1").expect("id");
        let binding = TunnelBinding::new(
            connection.clone(),
            41_000,
            25_565,
            "fixture-tunnel",
            Revision::new(4),
        )
        .expect("binding");
        assert!(binding.matches_room_port(25_565));
        assert!(!binding.matches_room_port(25_566));
        assert_eq!(binding.to_value()["loopbackHost"], json!(LOOPBACK_HOST));

        assert!(TunnelBinding::new(connection.clone(), 80, 25_565, "p", Revision::ZERO).is_err());
        assert!(TunnelBinding::new(connection.clone(), 41_000, 0, "p", Revision::ZERO).is_err());
        assert!(
            TunnelBinding::new(connection.clone(), 41_000, 41_000, "p", Revision::ZERO).is_err()
        );
        assert!(TunnelBinding::new(connection, 41_000, 25_565, "  ", Revision::ZERO).is_err());
    }

    #[test]
    fn evidence_markers_are_the_frozen_names() {
        assert_eq!(EVIDENCE_CONNECTION_CONNECTED, "connection_connected");
        assert_eq!(TransportKind::Direct.evidence_marker(), "transport_direct");
        assert_eq!(TransportKind::Relay.evidence_marker(), "transport_relay");
        assert_eq!(EVIDENCE_MEMBERSHIP_LEFT, "membership_left");
    }
}
