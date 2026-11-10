//! Module-local error vocabulary for the 09 research branch.
//!
//! Codes live in the `distribution` namespace so `scripts/check-codes.mjs` can
//! verify ownership without central registration.

use std::fmt;

use shtmcl_domain::{ErrorCategory, ErrorCode, ErrorEnvelope};

/// Stable codes produced by this branch.
pub mod codes {
    pub const MANIFEST_INVALID: &str = "distribution.manifest.invalid";
    pub const MANIFEST_SIGNATURE_MISSING: &str = "distribution.manifest.signature_missing";
    pub const MANIFEST_TOO_LARGE: &str = "distribution.manifest.too_large";
    pub const CHUNK_HASH_MISMATCH: &str = "distribution.chunk.hash_mismatch";
    pub const CHUNK_INDEX_OUT_OF_RANGE: &str = "distribution.chunk.out_of_range";
    pub const FILE_DIGEST_MISMATCH: &str = "distribution.file.digest_mismatch";
    pub const PLAN_EXHAUSTED: &str = "distribution.plan.exhausted";
    pub const PLAN_BUDGET_EXCEEDED: &str = "distribution.plan.budget_exceeded";
    pub const PEER_QUARANTINED: &str = "distribution.peer.quarantined";
    pub const PEER_NONE_AVAILABLE: &str = "distribution.peer.none_available";
    pub const GRANT_MISSING: &str = "distribution.grant.missing";
    pub const GRANT_EXPIRED: &str = "distribution.grant.expired";
    pub const INTERNAL: &str = "distribution.internal";
}

/// Reasons this branch can fail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DistributionError {
    ManifestInvalid(String),
    SignatureMissing(String),
    ManifestTooLarge { chunks: usize, limit: usize },
    ChunkHashMismatch { index: usize, peer: String },
    ChunkIndexOutOfRange { index: usize, total: usize },
    FileDigestMismatch { expected: String, actual: String },
    PlanExhausted,
    PlanBudgetExceeded { requested: usize, limit: usize },
    PeerQuarantined(String),
    PeerNoneAvailable,
    GrantMissing(String),
    GrantExpired(String),
    Internal,
}

impl DistributionError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::ManifestInvalid(_) => codes::MANIFEST_INVALID,
            Self::SignatureMissing(_) => codes::MANIFEST_SIGNATURE_MISSING,
            Self::ManifestTooLarge { .. } => codes::MANIFEST_TOO_LARGE,
            Self::ChunkHashMismatch { .. } => codes::CHUNK_HASH_MISMATCH,
            Self::ChunkIndexOutOfRange { .. } => codes::CHUNK_INDEX_OUT_OF_RANGE,
            Self::FileDigestMismatch { .. } => codes::FILE_DIGEST_MISMATCH,
            Self::PlanExhausted => codes::PLAN_EXHAUSTED,
            Self::PlanBudgetExceeded { .. } => codes::PLAN_BUDGET_EXCEEDED,
            Self::PeerQuarantined(_) => codes::PEER_QUARANTINED,
            Self::PeerNoneAvailable => codes::PEER_NONE_AVAILABLE,
            Self::GrantMissing(_) => codes::GRANT_MISSING,
            Self::GrantExpired(_) => codes::GRANT_EXPIRED,
            Self::Internal => codes::INTERNAL,
        }
    }

    fn category(&self) -> ErrorCategory {
        match self {
            Self::ManifestInvalid(_)
            | Self::SignatureMissing(_)
            | Self::ManifestTooLarge { .. }
            | Self::ChunkIndexOutOfRange { .. }
            | Self::PlanBudgetExceeded { .. } => ErrorCategory::InvalidRequest,
            Self::GrantMissing(_) | Self::GrantExpired(_) => ErrorCategory::PermissionDenied,
            Self::PeerNoneAvailable | Self::PlanExhausted => ErrorCategory::Unsupported,
            Self::ChunkHashMismatch { .. }
            | Self::FileDigestMismatch { .. }
            | Self::PeerQuarantined(_)
            | Self::Internal => ErrorCategory::Internal,
        }
    }

    /// A corrupt chunk is retryable from another source; a bad manifest is not.
    fn retryable(&self) -> bool {
        matches!(
            self,
            Self::ChunkHashMismatch { .. } | Self::PeerNoneAvailable | Self::PeerQuarantined(_)
        )
    }

    fn detail(&self) -> Option<String> {
        match self {
            Self::ManifestInvalid(reference) | Self::SignatureMissing(reference) => {
                Some(format!("manifest:{reference}"))
            }
            Self::ManifestTooLarge { chunks, limit } => Some(format!("chunks:{chunks}/{limit}")),
            Self::ChunkHashMismatch { index, peer } => Some(format!("chunk:{index}@{peer}")),
            Self::ChunkIndexOutOfRange { index, total } => Some(format!("chunk:{index}/{total}")),
            Self::FileDigestMismatch { expected, actual } => {
                Some(format!("digest:{expected}->{actual}"))
            }
            Self::PlanBudgetExceeded { requested, limit } => {
                Some(format!("inflight:{requested}/{limit}"))
            }
            Self::PeerQuarantined(peer) => Some(format!("peer:{peer}")),
            Self::GrantMissing(reference) | Self::GrantExpired(reference) => {
                Some(format!("grant:{reference}"))
            }
            Self::PlanExhausted | Self::PeerNoneAvailable | Self::Internal => None,
        }
    }

    pub fn to_envelope(&self) -> ErrorEnvelope {
        let mut envelope = ErrorEnvelope::new(
            ErrorCode::from_static(self.code()),
            self.category(),
            self.retryable(),
        );
        if let Some(detail) = self.detail() {
            envelope = envelope.with_detail_ref(detail);
        }
        envelope
    }
}

impl fmt::Display for DistributionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.code())
    }
}

impl std::error::Error for DistributionError {}

#[cfg(test)]
mod tests {
    use super::*;

    fn every_variant() -> Vec<DistributionError> {
        vec![
            DistributionError::ManifestInvalid("m".into()),
            DistributionError::SignatureMissing("m".into()),
            DistributionError::ManifestTooLarge {
                chunks: 10,
                limit: 5,
            },
            DistributionError::ChunkHashMismatch {
                index: 1,
                peer: "p".into(),
            },
            DistributionError::ChunkIndexOutOfRange { index: 9, total: 3 },
            DistributionError::FileDigestMismatch {
                expected: "a".into(),
                actual: "b".into(),
            },
            DistributionError::PlanExhausted,
            DistributionError::PlanBudgetExceeded {
                requested: 9,
                limit: 8,
            },
            DistributionError::PeerQuarantined("p".into()),
            DistributionError::PeerNoneAvailable,
            DistributionError::GrantMissing("g".into()),
            DistributionError::GrantExpired("g".into()),
            DistributionError::Internal,
        ]
    }

    #[test]
    fn every_code_is_in_this_branch_namespace() {
        for error in every_variant() {
            assert_eq!(
                error.code().split('.').next(),
                Some("distribution"),
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
            assert!(seen.insert(error.code()), "duplicate {}", error.code());
        }
        assert_eq!(seen.len(), all.len());
    }

    #[test]
    fn a_corrupt_chunk_is_retryable_but_a_bad_manifest_is_not() {
        assert!(
            DistributionError::ChunkHashMismatch {
                index: 0,
                peer: "p".into()
            }
            .to_envelope()
            .retryable
        );
        assert!(
            !DistributionError::ManifestInvalid("m".into())
                .to_envelope()
                .retryable
        );
    }
}
