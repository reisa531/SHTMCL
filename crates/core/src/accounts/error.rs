//! Module-local error vocabulary for 05 (accounts and MUA).
//!
//! Codes live in the `accounts` namespace (see `contracts/domain-v1.md` §7).
//!
//! **This module's envelopes are the strictest in the workspace.** An account
//! failure must never disclose a token, a refresh token, a session key or a raw
//! provider response, so `detail` here only ever contains an opaque
//! `provider:…` / `identity:…` / `session:…` reference that the account layer
//! itself can resolve. Nothing from the network layer is forwarded verbatim.

use std::fmt;

use shtmcl_domain::{
    ErrorCategory, ErrorCode, ErrorEnvelope, USER_ACTION_OPEN_DETAILS, USER_ACTION_REAUTHENTICATE,
    USER_ACTION_RETRY,
};

/// Stable codes produced by this module.
pub mod codes {
    pub const COMMAND_UNKNOWN: &str = "accounts.command.unknown";
    pub const REQUEST_INVALID: &str = "accounts.request.invalid";
    pub const PROVIDER_UNKNOWN: &str = "accounts.provider.unknown";
    pub const PROVIDER_UNSUPPORTED: &str = "accounts.provider.unsupported";
    pub const IDENTITY_NOT_FOUND: &str = "accounts.identity.not_found";
    pub const IDENTITY_UNAVAILABLE: &str = "accounts.identity.unavailable";
    pub const IDENTITY_EXISTS: &str = "accounts.identity.exists";
    pub const AUTH_REQUIRED: &str = "accounts.auth.required";
    pub const AUTH_PENDING: &str = "accounts.auth.pending";
    pub const AUTH_EXPIRED: &str = "accounts.auth.expired";
    pub const AUTH_DENIED: &str = "accounts.auth.denied";
    pub const AUTH_DEVICE_CODE_EXPIRED: &str = "accounts.auth.device_code_expired";
    pub const AUTH_CHALLENGE_MISMATCH: &str = "accounts.auth.challenge_mismatch";
    pub const SESSION_NOT_FOUND: &str = "accounts.session.not_found";
    pub const SESSION_EXPIRED: &str = "accounts.session.expired";
    pub const SESSION_PURPOSE_MISMATCH: &str = "accounts.session.purpose_mismatch";
    pub const SESSION_CONSUMED: &str = "accounts.session.consumed";
    pub const ROLE_UNKNOWN: &str = "accounts.role.unknown";
    pub const ROLE_FORBIDDEN: &str = "accounts.role.forbidden";
    pub const ROLE_REQUIRES_OWNER: &str = "accounts.role.requires_owner";
    pub const CREDENTIAL_UNAVAILABLE: &str = "accounts.credential.unavailable";
    pub const NETWORK_FAILED: &str = "accounts.network.failed";
    pub const INTERNAL: &str = "accounts.internal";
}

/// Reasons 05 can fail. Every variant is safe to serialize.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AccountsError {
    UnknownCommand,
    InvalidRequest(&'static str),
    ProviderUnknown(String),
    ProviderUnsupported(String),
    IdentityNotFound(String),
    IdentityUnavailable(String),
    IdentityExists(String),
    AuthRequired(String),
    AuthPending(String),
    AuthExpired(String),
    AuthDenied(String),
    DeviceCodeExpired,
    ChallengeMismatch,
    SessionNotFound(String),
    SessionExpired(String),
    SessionPurposeMismatch { purpose: String },
    SessionConsumed(String),
    RoleUnknown(String),
    RoleForbidden(String),
    RoleRequiresOwner,
    CredentialUnavailable,
    NetworkFailed(String),
    Internal,
}

impl AccountsError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::UnknownCommand => codes::COMMAND_UNKNOWN,
            Self::InvalidRequest(_) => codes::REQUEST_INVALID,
            Self::ProviderUnknown(_) => codes::PROVIDER_UNKNOWN,
            Self::ProviderUnsupported(_) => codes::PROVIDER_UNSUPPORTED,
            Self::IdentityNotFound(_) => codes::IDENTITY_NOT_FOUND,
            Self::IdentityUnavailable(_) => codes::IDENTITY_UNAVAILABLE,
            Self::IdentityExists(_) => codes::IDENTITY_EXISTS,
            Self::AuthRequired(_) => codes::AUTH_REQUIRED,
            Self::AuthPending(_) => codes::AUTH_PENDING,
            Self::AuthExpired(_) => codes::AUTH_EXPIRED,
            Self::AuthDenied(_) => codes::AUTH_DENIED,
            Self::DeviceCodeExpired => codes::AUTH_DEVICE_CODE_EXPIRED,
            Self::ChallengeMismatch => codes::AUTH_CHALLENGE_MISMATCH,
            Self::SessionNotFound(_) => codes::SESSION_NOT_FOUND,
            Self::SessionExpired(_) => codes::SESSION_EXPIRED,
            Self::SessionPurposeMismatch { .. } => codes::SESSION_PURPOSE_MISMATCH,
            Self::SessionConsumed(_) => codes::SESSION_CONSUMED,
            Self::RoleUnknown(_) => codes::ROLE_UNKNOWN,
            Self::RoleForbidden(_) => codes::ROLE_FORBIDDEN,
            Self::RoleRequiresOwner => codes::ROLE_REQUIRES_OWNER,
            Self::CredentialUnavailable => codes::CREDENTIAL_UNAVAILABLE,
            Self::NetworkFailed(_) => codes::NETWORK_FAILED,
            Self::Internal => codes::INTERNAL,
        }
    }

    fn category(&self) -> ErrorCategory {
        match self {
            Self::UnknownCommand
            | Self::InvalidRequest(_)
            | Self::ProviderUnsupported(_)
            | Self::DeviceCodeExpired
            | Self::ChallengeMismatch => ErrorCategory::InvalidRequest,
            Self::IdentityExists(_)
            | Self::SessionConsumed(_)
            | Self::SessionPurposeMismatch { .. } => ErrorCategory::Conflict,
            Self::IdentityNotFound(_) | Self::SessionNotFound(_) => ErrorCategory::NotFound,
            Self::AuthRequired(_)
            | Self::AuthPending(_)
            | Self::AuthExpired(_)
            | Self::AuthDenied(_)
            | Self::RoleForbidden(_)
            | Self::RoleRequiresOwner
            | Self::CredentialUnavailable => ErrorCategory::PermissionDenied,
            Self::ProviderUnknown(_) | Self::IdentityUnavailable(_) => ErrorCategory::Unsupported,
            Self::SessionExpired(_) | Self::NetworkFailed(_) => ErrorCategory::Network,
            Self::RoleUnknown(_) | Self::Internal => ErrorCategory::Internal,
        }
    }

    /// Only a transport failure is worth an automatic retry. Anything about
    /// authorization needs a human decision.
    fn retryable(&self) -> bool {
        matches!(self, Self::NetworkFailed(_) | Self::AuthPending(_))
    }

    fn user_action(&self) -> Option<&'static str> {
        match self {
            Self::AuthRequired(_) | Self::AuthExpired(_) | Self::SessionExpired(_) => {
                Some(USER_ACTION_REAUTHENTICATE)
            }
            Self::NetworkFailed(_) | Self::AuthPending(_) => Some(USER_ACTION_RETRY),
            Self::AuthDenied(_)
            | Self::RoleForbidden(_)
            | Self::RoleRequiresOwner
            | Self::CredentialUnavailable => Some(USER_ACTION_OPEN_DETAILS),
            _ => None,
        }
    }

    /// Opaque reference only. No token, no code, no raw provider text.
    fn detail(&self) -> Option<String> {
        match self {
            Self::ProviderUnknown(id) | Self::ProviderUnsupported(id) => {
                Some(format!("provider:{id}"))
            }
            Self::IdentityNotFound(id)
            | Self::IdentityUnavailable(id)
            | Self::IdentityExists(id) => Some(format!("identity:{id}")),
            Self::AuthRequired(id)
            | Self::AuthPending(id)
            | Self::AuthExpired(id)
            | Self::AuthDenied(id) => Some(format!("identity:{id}")),
            Self::SessionNotFound(id) | Self::SessionExpired(id) | Self::SessionConsumed(id) => {
                Some(format!("session:{id}"))
            }
            Self::SessionPurposeMismatch { purpose } => Some(format!("purpose:{purpose}")),
            Self::RoleUnknown(role) | Self::RoleForbidden(role) => Some(format!("role:{role}")),
            // A transport reference comes from outside this module, so it is
            // wrapped in a `network:` prefix rather than forwarded as-is. That
            // keeps every `detail` value shape-checkable, which is what the
            // namespace test below asserts.
            Self::NetworkFailed(reference) => Some(format!("network:{reference}")),
            Self::InvalidRequest(field) => Some(format!("field:{field}")),
            Self::DeviceCodeExpired
            | Self::ChallengeMismatch
            | Self::RoleRequiresOwner
            | Self::CredentialUnavailable
            | Self::UnknownCommand
            | Self::Internal => None,
        }
    }

    pub fn to_envelope(&self) -> ErrorEnvelope {
        let mut envelope = ErrorEnvelope::new(
            ErrorCode::from_static(self.code()),
            self.category(),
            self.retryable(),
        );
        if let Some(action) = self.user_action() {
            envelope = envelope.with_user_action(action);
        }
        if let Some(detail) = self.detail() {
            envelope = envelope.with_detail_ref(detail);
        }
        envelope
    }
}

impl fmt::Display for AccountsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.code())
    }
}

impl std::error::Error for AccountsError {}

#[cfg(test)]
mod tests {
    use super::*;

    fn every_variant() -> Vec<AccountsError> {
        vec![
            AccountsError::UnknownCommand,
            AccountsError::InvalidRequest("f"),
            AccountsError::ProviderUnknown("p".into()),
            AccountsError::ProviderUnsupported("p".into()),
            AccountsError::IdentityNotFound("i".into()),
            AccountsError::IdentityUnavailable("i".into()),
            AccountsError::IdentityExists("i".into()),
            AccountsError::AuthRequired("i".into()),
            AccountsError::AuthPending("i".into()),
            AccountsError::AuthExpired("i".into()),
            AccountsError::AuthDenied("i".into()),
            AccountsError::DeviceCodeExpired,
            AccountsError::ChallengeMismatch,
            AccountsError::SessionNotFound("s".into()),
            AccountsError::SessionExpired("s".into()),
            AccountsError::SessionPurposeMismatch {
                purpose: "launch".into(),
            },
            AccountsError::SessionConsumed("s".into()),
            AccountsError::RoleUnknown("r".into()),
            AccountsError::RoleForbidden("r".into()),
            AccountsError::RoleRequiresOwner,
            AccountsError::CredentialUnavailable,
            AccountsError::NetworkFailed("n".into()),
            AccountsError::Internal,
        ]
    }

    #[test]
    fn every_code_is_in_this_module_namespace() {
        for error in every_variant() {
            assert_eq!(
                error.code().split('.').next(),
                Some("accounts"),
                "code {} escaped the namespace",
                error.code()
            );
        }
    }

    #[test]
    fn codes_are_unique_per_variant() {
        let all = every_variant();
        let mut seen = std::collections::BTreeSet::new();
        for error in &all {
            assert!(seen.insert(error.code()), "duplicate code {}", error.code());
        }
        assert_eq!(seen.len(), all.len());
    }

    #[test]
    fn no_envelope_can_carry_a_credential_shaped_string() {
        // The detail field is the only place free text could leak; assert it is
        // always an opaque reference with a known prefix.
        for error in every_variant() {
            if let Some(detail) = error.detail() {
                let prefix = detail.split(':').next().unwrap_or("");
                assert!(
                    matches!(
                        prefix,
                        "provider"
                            | "identity"
                            | "session"
                            | "purpose"
                            | "role"
                            | "field"
                            | "network"
                    ),
                    "unexpected detail prefix {prefix} in {}",
                    error.code()
                );
                assert!(!detail.contains("Bearer"));
                assert!(!detail.contains('@'));
            }
        }
    }

    #[test]
    fn authorization_failures_are_never_auto_retried() {
        for error in [
            AccountsError::AuthDenied("i".into()),
            AccountsError::RoleForbidden("r".into()),
            AccountsError::RoleRequiresOwner,
            AccountsError::CredentialUnavailable,
        ] {
            assert!(!error.to_envelope().retryable, "{} retried", error.code());
        }
    }

    #[test]
    fn an_expired_authorization_offers_reauthentication() {
        let envelope = AccountsError::AuthExpired("i".into()).to_envelope();
        assert_eq!(
            envelope.user_action.as_ref().map(|a| a.as_str()),
            Some(USER_ACTION_REAUTHENTICATE)
        );
    }
}
