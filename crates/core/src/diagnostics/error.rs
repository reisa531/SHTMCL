//! Stable error codes for module 08.
//!
//! Namespace: `diagnostics.*` (first segment is the module id, see
//! `contracts/domain-v1.md` §7). `node scripts/check-codes.mjs` owns the
//! mechanical check. Every code here is reachable from a command path in this
//! build; the two that only trigger on an OS failure are marked below.

use shtmcl_domain::{ErrorCategory, ErrorEnvelope, UserAction};

pub mod codes {
    pub const COMMAND_UNKNOWN: &str = "diagnostics.command.unknown";
    pub const REQUEST_INVALID: &str = "diagnostics.request.invalid";
    pub const OWNER_UNAVAILABLE: &str = "diagnostics.owner.unavailable";

    pub const LOGS_NOT_FOUND: &str = "diagnostics.logs.not_found";
    pub const LOGS_STORE_FAILED: &str = "diagnostics.logs.store_failed";

    pub const ANALYSIS_NOT_FOUND: &str = "diagnostics.analysis.not_found";
    pub const REPAIR_UNSUPPORTED: &str = "diagnostics.repair.unsupported";
    pub const REPAIR_CONFLICT: &str = "diagnostics.repair.conflict";
    pub const REPAIR_BUSY: &str = "diagnostics.repair.busy";

    pub const EXPORT_TOO_LARGE: &str = "diagnostics.export.too_large";
    pub const EXPORT_DESTINATION_INVALID: &str = "diagnostics.export.destination_invalid";
    pub const EXPORT_WRITE_FAILED: &str = "diagnostics.export.write_failed";

    pub const UPDATE_UNAVAILABLE: &str = "diagnostics.update.unavailable";
    pub const UPDATE_VERSION_INVALID: &str = "diagnostics.update.version_invalid";
    pub const UPDATE_PLATFORM_MISMATCH: &str = "diagnostics.update.platform_mismatch";
    pub const UPDATE_CHANNEL_MISMATCH: &str = "diagnostics.update.channel_mismatch";
    pub const UPDATE_REQUIRES_NEWER_HOST: &str = "diagnostics.update.requires_newer_host";
    pub const UPDATE_ARTIFACT_MISMATCH: &str = "diagnostics.update.artifact_mismatch";
    pub const UPDATE_SIGNATURE_UNVERIFIED: &str = "diagnostics.update.signature_unverified";
    pub const UPDATE_TRANSACTION_NOT_FOUND: &str = "diagnostics.update.transaction_not_found";
    pub const UPDATE_STAGE_INVALID: &str = "diagnostics.update.stage_invalid";
    pub const UPDATE_ACTIVE_WORK_UNKNOWN: &str = "diagnostics.update.active_work_unknown";
    pub const UPDATE_STARTUP_UNCONFIRMED: &str = "diagnostics.update.startup_unconfirmed";
    pub const UPDATE_RECOVERY_REQUIRED: &str = "diagnostics.update.recovery_required";
    pub const UPDATE_STORE_FAILED: &str = "diagnostics.update.store_failed";

    pub const INTERNAL: &str = "diagnostics.internal";
}

/// Module-local failure. Converted to an [`ErrorEnvelope`] at the command edge;
/// the envelope never carries raw exception text or a credential.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiagnosticsError {
    UnknownCommand {
        command: String,
    },
    InvalidRequest {
        detail: String,
    },
    OwnerUnavailable {
        owner: &'static str,
    },
    LogsNotFound {
        session: String,
    },
    LogsStoreFailed {
        detail: String,
    },
    UpdateStoreFailed {
        detail: String,
    },
    AnalysisNotFound {
        report: String,
    },
    RepairUnsupported {
        rule: String,
        detail: String,
    },
    /// The instance moved on since the analysis was produced.
    RepairConflict {
        expected: u64,
        actual: u64,
    },
    /// The game (or another owner) still holds the instance.
    RepairBusy {
        instance: String,
    },
    ExportTooLarge {
        bytes: u64,
        budget: u64,
    },
    ExportDestinationInvalid {
        detail: String,
    },
    ExportWriteFailed {
        detail: String,
    },
    UpdateUnavailable {
        detail: String,
    },
    VersionInvalid {
        value: String,
    },
    PlatformMismatch {
        candidate: String,
        host: String,
    },
    ChannelMismatch {
        candidate: String,
        requested: String,
    },
    RequiresNewerHost {
        required: String,
        host: String,
    },
    ArtifactMismatch {
        expected: String,
        actual: String,
    },
    SignatureUnverified {
        detail: String,
    },
    TransactionNotFound {
        transaction: String,
    },
    StageInvalid {
        stage: String,
        action: String,
    },
    ActiveWorkUnknown,
    StartupUnconfirmed {
        detail: String,
    },
    RecoveryRequired {
        detail: String,
    },
    Internal {
        detail: String,
    },
}

impl DiagnosticsError {
    pub fn invalid(detail: impl Into<String>) -> Self {
        Self::InvalidRequest {
            detail: detail.into(),
        }
    }

    pub fn owner(owner: &'static str) -> Self {
        Self::OwnerUnavailable { owner }
    }

    fn envelope(
        code: &'static str,
        category: ErrorCategory,
        retryable: bool,
        user_action: Option<&str>,
        detail_ref: Option<String>,
    ) -> ErrorEnvelope {
        let mut envelope = ErrorEnvelope::new(
            shtmcl_domain::ErrorCode::from_static(code),
            category,
            retryable,
        );
        if let Some(action) = user_action {
            envelope = envelope.with_user_action(UserAction::new(action));
        }
        if let Some(reference) = detail_ref {
            envelope = envelope.with_detail_ref(reference);
        }
        envelope
    }

    pub fn to_envelope(&self) -> ErrorEnvelope {
        use codes::*;
        match self {
            Self::UnknownCommand { command } => Self::envelope(
                COMMAND_UNKNOWN,
                ErrorCategory::InvalidRequest,
                false,
                Some("open_details"),
                Some(format!("command:{command}")),
            ),
            Self::InvalidRequest { detail } => Self::envelope(
                REQUEST_INVALID,
                ErrorCategory::InvalidRequest,
                false,
                None,
                Some(format!("request:{detail}")),
            ),
            Self::OwnerUnavailable { owner } => Self::envelope(
                OWNER_UNAVAILABLE,
                ErrorCategory::Unsupported,
                false,
                Some("open_details"),
                Some(format!("owner:{owner}")),
            ),
            Self::LogsNotFound { session } => Self::envelope(
                LOGS_NOT_FOUND,
                ErrorCategory::NotFound,
                false,
                Some("reload"),
                Some(format!("log-session:{session}")),
            ),
            // Only reachable when the writer thread cannot drain the queue at
            // all (a stuck disk); the in-memory ring is still readable.
            Self::LogsStoreFailed { detail } => Self::envelope(
                LOGS_STORE_FAILED,
                ErrorCategory::Storage,
                true,
                Some("retry"),
                Some(format!("log-store:{detail}")),
            ),
            Self::AnalysisNotFound { report } => Self::envelope(
                ANALYSIS_NOT_FOUND,
                ErrorCategory::NotFound,
                false,
                Some("reload"),
                Some(format!("report:{report}")),
            ),
            Self::RepairUnsupported { rule, detail } => Self::envelope(
                REPAIR_UNSUPPORTED,
                ErrorCategory::Unsupported,
                false,
                Some("open_details"),
                Some(format!("rule:{rule}:{detail}")),
            ),
            Self::RepairConflict { expected, actual } => Self::envelope(
                REPAIR_CONFLICT,
                ErrorCategory::Conflict,
                true,
                Some("reload"),
                Some(format!("instance-revision:{expected}!={actual}")),
            ),
            Self::RepairBusy { instance } => Self::envelope(
                REPAIR_BUSY,
                ErrorCategory::Conflict,
                true,
                Some("retry"),
                Some(format!("instance-busy:{instance}")),
            ),
            Self::ExportTooLarge { bytes, budget } => Self::envelope(
                EXPORT_TOO_LARGE,
                ErrorCategory::InvalidRequest,
                false,
                Some("open_details"),
                Some(format!("export:{bytes}>{budget}")),
            ),
            Self::ExportDestinationInvalid { detail } => Self::envelope(
                EXPORT_DESTINATION_INVALID,
                ErrorCategory::InvalidRequest,
                false,
                Some("open_details"),
                Some(format!("destination:{detail}")),
            ),
            // Only reachable on an OS write/rename failure.
            Self::ExportWriteFailed { detail } => Self::envelope(
                EXPORT_WRITE_FAILED,
                ErrorCategory::Storage,
                true,
                Some("retry"),
                Some(format!("export-write:{detail}")),
            ),
            Self::UpdateUnavailable { detail } => Self::envelope(
                UPDATE_UNAVAILABLE,
                ErrorCategory::Unsupported,
                true,
                Some("retry"),
                Some(format!("release-source:{detail}")),
            ),
            Self::VersionInvalid { value } => Self::envelope(
                UPDATE_VERSION_INVALID,
                ErrorCategory::InvalidRequest,
                false,
                Some("open_details"),
                Some(format!("version:{value}")),
            ),
            Self::PlatformMismatch { candidate, host } => Self::envelope(
                UPDATE_PLATFORM_MISMATCH,
                ErrorCategory::Unsupported,
                false,
                Some("open_details"),
                Some(format!("platform:{candidate}!={host}")),
            ),
            Self::ChannelMismatch {
                candidate,
                requested,
            } => Self::envelope(
                UPDATE_CHANNEL_MISMATCH,
                ErrorCategory::Conflict,
                false,
                Some("reload"),
                Some(format!("channel:{candidate}!={requested}")),
            ),
            Self::RequiresNewerHost { required, host } => Self::envelope(
                UPDATE_REQUIRES_NEWER_HOST,
                ErrorCategory::Unsupported,
                false,
                Some("open_details"),
                Some(format!("min-host:{required}>{host}")),
            ),
            Self::ArtifactMismatch { expected, actual } => Self::envelope(
                UPDATE_ARTIFACT_MISMATCH,
                ErrorCategory::Storage,
                true,
                Some("retry"),
                Some(format!("digest:{expected}!={actual}")),
            ),
            Self::SignatureUnverified { detail } => Self::envelope(
                UPDATE_SIGNATURE_UNVERIFIED,
                ErrorCategory::PermissionDenied,
                false,
                Some("open_details"),
                Some(format!("signature:{detail}")),
            ),
            Self::TransactionNotFound { transaction } => Self::envelope(
                UPDATE_TRANSACTION_NOT_FOUND,
                ErrorCategory::NotFound,
                false,
                Some("reload"),
                Some(format!("update-transaction:{transaction}")),
            ),
            Self::StageInvalid { stage, action } => Self::envelope(
                UPDATE_STAGE_INVALID,
                ErrorCategory::Conflict,
                false,
                Some("reload"),
                Some(format!("stage:{stage}:{action}")),
            ),
            Self::ActiveWorkUnknown => Self::envelope(
                UPDATE_ACTIVE_WORK_UNKNOWN,
                ErrorCategory::Unsupported,
                true,
                Some("retry"),
                Some("active-work:unknown".to_string()),
            ),
            Self::StartupUnconfirmed { detail } => Self::envelope(
                UPDATE_STARTUP_UNCONFIRMED,
                ErrorCategory::Unsupported,
                true,
                Some("retry"),
                Some(format!("startup:{detail}")),
            ),
            Self::RecoveryRequired { detail } => Self::envelope(
                UPDATE_RECOVERY_REQUIRED,
                ErrorCategory::Conflict,
                false,
                Some("open_details"),
                Some(format!("recovery:{detail}")),
            ),
            // Only reachable when the transaction record cannot be written or read.
            Self::UpdateStoreFailed { detail } => Self::envelope(
                UPDATE_STORE_FAILED,
                ErrorCategory::Storage,
                true,
                Some("retry"),
                Some(format!("update-store:{detail}")),
            ),
            Self::Internal { detail } => Self::envelope(
                INTERNAL,
                ErrorCategory::Internal,
                false,
                None,
                Some(format!("internal:{detail}")),
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_code_stays_inside_the_module_namespace() {
        for code in [
            codes::COMMAND_UNKNOWN,
            codes::REQUEST_INVALID,
            codes::OWNER_UNAVAILABLE,
            codes::LOGS_NOT_FOUND,
            codes::LOGS_STORE_FAILED,
            codes::ANALYSIS_NOT_FOUND,
            codes::REPAIR_UNSUPPORTED,
            codes::REPAIR_CONFLICT,
            codes::REPAIR_BUSY,
            codes::EXPORT_TOO_LARGE,
            codes::EXPORT_DESTINATION_INVALID,
            codes::EXPORT_WRITE_FAILED,
            codes::UPDATE_UNAVAILABLE,
            codes::UPDATE_VERSION_INVALID,
            codes::UPDATE_PLATFORM_MISMATCH,
            codes::UPDATE_CHANNEL_MISMATCH,
            codes::UPDATE_REQUIRES_NEWER_HOST,
            codes::UPDATE_ARTIFACT_MISMATCH,
            codes::UPDATE_SIGNATURE_UNVERIFIED,
            codes::UPDATE_TRANSACTION_NOT_FOUND,
            codes::UPDATE_STAGE_INVALID,
            codes::UPDATE_ACTIVE_WORK_UNKNOWN,
            codes::UPDATE_STARTUP_UNCONFIRMED,
            codes::UPDATE_RECOVERY_REQUIRED,
            codes::UPDATE_STORE_FAILED,
            codes::INTERNAL,
        ] {
            assert!(
                code.starts_with("diagnostics."),
                "module 08 owns diagnostics.*: {code}"
            );
            assert!(
                shtmcl_domain::ErrorCode::new(code).is_ok(),
                "code must satisfy the shared format: {code}"
            );
        }
    }

    #[test]
    fn an_unsupported_repair_keeps_its_rule_in_the_detail_ref() {
        let envelope = DiagnosticsError::RepairUnsupported {
            rule: "diagnostics.rule.file_lock".to_string(),
            detail: "needs the game process to exit".to_string(),
        }
        .to_envelope();
        assert_eq!(envelope.code.as_str(), codes::REPAIR_UNSUPPORTED);
        assert!(envelope
            .detail_ref
            .as_deref()
            .unwrap_or_default()
            .contains("diagnostics.rule.file_lock"));
        assert_eq!(envelope.category, ErrorCategory::Unsupported);
    }
}
