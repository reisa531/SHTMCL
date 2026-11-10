//! In-memory room store: the server-side transaction point of module 06.
//!
//! Everything that decides a permission or consumes an invite happens **inside one
//! lock**, which is what gives this milestone the serialization property the design
//! asks for:
//! - cancel-then-approve: the cancellation commits first, so the later approval sees
//!   a terminal membership and fails,
//! - approve-then-cancel: the approval commits first, then the cancellation closes
//!   the permission, raises `authorization_generation` and revokes the connection
//!   lease,
//! - an invite use is consumed in the same transaction that creates the membership,
//!   so a retry with the same idempotency key cannot consume a second use,
//! - a repeated decision returns the same projection instead of bumping revisions.
//!
//! Methods return the primary outcome plus the [`StoreEvent`]s that must be
//! published. Port effects (signalling notices, relay releases, loopback release)
//! are returned as data and performed by the service **after** the lock is
//! released, so a slow port can never extend the critical section.

use std::collections::BTreeMap;
use std::sync::Mutex;

use serde_json::{json, Value};
use shtmcl_domain::{Revision, RoomId};

use super::error::RoomsError;
use super::invite::{digest_secret, generate_secret, verify_secret, Invite, InviteSpec};
use super::lease::{self, CONNECTION_LEASE_MS, MEMBER_LEASE_MS};
use super::state::{
    ConnectionId, ConnectionSession, ConnectionState, GrantId, InviteId, JoinPolicy, Membership,
    MembershipId, MembershipRole, MembershipState, RelayGrant, Room, RoomRequirements, RoomState,
    RoomVisibility, TransportKind,
};

/// What the service must publish after a transaction commits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StoreEvent {
    RoomChanged { room_id: RoomId },
    MembershipChanged { membership_id: MembershipId },
    ConnectionChanged { connection_id: ConnectionId },
    InviteChanged { invite_id: InviteId },
}

/// An idempotency key plus the digest of the request body it belongs to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdemKey {
    pub key: String,
    pub digest: String,
}

impl IdemKey {
    pub fn new(key: impl Into<String>, body: impl AsRef<str>) -> Self {
        let key = key.into();
        Self {
            digest: digest_secret(body.as_ref()),
            key,
        }
    }
}

#[derive(Debug, Clone)]
struct Idempotent {
    digest: String,
    result: Value,
}

#[derive(Debug, Default)]
struct Counters {
    room: u64,
    invite: u64,
    membership: u64,
    connection: u64,
}

#[derive(Debug, Default)]
struct State {
    rooms: BTreeMap<RoomId, Room>,
    invites: BTreeMap<InviteId, Invite>,
    memberships: BTreeMap<MembershipId, Membership>,
    connections: BTreeMap<ConnectionId, ConnectionSession>,
    grants: BTreeMap<GrantId, GrantEntry>,
    idempotency: BTreeMap<String, Idempotent>,
    counters: Counters,
}

/// A relay grant plus the objects it authorizes.
///
/// The association is what makes the release scope correct: releasing a grant is a
/// per-**member** / per-**connection** action, never a room-wide sweep. Only ending
/// the room revokes every grant in it.
#[derive(Debug, Clone)]
struct GrantEntry {
    grant: RelayGrant,
    membership_id: MembershipId,
    connection_id: ConnectionId,
}

/// Draft for a new room. The port results are already resolved by the caller.
#[derive(Debug, Clone)]
pub struct RoomDraft {
    pub owner_id: String,
    pub name: String,
    pub visibility: RoomVisibility,
    pub join_policy: JoinPolicy,
    pub requirements: RoomRequirements,
    pub requirements_revision: Revision,
    pub game_port: u16,
    pub created_at_ms: u64,
    pub expires_at_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JoinCall {
    pub idem: IdemKey,
    pub room_id: Option<RoomId>,
    pub secret: String,
    pub user_id: String,
    pub profile_capability: Option<String>,
    pub now_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JoinOutcome {
    pub membership: Membership,
    pub invite: Invite,
    /// `true` when the caller was already a member (nothing was consumed).
    pub already_member: bool,
    /// `true` when this call replayed a recorded result.
    pub replayed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecideCall {
    pub idem: IdemKey,
    pub membership_id: MembershipId,
    pub approve: bool,
    pub expected_revision: Option<Revision>,
    pub actor_id: String,
    pub now_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecisionOutcome {
    pub membership: Membership,
    /// `true` when the decision already matched the current state.
    pub idempotent: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeaveCall {
    pub idem: IdemKey,
    pub membership_id: MembershipId,
    pub expected_revision: Option<Revision>,
    pub actor_id: String,
    pub now_ms: u64,
    /// Removal by the owner publishes `removed` instead of `left`.
    pub by_owner: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeaveOutcome {
    pub membership: Membership,
    pub idempotent: bool,
    /// Connections that lost their permission in this transaction.
    pub revoked_connections: Vec<ConnectionSession>,
    /// Relay grants the service must release through the relay port.
    pub released_grants: Vec<GrantId>,
    /// `true` when the member themselves is the actor.
    pub left_by_self: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoomEndOutcome {
    pub room: Room,
    pub revoked_connections: Vec<ConnectionSession>,
    pub released_grants: Vec<GrantId>,
    /// Members whose permission this transaction ended; the service notifies each.
    pub affected_memberships: Vec<MembershipId>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectionPlan {
    pub connection: ConnectionSession,
    pub room: Room,
    pub member_lease_until_ms: Option<u64>,
}

#[derive(Debug, Default)]
pub struct RoomStore {
    inner: Mutex<State>,
}

impl RoomStore {
    pub fn new() -> Self {
        Self::default()
    }

    fn tx<T>(
        &self,
        f: impl FnOnce(&mut State) -> Result<(T, Vec<StoreEvent>), RoomsError>,
    ) -> Result<(T, Vec<StoreEvent>), RoomsError> {
        let mut state = self.inner.lock().map_err(|_| RoomsError::Internal {
            detail: "rooms store lock poisoned".to_string(),
        })?;
        f(&mut state)
    }

    /// Read-only transaction: the caller discards the (always empty) event list.
    fn read<T>(
        &self,
        f: impl FnOnce(&mut State) -> Result<T, RoomsError>,
    ) -> Result<T, RoomsError> {
        self.tx(|state| Ok((f(state)?, Vec::new())))
            .map(|(value, _)| value)
    }

    // -- rooms -------------------------------------------------------------

    pub fn commit_room(&self, draft: RoomDraft) -> Result<(Room, Vec<StoreEvent>), RoomsError> {
        self.tx(|state| {
            state.counters.room += 1;
            let room_id = RoomId::new(format!("room-{}", state.counters.room))?;
            let room = Room {
                room_id: room_id.clone(),
                owner_id: draft.owner_id,
                name: draft.name,
                visibility: draft.visibility,
                join_policy: draft.join_policy,
                requirements: draft.requirements,
                requirements_revision: draft.requirements_revision,
                game_port: draft.game_port,
                state: RoomState::Active,
                revision: Revision::new(1),
                created_at_ms: draft.created_at_ms,
                expires_at_ms: draft.expires_at_ms,
            };
            state.rooms.insert(room_id.clone(), room.clone());
            Ok((room, vec![StoreEvent::RoomChanged { room_id }]))
        })
    }

    pub fn room(&self, room_id: &RoomId) -> Result<Room, RoomsError> {
        self.read(|state| {
            state
                .rooms
                .get(room_id)
                .cloned()
                .ok_or_else(|| RoomsError::RoomNotFound {
                    room_id: room_id.as_str().to_string(),
                })
        })
    }

    pub fn connection(
        &self,
        connection_id: &ConnectionId,
    ) -> Result<ConnectionSession, RoomsError> {
        self.read(|state| {
            state
                .connections
                .get(connection_id)
                .cloned()
                .ok_or_else(|| RoomsError::ConnectionNotFound {
                    connection_id: connection_id.as_str().to_string(),
                })
        })
    }

    pub fn membership(&self, membership_id: &MembershipId) -> Result<Membership, RoomsError> {
        self.read(|state| {
            state
                .memberships
                .get(membership_id)
                .cloned()
                .ok_or_else(|| RoomsError::MembershipNotFound {
                    membership_id: membership_id.as_str().to_string(),
                })
        })
    }

    pub fn invite(&self, invite_id: &InviteId) -> Result<Invite, RoomsError> {
        self.read(|state| {
            state
                .invites
                .get(invite_id)
                .cloned()
                .ok_or(RoomsError::InviteNotFound)
        })
    }

    /// `active`/`closing` room, or a stable "not usable" error. Expiry is applied
    /// here so every caller sees the same answer.
    fn usable_room(state: &mut State, room_id: &RoomId, now_ms: u64) -> Result<Room, RoomsError> {
        let room = state
            .rooms
            .get_mut(room_id)
            .ok_or_else(|| RoomsError::RoomNotFound {
                room_id: room_id.as_str().to_string(),
            })?;
        if !room.state.is_terminal() && room.expired(now_ms) {
            room.transition(RoomState::Expired)?;
        }
        let room = room.clone();
        if room.state == RoomState::Expired {
            return Err(RoomsError::RoomExpired);
        }
        if room.state.is_terminal() {
            return Err(RoomsError::RoomTerminal {
                state: room.state.as_str().to_string(),
            });
        }
        Ok(room)
    }

    /// End the whole room: terminal state, every permission and invite revoked.
    pub fn end_room(
        &self,
        idem: &IdemKey,
        room_id: &RoomId,
        expected_revision: Option<Revision>,
        actor_id: &str,
        now_ms: u64,
    ) -> Result<(RoomEndOutcome, Vec<StoreEvent>), RoomsError> {
        self.tx(|state| {
            let current =
                state
                    .rooms
                    .get(room_id)
                    .cloned()
                    .ok_or_else(|| RoomsError::RoomNotFound {
                        room_id: room_id.as_str().to_string(),
                    })?;
            // Authorization first, then a replay (same subject), then the revision
            // fence — the same order as `leave` / `decide` / `close_joins`.
            if current.owner_id != actor_id {
                return Err(RoomsError::OwnerRequired);
            }
            if state.idempotency.contains_key(&idem.key) {
                check_replay_digest(state, idem)?;
                return Ok((
                    RoomEndOutcome {
                        room: current,
                        revoked_connections: Vec::new(),
                        released_grants: Vec::new(),
                        affected_memberships: Vec::new(),
                    },
                    Vec::new(),
                ));
            }
            if let Some(expected) = expected_revision {
                if expected != current.revision {
                    return Err(RoomsError::RoomConflict {
                        expected: expected.get(),
                        actual: current.revision.get(),
                    });
                }
            }

            let mut events = Vec::new();
            let room = {
                let room =
                    state
                        .rooms
                        .get_mut(room_id)
                        .ok_or_else(|| RoomsError::RoomNotFound {
                            room_id: room_id.as_str().to_string(),
                        })?;
                if let Some(expected) = expected_revision {
                    if expected != room.revision {
                        return Err(RoomsError::RoomConflict {
                            expected: expected.get(),
                            actual: room.revision.get(),
                        });
                    }
                }
                if room.state.is_terminal() {
                    return Err(RoomsError::RoomTerminal {
                        state: room.state.as_str().to_string(),
                    });
                }
                room.transition(RoomState::Ended)?;
                room.clone()
            };

            // Ending the room closes every permission it granted and revokes every
            // invite, so a stale link cannot be reused afterwards.
            let membership_ids: Vec<MembershipId> = state
                .memberships
                .values()
                .filter(|membership| {
                    membership.room_id == *room_id && !membership.state.is_terminal()
                })
                .map(|membership| membership.membership_id.clone())
                .collect();
            for membership_id in &membership_ids {
                if let Some(membership) = state.memberships.get_mut(membership_id) {
                    if membership.state == MembershipState::Requested {
                        membership.transition(MembershipState::Denied, now_ms)?;
                    } else {
                        membership.revoke_authorization()?;
                        membership.transition(MembershipState::Left, now_ms)?;
                    }
                    events.push(StoreEvent::MembershipChanged {
                        membership_id: membership_id.clone(),
                    });
                }
            }

            let mut revoked = Vec::new();
            for connection in state.connections.values_mut() {
                if connection.room_id != *room_id || connection.state.is_terminal() {
                    continue;
                }
                connection.transition(ConnectionState::Disconnected)?;
                revoked.push(connection.clone());
                events.push(StoreEvent::ConnectionChanged {
                    connection_id: connection.connection_id.clone(),
                });
            }

            for invite in state.invites.values_mut() {
                if invite.room_id != *room_id {
                    continue;
                }
                if invite.revoke(now_ms)? {
                    events.push(StoreEvent::InviteChanged {
                        invite_id: invite.invite_id.clone(),
                    });
                }
            }

            // Ending the room is the only place that revokes every grant in it.
            let grants: Vec<GrantId> = state
                .grants
                .iter()
                .filter(|(_, entry)| entry.grant.room_id == *room_id)
                .map(|(id, _)| id.clone())
                .collect();

            events.push(StoreEvent::RoomChanged {
                room_id: room_id.clone(),
            });
            record(
                state,
                idem,
                json!({ "roomId": room.room_id.as_str(), "state": room.state.as_str() }),
            );
            Ok((
                RoomEndOutcome {
                    room,
                    revoked_connections: revoked,
                    released_grants: grants,
                    affected_memberships: membership_ids,
                },
                events,
            ))
        })
    }

    pub fn set_joins_closed(
        &self,
        idem: &IdemKey,
        room_id: &RoomId,
        closed: bool,
        expected_revision: Option<Revision>,
        actor_id: &str,
    ) -> Result<(Room, Vec<StoreEvent>), RoomsError> {
        self.tx(|state| {
            // Authorization before any replay, so a foreign caller is never answered.
            let owner_id = state
                .rooms
                .get(room_id)
                .map(|room| room.owner_id.clone())
                .ok_or_else(|| RoomsError::RoomNotFound {
                    room_id: room_id.as_str().to_string(),
                })?;
            if owner_id != actor_id {
                return Err(RoomsError::OwnerRequired);
            }
            // Replay before the revision fence: a retry of a committed operation must
            // be repeatable even though its own commit moved the revision on (D7).
            if state.idempotency.contains_key(&idem.key) {
                check_replay_digest(state, idem)?;
                let room =
                    state
                        .rooms
                        .get(room_id)
                        .cloned()
                        .ok_or_else(|| RoomsError::RoomNotFound {
                            room_id: room_id.as_str().to_string(),
                        })?;
                return Ok((room, Vec::new()));
            }
            let room = {
                let room =
                    state
                        .rooms
                        .get_mut(room_id)
                        .ok_or_else(|| RoomsError::RoomNotFound {
                            room_id: room_id.as_str().to_string(),
                        })?;
                if let Some(expected) = expected_revision {
                    if expected != room.revision {
                        return Err(RoomsError::RoomConflict {
                            expected: expected.get(),
                            actual: room.revision.get(),
                        });
                    }
                }
                let target = if closed {
                    RoomState::Closing
                } else {
                    RoomState::Active
                };
                if room.state == target {
                    return Ok((room.clone(), Vec::new()));
                }
                if room.state.is_terminal() {
                    return Err(RoomsError::RoomTerminal {
                        state: room.state.as_str().to_string(),
                    });
                }
                room.transition(target)?;
                room.clone()
            };
            record(
                state,
                idem,
                json!({ "roomId": room.room_id.as_str(), "state": room.state.as_str() }),
            );
            Ok((
                room.clone(),
                vec![StoreEvent::RoomChanged {
                    room_id: room.room_id.clone(),
                }],
            ))
        })
    }

    // -- invites -----------------------------------------------------------

    pub fn create_invite(
        &self,
        room_id: &RoomId,
        spec: InviteSpec,
        actor_id: &str,
        now_ms: u64,
    ) -> Result<(Invite, String, Vec<StoreEvent>), RoomsError> {
        let ((invite, secret), events) = self.tx(|state| {
            let room = Self::usable_room(state, room_id, now_ms)?;
            if room.owner_id != actor_id {
                return Err(RoomsError::OwnerRequired);
            }
            let (invite, secret) = mint_invite(state, room_id, spec, now_ms)?;
            let events = vec![StoreEvent::InviteChanged {
                invite_id: invite.invite_id.clone(),
            }];
            Ok(((invite, secret), events))
        })?;
        Ok((invite, secret, events))
    }

    /// Rotate: revoke every active invite of the room and mint one new secret.
    pub fn rotate_invite(
        &self,
        room_id: &RoomId,
        spec: InviteSpec,
        actor_id: &str,
        now_ms: u64,
    ) -> Result<(Invite, String, Vec<StoreEvent>), RoomsError> {
        let ((invite, secret), events) = self.tx(|state| {
            let room = Self::usable_room(state, room_id, now_ms)?;
            if room.owner_id != actor_id {
                return Err(RoomsError::OwnerRequired);
            }
            let mut events = Vec::new();
            for invite in state.invites.values_mut() {
                if invite.room_id == *room_id && invite.revoke(now_ms)? {
                    events.push(StoreEvent::InviteChanged {
                        invite_id: invite.invite_id.clone(),
                    });
                }
            }
            let (invite, secret) = mint_invite(state, room_id, spec, now_ms)?;
            events.push(StoreEvent::InviteChanged {
                invite_id: invite.invite_id.clone(),
            });
            Ok(((invite, secret), events))
        })?;
        Ok((invite, secret, events))
    }

    pub fn revoke_invite(
        &self,
        invite_id: &InviteId,
        actor_id: &str,
        now_ms: u64,
    ) -> Result<(Invite, Vec<StoreEvent>), RoomsError> {
        self.tx(|state| {
            let room_id = state
                .invites
                .get(invite_id)
                .map(|invite| invite.room_id.clone())
                .ok_or(RoomsError::InviteNotFound)?;
            let owner = state
                .rooms
                .get(&room_id)
                .map(|room| room.owner_id.clone())
                .ok_or_else(|| RoomsError::RoomNotFound {
                    room_id: room_id.as_str().to_string(),
                })?;
            if owner != actor_id {
                return Err(RoomsError::OwnerRequired);
            }
            let invite = state
                .invites
                .get_mut(invite_id)
                .ok_or(RoomsError::InviteNotFound)?;
            let changed = invite.revoke(now_ms)?;
            let invite = invite.clone();
            let events = if changed {
                vec![StoreEvent::InviteChanged {
                    invite_id: invite_id.clone(),
                }]
            } else {
                Vec::new()
            };
            Ok((invite, events))
        })
    }

    // -- membership --------------------------------------------------------

    pub fn join(&self, call: JoinCall) -> Result<(JoinOutcome, Vec<StoreEvent>), RoomsError> {
        self.tx(|state| {
            if state.idempotency.contains_key(&call.idem.key) {
                // Replay: the membership already exists, nothing is consumed again.
                let stored = check_replay_digest(state, &call.idem)?;
                let membership_id = stored
                    .get("membershipId")
                    .and_then(Value::as_str)
                    .map(MembershipId::new)
                    .transpose()?
                    .ok_or_else(|| RoomsError::Internal {
                        detail: "idempotent join record is corrupt".to_string(),
                    })?;
                let membership =
                    state
                        .memberships
                        .get(&membership_id)
                        .cloned()
                        .ok_or_else(|| RoomsError::MembershipNotFound {
                            membership_id: membership_id.as_str().to_string(),
                        })?;
                // A replay only answers the subject that owns the recorded membership.
                if membership.user_id != call.user_id {
                    return Err(RoomsError::IdentityMismatch {
                        detail: "this idempotency key belongs to another member".to_string(),
                    });
                }
                // Report the invite this request actually consumed, not another one.
                let invite = state
                    .invites
                    .get(
                        &stored
                            .get("inviteId")
                            .and_then(Value::as_str)
                            .map(InviteId::new)
                            .transpose()?
                            .ok_or(RoomsError::InviteNotFound)?,
                    )
                    .cloned()
                    .ok_or(RoomsError::InviteNotFound)?;
                return Ok((
                    JoinOutcome {
                        membership,
                        invite,
                        already_member: true,
                        replayed: true,
                    },
                    Vec::new(),
                ));
            }

            // Locate the invite by digest; a secret that matches nothing is an
            // invalid invite rather than a leak of which ids exist.
            let matched = state
                .invites
                .values()
                .filter(|invite| {
                    call.room_id
                        .as_ref()
                        .map_or(true, |room| invite.room_id == *room)
                })
                .find(|invite| verify_secret(invite, &call.secret).is_ok())
                .map(|invite| invite.invite_id.clone())
                .ok_or(RoomsError::InviteInvalid)?;

            let room_id = state
                .invites
                .get(&matched)
                .map(|invite| invite.room_id.clone())
                .ok_or(RoomsError::InviteNotFound)?;
            let room = Self::usable_room(state, &room_id, call.now_ms)?;

            // Already a member (requested or approved): return it, consume nothing.
            if let Some(existing) = state
                .memberships
                .values()
                .find(|membership| {
                    membership.room_id == room_id
                        && membership.user_id == call.user_id
                        && !membership.state.is_terminal()
                })
                .cloned()
            {
                let invite = state
                    .invites
                    .get(&matched)
                    .cloned()
                    .ok_or(RoomsError::InviteNotFound)?;
                record(
                    state,
                    &call.idem,
                    json!({
                        "membershipId": existing.membership_id.as_str(),
                        "approvalState": existing.state.approval_state(),
                        "roomId": room_id.as_str(),
                        "inviteId": invite.invite_id.as_str(),
                        "inviteRemainingUses": invite.remaining_uses,
                        "alreadyMember": true,
                    }),
                );
                return Ok((
                    JoinOutcome {
                        membership: existing,
                        invite,
                        already_member: true,
                        replayed: false,
                    },
                    Vec::new(),
                ));
            }

            if !room.state.allows_joins() {
                return Err(RoomsError::RoomJoinsClosed);
            }
            if room.join_policy == JoinPolicy::InviteOnly && matched.as_str().is_empty() {
                return Err(RoomsError::InviteInvalid);
            }

            // One transaction: verify, consume, create. A retry cannot consume twice.
            let invite = {
                let invite = state
                    .invites
                    .get_mut(&matched)
                    .ok_or(RoomsError::InviteNotFound)?;
                invite.check_redeemable(call.now_ms)?;
                invite.consume()?;
                invite.clone()
            };

            state.counters.membership += 1;
            let membership_id =
                MembershipId::new(format!("membership-{}", state.counters.membership))?;
            let approved = room.join_policy == JoinPolicy::Open;
            let membership = Membership {
                membership_id: membership_id.clone(),
                room_id: room_id.clone(),
                user_id: call.user_id.clone(),
                role: MembershipRole::Member,
                state: if approved {
                    MembershipState::Approved
                } else {
                    MembershipState::Requested
                },
                authorization_generation: 1,
                revision: Revision::new(1),
                lease_until_ms: if approved {
                    Some(call.now_ms.saturating_add(MEMBER_LEASE_MS))
                } else {
                    None
                },
                requested_at_ms: call.now_ms,
                decided_at_ms: if approved { Some(call.now_ms) } else { None },
            };
            state
                .memberships
                .insert(membership_id.clone(), membership.clone());

            record(
                state,
                &call.idem,
                json!({
                    "membershipId": membership.membership_id.as_str(),
                    "approvalState": membership.state.approval_state(),
                    "roomId": room_id.as_str(),
                    "inviteId": invite.invite_id.as_str(),
                    "inviteRemainingUses": invite.remaining_uses,
                    "alreadyMember": false,
                }),
            );

            Ok((
                JoinOutcome {
                    membership: membership.clone(),
                    invite: invite.clone(),
                    already_member: false,
                    replayed: false,
                },
                vec![
                    StoreEvent::InviteChanged {
                        invite_id: invite.invite_id.clone(),
                    },
                    StoreEvent::MembershipChanged {
                        membership_id: membership_id.clone(),
                    },
                    StoreEvent::RoomChanged {
                        room_id: room_id.clone(),
                    },
                ],
            ))
        })
    }

    pub fn decide(
        &self,
        call: DecideCall,
    ) -> Result<(DecisionOutcome, Vec<StoreEvent>), RoomsError> {
        self.tx(|state| {
            let target = if call.approve {
                MembershipState::Approved
            } else {
                MembershipState::Denied
            };
            let room_id = state
                .memberships
                .get(&call.membership_id)
                .map(|membership| membership.room_id.clone())
                .ok_or_else(|| RoomsError::MembershipNotFound {
                    membership_id: call.membership_id.as_str().to_string(),
                })?;
            let room =
                state
                    .rooms
                    .get(&room_id)
                    .cloned()
                    .ok_or_else(|| RoomsError::RoomNotFound {
                        room_id: room_id.as_str().to_string(),
                    })?;
            if room.owner_id != call.actor_id {
                return Err(RoomsError::OwnerRequired);
            }

            let replayed = state.idempotency.contains_key(&call.idem.key);
            if replayed {
                check_replay_digest(state, &call.idem)?;
                let membership = state
                    .memberships
                    .get(&call.membership_id)
                    .cloned()
                    .ok_or_else(|| RoomsError::MembershipNotFound {
                        membership_id: call.membership_id.as_str().to_string(),
                    })?;
                return Ok((
                    DecisionOutcome {
                        membership,
                        idempotent: true,
                    },
                    Vec::new(),
                ));
            }

            let membership = {
                let membership =
                    state
                        .memberships
                        .get_mut(&call.membership_id)
                        .ok_or_else(|| RoomsError::MembershipNotFound {
                            membership_id: call.membership_id.as_str().to_string(),
                        })?;
                if let Some(expected) = call.expected_revision {
                    if expected != membership.revision {
                        return Err(RoomsError::MembershipConflict {
                            expected: expected.get(),
                            actual: membership.revision.get(),
                        });
                    }
                }
                if membership.state.is_terminal() {
                    // A final membership can never be revived by a new decision.
                    return Err(RoomsError::MembershipTerminal {
                        state: membership.state.as_str().to_string(),
                    });
                }
                if membership.state == target {
                    // Same decision again: same projection, no revision bump.
                    let membership = membership.clone();
                    record(
                        state,
                        &call.idem,
                        json!({
                            "membershipId": membership.membership_id.as_str(),
                            "approvalState": membership.state.approval_state(),
                            "revision": membership.revision.get(),
                            "idempotent": true,
                        }),
                    );
                    return Ok((
                        DecisionOutcome {
                            membership,
                            idempotent: true,
                        },
                        Vec::new(),
                    ));
                }
                membership.transition(target, call.now_ms)?;
                if target == MembershipState::Approved {
                    membership.lease_until_ms = Some(call.now_ms.saturating_add(MEMBER_LEASE_MS));
                }
                membership.clone()
            };

            record(
                state,
                &call.idem,
                json!({
                    "membershipId": membership.membership_id.as_str(),
                    "approvalState": membership.state.approval_state(),
                    "revision": membership.revision.get(),
                    "idempotent": false,
                }),
            );
            Ok((
                DecisionOutcome {
                    membership: membership.clone(),
                    idempotent: false,
                },
                vec![
                    StoreEvent::MembershipChanged {
                        membership_id: membership.membership_id.clone(),
                    },
                    StoreEvent::RoomChanged {
                        room_id: membership.room_id.clone(),
                    },
                ],
            ))
        })
    }

    /// Cancel a join request, leave an approved membership, or remove a member.
    ///
    /// Authorization is checked **before** any idempotent replay is answered: a
    /// replay may only return the cached result to the same subject, otherwise an
    /// unrelated caller could read someone else's projection by reusing a key.
    pub fn leave(&self, call: LeaveCall) -> Result<(LeaveOutcome, Vec<StoreEvent>), RoomsError> {
        self.tx(|state| {
            let (room_id, member_user_id, member_revision) = state
                .memberships
                .get(&call.membership_id)
                .map(|membership| {
                    (
                        membership.room_id.clone(),
                        membership.user_id.clone(),
                        membership.revision,
                    )
                })
                .ok_or_else(|| RoomsError::MembershipNotFound {
                    membership_id: call.membership_id.as_str().to_string(),
                })?;
            let room =
                state
                    .rooms
                    .get(&room_id)
                    .cloned()
                    .ok_or_else(|| RoomsError::RoomNotFound {
                        room_id: room_id.as_str().to_string(),
                    })?;
            let is_owner = room.owner_id == call.actor_id;
            let terminal_target = if call.by_owner {
                MembershipState::Removed
            } else {
                MembershipState::Left
            };

            // 1. Authorization first (member themselves, or the room owner).
            if !is_owner && member_user_id != call.actor_id {
                return Err(RoomsError::IdentityMismatch {
                    detail: "only the member or the room owner may act".to_string(),
                });
            }
            // 2. Then a replay may answer — and only with this subject's result.
            //    A retry of an operation that already committed must be replayable even
            //    though the revision moved on because of that very commit (D7). The
            //    version fence therefore comes *after* the replay, exactly like
            //    `decide` / `end` / `close_joins`.
            if state.idempotency.contains_key(&call.idem.key) {
                check_replay_digest(state, &call.idem)?;
                let membership = state
                    .memberships
                    .get(&call.membership_id)
                    .cloned()
                    .ok_or_else(|| RoomsError::MembershipNotFound {
                        membership_id: call.membership_id.as_str().to_string(),
                    })?;
                return Ok((
                    LeaveOutcome {
                        membership,
                        idempotent: true,
                        revoked_connections: Vec::new(),
                        released_grants: Vec::new(),
                        left_by_self: !call.by_owner,
                    },
                    Vec::new(),
                ));
            }
            // 3. Revision fence for the actual mutation.
            if let Some(expected) = call.expected_revision {
                if expected != member_revision {
                    return Err(RoomsError::MembershipConflict {
                        expected: expected.get(),
                        actual: member_revision.get(),
                    });
                }
            }

            let membership = {
                let membership =
                    state
                        .memberships
                        .get_mut(&call.membership_id)
                        .ok_or_else(|| RoomsError::MembershipNotFound {
                            membership_id: call.membership_id.as_str().to_string(),
                        })?;
                if membership.state == terminal_target {
                    let membership = membership.clone();
                    record(
                        state,
                        &call.idem,
                        json!({
                            "membershipId": membership.membership_id.as_str(),
                            "approvalState": membership.state.approval_state(),
                            "revision": membership.revision.get(),
                            "idempotent": true,
                        }),
                    );
                    return Ok((
                        LeaveOutcome {
                            membership,
                            idempotent: true,
                            revoked_connections: Vec::new(),
                            released_grants: Vec::new(),
                            left_by_self: !call.by_owner,
                        },
                        Vec::new(),
                    ));
                }
                if membership.state.is_terminal() {
                    return Err(RoomsError::MembershipTerminal {
                        state: membership.state.as_str().to_string(),
                    });
                }
                if membership.state == MembershipState::Approved {
                    // Close the permission first: generation +1, no renewal.
                    membership.revoke_authorization()?;
                }
                membership.transition(terminal_target, call.now_ms)?;
                membership.clone()
            };

            // Every connection this membership held loses its authorization.
            let mut revoked = Vec::new();
            let mut events = vec![StoreEvent::MembershipChanged {
                membership_id: membership.membership_id.clone(),
            }];
            for connection in state.connections.values_mut() {
                if connection.membership_id != call.membership_id || connection.state.is_terminal()
                {
                    continue;
                }
                connection.transition(ConnectionState::Disconnected)?;
                revoked.push(connection.clone());
                events.push(StoreEvent::ConnectionChanged {
                    connection_id: connection.connection_id.clone(),
                });
            }
            // Relay authorization is per member: only this member's grants are
            // released. Other online members keep their tunnels untouched.
            let granted: Vec<GrantId> = state
                .grants
                .iter()
                .filter(|(_, entry)| entry.membership_id == call.membership_id)
                .map(|(id, _)| id.clone())
                .collect();
            for grant_id in &granted {
                state.grants.remove(grant_id);
            }
            events.push(StoreEvent::RoomChanged {
                room_id: membership.room_id.clone(),
            });

            record(
                state,
                &call.idem,
                json!({
                    "membershipId": membership.membership_id.as_str(),
                    "approvalState": membership.state.approval_state(),
                    "revision": membership.revision.get(),
                    "authorizationGeneration": membership.authorization_generation,
                    "idempotent": false,
                }),
            );
            Ok((
                LeaveOutcome {
                    membership,
                    idempotent: false,
                    revoked_connections: revoked,
                    released_grants: granted,
                    left_by_self: !call.by_owner,
                },
                events,
            ))
        })
    }

    /// Renew the member lease. Refuses when the previous lease already expired, so
    /// a silent link cannot keep a permission alive past its deadline.
    pub fn heartbeat(
        &self,
        membership_id: &MembershipId,
        expected_generation: Option<u64>,
        actor_id: &str,
        server_now_ms: Option<u64>,
    ) -> Result<(Membership, Vec<StoreEvent>), RoomsError> {
        let now = server_now_ms.ok_or(RoomsError::LeaseUnauthorized)?;
        self.tx(|state| {
            let membership = state
                .memberships
                .get(membership_id)
                .cloned()
                .ok_or_else(|| RoomsError::MembershipNotFound {
                    membership_id: membership_id.as_str().to_string(),
                })?;
            if membership.user_id != actor_id {
                return Err(RoomsError::IdentityMismatch {
                    detail: "heartbeat must come from the member".to_string(),
                });
            }
            if !membership.state.allows_connection() {
                return Err(RoomsError::MembershipTerminal {
                    state: membership.state.as_str().to_string(),
                });
            }
            if let Some(expected) = expected_generation {
                if expected != membership.authorization_generation {
                    return Err(RoomsError::StaleAuthorization {
                        expected: membership.authorization_generation,
                        actual: expected,
                    });
                }
            }
            lease::require_valid(server_now_ms, membership.lease_until_ms)?;
            let membership = {
                let membership = state.memberships.get_mut(membership_id).ok_or_else(|| {
                    RoomsError::MembershipNotFound {
                        membership_id: membership_id.as_str().to_string(),
                    }
                })?;
                membership.lease_until_ms = Some(now.saturating_add(MEMBER_LEASE_MS));
                membership.bump_revision()?;
                membership.clone()
            };
            Ok((
                membership,
                vec![StoreEvent::MembershipChanged {
                    membership_id: membership_id.clone(),
                }],
            ))
        })
    }

    /// Member lease state for a caller that is about to act.
    pub fn lease_state(
        &self,
        membership_id: &MembershipId,
        server_now_ms: Option<u64>,
    ) -> Result<lease::LeaseCheck, RoomsError> {
        let membership = self.membership(membership_id)?;
        Ok(lease::check(server_now_ms, membership.lease_until_ms))
    }

    /// Terminate a membership whose lease lapsed: the same effect as a removal, so
    /// an interrupted control link ends by the lease deadline at the latest.
    pub fn expire_member(
        &self,
        membership_id: &MembershipId,
        now_ms: u64,
    ) -> Result<(LeaveOutcome, Vec<StoreEvent>), RoomsError> {
        let actor_id = self.membership(membership_id)?.user_id;
        self.leave(LeaveCall {
            idem: IdemKey::new(
                format!("lease-expiry:{}", membership_id.as_str()),
                membership_id.as_str(),
            ),
            membership_id: membership_id.clone(),
            expected_revision: None,
            actor_id,
            now_ms,
            by_owner: false,
        })
    }

    // -- connections -------------------------------------------------------

    /// Create the `gathering` connection record for an approved member.
    pub fn prepare_connection(
        &self,
        membership_id: &MembershipId,
        server_now_ms: Option<u64>,
    ) -> Result<(ConnectionPlan, Vec<StoreEvent>), RoomsError> {
        let now = server_now_ms.ok_or(RoomsError::LeaseUnauthorized)?;
        self.tx(|state| {
            let membership = state
                .memberships
                .get(membership_id)
                .cloned()
                .ok_or_else(|| RoomsError::MembershipNotFound {
                    membership_id: membership_id.as_str().to_string(),
                })?;
            if !membership.state.allows_connection() {
                return Err(RoomsError::ConnectionRejected {
                    detail: format!("membership is {}", membership.state.as_str()),
                });
            }
            let room = Self::usable_room(state, &membership.room_id, now)?;
            lease::require_valid(server_now_ms, membership.lease_until_ms)?;

            state.counters.connection += 1;
            let connection_id =
                ConnectionId::new(format!("connection-{}", state.counters.connection))?;
            let connection = ConnectionSession {
                connection_id: connection_id.clone(),
                membership_id: membership.membership_id.clone(),
                room_id: room.room_id.clone(),
                state: ConnectionState::Gathering,
                transport: None,
                generation: membership.authorization_generation,
                // A gathering connection holds no lease yet.
                lease_until_ms: None,
                revision: Revision::new(1),
                opened_at_ms: now,
            };
            state
                .connections
                .insert(connection_id.clone(), connection.clone());
            Ok((
                ConnectionPlan {
                    connection: connection.clone(),
                    room,
                    member_lease_until_ms: membership.lease_until_ms,
                },
                vec![StoreEvent::ConnectionChanged { connection_id }],
            ))
        })
    }

    /// Record the chosen path. Refuses when the membership permission moved on.
    pub fn commit_connected(
        &self,
        connection_id: &ConnectionId,
        transport: TransportKind,
        server_now_ms: Option<u64>,
        relay_grant: Option<RelayGrant>,
    ) -> Result<(ConnectionSession, Vec<StoreEvent>), RoomsError> {
        let now = server_now_ms.ok_or(RoomsError::LeaseUnauthorized)?;
        self.tx(|state| {
            let (membership_id, generation) = state
                .connections
                .get(connection_id)
                .map(|connection| (connection.membership_id.clone(), connection.generation))
                .ok_or_else(|| RoomsError::ConnectionNotFound {
                    connection_id: connection_id.as_str().to_string(),
                })?;
            let membership = state
                .memberships
                .get(&membership_id)
                .cloned()
                .ok_or_else(|| RoomsError::MembershipNotFound {
                    membership_id: membership_id.as_str().to_string(),
                })?;
            if membership.authorization_generation != generation {
                return Err(RoomsError::StaleAuthorization {
                    expected: membership.authorization_generation,
                    actual: generation,
                });
            }
            if !membership.state.allows_connection() {
                return Err(RoomsError::ConnectionRejected {
                    detail: format!("membership is {}", membership.state.as_str()),
                });
            }
            let member_lease = lease::require_valid(server_now_ms, membership.lease_until_ms)?;
            let deadline =
                lease::earliest(now.saturating_add(CONNECTION_LEASE_MS), Some(member_lease));

            if let Some(grant) = relay_grant {
                // Every authority narrows the window; nothing here may extend it.
                // `min` of (the relay's own deadline, the member lease, the connection
                // lease). A relay that hands back a shorter window keeps it.
                let grant_deadline =
                    lease::earliest(grant.expires_at_ms, Some(member_lease)).min(deadline);
                if grant_deadline <= now {
                    // An authorization that is already over is not a usable path.
                    return Err(RoomsError::RelayUnavailable);
                }
                let grant = RelayGrant {
                    expires_at_ms: grant_deadline,
                    ..grant
                };
                state.grants.insert(
                    grant.grant_id.clone(),
                    GrantEntry {
                        grant,
                        membership_id: membership_id.clone(),
                        connection_id: connection_id.clone(),
                    },
                );
            }

            let connection = {
                let connection = state.connections.get_mut(connection_id).ok_or_else(|| {
                    RoomsError::ConnectionNotFound {
                        connection_id: connection_id.as_str().to_string(),
                    }
                })?;
                connection.transport = Some(transport);
                connection.lease_until_ms = Some(deadline);
                connection.transition(ConnectionState::Connected)?;
                connection.clone()
            };
            Ok((
                connection.clone(),
                vec![StoreEvent::ConnectionChanged {
                    connection_id: connection.connection_id.clone(),
                }],
            ))
        })
    }

    /// Move a connection along its state machine (`gathering -> connecting …`).
    pub fn advance_connection(
        &self,
        connection_id: &ConnectionId,
        next: ConnectionState,
    ) -> Result<(ConnectionSession, Vec<StoreEvent>), RoomsError> {
        self.tx(|state| {
            let connection = {
                let connection = state.connections.get_mut(connection_id).ok_or_else(|| {
                    RoomsError::ConnectionNotFound {
                        connection_id: connection_id.as_str().to_string(),
                    }
                })?;
                connection.transition(next)?;
                connection.clone()
            };
            let events = vec![StoreEvent::ConnectionChanged {
                connection_id: connection.connection_id.clone(),
            }];
            Ok((connection, events))
        })
    }

    pub fn fail_connection(
        &self,
        connection_id: &ConnectionId,
    ) -> Result<(ConnectionSession, Vec<StoreEvent>), RoomsError> {
        self.tx(|state| {
            let connection = {
                let connection = state.connections.get_mut(connection_id).ok_or_else(|| {
                    RoomsError::ConnectionNotFound {
                        connection_id: connection_id.as_str().to_string(),
                    }
                })?;
                if connection.state.is_terminal() {
                    return Ok((connection.clone(), Vec::new()));
                }
                connection.transition(ConnectionState::Failed)?;
                connection.clone()
            };
            let events = vec![StoreEvent::ConnectionChanged {
                connection_id: connection.connection_id.clone(),
            }];
            Ok((connection, events))
        })
    }

    /// Close one connection. Relaying is released for **this** connection only, and
    /// the caller is told which grants to release so the endpoint can be notified.
    pub fn close_connection(
        &self,
        connection_id: &ConnectionId,
        actor_id: &str,
    ) -> Result<(ConnectionSession, Vec<GrantId>, Vec<StoreEvent>), RoomsError> {
        self.tx(|state| {
            let (membership_id, room_id) = state
                .connections
                .get(connection_id)
                .map(|connection| (connection.membership_id.clone(), connection.room_id.clone()))
                .ok_or_else(|| RoomsError::ConnectionNotFound {
                    connection_id: connection_id.as_str().to_string(),
                })?;
            let membership = state
                .memberships
                .get(&membership_id)
                .cloned()
                .ok_or_else(|| RoomsError::MembershipNotFound {
                    membership_id: membership_id.as_str().to_string(),
                })?;
            let room =
                state
                    .rooms
                    .get(&room_id)
                    .cloned()
                    .ok_or_else(|| RoomsError::RoomNotFound {
                        room_id: room_id.as_str().to_string(),
                    })?;
            if membership.user_id != actor_id && room.owner_id != actor_id {
                return Err(RoomsError::IdentityMismatch {
                    detail: "only the member or the room owner may close a connection".to_string(),
                });
            }
            let connection = {
                let connection = state.connections.get_mut(connection_id).ok_or_else(|| {
                    RoomsError::ConnectionNotFound {
                        connection_id: connection_id.as_str().to_string(),
                    }
                })?;
                if connection.state.is_terminal() {
                    return Ok(((connection.clone(), Vec::new()), Vec::new()));
                }
                connection.transition(ConnectionState::Disconnected)?;
                connection.clone()
            };
            // The tunnel is gone, so its relay authorization must be released now
            // instead of lingering until the lease deadline.
            let granted: Vec<GrantId> = state
                .grants
                .iter()
                .filter(|(_, entry)| entry.connection_id == *connection_id)
                .map(|(id, _)| id.clone())
                .collect();
            for grant_id in &granted {
                state.grants.remove(grant_id);
            }
            let events = vec![StoreEvent::ConnectionChanged {
                connection_id: connection.connection_id.clone(),
            }];
            Ok(((connection, granted), events))
        })
        .map(|((connection, granted), events)| (connection, granted, events))
    }

    /// Relay deadline currently held for a connection (test/inspection hook; proves
    /// the store never widened the relay's own window).
    pub fn grant_deadline(&self, connection_id: &ConnectionId) -> Option<u64> {
        self.read(|state| {
            Ok(state
                .grants
                .values()
                .find(|entry| entry.connection_id == *connection_id)
                .map(|entry| entry.grant.expires_at_ms))
        })
        .unwrap_or(None)
    }

    /// Gate used before forwarding bytes: the permission must still be current.
    pub fn authorize_connection(
        &self,
        connection_id: &ConnectionId,
        server_now_ms: Option<u64>,
    ) -> Result<ConnectionSession, RoomsError> {
        self.read(|state| {
            if server_now_ms.is_none() {
                return Err(RoomsError::LeaseUnauthorized);
            }
            let connection = state
                .connections
                .get(connection_id)
                .cloned()
                .ok_or_else(|| RoomsError::ConnectionNotFound {
                    connection_id: connection_id.as_str().to_string(),
                })?;
            let membership = state
                .memberships
                .get(&connection.membership_id)
                .cloned()
                .ok_or_else(|| RoomsError::MembershipNotFound {
                    membership_id: connection.membership_id.as_str().to_string(),
                })?;
            if membership.authorization_generation != connection.generation {
                return Err(RoomsError::StaleAuthorization {
                    expected: membership.authorization_generation,
                    actual: connection.generation,
                });
            }
            if !membership.state.allows_connection() {
                return Err(RoomsError::ConnectionRejected {
                    detail: format!("membership is {}", membership.state.as_str()),
                });
            }
            if connection.state.is_terminal() {
                return Err(RoomsError::ConnectionRejected {
                    detail: format!("connection is {}", connection.state.as_str()),
                });
            }
            lease::require_valid(server_now_ms, connection.lease_until_ms)?;
            lease::require_valid(server_now_ms, membership.lease_until_ms)?;
            Ok(connection)
        })
    }

    /// Gate used before accepting an inbound tunnel frame for a generation.
    pub fn require_generation(
        &self,
        membership_id: &MembershipId,
        presented_generation: u64,
    ) -> Result<(), RoomsError> {
        self.read(|state| {
            let membership = state
                .memberships
                .get(membership_id)
                .cloned()
                .ok_or_else(|| RoomsError::MembershipNotFound {
                    membership_id: membership_id.as_str().to_string(),
                })?;
            if membership.authorization_generation != presented_generation {
                return Err(RoomsError::StaleAuthorization {
                    expected: membership.authorization_generation,
                    actual: presented_generation,
                });
            }
            Ok(())
        })
    }

    // -- projection --------------------------------------------------------

    /// The `rooms.snapshot` projection, **trimmed by the caller's role**.
    ///
    /// Rules (documented in the 06 handoff):
    /// - `viewer = None` (no verified platform identity) → empty projection: a read
    ///   must not leak a projection just because identity is unavailable;
    /// - room owner → full projection for that room (members, connections, invites);
    /// - a member (any approval state, including terminal) → the room plus the caller's
    ///   own membership/connection in full, other members reduced to
    ///   `membershipId/userId/role/approvalState` and other connections reduced to
    ///   `connectionId/roomId/state(/transport)`, and **no invite records at all**
    ///   (only the aggregate `activeInvites` count on the room);
    /// - anyone else → the room is omitted.
    ///
    /// No projection ever contains an invite secret, a digest, a relay grant or a
    /// candidate address; the trimming removes administrative fields that only the
    /// owner needs.
    pub fn snapshot_value(
        &self,
        room_id: Option<&RoomId>,
        server_now_ms: Option<u64>,
        viewer: Option<&str>,
    ) -> Value {
        self.tx(|state| {
            // Expire due rooms first so a snapshot never shows a live deadline.
            if let Some(now) = server_now_ms {
                for room in state.rooms.values_mut() {
                    if !room.state.is_terminal() && room.expired(now) {
                        let _ = room.transition(RoomState::Expired);
                    }
                }
            }
            let mut views = Vec::new();
            let mut memberships = Vec::new();
            let mut connections = Vec::new();
            let mut invites = Vec::new();

            for (id, room) in state.rooms.iter() {
                if room_id.map_or(false, |wanted| wanted != id) {
                    continue;
                }
                let viewer_is_owner = viewer.map_or(false, |user| room.owner_id == user);
                let viewer_is_member = match viewer {
                    Some(user) => state
                        .memberships
                        .values()
                        .any(|membership| membership.room_id == *id && membership.user_id == user),
                    None => false,
                };
                if !viewer_is_owner && !viewer_is_member {
                    continue;
                }

                let members = state
                    .memberships
                    .values()
                    .filter(|membership| {
                        membership.room_id == *id && !membership.state.is_terminal()
                    })
                    .count();
                let active_invites = state
                    .invites
                    .values()
                    .filter(|invite| invite.room_id == *id && !invite.is_revoked())
                    .count();
                views.push(room.to_value(members, active_invites));

                for membership in state
                    .memberships
                    .values()
                    .filter(|membership| membership.room_id == *id)
                {
                    let own = viewer.map_or(false, |user| membership.user_id == user);
                    if viewer_is_owner || own {
                        memberships.push(Membership::to_value(membership));
                    } else {
                        // Another member: identity and approval only.
                        memberships.push(json!({
                            "membershipId": membership.membership_id.as_str(),
                            "roomId": membership.room_id.as_str(),
                            "userId": membership.user_id,
                            "role": membership.role.as_str(),
                            "approvalState": membership.state.approval_state(),
                        }));
                    }
                }

                for connection in state
                    .connections
                    .values()
                    .filter(|connection| connection.room_id == *id)
                {
                    let own = viewer.map_or(false, |user| {
                        state
                            .memberships
                            .get(&connection.membership_id)
                            .map(|membership| membership.user_id == user)
                            .unwrap_or(false)
                    });
                    if viewer_is_owner || own {
                        connections.push(ConnectionSession::to_value(connection));
                    } else {
                        let mut trimmed = json!({
                            "connectionId": connection.connection_id.as_str(),
                            "roomId": connection.room_id.as_str(),
                            "state": connection.state.as_str(),
                        });
                        if connection.state.transport_allowed() {
                            if let Some(transport) = connection.transport {
                                trimmed["transport"] = json!(transport.as_str());
                            }
                        }
                        connections.push(trimmed);
                    }
                }

                // Invite records (ids, remaining uses) are owner-only.
                if viewer_is_owner {
                    for invite in state
                        .invites
                        .values()
                        .filter(|invite| invite.room_id == *id)
                    {
                        invites.push(Invite::to_value(invite));
                    }
                }
            }

            Ok((
                json!({
                    "rooms": views,
                    "memberships": memberships,
                    "connections": connections,
                    "invites": invites,
                }),
                Vec::new(),
            ))
        })
        .map(|(value, _)| value)
        .unwrap_or_else(
            |_| json!({ "rooms": [], "memberships": [], "connections": [], "invites": [] }),
        )
    }

    /// One room projection with its real member and invite counts. Used by command
    /// results so a caller never sees `memberCount: 0` for a room with members.
    pub fn room_view(
        &self,
        room_id: &RoomId,
        server_now_ms: Option<u64>,
    ) -> Result<Value, RoomsError> {
        self.read(|state| {
            if let Some(now) = server_now_ms {
                if let Some(room) = state.rooms.get_mut(room_id) {
                    if !room.state.is_terminal() && room.expired(now) {
                        let _ = room.transition(RoomState::Expired);
                    }
                }
            }
            let room = state
                .rooms
                .get(room_id)
                .ok_or_else(|| RoomsError::RoomNotFound {
                    room_id: room_id.as_str().to_string(),
                })?;
            let members = state
                .memberships
                .values()
                .filter(|membership| {
                    membership.room_id == *room_id && !membership.state.is_terminal()
                })
                .count();
            let invites = state
                .invites
                .values()
                .filter(|invite| invite.room_id == *room_id && !invite.is_revoked())
                .count();
            Ok(room.to_value(members, invites))
        })
    }

    /// Inspection hook: how many invite uses are left for a room.
    pub fn remaining_uses(&self, room_id: &RoomId) -> u32 {
        self.read(|state| {
            Ok(state
                .invites
                .values()
                .filter(|invite| invite.room_id == *room_id && !invite.is_revoked())
                .map(|invite| invite.remaining_uses)
                .sum())
        })
        .unwrap_or(0)
    }
}

fn mint_invite(
    state: &mut State,
    room_id: &RoomId,
    spec: InviteSpec,
    now_ms: u64,
) -> Result<(Invite, String), RoomsError> {
    state.counters.invite += 1;
    let invite_id = InviteId::new(format!("invite-{}", state.counters.invite))?;
    // OS entropy, fail-closed: no secret means no invite. The counter is not rolled
    // back, so a failed mint can never reuse a previous invite id.
    let secret = generate_secret()?;
    let invite = Invite::new(
        invite_id.clone(),
        room_id.clone(),
        digest_secret(&secret),
        spec,
        now_ms,
        now_ms.saturating_add(spec.ttl_ms),
    );
    state.invites.insert(invite_id, invite.clone());
    Ok((invite, secret))
}

/// Refuse a reused idempotency key whose body differs, and return the stored result.
fn check_replay_digest(state: &State, idem: &IdemKey) -> Result<Value, RoomsError> {
    let stored = state
        .idempotency
        .get(&idem.key)
        .ok_or_else(|| RoomsError::Internal {
            detail: "idempotency record disappeared".to_string(),
        })?;
    if stored.digest != idem.digest {
        return Err(RoomsError::IdempotencyConflict {
            key: idem.key.clone(),
        });
    }
    Ok(stored.result.clone())
}

fn record(state: &mut State, idem: &IdemKey, result: Value) {
    state.idempotency.insert(
        idem.key.clone(),
        Idempotent {
            digest: idem.digest.clone(),
            result,
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> InviteSpec {
        InviteSpec::new(60_000, 2).expect("spec")
    }

    fn draft() -> RoomDraft {
        RoomDraft {
            owner_id: "user-owner".to_string(),
            name: "fixture room".to_string(),
            visibility: RoomVisibility::Private,
            join_policy: JoinPolicy::Approve,
            requirements: RoomRequirements::new("1.20.1", "fabric", "digest", "microsoft"),
            requirements_revision: Revision::new(3),
            game_port: 25_565,
            created_at_ms: 1_000,
            expires_at_ms: None,
        }
    }

    fn settled(store: &RoomStore) -> (RoomId, String) {
        let (room, _) = store.commit_room(draft()).expect("room");
        let (_, secret, _) = store
            .create_invite(&room.room_id, spec(), "user-owner", 1_000)
            .expect("invite");
        (room.room_id, secret)
    }

    fn join(
        store: &RoomStore,
        room: &RoomId,
        secret: &str,
        user: &str,
        key: &str,
        now: u64,
    ) -> Membership {
        let (outcome, _) = store
            .join(JoinCall {
                idem: IdemKey::new(key, format!("{secret}:{user}")),
                room_id: Some(room.clone()),
                secret: secret.to_string(),
                user_id: user.to_string(),
                profile_capability: None,
                now_ms: now,
            })
            .expect("join");
        outcome.membership
    }

    fn approve(store: &RoomStore, membership: &MembershipId, key: &str, now: u64) -> Membership {
        let (outcome, _) = store
            .decide(DecideCall {
                idem: IdemKey::new(key, "approve"),
                membership_id: membership.clone(),
                approve: true,
                expected_revision: None,
                actor_id: "user-owner".to_string(),
                now_ms: now,
            })
            .expect("approve");
        outcome.membership
    }

    #[test]
    fn invite_uses_are_consumed_once_per_membership() {
        let store = RoomStore::new();
        let (room, secret) = settled(&store);
        assert_eq!(store.remaining_uses(&room), 2);
        let membership = join(&store, &room, &secret, "user-a", "op-1", 1_100);
        assert_eq!(membership.state, MembershipState::Requested);
        assert_eq!(store.remaining_uses(&room), 1);

        // Retrying the same operation must not consume a second use.
        let retried = join(&store, &room, &secret, "user-a", "op-1", 1_200);
        assert_eq!(retried.membership_id, membership.membership_id);
        assert_eq!(store.remaining_uses(&room), 1);
    }

    #[test]
    fn cancel_before_approve_makes_the_approval_fail() {
        let store = RoomStore::new();
        let (room, secret) = settled(&store);
        let membership = join(&store, &room, &secret, "user-a", "op-1", 1_100);

        let (left, _) = store
            .leave(LeaveCall {
                idem: IdemKey::new("leave-1", "leave"),
                membership_id: membership.membership_id.clone(),
                expected_revision: None,
                actor_id: "user-a".to_string(),
                now_ms: 1_200,
                by_owner: false,
            })
            .expect("leave");
        assert_eq!(left.membership.state, MembershipState::Left);

        let error = store
            .decide(DecideCall {
                idem: IdemKey::new("decide-1", "approve"),
                membership_id: membership.membership_id.clone(),
                approve: true,
                expected_revision: None,
                actor_id: "user-owner".to_string(),
                now_ms: 1_300,
            })
            .expect_err("terminal membership cannot be approved");
        assert_eq!(
            error.code(),
            super::super::error::codes::MEMBERSHIP_TERMINAL
        );
        assert_eq!(
            store
                .membership(&membership.membership_id)
                .expect("still there")
                .state,
            MembershipState::Left
        );
    }

    #[test]
    fn approve_then_cancel_raises_the_generation() {
        let store = RoomStore::new();
        let (room, secret) = settled(&store);
        let membership = join(&store, &room, &secret, "user-a", "op-1", 1_100);
        let approved = approve(&store, &membership.membership_id, "decide-1", 1_200);
        assert_eq!(approved.state, MembershipState::Approved);
        assert_eq!(approved.authorization_generation, 1);

        let (plan, _) = store
            .prepare_connection(&membership.membership_id, Some(1_300))
            .expect("prepare");
        store
            .advance_connection(&plan.connection.connection_id, ConnectionState::Connecting)
            .expect("connecting");
        store
            .commit_connected(
                &plan.connection.connection_id,
                TransportKind::Direct,
                Some(1_400),
                None,
            )
            .expect("connect");

        let (left, events) = store
            .leave(LeaveCall {
                idem: IdemKey::new("leave-1", "leave"),
                membership_id: membership.membership_id.clone(),
                expected_revision: None,
                actor_id: "user-a".to_string(),
                now_ms: 1_500,
                by_owner: false,
            })
            .expect("leave");
        assert_eq!(left.membership.authorization_generation, 2);
        assert_eq!(left.membership.lease_until_ms, None);
        assert_eq!(left.revoked_connections.len(), 1);
        assert!(events
            .iter()
            .any(|event| matches!(event, StoreEvent::ConnectionChanged { .. })));

        let error = store
            .authorize_connection(&plan.connection.connection_id, Some(1_600))
            .expect_err("old connection is stale");
        assert_eq!(
            error.code(),
            super::super::error::codes::AUTHORIZATION_STALE
        );
    }

    #[test]
    fn repeated_approval_returns_the_same_result() {
        let store = RoomStore::new();
        let (room, secret) = settled(&store);
        let membership = join(&store, &room, &secret, "user-a", "op-1", 1_100);
        let (first, _) = store
            .decide(DecideCall {
                idem: IdemKey::new("decide-1", "approve"),
                membership_id: membership.membership_id.clone(),
                approve: true,
                expected_revision: None,
                actor_id: "user-owner".to_string(),
                now_ms: 1_200,
            })
            .expect("approve");
        let (second, events) = store
            .decide(DecideCall {
                idem: IdemKey::new("decide-2", "approve"),
                membership_id: membership.membership_id.clone(),
                approve: true,
                expected_revision: None,
                actor_id: "user-owner".to_string(),
                now_ms: 1_300,
            })
            .expect("approve again");
        assert!(!first.idempotent);
        assert!(second.idempotent);
        assert_eq!(first.membership.revision, second.membership.revision);
        assert!(events.is_empty());
    }

    #[test]
    fn lease_expiry_stops_renewal() {
        let store = RoomStore::new();
        let (room, secret) = settled(&store);
        let membership = join(&store, &room, &secret, "user-a", "op-1", 1_100);
        approve(&store, &membership.membership_id, "decide-1", 1_200);

        let (renewed, _) = store
            .heartbeat(&membership.membership_id, Some(1), "user-a", Some(30_000))
            .expect("heartbeat");
        assert_eq!(renewed.lease_until_ms, Some(90_000));

        let error = store
            .heartbeat(&membership.membership_id, Some(1), "user-a", Some(95_000))
            .expect_err("expired lease");
        assert_eq!(error.code(), super::super::error::codes::LEASE_EXPIRED);
    }

    #[test]
    fn an_unknown_secret_is_an_invalid_invite() {
        let store = RoomStore::new();
        let (room, _) = settled(&store);
        let error = store
            .join(JoinCall {
                idem: IdemKey::new("op-1", "x"),
                room_id: Some(room),
                secret: "not-a-secret".to_string(),
                user_id: "user-a".to_string(),
                profile_capability: None,
                now_ms: 1_100,
            })
            .expect_err("invalid invite");
        assert_eq!(error.code(), super::super::error::codes::INVITE_INVALID);
    }

    #[test]
    fn ending_a_room_revokes_every_permission() {
        let store = RoomStore::new();
        let (room, secret) = settled(&store);
        let membership = join(&store, &room, &secret, "user-a", "op-1", 1_100);
        let approved = approve(&store, &membership.membership_id, "decide-1", 1_200);
        assert_eq!(approved.state, MembershipState::Approved);

        let (outcome, _) = store
            .end_room(
                &IdemKey::new("end-1", "end"),
                &room,
                None,
                "user-owner",
                1_300,
            )
            .expect("end room");
        assert_eq!(outcome.room.state, RoomState::Ended);
        assert_eq!(
            store
                .membership(&membership.membership_id)
                .expect("membership")
                .state,
            MembershipState::Left
        );
        assert!(store
            .join(JoinCall {
                idem: IdemKey::new("op-2", "y"),
                room_id: Some(room.clone()),
                secret: secret.clone(),
                user_id: "user-b".to_string(),
                profile_capability: None,
                now_ms: 1_400,
            })
            .is_err());
    }
}
