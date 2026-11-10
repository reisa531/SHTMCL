//! Ports 05 needs, and the **fail-closed** default used when the app layer has
//! not wired a real provider transport.
//!
//! The module never performs HTTP itself. It asks an [`AccountsPorts`] for a
//! login challenge, a poll result and the current time, which makes the device
//! code flow deterministic in tests and keeps every provider-specific shape out
//! of the module's state model.
//!
//! ## Why the token hand-off is a port
//!
//! 02 (launch) needs the access token to start Minecraft. Passing it back through
//! the command result would put it on the wire, where it would land in event
//! payloads and logs. Instead [`AccountsPorts::offer_session_token`] hands it
//! in-process to whoever registered for the handle, so the value never gets
//! serialized.

use std::collections::BTreeMap;
use std::sync::Mutex;

/// What the provider said when a login was started.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoginChallenge {
    /// Code the user types on the provider's page.
    pub user_code: String,
    /// Where to type it.
    pub verification_uri: String,
    /// Deadline for the whole flow, ms since epoch.
    pub expires_at: u64,
    /// Poll interval the provider asked for.
    pub interval_ms: u64,
}

/// A completed authorization.
#[derive(Clone, PartialEq, Eq)]
pub struct LoginGrant {
    pub access_token: String,
    pub refresh_token: Option<String>,
    /// Token expiry, ms since epoch.
    pub expires_at: u64,
    pub display_name: String,
    pub account_uuid: String,
    pub profile_id: Option<String>,
}

impl std::fmt::Debug for LoginGrant {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("LoginGrant")
            .field("access_token", &"<redacted>")
            .field(
                "refresh_token",
                &self.refresh_token.as_ref().map(|_| "<redacted>"),
            )
            .field("expires_at", &self.expires_at)
            .field("display_name", &self.display_name)
            .field("account_uuid", &self.account_uuid)
            .field("profile_id", &self.profile_id)
            .finish()
    }
}

/// A profile selected by a Yggdrasil server during a direct password login.
/// Yggdrasil access tokens have no portable expiry field, so the launcher must
/// validate or refresh them when that flow is implemented; this grant does not
/// invent an expiry time.
#[derive(Clone, PartialEq, Eq)]
pub struct YggdrasilGrant {
    pub access_token: String,
    pub display_name: String,
    pub account_uuid: String,
    pub profile_id: String,
}

impl std::fmt::Debug for YggdrasilGrant {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("YggdrasilGrant")
            .field("access_token", &"<redacted>")
            .field("display_name", &self.display_name)
            .field("account_uuid", &self.account_uuid)
            .field("profile_id", &self.profile_id)
            .finish()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum YggdrasilAuthError {
    Rejected,
    ProfileSelectionRequired,
    Unavailable,
}

/// One poll outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PollOutcome {
    /// User has not finished yet.
    Pending,
    /// User or provider refused; the flow ends.
    Denied,
    /// Authorized.
    Granted(LoginGrant),
}

/// Everything 05 needs from the outside.
pub trait AccountsPorts: Send + Sync {
    /// Whether the injected adapter has an implemented interactive/device flow
    /// for this provider. Unsupported providers fail before a misleading retry.
    fn supports_device_login(&self, _provider_id: &str) -> bool {
        false
    }

    /// Start a device-code (or equivalent) flow.
    fn begin_login(&self, provider_id: &str, now: u64) -> Result<LoginChallenge, String>;

    /// Poll once for completion. The caller owns the cadence.
    fn poll_login(
        &self,
        provider_id: &str,
        identity_id: &str,
        user_code: &str,
        now: u64,
    ) -> Result<PollOutcome, String>;

    /// Authenticate against the configured MUA Yggdrasil endpoint. This is
    /// separate from the device-code flow used by Microsoft and deliberately
    /// accepts credentials only for the duration of the call.
    fn authenticate_yggdrasil(
        &self,
        _account: &str,
        _password: &str,
    ) -> Result<YggdrasilGrant, YggdrasilAuthError> {
        Err(YggdrasilAuthError::Unavailable)
    }

    /// Hand the token for a redeemed handle to its in-process consumer (02).
    ///
    /// The consumer is registered by handle id before the launch starts, so an
    /// unclaimed token is a contract violation and must be reported, not dropped.
    fn offer_session_token(&self, handle_id: &str, token: &str) -> Result<(), String>;

    fn now_ms(&self) -> u64;
}

/// Default ports: every provider call fails.
///
/// Fail-closed on purpose. A build without a wired transport must not invent a
/// token or a display name — the user sees "provider unavailable" instead of an
/// account that cannot actually play.
pub struct UnavailableAccounts;

impl AccountsPorts for UnavailableAccounts {
    fn begin_login(&self, provider_id: &str, _now: u64) -> Result<LoginChallenge, String> {
        Err(format!("provider:{provider_id}:unavailable"))
    }

    fn poll_login(
        &self,
        provider_id: &str,
        _identity_id: &str,
        _user_code: &str,
        _now: u64,
    ) -> Result<PollOutcome, String> {
        Err(format!("provider:{provider_id}:unavailable"))
    }

    fn offer_session_token(&self, handle_id: &str, _token: &str) -> Result<(), String> {
        Err(format!("session:{handle_id}:no_consumer"))
    }

    fn now_ms(&self) -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_millis() as u64)
            .unwrap_or(0)
    }
}

/// Scripted ports for tests: no network, no filesystem, deterministic clock.
pub struct FixtureAccounts {
    inner: Mutex<FixtureState>,
}

struct FixtureState {
    challenge: LoginChallenge,
    /// Poll answers consumed in order; the last one repeats.
    polls: Vec<PollOutcome>,
    /// Tokens handed to the in-process consumer, by handle id.
    handed: BTreeMap<String, String>,
    now: u64,
    begin_fails: bool,
    yggdrasil_grant: Option<YggdrasilGrant>,
}

impl FixtureAccounts {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(FixtureState {
                challenge: LoginChallenge {
                    user_code: "ABCD-1234".into(),
                    verification_uri: "https://example.invalid/device".into(),
                    expires_at: 600_000,
                    interval_ms: 5_000,
                },
                polls: vec![PollOutcome::Pending],
                handed: BTreeMap::new(),
                now: 1_000,
                begin_fails: false,
                yggdrasil_grant: Some(YggdrasilGrant {
                    access_token: "fixture-yggdrasil-token".into(),
                    display_name: "MuaPlayer".into(),
                    account_uuid: "00112233445566778899aabbccddeeff".into(),
                    profile_id: "00112233445566778899aabbccddeeff".into(),
                }),
            }),
        }
    }

    pub fn with_challenge(self, challenge: LoginChallenge) -> Self {
        self.inner.lock().expect("lock").challenge = challenge;
        self
    }

    pub fn with_polls(self, polls: Vec<PollOutcome>) -> Self {
        self.inner.lock().expect("lock").polls = polls;
        self
    }

    pub fn failing_begin(self) -> Self {
        self.inner.lock().expect("lock").begin_fails = true;
        self
    }

    pub fn with_yggdrasil_grant(self, grant: Option<YggdrasilGrant>) -> Self {
        self.inner.lock().expect("lock").yggdrasil_grant = grant;
        self
    }

    pub fn set_now(&self, now: u64) {
        self.inner.lock().expect("lock").now = now;
    }

    /// What was handed to the in-process consumer, for assertions.
    pub fn handed_token(&self, handle_id: &str) -> Option<String> {
        self.inner
            .lock()
            .expect("lock")
            .handed
            .get(handle_id)
            .cloned()
    }
}

impl Default for FixtureAccounts {
    fn default() -> Self {
        Self::new()
    }
}

impl AccountsPorts for FixtureAccounts {
    fn supports_device_login(&self, provider_id: &str) -> bool {
        provider_id == "microsoft"
    }

    fn begin_login(&self, _provider_id: &str, _now: u64) -> Result<LoginChallenge, String> {
        let inner = self.inner.lock().expect("lock");
        if inner.begin_fails {
            return Err("provider:unavailable".into());
        }
        Ok(inner.challenge.clone())
    }

    fn poll_login(
        &self,
        _provider_id: &str,
        _identity_id: &str,
        _user_code: &str,
        _now: u64,
    ) -> Result<PollOutcome, String> {
        let mut inner = self.inner.lock().expect("lock");
        if inner.polls.len() > 1 {
            Ok(inner.polls.remove(0))
        } else {
            Ok(inner.polls.first().cloned().unwrap_or(PollOutcome::Pending))
        }
    }

    fn authenticate_yggdrasil(
        &self,
        _account: &str,
        _password: &str,
    ) -> Result<YggdrasilGrant, YggdrasilAuthError> {
        self.inner
            .lock()
            .expect("lock")
            .yggdrasil_grant
            .clone()
            .ok_or(YggdrasilAuthError::Rejected)
    }

    fn offer_session_token(&self, handle_id: &str, token: &str) -> Result<(), String> {
        self.inner
            .lock()
            .expect("lock")
            .handed
            .insert(handle_id.to_string(), token.to_string());
        Ok(())
    }

    fn now_ms(&self) -> u64 {
        self.inner.lock().expect("lock").now
    }
}
