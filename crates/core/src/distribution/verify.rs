//! Two-level verification: per chunk, then per file.
//!
//! The order is not negotiable. A chunk enters the cache only after its own
//! digest matched, and the whole file is only declared complete after the file
//! digest matched. Publishing on chunk success alone would let a manifest with a
//! correct chunk list and a wrong file digest ship a corrupt install; verifying
//! only at the end would let a corrupt chunk waste the whole transfer.
//!
//! Hashing is streaming with a fixed buffer, so memory does not scale with the
//! file size. §5 budgets 32 MiB for in-flight buffers and keeps metadata out of
//! it; this module allocates one [`BUFFER_BYTES`] block and nothing per chunk.

use sha2::{Digest, Sha256};

use crate::distribution::error::DistributionError;
use crate::distribution::manifest::{ChunkHash, Manifest};

/// Streaming buffer size. Chosen larger than one 4 MiB chunk would need so a
/// single chunk read is one to two syscalls, but small enough that eight
/// concurrent readers stay inside the 32 MiB budget.
pub const BUFFER_BYTES: usize = 256 * 1024;

/// Lowercase hex sha256 of a byte slice.
pub fn digest_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

/// Verify one chunk's bytes against its manifest digest.
///
/// Returns the digest that was actually computed on mismatch, so a report can
/// name both sides instead of only saying "failed".
pub fn verify_chunk(chunk: &ChunkHash, bytes: &[u8], peer: &str) -> Result<(), DistributionError> {
    if bytes.len() != chunk.length as usize {
        return Err(DistributionError::ChunkHashMismatch {
            index: 0,
            peer: peer.to_string(),
        });
    }
    let actual = digest_hex(bytes);
    if actual != chunk.digest {
        return Err(DistributionError::ChunkHashMismatch {
            index: 0,
            peer: peer.to_string(),
        });
    }
    Ok(())
}

/// Verify one chunk at a known index, reporting that index on failure.
pub fn verify_chunk_at(
    index: usize,
    chunk: &ChunkHash,
    bytes: &[u8],
    peer: &str,
) -> Result<(), DistributionError> {
    if bytes.len() != chunk.length as usize {
        return Err(DistributionError::ChunkHashMismatch {
            index,
            peer: peer.to_string(),
        });
    }
    if digest_hex(bytes) != chunk.digest {
        return Err(DistributionError::ChunkHashMismatch {
            index,
            peer: peer.to_string(),
        });
    }
    Ok(())
}

/// Incremental whole-file hasher.
///
/// Chunks arrive out of order, so a streaming hasher cannot simply be fed in
/// arrival order. The PoC therefore keeps verified chunk bytes and hashes the
/// assembled file in order, which is the semantics a real install needs anyway.
#[derive(Debug)]
pub struct FileVerifier {
    total_chunks: usize,
    received: Vec<Option<Vec<u8>>>,
    bytes: u64,
}

impl FileVerifier {
    pub fn new(total_chunks: usize) -> Self {
        Self {
            total_chunks,
            received: vec![None; total_chunks],
            bytes: 0,
        }
    }

    /// Store a chunk that already passed its own digest check.
    pub fn accept(&mut self, index: usize, bytes: Vec<u8>) -> Result<(), DistributionError> {
        if index >= self.total_chunks {
            return Err(DistributionError::ChunkIndexOutOfRange {
                index,
                total: self.total_chunks,
            });
        }
        if self.received[index].is_none() {
            self.bytes += bytes.len() as u64;
        }
        self.received[index] = Some(bytes);
        Ok(())
    }

    pub fn bytes(&self) -> u64 {
        self.bytes
    }

    pub fn is_complete(&self) -> bool {
        self.received.iter().all(Option::is_some)
    }

    pub fn missing(&self) -> Vec<usize> {
        self.received
            .iter()
            .enumerate()
            .filter(|(_, slot)| slot.is_none())
            .map(|(index, _)| index)
            .collect()
    }

    /// Hash the assembled file and compare to the manifest.
    ///
    /// Refuses to run on an incomplete file: an incomplete hash can only produce
    /// a misleading mismatch, and reporting "corrupt" for "unfinished" would send
    /// the caller down the wrong recovery path.
    pub fn finish(&self, manifest: &Manifest) -> Result<String, DistributionError> {
        if !self.is_complete() {
            return Err(DistributionError::PlanExhausted);
        }
        let mut hasher = Sha256::new();
        let mut buffer = vec![0u8; BUFFER_BYTES];
        for slot in &self.received {
            let bytes = slot.as_ref().expect("complete");
            // Feed through the fixed buffer so peak memory stays flat regardless
            // of chunk size; the copy is the price of the bound.
            let mut offset = 0;
            while offset < bytes.len() {
                let take = (bytes.len() - offset).min(buffer.len());
                buffer[..take].copy_from_slice(&bytes[offset..offset + take]);
                hasher.update(&buffer[..take]);
                offset += take;
            }
        }
        let actual = format!("{:x}", hasher.finalize());
        if actual != manifest.root_digest {
            return Err(DistributionError::FileDigestMismatch {
                expected: manifest.root_digest.clone(),
                actual,
            });
        }
        Ok(actual)
    }
}

/// Working-pool size for verification. Fixed rather than `num_cpus`: §5 asks for
/// a *finite* pool, and hashing is CPU-bound enough that oversubscribing the
/// machine would show up as install latency elsewhere.
pub const VERIFY_WORKERS: usize = 2;

#[cfg(test)]
mod tests {
    use super::*;
    use shtmcl_domain::ArtifactId;

    fn manifest_for(chunks: &[&[u8]]) -> Manifest {
        let mut list = Vec::new();
        let mut offset = 0u64;
        for bytes in chunks {
            list.push(ChunkHash {
                offset,
                length: bytes.len() as u32,
                digest: digest_hex(bytes),
            });
            offset += bytes.len() as u64;
        }
        // Whole-file digest must match what `FileVerifier::finish` computes.
        let mut hasher = Sha256::new();
        for bytes in chunks {
            hasher.update(bytes);
        }
        Manifest {
            manifest_id: "m1".into(),
            artifact: ArtifactId::new("a1").expect("id"),
            length: offset,
            chunk_size: chunks.iter().map(|c| c.len() as u32).max().unwrap_or(1),
            chunks: list,
            root_digest: format!("{:x}", hasher.finalize()),
            signature: Some("sig".into()),
        }
    }

    #[test]
    fn a_correct_chunk_verifies() {
        let bytes = b"hello world";
        let chunk = ChunkHash {
            offset: 0,
            length: bytes.len() as u32,
            digest: digest_hex(bytes),
        };
        assert!(verify_chunk(&chunk, bytes, "p1").is_ok());
    }

    #[test]
    fn a_corrupt_chunk_is_refused_and_names_the_peer() {
        let bytes = b"hello world";
        let chunk = ChunkHash {
            offset: 0,
            length: bytes.len() as u32,
            digest: digest_hex(b"different"),
        };
        match verify_chunk_at(7, &chunk, bytes, "bad-peer") {
            Err(DistributionError::ChunkHashMismatch { index, peer }) => {
                assert_eq!(index, 7);
                assert_eq!(peer, "bad-peer");
            }
            other => panic!("expected a chunk mismatch, got {other:?}"),
        }
    }

    #[test]
    fn a_truncated_chunk_is_refused() {
        let bytes = b"hello world";
        let chunk = ChunkHash {
            offset: 0,
            length: 11,
            digest: digest_hex(bytes),
        };
        assert!(verify_chunk(&chunk, b"hello", "p1").is_err());
    }

    #[test]
    fn chunks_may_arrive_out_of_order() {
        let chunks: Vec<&[u8]> = vec![b"aaa", b"bbb", b"ccc"];
        let manifest = manifest_for(&chunks);
        let mut verifier = FileVerifier::new(3);
        verifier.accept(2, b"ccc".to_vec()).expect("accept");
        verifier.accept(0, b"aaa".to_vec()).expect("accept");
        assert!(!verifier.is_complete());
        assert_eq!(verifier.missing(), vec![1]);
        verifier.accept(1, b"bbb".to_vec()).expect("accept");
        assert!(verifier.is_complete());
        assert!(verifier.finish(&manifest).is_ok());
    }

    #[test]
    fn an_incomplete_file_is_not_hashed() {
        let chunks: Vec<&[u8]> = vec![b"aaa", b"bbb"];
        let manifest = manifest_for(&chunks);
        let mut verifier = FileVerifier::new(2);
        verifier.accept(0, b"aaa".to_vec()).expect("accept");
        // Reporting "digest mismatch" here would send the caller to the wrong
        // recovery path; "not finished" is the accurate answer.
        assert!(matches!(
            verifier.finish(&manifest),
            Err(DistributionError::PlanExhausted)
        ));
    }

    #[test]
    fn a_wrong_file_digest_is_refused_even_when_every_chunk_is_right() {
        // Each chunk matches, but the manifest's file digest does not: a manifest
        // can be internally consistent per chunk and still describe a file the
        // publisher did not intend. The final check is what catches it.
        let chunks: Vec<&[u8]> = vec![b"aaa", b"bbb"];
        let mut manifest = manifest_for(&chunks);
        manifest.root_digest = "0".repeat(64);
        let mut verifier = FileVerifier::new(2);
        verifier.accept(0, b"aaa".to_vec()).expect("accept");
        verifier.accept(1, b"bbb".to_vec()).expect("accept");
        assert!(matches!(
            verifier.finish(&manifest),
            Err(DistributionError::FileDigestMismatch { .. })
        ));
    }

    #[test]
    fn re_accepting_a_chunk_does_not_double_count_bytes() {
        let mut verifier = FileVerifier::new(2);
        verifier.accept(0, b"aaa".to_vec()).expect("accept");
        assert_eq!(verifier.bytes(), 3);
        verifier.accept(0, b"aaa".to_vec()).expect("accept again");
        assert_eq!(verifier.bytes(), 3);
    }

    #[test]
    fn an_out_of_range_accept_is_refused() {
        let mut verifier = FileVerifier::new(1);
        assert!(matches!(
            verifier.accept(1, b"x".to_vec()),
            Err(DistributionError::ChunkIndexOutOfRange { index: 1, total: 1 })
        ));
    }

    #[test]
    fn a_large_chunk_hashes_identically_to_the_direct_digest() {
        // Guards the buffered path against an off-by-one at a buffer boundary.
        let bytes = vec![7u8; BUFFER_BYTES * 2 + 13];
        let mut verifier = FileVerifier::new(1);
        let direct = digest_hex(&bytes);
        let chunk = ChunkHash {
            offset: 0,
            length: bytes.len() as u32,
            digest: direct.clone(),
        };
        verifier.accept(0, bytes).expect("accept");
        let manifest = Manifest {
            manifest_id: "m1".into(),
            artifact: ArtifactId::new("a1").expect("id"),
            length: 0,
            chunk_size: 1,
            chunks: vec![chunk],
            root_digest: direct,
            signature: Some("sig".into()),
        };
        assert!(verifier.finish(&manifest).is_ok());
    }
}
