//! Module-local error vocabulary for 07 (server platform).
//!
//! Codes live in the `servers` namespace (`contracts/domain-v1.md` §7).
//!
//! A server entry is user-facing and shareable, so envelopes here must never
//! carry a join token, an admin password or a raw NBT blob; the server is
//! referred to by `server:<id>`.

use std::fmt;

use shtmcl_domain::{
    ErrorCategory, ErrorCode, ErrorEnvelope, USER_ACTION_OPEN_DETAILS, USER_ACTION_REAUTHENTICATE,
    USER_ACTION_RELOAD, USER_ACTION_RETRY,
};

/// Stable codes produced by this module.
pub mod codes {
    pub const COMMAND_UNKNOWN: &str = "servers.command.unknown";
    pub const REQUEST_INVALID: &str = "servers.request.invalid";
    pub const SERVER_NOT_FOUND: &str = "servers.entry.not_found";
    pub const SERVER_EXISTS: &str = "servers.entry.exists";
    pub const SERVER_INVALID_ADDRESS: &str = "servers.entry.invalid_address";
    pub const SERVER_DUPLICATE_ADDRESS: &str = "servers.entry.duplicate_address";
    pub const PUBLISH_UNSUPPORTED: &str = "servers.publish.unsupported";
    pub const PUBLISH_NOT_FOUND: &str = "servers.publish.not_found";
    pub const PUBLISH_DENIED: &str = "servers.publish.denied";
    pub const DIRECTORY_UNAVAILABLE: &str = "servers.directory.unavailable";
    pub const JOIN_UNREACHABLE: &str = "servers.join.unreachable";
    pub const JOIN_VERSION_MISMATCH: &str = "servers.join.version_mismatch";
    pub const JOIN_AUTH_REQUIRED: &str = "servers.join.auth_required";
    pub const JOIN_INSTANCE_MISSING: &str = "servers.join.instance_missing";
    pub const INSTANCE_LOCKED: &str = "servers.instance.locked";
    pub const NBT_INVALID: &str = "servers.nbt.invalid";
    pub const NBT_UNKNOWN_FIELD_DROPPED: &str = "servers.nbt.unknown_field";
    pub const WRITE_FAILED: &str = "servers.write.failed";
    pub const INTERNAL: &str = "servers.internal";
}

/// Reasons 07 can fail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServersError {
    UnknownCommand,
    InvalidRequest(&'static str),
    NotFound(String),
    Exists(String),
    InvalidAddress(String),
    DuplicateAddress(String),
    PublishUnsupported(String),
    PublishNotFound(String),
    PublishDenied(String),
    DirectoryUnavailable(String),
    JoinUnreachable(String),
    JoinVersionMismatch {
        server: String,
        expected: String,
        actual: String,
    },
    JoinAuthRequired,
    JoinInstanceMissing(String),
    InstanceLocked(String),
    NbtInvalid(String),
    NbtUnknownField(String),
    WriteFailed(String),
    Internal,
}

impl ServersError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::UnknownCommand => codes::COMMAND_UNKNOWN,
            Self::InvalidRequest(_) => codes::REQUEST_INVALID,
            Self::NotFound(_) => codes::SERVER_NOT_FOUND,
            Self::Exists(_) => codes::SERVER_EXISTS,
            Self::InvalidAddress(_) => codes::SERVER_INVALID_ADDRESS,
            Self::DuplicateAddress(_) => codes::SERVER_DUPLICATE_ADDRESS,
            Self::PublishUnsupported(_) => codes::PUBLISH_UNSUPPORTED,
            Self::PublishNotFound(_) => codes::PUBLISH_NOT_FOUND,
            Self::PublishDenied(_) => codes::PUBLISH_DENIED,
            Self::DirectoryUnavailable(_) => codes::DIRECTORY_UNAVAILABLE,
            Self::JoinUnreachable(_) => codes::JOIN_UNREACHABLE,
            Self::JoinVersionMismatch { .. } => codes::JOIN_VERSION_MISMATCH,
            Self::JoinAuthRequired => codes::JOIN_AUTH_REQUIRED,
            Self::JoinInstanceMissing(_) => codes::JOIN_INSTANCE_MISSING,
            Self::InstanceLocked(_) => codes::INSTANCE_LOCKED,
            Self::NbtInvalid(_) => codes::NBT_INVALID,
            Self::NbtUnknownField(_) => codes::NBT_UNKNOWN_FIELD_DROPPED,
            Self::WriteFailed(_) => codes::WRITE_FAILED,
            Self::Internal => codes::INTERNAL,
        }
    }

    fn category(&self) -> ErrorCategory {
        match self {
            Self::UnknownCommand
            | Self::InvalidRequest(_)
            | Self::InvalidAddress(_)
            | Self::NbtInvalid(_)
            | Self::NbtUnknownField(_) => ErrorCategory::InvalidRequest,
            Self::Exists(_)
            | Self::DuplicateAddress(_)
            | Self::JoinVersionMismatch { .. }
            | Self::InstanceLocked(_) => ErrorCategory::Conflict,
            Self::NotFound(_) | Self::PublishNotFound(_) | Self::JoinInstanceMissing(_) => {
                ErrorCategory::NotFound
            }
            Self::PublishDenied(_) | Self::JoinAuthRequired => ErrorCategory::PermissionDenied,
            Self::PublishUnsupported(_) => ErrorCategory::Unsupported,
            Self::DirectoryUnavailable(_) | Self::JoinUnreachable(_) => ErrorCategory::Network,
            Self::WriteFailed(_) => ErrorCategory::Storage,
            Self::Internal => ErrorCategory::Internal,
        }
    }

    fn retryable(&self) -> bool {
        matches!(
            self,
            Self::DirectoryUnavailable(_)
                | Self::JoinUnreachable(_)
                | Self::InstanceLocked(_)
                | Self::WriteFailed(_)
        )
    }

    fn user_action(&self) -> Option<&'static str> {
        match self {
            Self::DirectoryUnavailable(_) | Self::JoinUnreachable(_) => Some(USER_ACTION_RETRY),
            Self::JoinAuthRequired => Some(USER_ACTION_REAUTHENTICATE),
            Self::JoinVersionMismatch { .. } | Self::InstanceLocked(_) => Some(USER_ACTION_RELOAD),
            Self::PublishDenied(_) => Some(USER_ACTION_OPEN_DETAILS),
            _ => None,
        }
    }

    /// Opaque references only: no address, no token, no NBT payload.
    fn detail(&self) -> Option<String> {
        match self {
            Self::NotFound(id) | Self::Exists(id) | Self::PublishNotFound(id) => {
                Some(format!("server:{id}"))
            }
            Self::InvalidAddress(reference) | Self::DuplicateAddress(reference) => {
                Some(format!("address:{reference}"))
            }
            Self::PublishUnsupported(reference) | Self::PublishDenied(reference) => {
                Some(format!("server:{reference}"))
            }
            Self::DirectoryUnavailable(reference)
            | Self::JoinUnreachable(reference)
            | Self::WriteFailed(reference) => Some(reference.clone()),
            Self::JoinVersionMismatch {
                server,
                expected,
                actual,
            } => Some(format!("server:{server}:{expected}/{actual}")),
            Self::JoinInstanceMissing(id) | Self::InstanceLocked(id) => {
                Some(format!("instance:{id}"))
            }
            Self::NbtInvalid(reference) | Self::NbtUnknownField(reference) => {
                Some(reference.clone())
            }
            Self::InvalidRequest(field) => Some(format!("field:{field}")),
            Self::JoinAuthRequired | Self::UnknownCommand | Self::Internal => None,
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

impl fmt::Display for ServersError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.code())
    }
}

impl std::error::Error for ServersError {}

#[cfg(test)]
mod tests {
    use super::*;

    fn every_variant() -> Vec<ServersError> {
        vec![
            ServersError::UnknownCommand,
            ServersError::InvalidRequest("f"),
            ServersError::NotFound("s".into()),
            ServersError::Exists("s".into()),
            ServersError::InvalidAddress("a".into()),
            ServersError::DuplicateAddress("a".into()),
            ServersError::PublishUnsupported("s".into()),
            ServersError::PublishNotFound("s".into()),
            ServersError::PublishDenied("s".into()),
            ServersError::DirectoryUnavailable("d".into()),
            ServersError::JoinUnreachable("j".into()),
            ServersError::JoinVersionMismatch {
                server: "s".into(),
                expected: "1.21.4".into(),
                actual: "1.20.1".into(),
            },
            ServersError::JoinAuthRequired,
            ServersError::JoinInstanceMissing("i".into()),
            ServersError::InstanceLocked("i".into()),
            ServersError::NbtInvalid("n".into()),
            ServersError::NbtUnknownField("n".into()),
            ServersError::WriteFailed("w".into()),
            ServersError::Internal,
        ]
    }

    #[test]
    fn every_code_is_in_this_module_namespace() {
        for error in every_variant() {
            assert_eq!(
                error.code().split('.').next(),
                Some("servers"),
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
    fn no_envelope_carries_a_raw_address_with_port() {
        for error in every_variant() {
            if let Some(detail) = error.detail() {
                assert!(
                    !detail.contains('@'),
                    "credential shape in {}",
                    error.code()
                );
                assert!(!detail.contains("Bearer"));
            }
        }
    }

    #[test]
    fn an_unreachable_server_is_retryable_but_a_version_mismatch_is_not() {
        assert!(
            ServersError::JoinUnreachable("x".into())
                .to_envelope()
                .retryable
        );
        assert!(
            !ServersError::JoinVersionMismatch {
                server: "s".into(),
                expected: "1.21".into(),
                actual: "1.20".into()
            }
            .to_envelope()
            .retryable
        );
    }

    #[test]
    fn missing_authorization_offers_reauthentication() {
        let envelope = ServersError::JoinAuthRequired.to_envelope();
        assert_eq!(
            envelope.user_action.as_ref().map(|a| a.as_str()),
            Some(USER_ACTION_REAUTHENTICATE)
        );
    }
}
