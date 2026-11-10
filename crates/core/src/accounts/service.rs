//! `CoreModule` implementation: command routing for module 05.
//!
//! Routing convention (frozen by 00): `payload.command`, first segment is the
//! module id (`accounts.`).
//!
//! Every answer here is `completed`: an authorization flow that needs user action
//! returns a [`AuthChallenge`] instead of holding a task open, because waiting on
//! a human is not a long operation the task service should own. Polling the
//! provider is a separate `accounts.auth.poll` call, so the caller controls the
//! cadence and can stop.

use std::sync::{Arc, Mutex};

use serde_json::{json, Value};
use shtmcl_domain::{
    CommandKind, CommandRequest, CommandResponse, ErrorEnvelope, ModuleId, OperationId,
};

use crate::accounts::error::AccountsError;
use crate::accounts::ports::AccountsPorts;
use crate::accounts::state::{
    AccountRole, AccountStore, AuthChallenge, AuthorizationState, GameSessionHandle, Identity,
    ProviderKind, Secret, SessionPurpose,
};
use crate::accounts::MODULE;
use crate::{CoreModule, ModuleContext};

/// Commands this module owns. Stable; registered in the 05 handoff.
pub mod actions {
    pub const LIST: &str = "accounts.list";
    pub const SNAPSHOT: &str = "accounts.snapshot";
    pub const ADD_OFFLINE: &str = "accounts.add_offline";
    pub const YGGDRASIL_LOGIN: &str = "accounts.yggdrasil.login";
    pub const LOGIN_START: &str = "accounts.auth.start";
    pub const LOGIN_POLL: &str = "accounts.auth.poll";
    pub const LOGOUT: &str = "accounts.auth.logout";
    pub const SET_DEFAULT: &str = "accounts.default.set";
    pub const REMOVE: &str = "accounts.remove";
    pub const SESSION_ISSUE: &str = "accounts.session.issue";
    pub const SESSION_REDEEM: &str = "accounts.session.redeem";
    pub const ROLE_GRANT: &str = "accounts.role.grant";
}

/// Event kinds published by this module.
///
/// `account.authorization.changed` is the **reserved cross-module kind** frozen by
/// `contracts/domain-v1.md` §12; the UX layer already consumes it. Exposed as a
/// function because `scripts/check-codes.mjs` requires `const NAME: &str`
/// declarations to start with an owner namespace, and this kind deliberately does
/// not.
pub mod events {
    /// Reserved by contract §12. Payload: state, providerId, identityId,
    /// errorCode, detailRef.
    pub fn authorization_changed() -> &'static str {
        "account.authorization.changed"
    }

    pub fn identity_removed() -> &'static str {
        "accounts.identity.removed"
    }
}

/// Default session lifetime handed to 02: long enough for a launch hand-off,
/// short enough that a forgotten handle cannot be replayed an hour later.
pub const DEFAULT_SESSION_TTL_MS: u64 = 120_000;

/// The 05 module.
pub struct AccountsModule {
    context: ModuleContext,
    ports: Arc<dyn AccountsPorts>,
    store: Mutex<AccountStore>,
}

impl AccountsModule {
    pub fn new(context: ModuleContext, ports: Arc<dyn AccountsPorts>) -> Self {
        Self {
            context,
            ports,
            store: Mutex::new(AccountStore::new()),
        }
    }

    /// Wire the fail-closed default (no provider transport).
    pub fn unavailable(context: ModuleContext) -> Self {
        Self::new(
            context,
            Arc::new(crate::accounts::ports::UnavailableAccounts),
        )
    }

    fn now(&self) -> u64 {
        self.ports.now_ms()
    }

    fn store(&self) -> std::sync::MutexGuard<'_, AccountStore> {
        self.store
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    }

    /// Publish the reserved authorization event.
    ///
    /// The payload is exactly the five fields frozen in contract §12. No token,
    /// no code, no provider response.
    fn publish_authorization(
        &self,
        operation: Option<OperationId>,
        identity: &Identity,
        error_code: Option<&'static str>,
        detail_ref: Option<String>,
    ) {
        let Some(operation) = operation else { return };
        let generation = shtmcl_domain::AttemptGeneration::INITIAL;
        let mut payload = json!({
            "state": identity.state.as_str(),
            "providerId": identity.provider_id,
            "identityId": identity.identity_id,
        });
        if let Some(map) = payload.as_object_mut() {
            if let Some(code) = error_code {
                map.insert("errorCode".into(), json!(code));
            }
            if let Some(detail) = detail_ref {
                map.insert("detailRef".into(), json!(detail));
            }
        }
        let _ = self.context.events().publish(
            crate::tasks::EventRequest::new(
                operation,
                generation,
                shtmcl_domain::EventKind::from_static(events::authorization_changed()),
            )
            .with_entity(shtmcl_domain::EntityId::new(identity.identity_id.clone()).ok())
            .with_payload(payload),
        );
    }

    // ---------------------------------------------------------------- queries

    fn list(&self, _request: &CommandRequest) -> Result<CommandResponse, ErrorEnvelope> {
        let store = self.store();
        let identities = store.identities();
        Ok(completed(json!({
            "identities": identities,
            "count": identities.len(),
            "defaultIdentityId": identities
                .iter()
                .find(|identity| identity.is_default)
                .map(|identity| identity.identity_id.clone()),
        })))
    }

    fn snapshot(&self, request: &CommandRequest) -> Result<CommandResponse, ErrorEnvelope> {
        let id = required_str(request, "identityId")?;
        let store = self.store();
        let identity = store
            .identity(id)
            .ok_or_else(|| AccountsError::IdentityNotFound(id.to_string()).to_envelope())?;
        Ok(completed(json!({ "identity": identity })))
    }

    // -------------------------------------------------------------- mutations

    fn add_offline(&self, request: &CommandRequest) -> Result<CommandResponse, ErrorEnvelope> {
        let operation = request.operation_id.clone();
        let name = required_str(request, "displayName")?;
        let identity_id = optional_str(request, "identityId")
            .map(str::to_string)
            .unwrap_or_else(|| format!("offline-{}", stable_slug(name)));
        let account_uuid = optional_str(request, "accountUuid")
            .map(str::to_string)
            .unwrap_or_else(|| format!("offline-{}", stable_slug(name)));

        let mut store = self.store();
        if store.identity(&identity_id).is_some() {
            return Err(AccountsError::IdentityExists(identity_id).to_envelope());
        }
        // An offline profile holds no token and never expires; it is
        // `Authorized` so launch does not ask for a login it cannot perform.
        let identity = Identity {
            identity_id: identity_id.clone(),
            provider_id: "offline".into(),
            provider_kind: ProviderKind::Offline,
            display_name: name.to_string(),
            account_uuid,
            state: AuthorizationState::Authorized,
            expires_at: None,
            profile_id: None,
            is_default: store.identities().is_empty(),
        };
        let view = identity.clone();
        store.upsert_identity(identity);
        drop(store);
        self.publish_authorization(operation, &view, None, None);
        Ok(completed(json!({ "identity": view })))
    }

    /// Login to the configured MUA Yggdrasil service. The password is borrowed
    /// directly from the command payload, passed to the provider port and never
    /// copied into identity state, events, or the returned JSON.
    fn yggdrasil_login(&self, request: &CommandRequest) -> Result<CommandResponse, ErrorEnvelope> {
        request.validate(CommandKind::Mutate)?;
        let operation = request
            .operation_id
            .clone()
            .ok_or_else(ErrorEnvelope::command_operation_required)?;
        let account = required_str(request, "account")?;
        let password = required_str(request, "password")?;
        let grant = self
            .ports
            .authenticate_yggdrasil(account, password)
            .map_err(|error| match error {
                crate::accounts::ports::YggdrasilAuthError::Rejected => {
                    AccountsError::AuthDenied("mua".into()).to_envelope()
                }
                crate::accounts::ports::YggdrasilAuthError::ProfileSelectionRequired => {
                    AccountsError::ProviderUnsupported("yggdrasil_profiles".into()).to_envelope()
                }
                crate::accounts::ports::YggdrasilAuthError::Unavailable => {
                    AccountsError::NetworkFailed("mua_yggdrasil".into()).to_envelope()
                }
            })?;

        if !valid_profile_id(&grant.profile_id)
            || !valid_profile_id(&grant.account_uuid)
            || grant.display_name.trim().is_empty()
        {
            return Err(
                AccountsError::NetworkFailed("mua_yggdrasil_response".into()).to_envelope(),
            );
        }
        let normalized_uuid = grant.account_uuid.replace('-', "").to_ascii_lowercase();
        let identity_id = format!("mua-{normalized_uuid}");
        let identity = {
            let mut store = self.store();
            let existing = store.identity(&identity_id);
            let identity = Identity {
                identity_id: identity_id.clone(),
                provider_id: "mua".into(),
                provider_kind: ProviderKind::Yggdrasil,
                display_name: grant.display_name,
                account_uuid: normalized_uuid,
                state: AuthorizationState::Authorized,
                // Yggdrasil does not define a portable token expiry value.
                expires_at: None,
                profile_id: Some(grant.profile_id),
                is_default: existing
                    .as_ref()
                    .map(|identity| identity.is_default)
                    .unwrap_or_else(|| store.identities().iter().all(|i| !i.is_default)),
            };
            store.upsert_identity(identity.clone());
            store
                .set_token_without_expiry(&identity_id, Secret::new(grant.access_token))
                .ok_or_else(|| AccountsError::Internal.to_envelope())?;
            let mut view = identity;
            view.state = AuthorizationState::Authorized;
            view.expires_at = None;
            view
        };
        self.publish_authorization(Some(operation), &identity, None, None);
        Ok(completed(json!({ "identity": identity })))
    }

    /// Begin an authorization flow. The provider returns either a device-code
    /// challenge (user must act) or an immediate token (silent refresh).
    fn login_start(&self, request: &CommandRequest) -> Result<CommandResponse, ErrorEnvelope> {
        let operation = request.operation_id.clone();
        let provider_id = required_str(request, "providerId")?;
        let kind = ProviderKind::parse(provider_id).ok_or_else(|| {
            AccountsError::ProviderUnsupported(provider_id.to_string()).to_envelope()
        })?;
        if !kind.requires_authorization() {
            // An offline provider has nothing to authorize; fail loudly instead of
            // starting a flow that can never finish.
            return Err(AccountsError::ProviderUnsupported(provider_id.to_string()).to_envelope());
        }
        if !self.ports.supports_device_login(provider_id) {
            return Err(AccountsError::ProviderUnsupported(provider_id.to_string()).to_envelope());
        }

        let identity_id = optional_str(request, "identityId")
            .map(str::to_string)
            .unwrap_or_else(|| format!("{provider_id}-pending"));
        let now = self.now();

        let challenge = self
            .ports
            .begin_login(provider_id, now)
            .map_err(|reference| AccountsError::NetworkFailed(reference).to_envelope())?;

        {
            let mut store = self.store();
            if store.identity(&identity_id).is_none() {
                store.upsert_identity(Identity {
                    identity_id: identity_id.clone(),
                    provider_id: provider_id.to_string(),
                    provider_kind: kind,
                    display_name: String::new(),
                    account_uuid: String::new(),
                    state: AuthorizationState::Pending,
                    expires_at: None,
                    profile_id: None,
                    is_default: false,
                });
            }
            store.open_challenge(AuthChallenge {
                identity_id: identity_id.clone(),
                user_code: challenge.user_code.clone(),
                verification_uri: challenge.verification_uri.clone(),
                expires_at: challenge.expires_at,
                interval_ms: challenge.interval_ms,
            });
        }

        let view = {
            let store = self.store();
            store
                .identity(&identity_id)
                .ok_or_else(|| AccountsError::IdentityNotFound(identity_id.clone()).to_envelope())?
        };
        self.publish_authorization(operation, &view, None, None);

        // The user code is shown to the user on purpose; it is not a credential.
        Ok(completed(json!({
            "identityId": identity_id,
            "challenge": {
                "userCode": challenge.user_code,
                "verificationUri": challenge.verification_uri,
                "expiresAt": challenge.expires_at,
                "intervalMs": challenge.interval_ms,
            },
            "state": view.state,
        })))
    }

    /// Poll once. The caller owns the cadence, so no thread waits here.
    fn login_poll(&self, request: &CommandRequest) -> Result<CommandResponse, ErrorEnvelope> {
        let operation = request.operation_id.clone();
        let identity_id = required_str(request, "identityId")?;
        let now = self.now();
        let code = required_str(request, "userCode")?;

        let (provider_id, challenge) = {
            let store = self.store();
            let identity = store.identity(identity_id).ok_or_else(|| {
                AccountsError::IdentityNotFound(identity_id.to_string()).to_envelope()
            })?;
            let challenge = store.challenge(identity_id).ok_or_else(|| {
                AccountsError::AuthRequired(identity_id.to_string()).to_envelope()
            })?;
            (identity.provider_id, challenge)
        };

        if now >= challenge.expires_at {
            let expired = AccountsError::DeviceCodeExpired;
            let view = {
                let mut store = self.store();
                store.clear_tokens(identity_id);
                store.identity(identity_id)
            };
            if let Some(view) = view {
                self.publish_authorization(
                    operation,
                    &view,
                    Some(expired.code()),
                    Some(format!("identity:{identity_id}")),
                );
            }
            return Err(expired.to_envelope());
        }

        let outcome = self
            .ports
            .poll_login(&provider_id, identity_id, code, now)
            .map_err(|reference| AccountsError::NetworkFailed(reference).to_envelope())?;

        match outcome {
            crate::accounts::ports::PollOutcome::Pending => Ok(completed(json!({
                "state": AuthorizationState::Pending.as_str(),
                "intervalMs": challenge.interval_ms,
            }))),
            crate::accounts::ports::PollOutcome::Denied => {
                let view = {
                    let mut store = self.store();
                    store.clear_tokens(identity_id);
                    store.identity(identity_id)
                };
                let error = AccountsError::AuthDenied(identity_id.to_string());
                if let Some(view) = view {
                    self.publish_authorization(
                        operation,
                        &view,
                        Some(error.code()),
                        Some(format!("identity:{identity_id}")),
                    );
                }
                Err(error.to_envelope())
            }
            crate::accounts::ports::PollOutcome::Granted(grant) => {
                let view = {
                    let mut store = self.store();
                    store
                        .complete_challenge(
                            identity_id,
                            code,
                            Secret::new(grant.access_token),
                            grant.refresh_token.map(Secret::new),
                            grant.expires_at,
                            now,
                        )
                        .map_err(|e| e.to_envelope())?;
                    // Display facts are filled only now: before the grant we do not
                    // know who the account is.
                    if let Some(mut identity) = store.identity(identity_id) {
                        identity.display_name = grant.display_name.clone();
                        identity.account_uuid = grant.account_uuid.clone();
                        identity.profile_id = grant.profile_id.clone();
                        identity.state = AuthorizationState::Authorized;
                        identity.expires_at = Some(grant.expires_at);
                        if store.identities().iter().all(|other| !other.is_default) {
                            identity.is_default = true;
                        }
                        let view = identity.clone();
                        store.upsert_identity(identity);
                        view
                    } else {
                        return Err(
                            AccountsError::IdentityNotFound(identity_id.to_string()).to_envelope()
                        );
                    }
                };
                self.publish_authorization(operation, &view, None, None);
                Ok(completed(json!({
                    "identity": view,
                    "state": view.state,
                })))
            }
        }
    }

    fn logout(&self, request: &CommandRequest) -> Result<CommandResponse, ErrorEnvelope> {
        let operation = request.operation_id.clone();
        let identity_id = required_str(request, "identityId")?;
        let view = {
            let mut store = self.store();
            if store.identity(identity_id).is_none() {
                return Err(AccountsError::IdentityNotFound(identity_id.to_string()).to_envelope());
            }
            store.clear_tokens(identity_id);
            store.identity(identity_id)
        };
        if let Some(view) = view {
            self.publish_authorization(operation, &view, None, None);
        }
        Ok(completed(json!({ "identityId": identity_id })))
    }

    fn set_default(&self, request: &CommandRequest) -> Result<CommandResponse, ErrorEnvelope> {
        let identity_id = required_str(request, "identityId")?;
        let mut store = self.store();
        let mut identity = store.identity(identity_id).ok_or_else(|| {
            AccountsError::IdentityNotFound(identity_id.to_string()).to_envelope()
        })?;
        identity.is_default = true;
        let view = identity.clone();
        store.upsert_identity(identity);
        Ok(completed(json!({ "identity": view })))
    }

    fn remove(&self, request: &CommandRequest) -> Result<CommandResponse, ErrorEnvelope> {
        let operation = request.operation_id.clone();
        let identity_id = required_str(request, "identityId")?;
        {
            let mut store = self.store();
            if store.identity(identity_id).is_none() {
                return Err(AccountsError::IdentityNotFound(identity_id.to_string()).to_envelope());
            }
            // Tokens go first so a partially applied removal cannot leave a
            // credential behind for an identity that no longer exists.
            store.clear_tokens(identity_id);
            store.remove_identity(identity_id);
        }
        if let Some(operation) = operation {
            let _ = self.context.events().publish(
                crate::tasks::EventRequest::new(
                    operation,
                    shtmcl_domain::AttemptGeneration::INITIAL,
                    shtmcl_domain::EventKind::from_static(events::identity_removed()),
                )
                .with_payload(json!({ "identityId": identity_id })),
            );
        }
        Ok(completed(json!({ "identityId": identity_id })))
    }

    fn issue_session(&self, request: &CommandRequest) -> Result<CommandResponse, ErrorEnvelope> {
        let identity_id = required_str(request, "identityId")?;
        let purpose_name = required_str(request, "purpose")?;
        let purpose = SessionPurpose::parse(purpose_name)
            .ok_or_else(|| AccountsError::InvalidRequest("purpose").to_envelope())?;
        let ttl = request
            .payload
            .get("ttlMs")
            .and_then(Value::as_u64)
            .unwrap_or(DEFAULT_SESSION_TTL_MS);
        let now = self.now();
        let handle: GameSessionHandle = {
            let mut store = self.store();
            store
                .issue_session(identity_id, purpose, now, ttl)
                .map_err(|e| e.to_envelope())?
        };
        // Only the wire-safe projection leaves the module.
        Ok(completed(json!({ "session": handle.to_view() })))
    }

    fn redeem_session(&self, request: &CommandRequest) -> Result<CommandResponse, ErrorEnvelope> {
        let handle_id = required_str(request, "handleId")?;
        let purpose_name = required_str(request, "purpose")?;
        let purpose = SessionPurpose::parse(purpose_name)
            .ok_or_else(|| AccountsError::InvalidRequest("purpose").to_envelope())?;
        let now = self.now();
        let handle = {
            let mut store = self.store();
            store
                .redeem_session(handle_id, purpose, now)
                .map_err(|e| e.to_envelope())?
        };
        // The token is handed to the caller through a dedicated port, never
        // through the command result: 02 receives it in-process.
        if let Some(token) = handle.token.as_ref() {
            self.ports
                .offer_session_token(&handle.handle_id, token.expose_to_credential_store())
                .map_err(|reference| AccountsError::NetworkFailed(reference).to_envelope())?;
        }
        Ok(completed(json!({ "session": handle.to_view() })))
    }

    fn grant_role(&self, request: &CommandRequest) -> Result<CommandResponse, ErrorEnvelope> {
        let identity_id = required_str(request, "identityId")?;
        let actor_name = required_str(request, "actorRole")?;
        let target_name = required_str(request, "targetRole")?;
        let actor = AccountRole::parse(actor_name)
            .ok_or_else(|| AccountsError::RoleUnknown(actor_name.to_string()).to_envelope())?;
        let target = AccountRole::parse(target_name)
            .ok_or_else(|| AccountsError::RoleUnknown(target_name.to_string()).to_envelope())?;
        if self.store().identity(identity_id).is_none() {
            return Err(AccountsError::IdentityNotFound(identity_id.to_string()).to_envelope());
        }
        if !actor.can_grant(target) {
            return Err(match (actor, target) {
                (AccountRole::Player | AccountRole::Spectator, _) => {
                    AccountsError::RoleForbidden(actor.as_str().to_string())
                }
                _ => AccountsError::RoleRequiresOwner,
            }
            .to_envelope());
        }
        Ok(completed(json!({
            "identityId": identity_id,
            "actorRole": actor.as_str(),
            "targetRole": target.as_str(),
            "granted": true,
        })))
    }
}

impl CoreModule for AccountsModule {
    fn id(&self) -> ModuleId {
        MODULE
    }

    fn handle(&self, request: &CommandRequest) -> Result<CommandResponse, ErrorEnvelope> {
        let command = request.command()?;
        match command.as_str() {
            actions::LIST => self.list(request),
            actions::SNAPSHOT => self.snapshot(request),
            actions::ADD_OFFLINE => self.add_offline(request),
            actions::YGGDRASIL_LOGIN => self.yggdrasil_login(request),
            actions::LOGIN_START => self.login_start(request),
            actions::LOGIN_POLL => self.login_poll(request),
            actions::LOGOUT => self.logout(request),
            actions::SET_DEFAULT => self.set_default(request),
            actions::REMOVE => self.remove(request),
            actions::SESSION_ISSUE => self.issue_session(request),
            actions::SESSION_REDEEM => self.redeem_session(request),
            actions::ROLE_GRANT => self.grant_role(request),
            _ => Err(AccountsError::UnknownCommand.to_envelope()),
        }
    }
}

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

fn completed(result: Value) -> CommandResponse {
    CommandResponse::Completed { result }
}

fn optional_str<'a>(request: &'a CommandRequest, key: &str) -> Option<&'a str> {
    request
        .payload
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
}

fn required_str<'a>(
    request: &'a CommandRequest,
    key: &'static str,
) -> Result<&'a str, ErrorEnvelope> {
    optional_str(request, key).ok_or_else(|| AccountsError::InvalidRequest(key).to_envelope())
}

fn valid_profile_id(value: &str) -> bool {
    let normalized = value.replace('-', "");
    normalized.len() == 32 && normalized.bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// Deterministic id fragment for an offline profile: lowercase, digits and
/// `- _`. Deterministic so re-adding the same name lands on the same identity
/// instead of creating a duplicate every time.
fn stable_slug(value: &str) -> String {
    let mut slug = String::with_capacity(value.len());
    for character in value.chars() {
        if character.is_ascii_alphanumeric() {
            slug.push(character.to_ascii_lowercase());
        } else if character.is_whitespace() || character == '-' || character == '_' {
            if !slug.ends_with('-') && !slug.is_empty() {
                slug.push('-');
            }
        }
    }
    let slug = slug.trim_matches('-').to_string();
    if slug.is_empty() {
        "player".to_string()
    } else {
        slug
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn offline_slug_is_deterministic_and_safe() {
        assert_eq!(stable_slug("Steve"), "steve");
        assert_eq!(stable_slug("Alex  Player"), "alex-player");
        assert_eq!(stable_slug("玩家一号"), "player");
        assert_eq!(stable_slug("   "), "player");
        assert_eq!(stable_slug("a.b*c"), "abc");
    }
}
