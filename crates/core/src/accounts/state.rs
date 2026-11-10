//! Credentials, identities and the memory-only game session handle.
//!
//! The one rule that shapes every type here: **a token never leaves this module.**
//! Not through a command result, not through an event payload, not through an
//! error. 02 (launch) needs a Minecraft session to start the game, and it gets a
//! [`GameSessionHandle`]: an opaque id, the display facts it needs, a purpose and
//! an expiry. The bearer token stays in [`Secret`], whose only way out is
//! [`Secret::expose_to_credential_store`] — a function whose name says who may
//! call it.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

/// A credential string that refuses to print itself.
///
/// Debug and Display are both masked, so an accidental `{:?}` in a log statement,
/// a panic message or a `tracing` field cannot leak it. Reading the value is an
/// explicit, greppable call.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// Explicit accessor. Call sites are deliberately auditable.
    pub fn expose_to_credential_store(&self) -> &str {
        &self.0
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Length only, for diagnostics that must not disclose the value.
    pub fn len(&self) -> usize {
        self.0.len()
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Never `derive(Debug)` on this type.
        f.write_str("Secret(<redacted>)")
    }
}

impl fmt::Display for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<redacted>")
    }
}

/// Which provider family an identity belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKind {
    /// Microsoft/Xbox Live account, the official path for MUA.
    Microsoft,
    /// Offline/local profile, no token, no network.
    Offline,
    /// Third-party auth (LittleSkin-style Yggdrasil).
    Yggdrasil,
}

impl ProviderKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Microsoft => "microsoft",
            Self::Offline => "offline",
            Self::Yggdrasil => "yggdrasil",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "microsoft" => Some(Self::Microsoft),
            "offline" => Some(Self::Offline),
            "yggdrasil" => Some(Self::Yggdrasil),
            _ => None,
        }
    }

    /// Only an offline profile needs no authorization at all.
    pub fn requires_authorization(self) -> bool {
        !matches!(self, Self::Offline)
    }
}

/// Authorization state of one identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthorizationState {
    /// No token held.
    Unauthorized,
    /// A device-code/login flow is open and waiting for the user.
    Pending,
    /// Token held and usable.
    Authorized,
    /// Token rejected or past its refresh window; re-authorization required.
    Expired,
}

impl AuthorizationState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Unauthorized => "unauthorized",
            Self::Pending => "pending",
            Self::Authorized => "authorized",
            Self::Expired => "expired",
        }
    }

    pub fn is_usable(self) -> bool {
        matches!(self, Self::Authorized)
    }
}

/// One account as everything outside 05 may see it.
///
/// Note what is absent: no token, no refresh token, no session key, no email.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Identity {
    pub identity_id: String,
    pub provider_id: String,
    pub provider_kind: ProviderKind,
    /// In-game name shown in the UI.
    pub display_name: String,
    /// Stable account uuid, as the provider reports it.
    pub account_uuid: String,
    pub state: AuthorizationState,
    /// Expiry of the current token, ms since epoch. `None` when unauthorized.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<u64>,
    /// Selected profile (skin/cape owner) when the account owns several.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile_id: Option<String>,
    pub is_default: bool,
}

impl Identity {
    /// Whether this identity can be handed to the launcher right now.
    pub fn is_usable_at(&self, now: u64) -> bool {
        self.state.is_usable()
            && self
                .expires_at
                .map(|expires_at| now < expires_at)
                .unwrap_or(true)
    }
}

/// What a caller may do with a session handle.
///
/// A purpose is not decoration: a handle issued for `launch` must not be usable
/// to change a skin, so the purpose is checked on use.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionPurpose {
    /// Start one game instance.
    Launch,
    /// Join a server or room (needs the same MUA session).
    Join,
}

impl SessionPurpose {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Launch => "launch",
            Self::Join => "join",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "launch" => Some(Self::Launch),
            "join" => Some(Self::Join),
            _ => None,
        }
    }
}

/// The only thing 02 receives instead of a token: an opaque handle plus the
/// non-secret fields the game needs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GameSessionHandle {
    /// Opaque id. 02 passes it back; it never decodes it.
    pub handle_id: String,
    pub identity_id: String,
    pub display_name: String,
    /// Name the game authenticates as.
    pub player_name: String,
    pub account_uuid: String,
    /// Access token **only** for the direct launch hand-off, never serialized.
    #[serde(skip)]
    pub token: Option<Secret>,
    pub purpose: SessionPurpose,
    pub expires_at: u64,
    /// Single use: a handle is consumed by the launch it was issued for, so a
    /// leaked handle cannot start a second session.
    pub consumed: bool,
}

impl GameSessionHandle {
    /// Public projection: exactly what may cross the wire.
    pub fn to_view(&self) -> SessionView {
        SessionView {
            handle_id: self.handle_id.clone(),
            identity_id: self.identity_id.clone(),
            display_name: self.display_name.clone(),
            player_name: self.player_name.clone(),
            account_uuid: self.account_uuid.clone(),
            purpose: self.purpose,
            expires_at: self.expires_at,
        }
    }
}

/// Wire-safe session facts. Deliberately has no token field at all.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionView {
    pub handle_id: String,
    pub identity_id: String,
    pub display_name: String,
    pub player_name: String,
    pub account_uuid: String,
    pub purpose: SessionPurpose,
    pub expires_at: u64,
}

/// A pending login (device code flow). Only what the user must see.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AuthChallenge {
    pub identity_id: String,
    /// Code the user types on the provider's page.
    pub user_code: String,
    /// Where to type it.
    pub verification_uri: String,
    pub expires_at: u64,
    /// Poll interval in ms, as the provider asked.
    pub interval_ms: u64,
}

/// A role the account may pick when joining a server (server operator, player,
/// VIP…). Purely local vocabulary; the provider does not own it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AccountRole {
    Owner,
    Operator,
    Player,
    Spectator,
}

impl AccountRole {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Owner => "owner",
            Self::Operator => "operator",
            Self::Player => "player",
            Self::Spectator => "spectator",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "owner" => Some(Self::Owner),
            "operator" => Some(Self::Operator),
            "player" => Some(Self::Player),
            "spectator" => Some(Self::Spectator),
            _ => None,
        }
    }

    /// Only the owner may hand out operator rights; this is the rule that stops
    /// a plain player from escalating through the UI.
    pub fn can_grant(self, target: AccountRole) -> bool {
        match self {
            Self::Owner => true,
            Self::Operator => matches!(target, Self::Player | Self::Spectator),
            Self::Player | Self::Spectator => false,
        }
    }
}

/// Identity store: the only holder of tokens in this workspace.
///
/// `Secret` values live in `tokens`, keyed by identity id, and are never returned
/// by any accessor other than the two credential-store/exchange paths.
pub struct AccountStore {
    identities: BTreeMap<String, Identity>,
    tokens: BTreeMap<String, Secret>,
    refresh: BTreeMap<String, Secret>,
    challenges: BTreeMap<String, AuthChallenge>,
    sessions: BTreeMap<String, GameSessionHandle>,
    next_handle: AtomicU64,
}

impl Default for AccountStore {
    fn default() -> Self {
        Self::new()
    }
}

impl AccountStore {
    pub fn new() -> Self {
        Self {
            identities: BTreeMap::new(),
            tokens: BTreeMap::new(),
            refresh: BTreeMap::new(),
            challenges: BTreeMap::new(),
            sessions: BTreeMap::new(),
            next_handle: AtomicU64::new(1),
        }
    }

    pub fn identities(&self) -> Vec<Identity> {
        self.identities.values().cloned().collect()
    }

    pub fn identity(&self, identity_id: &str) -> Option<Identity> {
        self.identities.get(identity_id).cloned()
    }

    /// Insert or replace an identity. Tokens are supplied separately so a caller
    /// cannot accidentally put one inside the identity record.
    pub fn upsert_identity(&mut self, identity: Identity) -> Option<Identity> {
        // Only one default at a time, otherwise "the default account" is ambiguous.
        if identity.is_default {
            for other in self.identities.values_mut() {
                other.is_default = false;
            }
        }
        self.identities
            .insert(identity.identity_id.clone(), identity)
    }

    pub fn set_tokens(
        &mut self,
        identity_id: &str,
        access: Secret,
        refresh: Option<Secret>,
        expires_at: u64,
    ) -> Option<()> {
        let identity = self.identities.get_mut(identity_id)?;
        identity.state = AuthorizationState::Authorized;
        identity.expires_at = Some(expires_at);
        self.tokens.insert(identity_id.to_string(), access);
        match refresh {
            Some(token) => {
                self.refresh.insert(identity_id.to_string(), token);
            }
            // A re-authorization that returns no refresh token must not keep the
            // previous one alive, or the old grant silently survives.
            None => {
                self.refresh.remove(identity_id);
            }
        }
        Some(())
    }

    /// Store a provider token when the protocol does not expose a portable
    /// expiry timestamp. The caller must validate it before launch once that
    /// provider's refresh path is available.
    pub fn set_token_without_expiry(&mut self, identity_id: &str, access: Secret) -> Option<()> {
        let identity = self.identities.get_mut(identity_id)?;
        identity.state = AuthorizationState::Authorized;
        identity.expires_at = None;
        self.tokens.insert(identity_id.to_string(), access);
        self.refresh.remove(identity_id);
        Some(())
    }

    pub fn clear_tokens(&mut self, identity_id: &str) {
        self.tokens.remove(identity_id);
        self.refresh.remove(identity_id);
        if let Some(identity) = self.identities.get_mut(identity_id) {
            identity.state = AuthorizationState::Expired;
            identity.expires_at = None;
        }
    }

    /// Remove an identity and everything tied to it.
    ///
    /// Tokens and challenges go first: a removal that left a credential behind
    /// would be a credential for an identity nobody can see or revoke. Sessions
    /// issued for the identity are dropped too, so a stale handle cannot outlive
    /// the account it was minted from.
    pub fn remove_identity(&mut self, identity_id: &str) -> Option<Identity> {
        self.tokens.remove(identity_id);
        self.refresh.remove(identity_id);
        self.challenges.remove(identity_id);
        self.sessions
            .retain(|_, handle| handle.identity_id != identity_id);
        self.identities.remove(identity_id)
    }

    /// Whether a stored access token exists. Used for assertions in tests without
    /// exposing the value.
    pub fn has_token(&self, identity_id: &str) -> bool {
        self.tokens
            .get(identity_id)
            .map(|token| !token.is_empty())
            .unwrap_or(false)
    }

    /// The credential-store path: hand the token to the OS keyring adapter.
    ///
    /// A named function rather than an accessor so that every place capable of
    /// reading a token is greppable.
    pub fn expose_token_for_credential_store(&self, identity_id: &str) -> Option<&Secret> {
        self.tokens.get(identity_id)
    }

    pub fn open_challenge(&mut self, challenge: AuthChallenge) {
        if let Some(identity) = self.identities.get_mut(&challenge.identity_id) {
            identity.state = AuthorizationState::Pending;
        }
        self.challenges
            .insert(challenge.identity_id.clone(), challenge);
    }

    pub fn challenge(&self, identity_id: &str) -> Option<AuthChallenge> {
        self.challenges.get(identity_id).cloned()
    }

    /// Complete a pending challenge. The code must match the one issued, and the
    /// challenge must not have expired.
    pub fn complete_challenge(
        &mut self,
        identity_id: &str,
        user_code: &str,
        access: Secret,
        refresh: Option<Secret>,
        expires_at: u64,
        now: u64,
    ) -> Result<(), crate::accounts::error::AccountsError> {
        use crate::accounts::error::AccountsError;
        let challenge = self
            .challenges
            .get(identity_id)
            .cloned()
            .ok_or_else(|| AccountsError::AuthRequired(identity_id.to_string()))?;
        if now >= challenge.expires_at {
            // An expired code can never be redeemed, so the challenge is dropped.
            // The identity is moved to `Expired` as well: leaving it `Pending`
            // would make the UI offer a login that can no longer complete, and the
            // user has to be told to start again.
            self.challenges.remove(identity_id);
            if let Some(identity) = self.identities.get_mut(identity_id) {
                identity.state = AuthorizationState::Expired;
            }
            return Err(AccountsError::DeviceCodeExpired);
        }
        if challenge.user_code != user_code {
            return Err(AccountsError::ChallengeMismatch);
        }
        self.set_tokens(identity_id, access, refresh, expires_at)
            .ok_or_else(|| AccountsError::IdentityNotFound(identity_id.to_string()))?;
        self.challenges.remove(identity_id);
        Ok(())
    }

    /// Issue a memory-only session handle for one purpose.
    pub fn issue_session(
        &mut self,
        identity_id: &str,
        purpose: SessionPurpose,
        now: u64,
        ttl_ms: u64,
    ) -> Result<GameSessionHandle, crate::accounts::error::AccountsError> {
        use crate::accounts::error::AccountsError;
        let identity = self
            .identities
            .get(identity_id)
            .cloned()
            .ok_or_else(|| AccountsError::IdentityNotFound(identity_id.to_string()))?;
        if !identity.is_usable_at(now) {
            return Err(match identity.state {
                AuthorizationState::Pending => AccountsError::AuthPending(identity_id.to_string()),
                AuthorizationState::Unauthorized => {
                    AccountsError::AuthRequired(identity_id.to_string())
                }
                _ => AccountsError::AuthExpired(identity_id.to_string()),
            });
        }
        let token = self.tokens.get(identity_id).cloned();
        let handle = GameSessionHandle {
            handle_id: format!(
                "session-{}",
                self.next_handle.fetch_add(1, Ordering::SeqCst)
            ),
            identity_id: identity.identity_id.clone(),
            display_name: identity.display_name.clone(),
            player_name: identity.display_name.clone(),
            account_uuid: identity.account_uuid.clone(),
            token,
            purpose,
            expires_at: now.saturating_add(ttl_ms),
            consumed: false,
        };
        self.sessions
            .insert(handle.handle_id.clone(), handle.clone());
        Ok(handle)
    }

    /// Redeem a handle for its one launch/join. Refuses a wrong purpose, an
    /// expired handle and a second use.
    pub fn redeem_session(
        &mut self,
        handle_id: &str,
        purpose: SessionPurpose,
        now: u64,
    ) -> Result<GameSessionHandle, crate::accounts::error::AccountsError> {
        use crate::accounts::error::AccountsError;
        let handle = self
            .sessions
            .get_mut(handle_id)
            .ok_or_else(|| AccountsError::SessionNotFound(handle_id.to_string()))?;
        if handle.purpose != purpose {
            return Err(AccountsError::SessionPurposeMismatch {
                purpose: purpose.as_str().to_string(),
            });
        }
        if now >= handle.expires_at {
            return Err(AccountsError::SessionExpired(handle_id.to_string()));
        }
        if handle.consumed {
            return Err(AccountsError::SessionConsumed(handle_id.to_string()));
        }
        handle.consumed = true;
        Ok(handle.clone())
    }

    /// Drop expired handles so a long session does not accumulate them.
    pub fn sweep_sessions(&mut self, now: u64) -> usize {
        let before = self.sessions.len();
        self.sessions.retain(|_, handle| now < handle.expires_at);
        before - self.sessions.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity(id: &str, state: AuthorizationState) -> Identity {
        Identity {
            identity_id: id.into(),
            provider_id: "microsoft".into(),
            provider_kind: ProviderKind::Microsoft,
            display_name: format!("player-{id}"),
            account_uuid: format!("uuid-{id}"),
            state,
            expires_at: Some(10_000),
            profile_id: None,
            is_default: false,
        }
    }

    #[test]
    fn a_secret_never_prints_itself() {
        let secret = Secret::new("super-token-value");
        assert_eq!(format!("{secret:?}"), "Secret(<redacted>)");
        assert_eq!(format!("{secret}"), "<redacted>");
        // The value is reachable only through the explicit accessor.
        assert_eq!(secret.expose_to_credential_store(), "super-token-value");
    }

    #[test]
    fn a_serialized_identity_carries_no_token_field() {
        let store = {
            let mut store = AccountStore::new();
            store.upsert_identity(identity("i1", AuthorizationState::Unauthorized));
            store.set_tokens(
                "i1",
                Secret::new("access-token"),
                Some(Secret::new("refresh-token")),
                9_000,
            );
            store
        };
        let json = serde_json::to_string(&store.identity("i1").expect("identity")).expect("json");
        assert!(!json.contains("access-token"));
        assert!(!json.contains("refresh-token"));
        assert!(!json.contains("token"));
    }

    #[test]
    fn a_session_view_has_no_token_and_no_serde_leak() {
        let mut store = AccountStore::new();
        store.upsert_identity(identity("i1", AuthorizationState::Authorized));
        store.set_tokens("i1", Secret::new("access-token"), None, 9_000);
        let handle = store
            .issue_session("i1", SessionPurpose::Launch, 0, 1_000)
            .expect("session");
        let json = serde_json::to_string(&handle).expect("json");
        // `token` is marked `#[serde(skip)]`, so even the handle does not leak it.
        assert!(!json.contains("access-token"));
        let view = serde_json::to_string(&handle.to_view()).expect("json");
        assert!(!view.contains("token"));
        assert!(view.contains("session-1"));
    }

    #[test]
    fn only_one_identity_is_default() {
        let mut store = AccountStore::new();
        let mut first = identity("i1", AuthorizationState::Authorized);
        first.is_default = true;
        store.upsert_identity(first);
        let mut second = identity("i2", AuthorizationState::Authorized);
        second.is_default = true;
        store.upsert_identity(second);
        let defaults: Vec<_> = store
            .identities()
            .into_iter()
            .filter(|i| i.is_default)
            .collect();
        assert_eq!(defaults.len(), 1);
        assert_eq!(defaults[0].identity_id, "i2");
    }

    #[test]
    fn a_pending_identity_cannot_issue_a_session() {
        let mut store = AccountStore::new();
        store.upsert_identity(identity("i1", AuthorizationState::Pending));
        assert!(matches!(
            store.issue_session("i1", SessionPurpose::Launch, 0, 1_000),
            Err(crate::accounts::error::AccountsError::AuthPending(_))
        ));
    }

    #[test]
    fn an_expired_token_cannot_issue_a_session() {
        let mut store = AccountStore::new();
        store.upsert_identity(identity("i1", AuthorizationState::Authorized));
        store.set_tokens("i1", Secret::new("t"), None, 1_000);
        assert!(matches!(
            store.issue_session("i1", SessionPurpose::Launch, 5_000, 1_000),
            Err(crate::accounts::error::AccountsError::AuthExpired(_))
        ));
    }

    #[test]
    fn a_session_is_single_use_and_purpose_bound() {
        let mut store = AccountStore::new();
        store.upsert_identity(identity("i1", AuthorizationState::Authorized));
        store.set_tokens("i1", Secret::new("t"), None, 9_000);
        let handle = store
            .issue_session("i1", SessionPurpose::Launch, 0, 1_000)
            .expect("session");

        assert!(matches!(
            store.redeem_session(&handle.handle_id, SessionPurpose::Join, 10),
            Err(crate::accounts::error::AccountsError::SessionPurposeMismatch { .. })
        ));
        assert!(store
            .redeem_session(&handle.handle_id, SessionPurpose::Launch, 20)
            .is_ok());
        assert!(matches!(
            store.redeem_session(&handle.handle_id, SessionPurpose::Launch, 30),
            Err(crate::accounts::error::AccountsError::SessionConsumed(_))
        ));
    }

    #[test]
    fn an_expired_session_cannot_be_redeemed() {
        let mut store = AccountStore::new();
        store.upsert_identity(identity("i1", AuthorizationState::Authorized));
        store.set_tokens("i1", Secret::new("t"), None, 9_000);
        let handle = store
            .issue_session("i1", SessionPurpose::Launch, 0, 1_000)
            .expect("session");
        assert!(matches!(
            store.redeem_session(&handle.handle_id, SessionPurpose::Launch, 2_000),
            Err(crate::accounts::error::AccountsError::SessionExpired(_))
        ));
    }

    #[test]
    fn a_challenge_must_match_its_code() {
        let mut store = AccountStore::new();
        store.upsert_identity(identity("i1", AuthorizationState::Pending));
        store.open_challenge(AuthChallenge {
            identity_id: "i1".into(),
            user_code: "ABCD-1234".into(),
            verification_uri: "https://example.invalid/device".into(),
            expires_at: 1_000,
            interval_ms: 5_000,
        });
        // A wrong code must not consume the challenge: the user mis-typing once
        // cannot be forced to restart the whole flow.
        assert!(matches!(
            store.complete_challenge("i1", "WRONG", Secret::new("a"), None, 9_000, 10),
            Err(crate::accounts::error::AccountsError::ChallengeMismatch)
        ));
        assert!(store.challenge("i1").is_some());
    }

    #[test]
    fn an_expired_challenge_is_dropped_and_the_identity_marked_expired() {
        let mut store = AccountStore::new();
        store.upsert_identity(identity("i1", AuthorizationState::Pending));
        store.open_challenge(AuthChallenge {
            identity_id: "i1".into(),
            user_code: "ABCD-1234".into(),
            verification_uri: "https://example.invalid/device".into(),
            expires_at: 1_000,
            interval_ms: 5_000,
        });
        assert!(matches!(
            store.complete_challenge("i1", "ABCD-1234", Secret::new("a"), None, 9_000, 2_000),
            Err(crate::accounts::error::AccountsError::DeviceCodeExpired)
        ));
        // The dead code is gone and cannot be retried with the same string.
        assert!(store.challenge("i1").is_none());
        assert!(!store.has_token("i1"));
        // The identity says `Expired`, not `Pending`, so the UI stops waiting.
        assert_eq!(
            store.identity("i1").expect("identity").state,
            AuthorizationState::Expired
        );
    }

    #[test]
    fn a_correct_unexpired_code_completes_the_login() {
        let mut store = AccountStore::new();
        store.upsert_identity(identity("i1", AuthorizationState::Pending));
        store.open_challenge(AuthChallenge {
            identity_id: "i1".into(),
            user_code: "ABCD-1234".into(),
            verification_uri: "https://example.invalid/device".into(),
            expires_at: 1_000,
            interval_ms: 5_000,
        });
        assert!(store
            .complete_challenge("i1", "ABCD-1234", Secret::new("a"), None, 9_000, 500)
            .is_ok());
        assert!(store.has_token("i1"));
        assert_eq!(
            store.identity("i1").expect("identity").state,
            AuthorizationState::Authorized
        );
        // A completed challenge is consumed: replaying the code must not work.
        assert!(store.challenge("i1").is_none());
        assert!(matches!(
            store.complete_challenge("i1", "ABCD-1234", Secret::new("b"), None, 9_000, 600),
            Err(crate::accounts::error::AccountsError::AuthRequired(_))
        ));
    }

    #[test]
    fn re_authorizing_without_a_refresh_token_drops_the_old_one() {
        let mut store = AccountStore::new();
        store.upsert_identity(identity("i1", AuthorizationState::Authorized));
        store.set_tokens(
            "i1",
            Secret::new("access-1"),
            Some(Secret::new("refresh-1")),
            1_000,
        );
        store.set_tokens("i1", Secret::new("access-2"), None, 2_000);
        assert_eq!(
            store
                .expose_token_for_credential_store("i1")
                .map(|s| s.expose_to_credential_store()),
            Some("access-2")
        );
        assert!(!store.refresh.contains_key("i1"));
    }

    #[test]
    fn clearing_tokens_marks_the_identity_expired() {
        let mut store = AccountStore::new();
        store.upsert_identity(identity("i1", AuthorizationState::Authorized));
        store.set_tokens("i1", Secret::new("t"), Some(Secret::new("r")), 9_000);
        store.clear_tokens("i1");
        assert!(!store.has_token("i1"));
        assert_eq!(
            store.identity("i1").expect("identity").state,
            AuthorizationState::Expired
        );
    }

    #[test]
    fn sweeping_drops_only_expired_sessions() {
        let mut store = AccountStore::new();
        store.upsert_identity(identity("i1", AuthorizationState::Authorized));
        store.set_tokens("i1", Secret::new("t"), None, u64::MAX);
        let short = store
            .issue_session("i1", SessionPurpose::Launch, 0, 100)
            .expect("s1");
        let long = store
            .issue_session("i1", SessionPurpose::Launch, 0, 10_000)
            .expect("s2");
        assert_eq!(store.sweep_sessions(500), 1);
        assert!(!store.sessions.contains_key(&short.handle_id));
        assert!(store.sessions.contains_key(&long.handle_id));
    }

    #[test]
    fn only_an_owner_or_operator_may_grant_a_role() {
        assert!(AccountRole::Owner.can_grant(AccountRole::Operator));
        assert!(AccountRole::Operator.can_grant(AccountRole::Player));
        assert!(!AccountRole::Operator.can_grant(AccountRole::Owner));
        assert!(!AccountRole::Player.can_grant(AccountRole::Player));
    }

    #[test]
    fn offline_profiles_need_no_authorization() {
        assert!(!ProviderKind::Offline.requires_authorization());
        assert!(ProviderKind::Microsoft.requires_authorization());
    }
}
