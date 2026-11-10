//! Module-local error vocabulary for 04 (content, loaders, modpacks).
//!
//! Every variant maps to a stable code that matches the public error-code format
//! `^[a-z][a-z0-9._-]{2,63}$` and to one of the frozen `ErrorCategory` values.
//! The codes are registered in `docs/development/handoffs/04/HANDOFF.md`; the
//! authoritative registry stays in `crates/domain` (owned by 00).

use std::fmt;

use shtmcl_domain::{
    ErrorCategory, ErrorCode, ErrorEnvelope, USER_ACTION_CHOOSE_INSTANCE, USER_ACTION_OPEN_DETAILS,
    USER_ACTION_RETRY,
};

/// Stable codes produced by this module.
///
/// Every code here has at least one production path (listing in
/// `HANDOFF-fixes.md` §3); a code with no producer must not be declared.
pub mod codes {
    pub const PACK_INVALID_MANIFEST: &str = "content.pack.invalid_manifest";
    pub const PACK_PATH_ESCAPE: &str = "content.pack.path_escape";
    pub const PACK_DUPLICATE_PATH: &str = "content.pack.duplicate_path";
    pub const PACK_BUDGET_EXCEEDED: &str = "content.pack.budget_exceeded";
    pub const PACK_UNSUPPORTED_ARCHIVE: &str = "content.pack.unsupported_archive";
    pub const PACK_CHECKSUM_MISMATCH: &str = "content.pack.checksum_mismatch";
    pub const PACK_RESTRICTED_FILE: &str = "content.pack.restricted_file";
    pub const PACK_UNSUPPORTED_FORMAT: &str = "content.pack.unsupported_format";
    pub const LOADER_UNSUPPORTED: &str = "content.loader.unsupported";
    pub const LOADER_INCOMPATIBLE: &str = "content.loader.incompatible";
    pub const LOADER_METADATA_INVALID: &str = "content.loader.metadata_invalid";
    /// `content.solve.task` fails with this when the search is proven unsatisfiable.
    pub const SOLVE_UNSATISFIABLE: &str = "content.solve.unsatisfiable";
    /// `content.solve.task` fails with this when a budget stops the search, and a
    /// `content.solve` read reports it when the request exceeds the read cap.
    pub const SOLVE_BUDGET_EXHAUSTED: &str = "content.solve.budget_exhausted";
    pub const PLAN_CONFLICT: &str = "content.plan.conflict";
    /// A user-owned (`managed:false`) file occupies a path the lock wants.
    pub const PLAN_UNMANAGED_CONFLICT: &str = "content.plan.unmanaged_conflict";
    pub const PLAN_INVALID: &str = "content.plan.invalid";
    pub const INSTANCE_REJECTED: &str = "content.instance.rejected";
    pub const INSTANCE_BUSY: &str = "content.instance.busy";
    pub const SOURCE_UNAVAILABLE: &str = "content.source.unavailable";
    pub const RECOVERY_MISSING: &str = "content.recovery.missing";
    pub const RECOVERY_CORRUPT: &str = "content.recovery.corrupt";
    /// A port whose real owner (01, 03, platform adapter) is not wired yet.
    pub const OWNER_UNAVAILABLE: &str = "content.owner.unavailable";
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContentError {
    /// A manifest field is missing, empty or of the wrong JSON type.
    InvalidManifest {
        detail: String,
    },
    /// A path inside a pack or lock leaves the target root (`..`, absolute, drive).
    PathEscape {
        path: String,
    },
    /// Two entries normalise to the same path with different identities.
    DuplicatePath {
        path: String,
    },
    /// A documented parse budget (size, entry count, depth) was exceeded.
    BudgetExceeded {
        budget: &'static str,
        limit: u64,
        observed: u64,
    },
    /// The archive uses a feature this build cannot read (zip64, encryption, deflate).
    UnsupportedArchive {
        detail: String,
    },
    ChecksumMismatch {
        path: String,
    },
    /// The file has no download URL and needs a user-supplied copy.
    RestrictedFile {
        path: String,
    },
    UnsupportedFormat {
        format: String,
    },
    UnsupportedLoader {
        loader: String,
    },
    IncompatibleLoader {
        detail: String,
    },
    InvalidLoaderMetadata {
        detail: String,
    },
    InvalidVersion {
        value: String,
    },
    InvalidVersionRange {
        value: String,
    },
    Unsatisfiable {
        detail: String,
    },
    SolveBudgetExhausted {
        states: u64,
        limit: &'static str,
    },
    PlanConflict {
        expected: u64,
        actual: u64,
    },
    /// A user-owned file sits at a path the lock wants; the plan refuses to replace it.
    UnmanagedConflict {
        path: String,
    },
    InvalidPlan {
        detail: String,
    },
    InstanceRejected {
        detail: String,
    },
    InstanceBusy {
        instance: String,
    },
    SourceUnavailable {
        provider: String,
    },
    RecoveryMissing {
        instance: String,
    },
    RecoveryCorrupt {
        detail: String,
    },
    /// The owner of a required port (instance transaction, download engine,
    /// platform adapter) is not integrated in this build. Fail closed: never
    /// fabricate a successful write or a fabricated loader library list.
    OwnerUnavailable {
        owner: &'static str,
    },
    Io {
        detail: String,
    },
    Internal {
        detail: String,
    },
}

impl ContentError {
    pub fn code(&self) -> &'static str {
        use codes::*;
        match self {
            Self::InvalidManifest { .. } => PACK_INVALID_MANIFEST,
            Self::PathEscape { .. } => PACK_PATH_ESCAPE,
            Self::DuplicatePath { .. } => PACK_DUPLICATE_PATH,
            Self::BudgetExceeded { .. } => PACK_BUDGET_EXCEEDED,
            Self::UnsupportedArchive { .. } => PACK_UNSUPPORTED_ARCHIVE,
            Self::ChecksumMismatch { .. } => PACK_CHECKSUM_MISMATCH,
            Self::RestrictedFile { .. } => PACK_RESTRICTED_FILE,
            Self::UnsupportedFormat { .. } => PACK_UNSUPPORTED_FORMAT,
            Self::UnsupportedLoader { .. } => LOADER_UNSUPPORTED,
            Self::IncompatibleLoader { .. } => LOADER_INCOMPATIBLE,
            Self::InvalidLoaderMetadata { .. } => LOADER_METADATA_INVALID,
            Self::InvalidVersion { .. } | Self::InvalidVersionRange { .. } => PLAN_INVALID,
            Self::Unsatisfiable { .. } => SOLVE_UNSATISFIABLE,
            Self::SolveBudgetExhausted { .. } => SOLVE_BUDGET_EXHAUSTED,
            Self::PlanConflict { .. } => PLAN_CONFLICT,
            Self::UnmanagedConflict { .. } => PLAN_UNMANAGED_CONFLICT,
            Self::InvalidPlan { .. } => PLAN_INVALID,
            Self::InstanceRejected { .. } => INSTANCE_REJECTED,
            Self::InstanceBusy { .. } => INSTANCE_BUSY,
            Self::SourceUnavailable { .. } => SOURCE_UNAVAILABLE,
            Self::RecoveryMissing { .. } => RECOVERY_MISSING,
            Self::RecoveryCorrupt { .. } => RECOVERY_CORRUPT,
            Self::OwnerUnavailable { .. } => OWNER_UNAVAILABLE,
            Self::Io { .. } | Self::Internal { .. } => {
                shtmcl_domain::ErrorCode::INTERNAL_UNAVAILABLE
            }
        }
    }

    pub fn category(&self) -> ErrorCategory {
        match self {
            Self::InvalidManifest { .. }
            | Self::PathEscape { .. }
            | Self::DuplicatePath { .. }
            | Self::BudgetExceeded { .. }
            | Self::ChecksumMismatch { .. }
            | Self::InvalidLoaderMetadata { .. }
            | Self::InvalidVersion { .. }
            | Self::InvalidVersionRange { .. }
            | Self::InvalidPlan { .. }
            | Self::RecoveryCorrupt { .. } => ErrorCategory::InvalidRequest,
            Self::UnsupportedArchive { .. }
            | Self::UnsupportedFormat { .. }
            | Self::UnsupportedLoader { .. }
            | Self::IncompatibleLoader { .. } => ErrorCategory::Unsupported,
            Self::OwnerUnavailable { .. } => ErrorCategory::Unsupported,
            Self::RestrictedFile { .. } => ErrorCategory::Conflict,
            Self::Unsatisfiable { .. } | Self::PlanConflict { .. } => ErrorCategory::Conflict,
            Self::UnmanagedConflict { .. } => ErrorCategory::Conflict,
            Self::SolveBudgetExhausted { .. } => ErrorCategory::Unsupported,
            Self::InstanceRejected { .. } | Self::Io { .. } => ErrorCategory::Storage,
            Self::InstanceBusy { .. } => ErrorCategory::Conflict,
            Self::SourceUnavailable { .. } => ErrorCategory::Network,
            Self::RecoveryMissing { .. } => ErrorCategory::NotFound,
            Self::Internal { .. } => ErrorCategory::Internal,
        }
    }

    pub fn retryable(&self) -> bool {
        matches!(self, Self::SourceUnavailable { .. } | Self::Io { .. })
    }

    pub fn user_action(&self) -> Option<&'static str> {
        match self {
            Self::RestrictedFile { .. } => Some(USER_ACTION_OPEN_DETAILS),
            Self::SourceUnavailable { .. } | Self::Io { .. } => Some(USER_ACTION_RETRY),
            Self::SolveBudgetExhausted { .. } | Self::Unsatisfiable { .. } => {
                Some(USER_ACTION_OPEN_DETAILS)
            }
            Self::RecoveryMissing { .. } => Some(USER_ACTION_CHOOSE_INSTANCE),
            _ => None,
        }
    }

    pub fn detail_ref(&self) -> Option<String> {
        match self {
            Self::PathEscape { path } => Some(format!("content:path:{path}")),
            Self::DuplicatePath { path } => Some(format!("content:path:{path}")),
            Self::ChecksumMismatch { path } => Some(format!("content:path:{path}")),
            Self::RestrictedFile { path } => Some(format!("content:path:{path}")),
            Self::PlanConflict { expected, actual } => {
                Some(format!("content:revision:{expected}->{actual}"))
            }
            Self::RecoveryMissing { instance } => Some(format!("content:recovery:{instance}")),
            Self::UnmanagedConflict { path } => Some(format!("content:path:{path}")),
            Self::UnsupportedFormat { format } => Some(format!("content:format:{format}")),
            Self::UnsupportedLoader { loader } => Some(format!("content:loader:{loader}")),
            Self::SourceUnavailable { provider } => Some(format!("content:source:{provider}")),
            Self::InstanceBusy { instance } => Some(format!("content:instance:{instance}")),
            Self::OwnerUnavailable { owner } => Some(format!("content:owner:{owner}")),
            _ => None,
        }
    }

    /// Convert to the frozen error envelope. No stack trace, no credentials.
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

impl fmt::Display for ContentError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidManifest { detail } => write!(f, "invalid pack manifest: {detail}"),
            Self::PathEscape { path } => write!(f, "path escapes the target root: {path}"),
            Self::DuplicatePath { path } => write!(f, "duplicate content path: {path}"),
            Self::BudgetExceeded {
                budget,
                limit,
                observed,
            } => write!(
                f,
                "{budget} budget exceeded: limit {limit}, observed {observed}"
            ),
            Self::UnsupportedArchive { detail } => write!(f, "unsupported archive: {detail}"),
            Self::ChecksumMismatch { path } => write!(f, "checksum mismatch for {path}"),
            Self::RestrictedFile { path } => {
                write!(f, "restricted file needs a local copy: {path}")
            }
            Self::UnsupportedFormat { format } => write!(f, "unsupported format: {format}"),
            Self::UnsupportedLoader { loader } => write!(f, "unsupported loader: {loader}"),
            Self::IncompatibleLoader { detail } => write!(f, "loader incompatibility: {detail}"),
            Self::InvalidLoaderMetadata { detail } => {
                write!(f, "invalid loader metadata: {detail}")
            }
            Self::InvalidVersion { value } => write!(f, "invalid version: {value}"),
            Self::InvalidVersionRange { value } => write!(f, "invalid version range: {value}"),
            Self::Unsatisfiable { detail } => write!(f, "no complete solution: {detail}"),
            Self::SolveBudgetExhausted { states, .. } => {
                write!(f, "solver budget exhausted after {states} states")
            }
            Self::PlanConflict { expected, actual } => {
                write!(
                    f,
                    "instance revision changed: expected {expected}, actual {actual}"
                )
            }
            Self::InvalidPlan { detail } => write!(f, "invalid plan: {detail}"),
            Self::UnmanagedConflict { path } => {
                write!(f, "a user-owned file is in the way: {path}")
            }
            Self::InstanceRejected { detail } => write!(f, "instance rejected the plan: {detail}"),
            Self::InstanceBusy { instance } => write!(f, "instance is running: {instance}"),
            Self::SourceUnavailable { provider } => write!(f, "source unavailable: {provider}"),
            Self::RecoveryMissing { instance } => {
                write!(f, "no content recovery record for {instance}")
            }
            Self::RecoveryCorrupt { detail } => write!(f, "corrupt recovery record: {detail}"),
            Self::OwnerUnavailable { owner } => {
                write!(f, "the {owner} owner is not integrated in this build")
            }
            Self::Io { detail } => write!(f, "i/o failure: {detail}"),
            Self::Internal { detail } => write!(f, "internal failure: {detail}"),
        }
    }
}

impl std::error::Error for ContentError {}

impl From<std::io::Error> for ContentError {
    fn from(error: std::io::Error) -> Self {
        Self::Io {
            detail: error.kind().to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn all_codes() -> Vec<&'static str> {
        use codes::*;
        vec![
            PACK_INVALID_MANIFEST,
            PACK_PATH_ESCAPE,
            PACK_DUPLICATE_PATH,
            PACK_BUDGET_EXCEEDED,
            PACK_UNSUPPORTED_ARCHIVE,
            PACK_CHECKSUM_MISMATCH,
            PACK_RESTRICTED_FILE,
            PACK_UNSUPPORTED_FORMAT,
            LOADER_UNSUPPORTED,
            LOADER_INCOMPATIBLE,
            LOADER_METADATA_INVALID,
            SOLVE_UNSATISFIABLE,
            SOLVE_BUDGET_EXHAUSTED,
            PLAN_CONFLICT,
            PLAN_UNMANAGED_CONFLICT,
            PLAN_INVALID,
            INSTANCE_REJECTED,
            INSTANCE_BUSY,
            SOURCE_UNAVAILABLE,
            RECOVERY_MISSING,
            RECOVERY_CORRUPT,
            OWNER_UNAVAILABLE,
        ]
    }

    #[test]
    fn every_module_code_matches_the_public_format() {
        for code in all_codes() {
            assert!(
                ErrorCode::new(code).is_ok(),
                "code {code} does not match the public error-code format"
            );
            assert!(
                code.starts_with("content."),
                "code {code} must be namespaced"
            );
        }
    }

    #[test]
    fn envelopes_carry_no_credentials() {
        let error = ContentError::SourceUnavailable {
            provider: "modrinth".to_string(),
        };
        let envelope = error.to_envelope();
        let json = serde_json::to_string(&envelope).expect("serialize");
        for forbidden in ["token", "secret", "cookie", "password"] {
            assert!(!json.contains(forbidden), "envelope leaked {forbidden}");
        }
        assert_eq!(envelope.category, ErrorCategory::Network);
        assert!(envelope.retryable);
    }
}
