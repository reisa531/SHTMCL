//! Module-local error vocabulary for 01 (instance core).
//!
//! Every variant maps to a stable code whose first segment is the module
//! namespace `instances`, so `scripts/check-codes.mjs` can verify ownership
//! without 00 registering anything centrally (see `contracts/domain-v1.md` §7).
//!
//! Only codes this build can actually produce are declared: a code for a future
//! adapter is added together with that adapter, so the vocabulary never
//! advertises behaviour that does not exist.
//!
//! No envelope carries a filesystem path, a credential or raw exception text;
//! `detail_ref` refers to a log record the diagnostics layer can read.

use std::fmt;

use shtmcl_domain::{
    ErrorCategory, ErrorCode, ErrorEnvelope, USER_ACTION_CHOOSE_INSTANCE, USER_ACTION_OPEN_DETAILS,
    USER_ACTION_RELOAD, USER_ACTION_RETRY,
};

/// Stable codes produced by this module.
pub mod codes {
    pub const COMMAND_UNKNOWN: &str = "instances.command.unknown";
    pub const REQUEST_INVALID: &str = "instances.request.invalid";
    pub const REQUEST_IDEMPOTENCY_CONFLICT: &str = "instances.request.idempotency_conflict";
    pub const INSTANCE_NOT_FOUND: &str = "instances.instance.not_found";
    pub const INSTANCE_DUPLICATE: &str = "instances.instance.duplicate";
    pub const INSTANCE_LOCKED: &str = "instances.instance.locked";
    pub const INSTANCE_TERMINAL: &str = "instances.instance.terminal";
    pub const INSTANCE_RUNNING: &str = "instances.instance.running";
    pub const REVISION_STALE: &str = "instances.revision.stale";
    pub const REVISION_CONFLICT: &str = "instances.revision.conflict";
    pub const LOCATION_UNAVAILABLE: &str = "instances.location.unavailable";
    pub const LOCATION_NOT_ABSOLUTE: &str = "instances.location.not_absolute";
    pub const LOCATION_NOT_WRITABLE: &str = "instances.location.not_writable";
    pub const INDEX_CORRUPT: &str = "instances.index.corrupt";
    pub const INDEX_IO: &str = "instances.index.io";
    pub const IMPORT_SOURCE_MISSING: &str = "instances.import.source_missing";
    pub const IMPORT_SOURCE_EMPTY: &str = "instances.import.source_empty";
    pub const LEASE_EXPIRED: &str = "instances.lease.expired";
    pub const LEASE_UNAUTHORIZED: &str = "instances.lease.unauthorized";
    pub const RECOVERY_UNKNOWN_OPERATION: &str = "instances.recovery.unknown_operation";
    pub const STATE_INVALID_TRANSITION: &str = "instances.state.invalid_transition";
    pub const INTERNAL: &str = "instances.internal";
    pub const MOD_UNAVAILABLE: &str = "instances.mod.unavailable";
    pub const MOD_CHANGED: &str = "instances.mod.changed";
    pub const MOD_TARGET_EXISTS: &str = "instances.mod.target_exists";
}

/// Reasons 01 can fail, each with one stable code and category.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstancesError {
    UnknownCommand,
    InvalidRequest(&'static str),
    IdempotencyConflict,
    NotFound(String),
    Duplicate(String),
    Locked(String),
    Terminal(String),
    Running(String),
    RevisionStale { expected: u64, actual: u64 },
    RevisionConflict { expected: u64, actual: u64 },
    LocationUnavailable(String),
    LocationNotAbsolute(String),
    LocationNotWritable(String),
    IndexCorrupt(String),
    IndexIo(String),
    ImportSourceMissing(String),
    ImportSourceEmpty(String),
    LeaseExpired(String),
    LeaseUnauthorized(String),
    RecoveryUnknownOperation(String),
    InvalidTransition(&'static str),
    Internal,
    ModUnavailable,
    ModChanged,
    ModTargetExists,
}

impl InstancesError {
    /// The stable code for this variant.
    pub fn code(&self) -> &'static str {
        match self {
            Self::UnknownCommand => codes::COMMAND_UNKNOWN,
            Self::InvalidRequest(_) => codes::REQUEST_INVALID,
            Self::IdempotencyConflict => codes::REQUEST_IDEMPOTENCY_CONFLICT,
            Self::NotFound(_) => codes::INSTANCE_NOT_FOUND,
            Self::Duplicate(_) => codes::INSTANCE_DUPLICATE,
            Self::Locked(_) => codes::INSTANCE_LOCKED,
            Self::Terminal(_) => codes::INSTANCE_TERMINAL,
            Self::Running(_) => codes::INSTANCE_RUNNING,
            Self::RevisionStale { .. } => codes::REVISION_STALE,
            Self::RevisionConflict { .. } => codes::REVISION_CONFLICT,
            Self::LocationUnavailable(_) => codes::LOCATION_UNAVAILABLE,
            Self::LocationNotAbsolute(_) => codes::LOCATION_NOT_ABSOLUTE,
            Self::LocationNotWritable(_) => codes::LOCATION_NOT_WRITABLE,
            Self::IndexCorrupt(_) => codes::INDEX_CORRUPT,
            Self::IndexIo(_) => codes::INDEX_IO,
            Self::ImportSourceMissing(_) => codes::IMPORT_SOURCE_MISSING,
            Self::ImportSourceEmpty(_) => codes::IMPORT_SOURCE_EMPTY,
            Self::LeaseExpired(_) => codes::LEASE_EXPIRED,
            Self::LeaseUnauthorized(_) => codes::LEASE_UNAUTHORIZED,
            Self::RecoveryUnknownOperation(_) => codes::RECOVERY_UNKNOWN_OPERATION,
            Self::InvalidTransition(_) => codes::STATE_INVALID_TRANSITION,
            Self::Internal => codes::INTERNAL,
            Self::ModUnavailable => codes::MOD_UNAVAILABLE,
            Self::ModChanged => codes::MOD_CHANGED,
            Self::ModTargetExists => codes::MOD_TARGET_EXISTS,
        }
    }

    fn category(&self) -> ErrorCategory {
        match self {
            Self::UnknownCommand | Self::InvalidRequest(_) | Self::LocationNotAbsolute(_) => {
                ErrorCategory::InvalidRequest
            }
            Self::IdempotencyConflict
            | Self::Duplicate(_)
            | Self::Locked(_)
            | Self::Terminal(_)
            | Self::Running(_)
            | Self::RevisionStale { .. }
            | Self::RevisionConflict { .. }
            | Self::LeaseExpired(_)
            | Self::LeaseUnauthorized(_)
            | Self::InvalidTransition(_) => ErrorCategory::Conflict,
            Self::ModChanged | Self::ModTargetExists => ErrorCategory::Conflict,
            Self::ModUnavailable => ErrorCategory::Storage,
            Self::NotFound(_)
            | Self::ImportSourceMissing(_)
            | Self::ImportSourceEmpty(_)
            | Self::RecoveryUnknownOperation(_) => ErrorCategory::NotFound,
            Self::LocationUnavailable(_) | Self::LocationNotWritable(_) => ErrorCategory::Storage,
            Self::IndexCorrupt(_) | Self::IndexIo(_) | Self::Internal => ErrorCategory::Internal,
        }
    }

    /// Whether repeating the same request could plausibly succeed. A revision
    /// conflict is not retryable as-is: the caller must re-read first.
    fn retryable(&self) -> bool {
        matches!(
            self,
            Self::Locked(_)
                | Self::Running(_)
                | Self::IndexIo(_)
                | Self::LocationUnavailable(_)
                | Self::LocationNotWritable(_)
        )
    }

    fn user_action(&self) -> Option<&'static str> {
        match self {
            Self::RevisionStale { .. } | Self::RevisionConflict { .. } | Self::IndexCorrupt(_) => {
                Some(USER_ACTION_RELOAD)
            }
            Self::Locked(_) | Self::Running(_) | Self::IndexIo(_) => Some(USER_ACTION_RETRY),
            Self::NotFound(_) => Some(USER_ACTION_CHOOSE_INSTANCE),
            Self::LeaseExpired(_) | Self::LeaseUnauthorized(_) => Some(USER_ACTION_OPEN_DETAILS),
            _ => None,
        }
    }

    /// A stable pointer for the diagnostics layer. It never contains a raw path.
    fn detail(&self) -> Option<String> {
        match self {
            Self::NotFound(id) => Some(format!("instance:{id}")),
            Self::Duplicate(id) => Some(format!("instance:{id}")),
            Self::Locked(id) => Some(format!("lease:{id}")),
            Self::Terminal(id) => Some(format!("instance:{id}")),
            Self::Running(id) => Some(format!("instance:{id}")),
            Self::RevisionStale { expected, actual } => {
                Some(format!("revision:{expected}->{actual}"))
            }
            Self::RevisionConflict { expected, actual } => {
                Some(format!("revision:{expected}->{actual}"))
            }
            Self::IndexCorrupt(reference) => Some(reference.clone()),
            Self::IndexIo(reference) => Some(reference.clone()),
            Self::LeaseExpired(id) | Self::LeaseUnauthorized(id) => Some(format!("lease:{id}")),
            Self::RecoveryUnknownOperation(id) => Some(format!("operation:{id}")),
            Self::LocationUnavailable(reference)
            | Self::LocationNotAbsolute(reference)
            | Self::LocationNotWritable(reference)
            | Self::ImportSourceMissing(reference)
            | Self::ImportSourceEmpty(reference) => Some(reference.clone()),
            Self::InvalidRequest(field) => Some(format!("field:{field}")),
            Self::InvalidTransition(what) => Some(format!("transition:{what}")),
            Self::UnknownCommand
            | Self::IdempotencyConflict
            | Self::Internal
            | Self::ModUnavailable
            | Self::ModChanged
            | Self::ModTargetExists => None,
        }
    }

    /// Convert into the shared wire envelope.
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

impl fmt::Display for InstancesError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.code())
    }
}

impl std::error::Error for InstancesError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_code_is_in_this_module_namespace() {
        let all = [
            InstancesError::UnknownCommand,
            InstancesError::InvalidRequest("f"),
            InstancesError::IdempotencyConflict,
            InstancesError::NotFound("i".into()),
            InstancesError::Duplicate("i".into()),
            InstancesError::Locked("i".into()),
            InstancesError::Terminal("i".into()),
            InstancesError::Running("i".into()),
            InstancesError::RevisionStale {
                expected: 1,
                actual: 2,
            },
            InstancesError::RevisionConflict {
                expected: 1,
                actual: 2,
            },
            InstancesError::LocationUnavailable("l".into()),
            InstancesError::LocationNotAbsolute("l".into()),
            InstancesError::LocationNotWritable("l".into()),
            InstancesError::IndexCorrupt("r".into()),
            InstancesError::IndexIo("r".into()),
            InstancesError::ImportSourceMissing("p".into()),
            InstancesError::ImportSourceEmpty("p".into()),
            InstancesError::LeaseExpired("l".into()),
            InstancesError::LeaseUnauthorized("l".into()),
            InstancesError::RecoveryUnknownOperation("o".into()),
            InstancesError::InvalidTransition("t"),
            InstancesError::Internal,
            InstancesError::ModUnavailable,
            InstancesError::ModChanged,
            InstancesError::ModTargetExists,
        ];
        for error in all {
            assert_eq!(
                error.code().split('.').next(),
                Some("instances"),
                "code {} escaped the module namespace",
                error.code()
            );
        }
    }

    #[test]
    fn envelopes_never_leak_the_message_body() {
        let error = InstancesError::NotFound("instance-1".into());
        let envelope = error.to_envelope();
        let json = serde_json::to_string(&envelope).expect("serializable");
        assert!(json.contains("instances.instance.not_found"));
        // The detail is a reference, not a user-facing sentence.
        assert!(json.contains("instance:instance-1"));
    }

    #[test]
    fn revision_conflict_asks_for_a_reload_instead_of_a_retry() {
        let envelope = InstancesError::RevisionStale {
            expected: 3,
            actual: 7,
        }
        .to_envelope();
        assert!(!envelope.retryable);
        assert_eq!(
            envelope.user_action.as_ref().map(|a| a.as_str()),
            Some(USER_ACTION_RELOAD)
        );
    }
}
