//! Module-local error vocabulary for 03 (downloads and cache).
//!
//! Codes live in the `downloads` namespace so `scripts/check-codes.mjs` can
//! verify ownership without central registration (`contracts/domain-v1.md` §7).
//!
//! No envelope carries a URL, a mirror host, an access token or raw exception
//! text. A download source is referred to by an opaque `sourceId`, and any
//! technical detail is a `detail_ref` the diagnostics layer resolves.

use std::fmt;

use shtmcl_domain::{
    ErrorCategory, ErrorCode, ErrorEnvelope, USER_ACTION_OPEN_DETAILS, USER_ACTION_RELOAD,
    USER_ACTION_RETRY,
};

/// Stable codes produced by this module.
pub mod codes {
    pub const COMMAND_UNKNOWN: &str = "downloads.command.unknown";
    pub const REQUEST_INVALID: &str = "downloads.request.invalid";
    pub const REQUEST_IDEMPOTENCY_CONFLICT: &str = "downloads.request.idempotency_conflict";
    pub const ARTIFACT_NOT_FOUND: &str = "downloads.artifact.not_found";
    pub const ARTIFACT_EXISTS: &str = "downloads.artifact.exists";
    pub const ARTIFACT_TOO_LARGE: &str = "downloads.artifact.too_large";
    pub const SOURCE_UNAVAILABLE: &str = "downloads.source.unavailable";
    pub const SOURCE_BUSY: &str = "downloads.source.busy";
    pub const SOURCE_UNKNOWN: &str = "downloads.source.unknown";
    pub const QUEUE_FULL: &str = "downloads.queue.full";
    pub const HASH_MISMATCH: &str = "downloads.hash.mismatch";
    pub const HASH_MISSING: &str = "downloads.hash.missing";
    pub const RANGE_UNSUPPORTED: &str = "downloads.range.unsupported";
    pub const RANGE_INVALID: &str = "downloads.range.invalid";
    pub const RANGE_LIMIT: &str = "downloads.range.limit";
    pub const DISK_FULL: &str = "downloads.disk.full";
    pub const CACHE_CORRUPT: &str = "downloads.cache.corrupt";
    pub const CACHE_MISS: &str = "downloads.cache.miss";
    pub const IO_FAILED: &str = "downloads.io.failed";
    pub const NETWORK_FAILED: &str = "downloads.network.failed";
    pub const CANCELLED: &str = "downloads.cancelled";
    pub const INTERNAL: &str = "downloads.internal";
}

/// Evidence markers 03 can attach to a task. The UX layer gates the
/// "progress was kept" and "files are ready" copy on exactly these strings.
pub mod evidence {
    /// Byte ranges survived the last interruption, so resuming will not restart.
    pub const RESUMABLE_RANGES: &str = "resumable_ranges";
    /// Every requested artifact is present and verified.
    pub const COMPLETED_ARTIFACTS: &str = "completed_artifacts";
}

/// Reasons 03 can fail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DownloadsError {
    UnknownCommand,
    InvalidRequest(&'static str),
    IdempotencyConflict,
    NotFound(String),
    AlreadyPresent(String),
    TooLarge { artifact: String, limit: u64 },
    SourceUnavailable(String),
    SourceBusy(String),
    SourceUnknown(String),
    QueueFull,
    HashMismatch(String),
    HashMissing(String),
    RangeUnsupported(String),
    RangeInvalid(String),
    RangeLimit(String),
    DiskFull { needed: u64, available: u64 },
    CacheCorrupt(String),
    CacheMiss(String),
    IoFailed(String),
    NetworkFailed(String),
    Cancelled,
    Internal,
}

impl DownloadsError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::UnknownCommand => codes::COMMAND_UNKNOWN,
            Self::InvalidRequest(_) => codes::REQUEST_INVALID,
            Self::IdempotencyConflict => codes::REQUEST_IDEMPOTENCY_CONFLICT,
            Self::NotFound(_) => codes::ARTIFACT_NOT_FOUND,
            Self::AlreadyPresent(_) => codes::ARTIFACT_EXISTS,
            Self::TooLarge { .. } => codes::ARTIFACT_TOO_LARGE,
            Self::SourceUnavailable(_) => codes::SOURCE_UNAVAILABLE,
            Self::SourceBusy(_) => codes::SOURCE_BUSY,
            Self::SourceUnknown(_) => codes::SOURCE_UNKNOWN,
            Self::QueueFull => codes::QUEUE_FULL,
            Self::HashMismatch(_) => codes::HASH_MISMATCH,
            Self::HashMissing(_) => codes::HASH_MISSING,
            Self::RangeUnsupported(_) => codes::RANGE_UNSUPPORTED,
            Self::RangeInvalid(_) => codes::RANGE_INVALID,
            Self::RangeLimit(_) => codes::RANGE_LIMIT,
            Self::DiskFull { .. } => codes::DISK_FULL,
            Self::CacheCorrupt(_) => codes::CACHE_CORRUPT,
            Self::CacheMiss(_) => codes::CACHE_MISS,
            Self::IoFailed(_) => codes::IO_FAILED,
            Self::NetworkFailed(_) => codes::NETWORK_FAILED,
            Self::Cancelled => codes::CANCELLED,
            Self::Internal => codes::INTERNAL,
        }
    }

    fn category(&self) -> ErrorCategory {
        match self {
            Self::UnknownCommand
            | Self::InvalidRequest(_)
            | Self::RangeInvalid(_)
            | Self::RangeLimit(_) => ErrorCategory::InvalidRequest,
            Self::IdempotencyConflict
            | Self::AlreadyPresent(_)
            | Self::SourceBusy(_)
            | Self::QueueFull => ErrorCategory::Conflict,
            Self::NotFound(_) | Self::CacheMiss(_) | Self::SourceUnknown(_) => {
                ErrorCategory::NotFound
            }
            Self::SourceUnavailable(_) | Self::NetworkFailed(_) => ErrorCategory::Network,
            Self::TooLarge { .. } | Self::DiskFull { .. } | Self::IoFailed(_) => {
                ErrorCategory::Storage
            }
            Self::HashMismatch(_)
            | Self::HashMissing(_)
            | Self::RangeUnsupported(_)
            | Self::CacheCorrupt(_)
            | Self::Internal => ErrorCategory::Internal,
            Self::Cancelled => ErrorCategory::Cancelled,
        }
    }

    /// A hash mismatch is retryable (a mirror can be stale or truncated) but a
    /// missing expected hash is a caller bug, not a flaky network.
    fn retryable(&self) -> bool {
        matches!(
            self,
            Self::SourceBusy(_)
                | Self::SourceUnavailable(_)
                | Self::NetworkFailed(_)
                | Self::IoFailed(_)
                | Self::QueueFull
                | Self::HashMismatch(_)
                | Self::CacheMiss(_)
        )
    }

    fn user_action(&self) -> Option<&'static str> {
        match self {
            Self::SourceUnavailable(_) | Self::NetworkFailed(_) | Self::SourceBusy(_) => {
                Some(USER_ACTION_RETRY)
            }
            Self::QueueFull => Some(USER_ACTION_RETRY),
            Self::HashMismatch(_) | Self::CacheCorrupt(_) => Some(USER_ACTION_RELOAD),
            Self::DiskFull { .. } | Self::TooLarge { .. } => Some(USER_ACTION_OPEN_DETAILS),
            _ => None,
        }
    }

    fn detail(&self) -> Option<String> {
        match self {
            Self::NotFound(id) | Self::AlreadyPresent(id) => Some(format!("artifact:{id}")),
            Self::TooLarge { artifact, limit } => Some(format!("artifact:{artifact}:max:{limit}")),
            Self::SourceUnavailable(id) | Self::SourceBusy(id) | Self::SourceUnknown(id) => {
                Some(format!("source:{id}"))
            }
            Self::HashMismatch(id) | Self::HashMissing(id) => Some(format!("artifact:{id}")),
            Self::RangeUnsupported(id) | Self::RangeInvalid(id) | Self::RangeLimit(id) => {
                Some(format!("artifact:{id}"))
            }
            Self::DiskFull { needed, available } => Some(format!("disk:{needed}/{available}")),
            Self::CacheCorrupt(reference)
            | Self::CacheMiss(reference)
            | Self::IoFailed(reference)
            | Self::NetworkFailed(reference) => Some(reference.clone()),
            Self::InvalidRequest(field) => Some(format!("field:{field}")),
            Self::UnknownCommand
            | Self::IdempotencyConflict
            | Self::QueueFull
            | Self::Cancelled
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

impl fmt::Display for DownloadsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.code())
    }
}

impl std::error::Error for DownloadsError {}

#[cfg(test)]
mod tests {
    use super::*;

    fn every_variant() -> Vec<DownloadsError> {
        vec![
            DownloadsError::UnknownCommand,
            DownloadsError::InvalidRequest("f"),
            DownloadsError::IdempotencyConflict,
            DownloadsError::NotFound("a".into()),
            DownloadsError::AlreadyPresent("a".into()),
            DownloadsError::TooLarge {
                artifact: "a".into(),
                limit: 10,
            },
            DownloadsError::SourceUnavailable("s".into()),
            DownloadsError::SourceBusy("s".into()),
            DownloadsError::SourceUnknown("s".into()),
            DownloadsError::QueueFull,
            DownloadsError::HashMismatch("a".into()),
            DownloadsError::HashMissing("a".into()),
            DownloadsError::RangeUnsupported("a".into()),
            DownloadsError::RangeInvalid("a".into()),
            DownloadsError::RangeLimit("a".into()),
            DownloadsError::DiskFull {
                needed: 10,
                available: 1,
            },
            DownloadsError::CacheCorrupt("c".into()),
            DownloadsError::CacheMiss("c".into()),
            DownloadsError::IoFailed("i".into()),
            DownloadsError::NetworkFailed("n".into()),
            DownloadsError::Cancelled,
            DownloadsError::Internal,
        ]
    }

    #[test]
    fn every_code_is_in_this_module_namespace() {
        for error in every_variant() {
            assert_eq!(
                error.code().split('.').next(),
                Some("downloads"),
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
    fn a_hash_mismatch_is_retryable_and_offers_a_reload() {
        let envelope = DownloadsError::HashMismatch("a".into()).to_envelope();
        assert!(envelope.retryable);
        assert_eq!(
            envelope.user_action.as_ref().map(|a| a.as_str()),
            Some(USER_ACTION_RELOAD)
        );
    }

    #[test]
    fn a_full_disk_is_not_advertised_as_retryable() {
        let envelope = DownloadsError::DiskFull {
            needed: 100,
            available: 1,
        }
        .to_envelope();
        assert!(!envelope.retryable);
        assert_eq!(envelope.category, ErrorCategory::Storage);
    }

    #[test]
    fn envelopes_carry_no_url_or_token() {
        let envelope = DownloadsError::SourceUnavailable("mirror-a".into()).to_envelope();
        let json = serde_json::to_string(&envelope).expect("serializable");
        assert!(json.contains("source:mirror-a"));
        assert!(!json.contains("http"));
    }
}
