//! Ports 06 consumes, plus the explicitly named controlled doubles.
//!
//! Ownership (see `docs/development/00-architecture-and-contracts.md` §2):
//! - platform identity and the account/session behind it → 05
//! - instance requirements (game version, loader, content lock) → 01
//! - "can this instance actually join?" capability table → 02
//! - signalling and relay services → platform services, **not delivered**
//! - loopback port allocation and the tunnel process identity → `apps/tunnel`,
//!   **not delivered**
//! - the authoritative service clock → the room service
//!
//! Every default here is **fail-closed**: `module(ctx)` wires
//! [`RoomsPorts::unavailable`], so a caller gets a stable `rooms.*` code instead
//! of a fabricated "connected" state. Doubles are named `Fixture*` on purpose and
//! are only constructed by tests or by an explicit `module_with_ports` call.

use std::sync::{Arc, Mutex};

use serde_json::{json, Value};
use shtmcl_domain::{InstanceId, Revision, RoomId};

use super::error::RoomsError;
use super::state::{
    ConnectionId, GrantId, MembershipId, RelayGrant, RoomRequirements, RoomVisibility,
    TunnelBinding,
};

// ---------------------------------------------------------------------------
// 05 · platform identity
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlatformIdentity {
    pub user_id: String,
    pub display_name: String,
    /// A platform-verified identity is required to create, join or be decided on.
    pub verified: bool,
}

pub trait PlatformIdentityPort: Send + Sync {
    /// The identity of the caller as the platform asserts it. Never a claim from
    /// the payload: a caller-supplied user id is not authority.
    fn current_identity(&self) -> Result<PlatformIdentity, RoomsError>;
}

// ---------------------------------------------------------------------------
// 01 · instance requirements
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstanceRequirements {
    pub requirements: RoomRequirements,
    /// The instance revision the requirements were read at.
    pub revision: Revision,
}

pub trait InstanceRequirementPort: Send + Sync {
    fn requirements(&self, instance_id: &InstanceId) -> Result<InstanceRequirements, RoomsError>;
}

// ---------------------------------------------------------------------------
// 02 · launch capability
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapabilityRequest {
    pub game_version: String,
    pub loader: String,
    pub auth_provider: String,
}

impl CapabilityRequest {
    pub fn from_requirements(requirements: &RoomRequirements) -> Self {
        Self {
            game_version: requirements.game_version.clone(),
            loader: requirements.loader.clone(),
            auth_provider: requirements.auth_provider.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JoinCapability {
    /// Stable capability id from 02's table (`direct_join`, `launch_and_join` …).
    pub capability: String,
    pub verified: bool,
    /// Only a verified capability is usable; consumers must not infer it.
    pub available: bool,
    pub reason_code: String,
}

impl JoinCapability {
    pub fn to_value(&self) -> Value {
        json!({
            "capability": self.capability,
            "verified": self.verified,
            "available": self.available,
            "reasonCode": self.reason_code,
        })
    }
}

pub trait LaunchCapabilityPort: Send + Sync {
    fn join_capability(&self, request: &CapabilityRequest) -> Result<JoinCapability, RoomsError>;
}

// ---------------------------------------------------------------------------
// signalling / relay (not delivered)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoomAdvertisement {
    pub room_id: RoomId,
    pub visibility: RoomVisibility,
    pub requirements: RoomRequirements,
    pub game_port: u16,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignalingReceipt {
    /// Opaque endpoint registration id. Not an address.
    pub endpoint_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CandidateExchange {
    pub room_id: RoomId,
    pub membership_id: MembershipId,
    pub connection_id: ConnectionId,
    pub generation: u64,
}

/// Opaque candidate tokens. Addresses are swapped endpoint-to-endpoint by the
/// signalling service and never enter this module's state, events or errors.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CandidateSet {
    pub tokens: Vec<String>,
    pub endpoint_count: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirectProbe {
    pub reachable: bool,
    /// Elapsed service time spent probing. The authority supplies it, so a test
    /// can prove a budget timeout without sleeping.
    pub elapsed_ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EndpointNoticeKind {
    MemberRemoved,
    MemberLeft,
    RoomEnded,
    AuthorizationChanged,
    ConnectionClosed,
}

impl EndpointNoticeKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::MemberRemoved => "member_removed",
            Self::MemberLeft => "member_left",
            Self::RoomEnded => "room_ended",
            Self::AuthorizationChanged => "authorization_changed",
            Self::ConnectionClosed => "connection_closed",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EndpointNotice {
    pub kind: EndpointNoticeKind,
    pub room_id: RoomId,
    pub membership_id: Option<MembershipId>,
    pub connection_id: Option<ConnectionId>,
    pub generation: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelayRequest {
    pub room_id: RoomId,
    pub membership_id: MembershipId,
    pub generation: u64,
    pub region: Option<String>,
    pub bytes_limit: u64,
    /// Remaining relay budget in service milliseconds.
    pub budget_ms: u64,
    /// Service time the attempt started at, so an adapter can express the grant
    /// deadline it proposes. The store only ever narrows that deadline.
    pub requested_at_ms: u64,
}

pub trait SignalingPort: Send + Sync {
    fn register_room(
        &self,
        advertisement: &RoomAdvertisement,
    ) -> Result<SignalingReceipt, RoomsError>;
    fn withdraw_room(&self, room_id: &RoomId) -> Result<(), RoomsError>;
    fn exchange_candidates(&self, request: &CandidateExchange) -> Result<CandidateSet, RoomsError>;
    /// Probe the direct path within `budget_ms`. The returned `elapsed_ms` is what
    /// the budget is judged against.
    fn probe_direct(
        &self,
        request: &CandidateExchange,
        budget_ms: u64,
    ) -> Result<DirectProbe, RoomsError>;
    fn notify_endpoint(&self, notice: &EndpointNotice) -> Result<(), RoomsError>;
}

pub trait RelayPort: Send + Sync {
    fn allocate(&self, request: &RelayRequest) -> Result<RelayGrant, RoomsError>;
    fn release(&self, grant_id: &GrantId) -> Result<(), RoomsError>;
}

// ---------------------------------------------------------------------------
// tunnel process / loopback
// ---------------------------------------------------------------------------

/// What the tunnel process owner reports back: a loopback port it listens on and
/// its process identity. The module composes the [`TunnelBinding`] itself so the
/// "loopback only, room port only" rule is enforced in one place.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoopbackAllocation {
    pub loopback_port: u16,
    pub process_identity: String,
}

pub trait LoopbackPort: Send + Sync {
    fn allocate(
        &self,
        room_id: &RoomId,
        target_port: u16,
    ) -> Result<LoopbackAllocation, RoomsError>;
    fn release(&self, binding: &TunnelBinding) -> Result<(), RoomsError>;
}

// ---------------------------------------------------------------------------
// service clock
// ---------------------------------------------------------------------------

pub trait ServiceClock: Send + Sync {
    /// The room service's own time in milliseconds. `None` means "no authority",
    /// which turns every lease check into a refusal.
    fn server_now_ms(&self) -> Option<u64>;
}

// ---------------------------------------------------------------------------
// the bundle
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct RoomsPorts {
    pub identity: Arc<dyn PlatformIdentityPort>,
    pub instances: Arc<dyn InstanceRequirementPort>,
    pub launch: Arc<dyn LaunchCapabilityPort>,
    pub signaling: Arc<dyn SignalingPort>,
    pub relay: Arc<dyn RelayPort>,
    pub loopback: Arc<dyn LoopbackPort>,
    pub clock: Arc<dyn ServiceClock>,
}

impl RoomsPorts {
    /// Default wiring for [`super::module`]: every neighbour is missing, so no
    /// command can pretend a room exists, a member was approved or a path exists.
    pub fn unavailable() -> Self {
        Self {
            identity: Arc::new(UnavailableIdentity),
            instances: Arc::new(UnavailableInstances),
            launch: Arc::new(UnavailableLaunch),
            signaling: Arc::new(UnavailableSignaling),
            relay: Arc::new(UnavailableRelay),
            loopback: Arc::new(UnavailableLoopback),
            clock: Arc::new(UnavailableClock),
        }
    }

    /// `true` while a real owner is still missing. The application layer uses this
    /// to avoid advertising multiplayer as available.
    pub fn is_provisional(&self) -> bool {
        true
    }
}

impl std::fmt::Debug for RoomsPorts {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RoomsPorts")
            .field("provisional", &true)
            .finish_non_exhaustive()
    }
}

// ---------------------------------------------------------------------------
// fail-closed defaults
// ---------------------------------------------------------------------------

#[derive(Debug, Default)]
pub struct UnavailableIdentity;

impl PlatformIdentityPort for UnavailableIdentity {
    fn current_identity(&self) -> Result<PlatformIdentity, RoomsError> {
        Err(RoomsError::IdentityUnavailable)
    }
}

#[derive(Debug, Default)]
pub struct UnavailableInstances;

impl InstanceRequirementPort for UnavailableInstances {
    fn requirements(&self, _instance_id: &InstanceId) -> Result<InstanceRequirements, RoomsError> {
        Err(RoomsError::OwnerUnavailable { owner: "instances" })
    }
}

#[derive(Debug, Default)]
pub struct UnavailableLaunch;

impl LaunchCapabilityPort for UnavailableLaunch {
    fn join_capability(&self, _request: &CapabilityRequest) -> Result<JoinCapability, RoomsError> {
        Err(RoomsError::OwnerUnavailable { owner: "launch" })
    }
}

#[derive(Debug, Default)]
pub struct UnavailableSignaling;

impl SignalingPort for UnavailableSignaling {
    fn register_room(
        &self,
        _advertisement: &RoomAdvertisement,
    ) -> Result<SignalingReceipt, RoomsError> {
        Err(RoomsError::SignalingUnavailable)
    }

    fn withdraw_room(&self, _room_id: &RoomId) -> Result<(), RoomsError> {
        Err(RoomsError::SignalingUnavailable)
    }

    fn exchange_candidates(
        &self,
        _request: &CandidateExchange,
    ) -> Result<CandidateSet, RoomsError> {
        Err(RoomsError::SignalingUnavailable)
    }

    fn probe_direct(
        &self,
        _request: &CandidateExchange,
        _budget_ms: u64,
    ) -> Result<DirectProbe, RoomsError> {
        Err(RoomsError::SignalingUnavailable)
    }

    fn notify_endpoint(&self, _notice: &EndpointNotice) -> Result<(), RoomsError> {
        Err(RoomsError::SignalingUnavailable)
    }
}

#[derive(Debug, Default)]
pub struct UnavailableRelay;

impl RelayPort for UnavailableRelay {
    fn allocate(&self, _request: &RelayRequest) -> Result<RelayGrant, RoomsError> {
        Err(RoomsError::RelayUnavailable)
    }

    fn release(&self, _grant_id: &GrantId) -> Result<(), RoomsError> {
        Err(RoomsError::RelayUnavailable)
    }
}

#[derive(Debug, Default)]
pub struct UnavailableLoopback;

impl LoopbackPort for UnavailableLoopback {
    fn allocate(
        &self,
        _room_id: &RoomId,
        _target_port: u16,
    ) -> Result<LoopbackAllocation, RoomsError> {
        Err(RoomsError::LoopbackUnavailable)
    }

    fn release(&self, _binding: &TunnelBinding) -> Result<(), RoomsError> {
        Err(RoomsError::LoopbackUnavailable)
    }
}

/// No service time: every lease check refuses instead of trusting a local clock.
#[derive(Debug, Default)]
pub struct UnavailableClock;

impl ServiceClock for UnavailableClock {
    fn server_now_ms(&self) -> Option<u64> {
        None
    }
}

// ---------------------------------------------------------------------------
// controlled doubles (tests only; never a default)
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub struct FixtureIdentity {
    identity: Mutex<PlatformIdentity>,
}

impl FixtureIdentity {
    pub fn new(user_id: impl Into<String>) -> Self {
        Self {
            identity: Mutex::new(PlatformIdentity {
                user_id: user_id.into(),
                display_name: "fixture player".to_string(),
                verified: true,
            }),
        }
    }

    /// Switch the caller, which is how a test plays two players.
    pub fn acting_as(&self, user_id: impl Into<String>) -> &Self {
        if let Ok(mut identity) = self.identity.lock() {
            identity.user_id = user_id.into();
        }
        self
    }

    pub fn set_verified(&self, verified: bool) -> &Self {
        if let Ok(mut identity) = self.identity.lock() {
            identity.verified = verified;
        }
        self
    }

    pub fn user_id(&self) -> String {
        self.identity
            .lock()
            .map(|identity| identity.user_id.clone())
            .unwrap_or_default()
    }
}

impl PlatformIdentityPort for FixtureIdentity {
    fn current_identity(&self) -> Result<PlatformIdentity, RoomsError> {
        self.identity
            .lock()
            .map(|identity| identity.clone())
            .map_err(|_| RoomsError::Internal {
                detail: "identity fixture lock poisoned".to_string(),
            })
    }
}

#[derive(Debug)]
pub struct FixtureInstances {
    state: Mutex<InstanceRequirements>,
}

impl FixtureInstances {
    pub fn new(requirements: RoomRequirements, revision: Revision) -> Self {
        Self {
            state: Mutex::new(InstanceRequirements {
                requirements,
                revision,
            }),
        }
    }

    pub fn standard() -> Self {
        Self::new(
            RoomRequirements::new("1.20.1", "fabric", "content-lock-digest", "microsoft"),
            Revision::new(3),
        )
    }
}

impl InstanceRequirementPort for FixtureInstances {
    fn requirements(&self, _instance_id: &InstanceId) -> Result<InstanceRequirements, RoomsError> {
        self.state
            .lock()
            .map(|state| state.clone())
            .map_err(|_| RoomsError::Internal {
                detail: "instance fixture lock poisoned".to_string(),
            })
    }
}

#[derive(Debug)]
pub struct FixtureLaunch {
    capability: Mutex<JoinCapability>,
}

impl FixtureLaunch {
    pub fn available() -> Self {
        Self {
            capability: Mutex::new(JoinCapability {
                capability: "direct_join".to_string(),
                verified: true,
                available: true,
                reason_code: "launch.quickplay.direct_join".to_string(),
            }),
        }
    }

    pub fn unavailable(reason: impl Into<String>) -> Self {
        Self {
            capability: Mutex::new(JoinCapability {
                capability: "unknown".to_string(),
                verified: false,
                available: false,
                reason_code: reason.into(),
            }),
        }
    }
}

impl LaunchCapabilityPort for FixtureLaunch {
    fn join_capability(&self, _request: &CapabilityRequest) -> Result<JoinCapability, RoomsError> {
        self.capability
            .lock()
            .map(|capability| capability.clone())
            .map_err(|_| RoomsError::Internal {
                detail: "launch fixture lock poisoned".to_string(),
            })
    }
}

#[derive(Debug, Default)]
pub struct FixtureSignaling {
    registered: Mutex<Vec<String>>,
    withdrawn: Mutex<Vec<String>>,
    notices: Mutex<Vec<EndpointNotice>>,
    direct_reachable: Mutex<bool>,
    direct_elapsed_ms: Mutex<u64>,
    exchange_calls: Mutex<u32>,
    unavailable: Mutex<bool>,
}

impl FixtureSignaling {
    pub fn new() -> Self {
        Self {
            registered: Mutex::new(Vec::new()),
            withdrawn: Mutex::new(Vec::new()),
            notices: Mutex::new(Vec::new()),
            direct_reachable: Mutex::new(true),
            direct_elapsed_ms: Mutex::new(120),
            exchange_calls: Mutex::new(0),
            unavailable: Mutex::new(false),
        }
    }

    /// Simulate a signalling outage: registration and candidate exchange fail with
    /// the same stable code the real adapter must produce.
    pub fn set_unavailable(&self, unavailable: bool) -> &Self {
        if let Ok(mut value) = self.unavailable.lock() {
            *value = unavailable;
        }
        self
    }

    fn is_unavailable(&self) -> bool {
        self.unavailable.lock().map(|value| *value).unwrap_or(true)
    }

    pub fn set_direct_reachable(&self, reachable: bool) -> &Self {
        if let Ok(mut value) = self.direct_reachable.lock() {
            *value = reachable;
        }
        self
    }

    pub fn set_direct_elapsed(&self, elapsed_ms: u64) -> &Self {
        if let Ok(mut value) = self.direct_elapsed_ms.lock() {
            *value = elapsed_ms;
        }
        self
    }

    pub fn registered(&self) -> Vec<String> {
        self.registered
            .lock()
            .map(|list| list.clone())
            .unwrap_or_default()
    }

    pub fn withdrawn(&self) -> Vec<String> {
        self.withdrawn
            .lock()
            .map(|list| list.clone())
            .unwrap_or_default()
    }

    pub fn notices(&self) -> Vec<EndpointNotice> {
        self.notices
            .lock()
            .map(|list| list.clone())
            .unwrap_or_default()
    }

    pub fn exchange_calls(&self) -> u32 {
        self.exchange_calls.lock().map(|value| *value).unwrap_or(0)
    }
}

impl SignalingPort for FixtureSignaling {
    fn register_room(
        &self,
        advertisement: &RoomAdvertisement,
    ) -> Result<SignalingReceipt, RoomsError> {
        if self.is_unavailable() {
            return Err(RoomsError::SignalingUnavailable);
        }
        // The fixture records only the room id: no address is invented.
        if let Ok(mut registered) = self.registered.lock() {
            registered.push(advertisement.room_id.as_str().to_string());
        }
        Ok(SignalingReceipt {
            endpoint_id: format!("fixture-endpoint-{}", advertisement.room_id.as_str()),
        })
    }

    fn withdraw_room(&self, room_id: &RoomId) -> Result<(), RoomsError> {
        if self.is_unavailable() {
            return Err(RoomsError::SignalingUnavailable);
        }
        if let Ok(mut withdrawn) = self.withdrawn.lock() {
            withdrawn.push(room_id.as_str().to_string());
        }
        Ok(())
    }

    fn exchange_candidates(&self, request: &CandidateExchange) -> Result<CandidateSet, RoomsError> {
        if self.is_unavailable() {
            return Err(RoomsError::SignalingUnavailable);
        }
        if let Ok(mut calls) = self.exchange_calls.lock() {
            *calls += 1;
        }
        Ok(CandidateSet {
            // Opaque tokens: deliberately not addresses.
            tokens: vec![format!("candidate-{}", request.connection_id.as_str())],
            endpoint_count: 2,
        })
    }

    fn probe_direct(
        &self,
        _request: &CandidateExchange,
        budget_ms: u64,
    ) -> Result<DirectProbe, RoomsError> {
        if self.is_unavailable() {
            return Err(RoomsError::SignalingUnavailable);
        }
        let reachable = self
            .direct_reachable
            .lock()
            .map(|value| *value)
            .unwrap_or(false);
        let elapsed_ms = self
            .direct_elapsed_ms
            .lock()
            .map(|value| *value)
            .unwrap_or(0)
            .min(budget_ms);
        Ok(DirectProbe {
            reachable,
            elapsed_ms,
        })
    }

    fn notify_endpoint(&self, notice: &EndpointNotice) -> Result<(), RoomsError> {
        if self.is_unavailable() {
            return Err(RoomsError::SignalingUnavailable);
        }
        if let Ok(mut notices) = self.notices.lock() {
            notices.push(notice.clone());
        }
        Ok(())
    }
}

#[derive(Debug)]
pub struct FixtureRelay {
    available: Mutex<bool>,
    region: String,
    bytes_limit: u64,
    /// How long the fixture proposes its grant stays valid. A real relay may propose
    /// a much shorter window; the store must never widen it.
    grant_ttl_ms: Mutex<u64>,
    released: Mutex<Vec<String>>,
}

impl FixtureRelay {
    pub fn available() -> Self {
        Self {
            available: Mutex::new(true),
            region: "fixture-region".to_string(),
            bytes_limit: 64 * 1024 * 1024,
            grant_ttl_ms: Mutex::new(60_000),
            released: Mutex::new(Vec::new()),
        }
    }

    pub fn unavailable() -> Self {
        Self {
            available: Mutex::new(false),
            ..Self::available()
        }
    }

    pub fn set_available(&self, available: bool) -> &Self {
        if let Ok(mut value) = self.available.lock() {
            *value = available;
        }
        self
    }

    /// Propose a shorter grant window than the connection lease.
    pub fn set_grant_ttl_ms(&self, ttl_ms: u64) -> &Self {
        if let Ok(mut value) = self.grant_ttl_ms.lock() {
            *value = ttl_ms;
        }
        self
    }

    pub fn released(&self) -> Vec<String> {
        self.released
            .lock()
            .map(|list| list.clone())
            .unwrap_or_default()
    }
}

impl RelayPort for FixtureRelay {
    fn allocate(&self, request: &RelayRequest) -> Result<RelayGrant, RoomsError> {
        let available = self.available.lock().map(|value| *value).unwrap_or(false);
        if !available {
            return Err(RoomsError::RelayUnavailable);
        }
        let ttl_ms = self.grant_ttl_ms.lock().map(|value| *value).unwrap_or(0);
        Ok(RelayGrant {
            grant_id: GrantId::new(format!("grant-{}", request.membership_id.as_str()))?,
            room_id: request.room_id.clone(),
            region: request
                .region
                .clone()
                .unwrap_or_else(|| self.region.clone()),
            bytes_limit: self.bytes_limit.min(request.bytes_limit.max(1)),
            // The fixture proposes exactly its own window; the store narrows it
            // further (member lease / connection lease / already-expired check).
            expires_at_ms: request.requested_at_ms.saturating_add(ttl_ms),
            peer_pair: (
                format!("peer-{}", request.membership_id.as_str()),
                "peer-room-owner".to_string(),
            ),
        })
    }

    fn release(&self, grant_id: &GrantId) -> Result<(), RoomsError> {
        if let Ok(mut released) = self.released.lock() {
            released.push(grant_id.as_str().to_string());
        }
        Ok(())
    }
}

#[derive(Debug)]
pub struct FixtureLoopback {
    next_port: Mutex<u16>,
    released: Mutex<Vec<u16>>,
}

impl FixtureLoopback {
    pub fn new(first_port: u16) -> Self {
        Self {
            next_port: Mutex::new(first_port),
            released: Mutex::new(Vec::new()),
        }
    }

    pub fn released(&self) -> Vec<u16> {
        self.released
            .lock()
            .map(|list| list.clone())
            .unwrap_or_default()
    }
}

impl LoopbackPort for FixtureLoopback {
    fn allocate(
        &self,
        _room_id: &RoomId,
        _target_port: u16,
    ) -> Result<LoopbackAllocation, RoomsError> {
        let mut port = self.next_port.lock().map_err(|_| RoomsError::Internal {
            detail: "loopback fixture lock poisoned".to_string(),
        })?;
        let allocated = *port;
        *port = port.saturating_add(1);
        Ok(LoopbackAllocation {
            // Always loopback; the module never binds a routable address.
            loopback_port: allocated,
            process_identity: "fixture-tunnel-process".to_string(),
        })
    }

    fn release(&self, binding: &TunnelBinding) -> Result<(), RoomsError> {
        if let Ok(mut released) = self.released.lock() {
            released.push(binding.loopback_port);
        }
        Ok(())
    }
}

/// Manually advanced service clock: the only way a test can cross a lease.
#[derive(Debug, Default)]
pub struct FixtureServiceClock {
    now_ms: Mutex<u64>,
}

impl FixtureServiceClock {
    pub fn new(now_ms: u64) -> Self {
        Self {
            now_ms: Mutex::new(now_ms),
        }
    }

    pub fn advance(&self, delta_ms: u64) -> &Self {
        if let Ok(mut now) = self.now_ms.lock() {
            *now = now.saturating_add(delta_ms);
        }
        self
    }

    pub fn set(&self, now_ms: u64) -> &Self {
        if let Ok(mut now) = self.now_ms.lock() {
            *now = now_ms;
        }
        self
    }

    pub fn now(&self) -> u64 {
        self.now_ms.lock().map(|now| *now).unwrap_or(0)
    }
}

impl ServiceClock for FixtureServiceClock {
    fn server_now_ms(&self) -> Option<u64> {
        Some(self.now())
    }
}

/// One bundle of controlled doubles with handles for assertions.
///
/// Construct it in a test, hand [`RoomsFixtures::ports`] to
/// [`super::module_with_ports`], and keep the struct to drive the clock and read
/// what the module told the (fake) platform.
pub struct RoomsFixtures {
    pub identity: Arc<FixtureIdentity>,
    pub instances: Arc<FixtureInstances>,
    pub launch: Arc<FixtureLaunch>,
    pub signaling: Arc<FixtureSignaling>,
    pub relay: Arc<FixtureRelay>,
    pub loopback: Arc<FixtureLoopback>,
    pub clock: Arc<FixtureServiceClock>,
}

impl Default for RoomsFixtures {
    fn default() -> Self {
        Self::new()
    }
}

impl RoomsFixtures {
    pub fn new() -> Self {
        Self {
            identity: Arc::new(FixtureIdentity::new("fixture-owner")),
            instances: Arc::new(FixtureInstances::standard()),
            launch: Arc::new(FixtureLaunch::available()),
            signaling: Arc::new(FixtureSignaling::new()),
            relay: Arc::new(FixtureRelay::available()),
            loopback: Arc::new(FixtureLoopback::new(41_000)),
            clock: Arc::new(FixtureServiceClock::new(1_000)),
        }
    }

    pub fn ports(&self) -> RoomsPorts {
        RoomsPorts {
            identity: self.identity.clone(),
            instances: self.instances.clone(),
            launch: self.launch.clone(),
            signaling: self.signaling.clone(),
            relay: self.relay.clone(),
            loopback: self.loopback.clone(),
            clock: self.clock.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_ports_fail_closed_with_stable_codes() {
        let ports = RoomsPorts::unavailable();
        assert_eq!(
            ports
                .identity
                .current_identity()
                .expect_err("no identity")
                .code(),
            super::super::error::codes::IDENTITY_UNAVAILABLE
        );
        let instance = InstanceId::new("instance-1").expect("id");
        assert_eq!(
            ports
                .instances
                .requirements(&instance)
                .expect_err("no 01")
                .code(),
            super::super::error::codes::OWNER_UNAVAILABLE
        );
        let room = RoomId::new("room-1").expect("id");
        assert_eq!(
            ports
                .signaling
                .register_room(&RoomAdvertisement {
                    room_id: room.clone(),
                    visibility: RoomVisibility::Private,
                    requirements: RoomRequirements::new("1.20.1", "vanilla", "d", "offline"),
                    game_port: 25_565,
                })
                .expect_err("no signalling")
                .code(),
            super::super::error::codes::SIGNALING_UNAVAILABLE
        );
        assert_eq!(
            ports
                .loopback
                .allocate(&room, 25_565)
                .expect_err("no tunnel")
                .code(),
            super::super::error::codes::LOOPBACK_UNAVAILABLE
        );
        assert!(ports.clock.server_now_ms().is_none());
    }

    #[test]
    fn the_clock_fixture_only_moves_when_told_to() {
        let clock = FixtureServiceClock::new(1_000);
        assert_eq!(clock.server_now_ms(), Some(1_000));
        clock.advance(500);
        assert_eq!(clock.server_now_ms(), Some(1_500));
        clock.set(9);
        assert_eq!(clock.server_now_ms(), Some(9));
    }

    #[test]
    fn the_signalling_fixture_never_invents_an_address() {
        let signaling = FixtureSignaling::new();
        let request = CandidateExchange {
            room_id: RoomId::new("room-1").expect("id"),
            membership_id: MembershipId::new("membership-1").expect("id"),
            connection_id: ConnectionId::new("connection-1").expect("id"),
            generation: 1,
        };
        let set = signaling.exchange_candidates(&request).expect("candidates");
        for token in &set.tokens {
            assert!(
                !token.contains('.') && !token.contains("://"),
                "token is not an address"
            );
        }
        let probe = signaling.probe_direct(&request, 3_000).expect("probe");
        assert!(probe.reachable);
        assert!(probe.elapsed_ms <= 3_000);
    }
}
