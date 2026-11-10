//! Manifest: the immutable description of one distributable file.
//!
//! Three properties are load-bearing and each is checked separately, because in
//! the design document they are three independent judgements (§3):
//!
//! 1. **content integrity** — the chunk digests and the whole-file digest agree,
//! 2. **signature presence** — the manifest was signed over a defined field set,
//! 3. **authorization** — a [`Grant`] says this resource may be shared at all.
//!
//! A manifest that passes (1) and (2) but has no live grant must not be offered
//! to peers. Keeping the checks separate is what stops "the bytes are correct"
//! from being mistaken for "we may distribute them".

use shtmcl_domain::ArtifactId;

use crate::distribution::error::DistributionError;

/// Maximum chunk count accepted. A manifest is small metadata; a 10-million
/// chunk manifest is either an attack or a mistake, and both must be refused
/// before allocating.
pub const MAX_CHUNKS: usize = 1_000_000;

/// Allowed chunk sizes the PoC measures (1 / 2 / 4 MiB per §5). Not a
/// constraint on the protocol, only on what this branch will plan for.
pub const MEASURED_CHUNK_SIZES: [u32; 3] = [1 << 20, 2 << 20, 4 << 20];

/// One chunk's identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChunkHash {
    /// Offset of the chunk in the file.
    pub offset: u64,
    /// Length in bytes; the last chunk may be shorter.
    pub length: u32,
    /// Lowercase hex sha256 of exactly those bytes.
    pub digest: String,
}

/// Rights to distribute one resource.
///
/// Present and live is a precondition for offering a manifest to peers. The
/// evidence reference is opaque: this branch does not judge the evidence, it only
/// refuses to proceed without one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Grant {
    pub grant_id: String,
    pub resource_id: String,
    /// Opaque pointer to the recorded permission.
    pub rights_evidence_ref: String,
    pub allowed_regions: Vec<String>,
    pub expires_at: u64,
    pub revoked_at: Option<u64>,
}

impl Grant {
    /// Whether sharing is permitted at `now`, in `region`.
    pub fn is_live(&self, now: u64, region: &str) -> Result<(), DistributionError> {
        if self.revoked_at.is_some() {
            return Err(DistributionError::GrantExpired(self.grant_id.clone()));
        }
        if now >= self.expires_at {
            return Err(DistributionError::GrantExpired(self.grant_id.clone()));
        }
        if self.rights_evidence_ref.trim().is_empty() {
            return Err(DistributionError::GrantMissing(self.grant_id.clone()));
        }
        // An empty region list means "no regional limitation was recorded". An
        // explicit list is binding: sharing outside it is not authorized.
        if !self.allowed_regions.is_empty()
            && !self
                .allowed_regions
                .iter()
                .any(|allowed| allowed.eq_ignore_ascii_case(region))
        {
            return Err(DistributionError::GrantMissing(self.grant_id.clone()));
        }
        Ok(())
    }
}

/// One signed manifest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Manifest {
    pub manifest_id: String,
    /// Immutable artifact identity; URL changes do not change it.
    pub artifact: ArtifactId,
    /// Total file length in bytes.
    pub length: u64,
    pub chunk_size: u32,
    pub chunks: Vec<ChunkHash>,
    /// Root digest over the manifest's signed fields.
    pub root_digest: String,
    /// Signature over `root_digest`. Absence is a hard failure, not a warning.
    pub signature: Option<String>,
}

impl Manifest {
    /// Number of chunks the file needs at this chunk size.
    pub fn chunk_count(length: u64, chunk_size: u32) -> usize {
        if chunk_size == 0 || length == 0 {
            return 0;
        }
        length.div_ceil(chunk_size as u64) as usize
    }

    /// Validate structure, count, signature presence and internal consistency.
    ///
    /// This does **not** verify chunk bytes; that happens during transfer.
    pub fn validate(&self) -> Result<(), DistributionError> {
        if self.manifest_id.trim().is_empty() || self.root_digest.trim().is_empty() {
            return Err(DistributionError::ManifestInvalid(self.manifest_id.clone()));
        }
        if self.chunk_size == 0 {
            return Err(DistributionError::ManifestInvalid(self.manifest_id.clone()));
        }
        if self.chunks.len() > MAX_CHUNKS {
            return Err(DistributionError::ManifestTooLarge {
                chunks: self.chunks.len(),
                limit: MAX_CHUNKS,
            });
        }
        let expected = Self::chunk_count(self.length, self.chunk_size);
        if expected == 0 {
            return Err(DistributionError::ManifestInvalid(self.manifest_id.clone()));
        }
        if self.chunks.len() != expected {
            return Err(DistributionError::ManifestInvalid(self.manifest_id.clone()));
        }
        // Offsets must tile the file exactly: no gap and no overlap, because a
        // gap would be silently never downloaded and an overlap would corrupt.
        let mut cursor: u64 = 0;
        for (index, chunk) in self.chunks.iter().enumerate() {
            if chunk.offset != cursor {
                return Err(DistributionError::ManifestInvalid(format!(
                    "{}#offset@{index}",
                    self.manifest_id
                )));
            }
            if chunk.length == 0 || chunk.length > self.chunk_size {
                return Err(DistributionError::ManifestInvalid(format!(
                    "{}#length@{index}",
                    self.manifest_id
                )));
            }
            if chunk.digest.len() != 64 || !chunk.digest.chars().all(|c| c.is_ascii_hexdigit()) {
                return Err(DistributionError::ManifestInvalid(format!(
                    "{}#digest@{index}",
                    self.manifest_id
                )));
            }
            cursor += chunk.length as u64;
        }
        if cursor != self.length {
            return Err(DistributionError::ManifestInvalid(format!(
                "{}#total",
                self.manifest_id
            )));
        }
        if self
            .signature
            .as_deref()
            .map(str::trim)
            .unwrap_or("")
            .is_empty()
        {
            return Err(DistributionError::SignatureMissing(
                self.manifest_id.clone(),
            ));
        }
        Ok(())
    }

    /// Chunk at `index`, with a range check that cannot be skipped.
    pub fn chunk(&self, index: usize) -> Result<&ChunkHash, DistributionError> {
        self.chunks
            .get(index)
            .ok_or(DistributionError::ChunkIndexOutOfRange {
                index,
                total: self.chunks.len(),
            })
    }

    /// The byte range one chunk covers.
    pub fn range(&self, index: usize) -> Result<(u64, u64), DistributionError> {
        let chunk = self.chunk(index)?;
        Ok((chunk.offset, chunk.offset + chunk.length as u64))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn digest(seed: u8) -> String {
        format!("{:02x}", seed).repeat(32)
    }

    /// A valid 4-chunk manifest of 4 * 1024 bytes at a 1024-byte chunk size.
    fn manifest(chunk_size: u32, chunks: usize) -> Manifest {
        let mut list = Vec::new();
        for index in 0..chunks {
            list.push(ChunkHash {
                offset: index as u64 * chunk_size as u64,
                length: chunk_size,
                digest: digest(index as u8),
            });
        }
        Manifest {
            manifest_id: "m1".into(),
            artifact: ArtifactId::new("artifact-1").expect("id"),
            length: chunks as u64 * chunk_size as u64,
            chunk_size,
            chunks: list,
            root_digest: digest(255),
            signature: Some("sig".into()),
        }
    }

    #[test]
    fn a_well_formed_manifest_validates() {
        assert!(manifest(1024, 4).validate().is_ok());
    }

    #[test]
    fn a_missing_signature_is_a_hard_failure() {
        let mut m = manifest(1024, 4);
        m.signature = None;
        assert!(matches!(
            m.validate(),
            Err(DistributionError::SignatureMissing(_))
        ));
        // An empty or whitespace signature is not a signature either.
        m.signature = Some("   ".into());
        assert!(matches!(
            m.validate(),
            Err(DistributionError::SignatureMissing(_))
        ));
    }

    #[test]
    fn chunks_must_tile_the_file_without_gap_or_overlap() {
        let mut m = manifest(1024, 4);
        m.chunks[2].offset += 1; // introduces a gap
        assert!(matches!(
            m.validate(),
            Err(DistributionError::ManifestInvalid(_))
        ));

        let mut m = manifest(1024, 4);
        m.chunks[1].offset -= 1; // introduces an overlap
        assert!(matches!(
            m.validate(),
            Err(DistributionError::ManifestInvalid(_))
        ));
    }

    #[test]
    fn the_chunk_count_must_match_the_length() {
        let mut m = manifest(1024, 4);
        m.length += 1; // now one byte short of the tiled total
        assert!(matches!(
            m.validate(),
            Err(DistributionError::ManifestInvalid(_))
        ));

        let mut m = manifest(1024, 4);
        m.chunks.pop();
        assert!(matches!(
            m.validate(),
            Err(DistributionError::ManifestInvalid(_))
        ));
    }

    #[test]
    fn a_chunk_longer_than_the_chunk_size_is_refused() {
        let mut m = manifest(1024, 4);
        m.chunks[0].length = 2048;
        assert!(matches!(
            m.validate(),
            Err(DistributionError::ManifestInvalid(_))
        ));
    }

    #[test]
    fn an_oversized_manifest_is_refused_before_allocating() {
        let mut m = manifest(1024, 1);
        m.chunks = vec![
            ChunkHash {
                offset: 0,
                length: 1,
                digest: digest(0),
            };
            MAX_CHUNKS + 1
        ];
        assert!(matches!(
            m.validate(),
            Err(DistributionError::ManifestTooLarge { .. })
        ));
    }

    #[test]
    fn a_bad_digest_shape_is_refused() {
        let mut m = manifest(1024, 4);
        m.chunks[0].digest = "zz".repeat(32);
        assert!(matches!(
            m.validate(),
            Err(DistributionError::ManifestInvalid(_))
        ));
    }

    #[test]
    fn a_live_grant_is_required_and_region_is_binding() {
        let grant = Grant {
            grant_id: "g1".into(),
            resource_id: "r1".into(),
            rights_evidence_ref: "evidence-1".into(),
            allowed_regions: vec!["EU".into()],
            expires_at: 1_000,
            revoked_at: None,
        };
        assert!(grant.is_live(500, "eu").is_ok());
        assert!(grant.is_live(500, "US").is_err());
        assert!(grant.is_live(1_000, "EU").is_err());
        let mut revoked = grant.clone();
        revoked.revoked_at = Some(400);
        assert!(revoked.is_live(500, "EU").is_err());
        let mut no_evidence = grant.clone();
        no_evidence.rights_evidence_ref = "  ".into();
        assert!(no_evidence.is_live(500, "EU").is_err());
    }

    #[test]
    fn an_empty_region_list_means_unrestricted() {
        let grant = Grant {
            grant_id: "g1".into(),
            resource_id: "r1".into(),
            rights_evidence_ref: "evidence-1".into(),
            allowed_regions: Vec::new(),
            expires_at: 1_000,
            revoked_at: None,
        };
        assert!(grant.is_live(500, "ANYWHERE").is_ok());
    }

    #[test]
    fn chunk_offsets_cover_the_file() {
        let m = manifest(1024, 4);
        assert_eq!(m.range(0).expect("range"), (0, 1024));
        assert_eq!(m.range(3).expect("range"), (3072, 4096));
        assert!(matches!(
            m.range(4),
            Err(DistributionError::ChunkIndexOutOfRange { index: 4, total: 4 })
        ));
    }

    #[test]
    fn the_last_chunk_may_be_short() {
        let mut m = manifest(1024, 4);
        m.length = 3 * 1024 + 100;
        m.chunks[3].length = 100;
        assert!(m.validate().is_ok());
    }
}
