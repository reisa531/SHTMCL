//! Chunk scheduling: bounded rare-first with an in-flight set and tail-chunk
//! duplicate cancellation.
//!
//! The scheduler is deliberately pure — it owns no threads, no I/O and no clock.
//! Every decision is a function of the state passed in, which is what makes the
//! claims in §5 measurable: the same chunk order and the same in-flight bound can
//! be replayed from a recorded state instead of inferred from a log.

use std::collections::{BTreeMap, BTreeSet};

use crate::distribution::error::DistributionError;
use crate::distribution::manifest::Manifest;

/// Concurrency limits. §5 starts at 8 per file and 2 per peer and compares
/// 4/8/16 on the same network; these are the comparison points, not conclusions.
pub const DEFAULT_FILE_CONCURRENCY: usize = 8;
pub const DEFAULT_PEER_CONCURRENCY: usize = 2;
pub const CONCURRENCY_SWEEP: [usize; 3] = [4, 8, 16];

/// Which chunks are still wanted, and who is already asking for them.
///
/// `wanted` is a set rather than a queue because rare-first needs the whole
/// remaining population to compute rarity; a queue would make "rarer than the
/// next item" impossible to answer.
#[derive(Debug, Clone)]
pub struct Scheduler {
    total: usize,
    /// Chunk indices not yet verified.
    wanted: BTreeSet<usize>,
    /// Chunk index → peers currently fetching it.
    in_flight: BTreeMap<usize, BTreeSet<String>>,
    /// Verified chunks; a chunk leaves `wanted` only after this is set.
    verified: BTreeSet<usize>,
    /// Chunk index → how many peers claim to have it.
    availability: BTreeMap<usize, usize>,
    max_file_concurrency: usize,
    max_peer_concurrency: usize,
    /// Requests issued for the same chunk. The first verified response cancels
    /// the rest; the count is retained so the duplicate cost is reportable.
    duplicate_requests: u64,
    /// Cache attribution: bytes served by peers, for the savings claim.
    host_cache_hits: u64,
}

/// One assignment handed back to the caller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Assignment {
    pub chunk: usize,
    pub peer_id: String,
    /// True when this chunk was already in flight and the caller is opening a
    /// duplicate request on purpose (tail-chunk hedging).
    pub duplicate: bool,
}

impl Scheduler {
    pub fn new(total: usize) -> Self {
        Self {
            total,
            wanted: (0..total).collect(),
            in_flight: BTreeMap::new(),
            verified: BTreeSet::new(),
            availability: BTreeMap::new(),
            max_file_concurrency: DEFAULT_FILE_CONCURRENCY,
            max_peer_concurrency: DEFAULT_PEER_CONCURRENCY,
            duplicate_requests: 0,
            host_cache_hits: 0,
        }
    }

    pub fn with_limits(mut self, file: usize, peer: usize) -> Self {
        self.max_file_concurrency = file.max(1);
        self.max_peer_concurrency = peer.max(1);
        self
    }

    /// Declare how many peers hold a chunk. Called as peer lists arrive.
    pub fn set_availability(&mut self, availability: &BTreeMap<usize, usize>) {
        self.availability = availability.clone();
    }

    pub fn total(&self) -> usize {
        self.total
    }

    pub fn remaining(&self) -> usize {
        self.wanted.len()
    }

    pub fn verified_count(&self) -> usize {
        self.verified.len()
    }

    pub fn duplicate_requests(&self) -> u64 {
        self.duplicate_requests
    }

    pub fn in_flight_count(&self) -> usize {
        self.in_flight.values().map(BTreeSet::len).sum()
    }

    /// Record that a chunk verified. Returns `false` when it was already verified,
    /// so a caller can tell a real advance from a duplicate response.
    pub fn mark_verified(&mut self, chunk: usize) -> Result<bool, DistributionError> {
        if chunk >= self.total {
            return Err(DistributionError::ChunkIndexOutOfRange {
                index: chunk,
                total: self.total,
            });
        }
        let newly = self.wanted.remove(&chunk);
        self.verified.insert(chunk);
        // The first verified response cancels every duplicate for this chunk.
        self.in_flight.remove(&chunk);
        Ok(newly)
    }

    /// Release an in-flight claim without verifying (failure or cancellation).
    pub fn release(&mut self, chunk: usize, peer_id: &str) {
        if let Some(peers) = self.in_flight.get_mut(&chunk) {
            peers.remove(peer_id);
            if peers.is_empty() {
                self.in_flight.remove(&chunk);
            }
        }
    }

    pub fn is_complete(&self) -> bool {
        self.wanted.is_empty()
    }

    /// Every chunk index still owed, in ascending order.
    pub fn outstanding(&self) -> Vec<usize> {
        self.wanted.iter().copied().collect()
    }

    /// Rarity: fewer holders means more urgent, computed over the remaining set.
    ///
    /// A chunk nobody advertises sorts first: it needs the bounded HTTP fallback,
    /// and leaving it until the end is what makes a download appear finished and
    /// then hang on the last missing piece.
    fn rarity(&self, chunk: usize) -> usize {
        self.availability.get(&chunk).copied().unwrap_or(0)
    }

    /// Pick the next chunks to request, rarest first, respecting both bounds.
    ///
    /// `ranking` is the caller's peer preference (already scored by [`super::peer`]).
    /// Peers not in `ranking` are never chosen, so a quarantined peer cannot be
    /// re-admitted by naming it here.
    pub fn next_assignments(&mut self, ranking: &[String]) -> Vec<Assignment> {
        let mut chosen = Vec::new();
        if ranking.is_empty() {
            return chosen;
        }

        // Per-peer in-flight count, so the per-peer cap is enforced against what
        // this scheduler actually handed out rather than a caller-maintained tally.
        // Keyed by an owned `String`: borrowing the ids out of `self.in_flight`
        // would hold an immutable borrow of `self` across the mutations below.
        let mut per_peer: BTreeMap<String, usize> = BTreeMap::new();
        for peers in self.in_flight.values() {
            for peer in peers {
                *per_peer.entry(peer.clone()).or_insert(0) += 1;
            }
        }

        // Round-robin, one chunk per peer per round. Handing a peer several chunks
        // before its peers get their first is what makes the "share the work"
        // property false in practice, and it also starves the per-peer bound's
        // purpose: the cap exists to keep one slow link from owning the tail, not
        // to let the first-ranked peer fill the window alone.
        let mut idle_rounds = 0usize;
        while self.in_flight_count() < self.max_file_concurrency {
            let eligible: Vec<String> = ranking
                .iter()
                .filter(|peer| {
                    per_peer.get(peer.as_str()).copied().unwrap_or(0) < self.max_peer_concurrency
                })
                .cloned()
                .collect();
            if eligible.is_empty() {
                break;
            }

            let mut progressed = false;
            for peer in eligible {
                if self.in_flight_count() >= self.max_file_concurrency {
                    break;
                }
                match self.pick_chunk_for(&peer) {
                    Some((chunk, duplicate)) => {
                        if duplicate {
                            self.duplicate_requests = self.duplicate_requests.saturating_add(1);
                        }
                        self.in_flight
                            .entry(chunk)
                            .or_default()
                            .insert(peer.clone());
                        per_peer
                            .entry(peer.clone())
                            .and_modify(|count| *count += 1)
                            .or_insert(1);
                        chosen.push(Assignment {
                            chunk,
                            peer_id: peer,
                            duplicate,
                        });
                        progressed = true;
                    }
                    // This peer has nothing to do this round; its peers still can.
                    None => continue,
                }
            }

            if !progressed {
                idle_rounds += 1;
                // Two consecutive rounds with no assignment means the remaining
                // peers are all saturated; stop instead of spinning.
                if idle_rounds >= 2 {
                    break;
                }
            } else {
                idle_rounds = 0;
            }
        }
        chosen
    }

    /// Choose one chunk for `peer`, or `None` when this peer has nothing to do.
    ///
    /// Preference order:
    /// 1. the rarest chunk this peer is not already fetching;
    /// 2. otherwise the rare tail, if it may be hedged (see [`Self::may_hedge`]).
    ///
    /// The candidate is selected over the *whole* remaining set, not over the
    /// subset this peer has not touched: filtering by peer first would let a
    /// later peer pick a chunk that a ranking-earlier peer should have received,
    /// which is exactly how rare-first degrades into arbitrary order.
    fn pick_chunk_for(&self, peer: &str) -> Option<(usize, bool)> {
        // Fresh work first, always. A duplicate is only worth opening once every
        // remaining chunk is already claimed by somebody: before that, the extra
        // request steals bandwidth from a chunk nobody is fetching yet.
        if let Some(chunk) = self
            .wanted
            .iter()
            .copied()
            .filter(|chunk| !self.in_flight.contains_key(chunk))
            .min_by_key(|chunk| (self.rarity(*chunk), *chunk))
        {
            return Some((chunk, false));
        }

        // Everything outstanding is claimed. Now the only useful move is hedging
        // the rare tail, and only when this peer is not already holding it.
        let rarest = self.rarest_outstanding()?;
        if self.may_hedge(rarest, peer) {
            return Some((rarest, true));
        }
        None
    }

    /// The rarest outstanding chunk, ties broken by index for determinism.
    fn rarest_outstanding(&self) -> Option<usize> {
        self.wanted
            .iter()
            .copied()
            .min_by_key(|chunk| (self.rarity(*chunk), *chunk))
    }

    /// Whether `peer` may open one extra request for `chunk`.
    ///
    /// A duplicate is only ever useful for the rare tail: everywhere else the
    /// chunk has other holders, so a second request is pure waste. The bound is
    /// two holders in total — the original plus one hedge. Without this cap every
    /// arriving peer would open a copy of the same chunk and the "hedge" would
    /// become a broadcast.
    fn may_hedge(&self, chunk: usize, peer: &str) -> bool {
        let holders = match self.in_flight.get(&chunk) {
            Some(holders) => holders,
            None => return false,
        };
        if holders.contains(peer) {
            return false;
        }
        // Two holders means the hedge already exists.
        holders.len() < 2
    }

    /// Byte progress as `(verified, total)` for reporting.
    pub fn byte_progress(&self, manifest: &Manifest) -> (u64, u64) {
        let verified: u64 = self
            .verified
            .iter()
            .map(|chunk| manifest.chunks[*chunk].length as u64)
            .sum();
        (verified, manifest.length)
    }

    pub fn record_host_cache_hit(&mut self, bytes: u64) {
        self.host_cache_hits = self.host_cache_hits.saturating_add(bytes);
    }

    pub fn host_cache_hits(&self) -> u64 {
        self.host_cache_hits
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn peers(names: &[&str]) -> Vec<String> {
        names.iter().map(|n| n.to_string()).collect()
    }

    #[test]
    fn assignments_are_rarest_first() {
        let mut scheduler = Scheduler::new(4).with_limits(4, 4);
        let mut availability = BTreeMap::new();
        availability.insert(0, 9);
        availability.insert(1, 1);
        availability.insert(2, 5);
        availability.insert(3, 2);
        scheduler.set_availability(&availability);

        let assigned = scheduler.next_assignments(&peers(&["p1", "p2"]));
        // Chunk 1 (single holder) must be requested before chunk 0 (nine holders).
        assert_eq!(assigned[0].chunk, 1);
        assert_eq!(assigned[1].chunk, 3);
    }

    #[test]
    fn a_chunk_with_no_advertised_holder_is_requested_first() {
        let mut scheduler = Scheduler::new(3).with_limits(1, 1);
        let mut availability = BTreeMap::new();
        availability.insert(0, 10);
        availability.insert(1, 10);
        // Chunk 2 is advertised by nobody.
        scheduler.set_availability(&availability);
        let assigned = scheduler.next_assignments(&peers(&["p1"]));
        assert_eq!(assigned[0].chunk, 2);
    }

    #[test]
    fn the_file_concurrency_bound_is_respected() {
        let mut scheduler = Scheduler::new(20).with_limits(4, 8);
        let assigned = scheduler.next_assignments(&peers(&["p1", "p2", "p3", "p4"]));
        assert_eq!(assigned.len(), 4);
        assert_eq!(scheduler.in_flight_count(), 4);
        // A second call cannot exceed the bound while the first four are in flight.
        assert!(scheduler
            .next_assignments(&peers(&["p1", "p2", "p3", "p4"]))
            .is_empty());
    }

    #[test]
    fn the_per_peer_bound_is_respected() {
        let mut scheduler = Scheduler::new(20).with_limits(8, 2);
        let assigned = scheduler.next_assignments(&peers(&["p1"]));
        assert_eq!(assigned.len(), 2, "one peer must not exceed its cap");
    }

    #[test]
    fn work_is_shared_across_peers() {
        let mut scheduler = Scheduler::new(20).with_limits(6, 2);
        let assigned = scheduler.next_assignments(&peers(&["p1", "p2", "p3"]));
        let distinct: BTreeSet<&str> = assigned.iter().map(|a| a.peer_id.as_str()).collect();
        assert_eq!(distinct.len(), 3, "all peers should get work");
    }

    #[test]
    fn a_verified_chunk_leaves_the_outstanding_set_exactly_once() {
        let mut scheduler = Scheduler::new(3);
        assert_eq!(scheduler.remaining(), 3);
        assert!(scheduler.mark_verified(1).expect("verify"));
        assert_eq!(scheduler.verified_count(), 1);
        assert_eq!(scheduler.remaining(), 2);
        // A duplicate response is not a second advance.
        assert!(!scheduler.mark_verified(1).expect("verify"));
        assert_eq!(scheduler.verified_count(), 1);
    }

    #[test]
    fn verification_clears_in_flight_so_the_slot_is_reusable() {
        let mut scheduler = Scheduler::new(4).with_limits(1, 1);
        let first = scheduler.next_assignments(&peers(&["p1"]));
        assert_eq!(scheduler.in_flight_count(), 1);
        scheduler.mark_verified(first[0].chunk).expect("verify");
        assert_eq!(scheduler.in_flight_count(), 0);
        // The freed slot is usable for the next chunk.
        assert_eq!(scheduler.next_assignments(&peers(&["p1"])).len(), 1);
    }

    #[test]
    fn a_released_chunk_returns_to_the_pool() {
        let mut scheduler = Scheduler::new(4).with_limits(1, 1);
        let assigned = scheduler.next_assignments(&peers(&["p1"]));
        scheduler.release(assigned[0].chunk, "p1");
        assert_eq!(scheduler.in_flight_count(), 0);
        assert_eq!(scheduler.remaining(), 4, "a release must not verify");
    }

    #[test]
    fn an_out_of_range_verification_is_refused() {
        let mut scheduler = Scheduler::new(2);
        assert!(matches!(
            scheduler.mark_verified(5),
            Err(DistributionError::ChunkIndexOutOfRange { index: 5, total: 2 })
        ));
    }

    #[test]
    fn a_slow_single_holder_tail_is_hedged_at_most_once() {
        let mut scheduler = Scheduler::new(1).with_limits(4, 4);
        let mut availability = BTreeMap::new();
        availability.insert(0, 1);
        scheduler.set_availability(&availability);

        // p1 takes the only chunk.
        let first = scheduler.next_assignments(&peers(&["p1"]));
        assert_eq!(first.len(), 1);
        assert!(!first[0].duplicate);

        // p2 arrives while p1 still holds it: hedge.
        let hedge = scheduler.next_assignments(&peers(&["p2"]));
        assert_eq!(hedge.len(), 1);
        assert!(hedge[0].duplicate);
        assert_eq!(scheduler.duplicate_requests(), 1);

        // p3 must not add a third copy.
        assert!(scheduler.next_assignments(&peers(&["p3"])).is_empty());
    }

    #[test]
    fn an_abundant_chunk_is_never_hedged() {
        // One chunk per peer, so the second peer's choice is visible in isolation.
        let mut scheduler = Scheduler::new(2).with_limits(4, 1);
        let mut availability = BTreeMap::new();
        availability.insert(0, 5);
        availability.insert(1, 5);
        scheduler.set_availability(&availability);

        let first = scheduler.next_assignments(&peers(&["p1"]));
        assert_eq!(first.len(), 1);
        assert!(!first[0].duplicate);
        // The second peer must take the other chunk, not duplicate the first.
        let second = scheduler.next_assignments(&peers(&["p2"]));
        assert_eq!(second.len(), 1);
        assert!(
            !second[0].duplicate,
            "an abundant chunk must not be duplicated"
        );
        assert_eq!(scheduler.duplicate_requests(), 0);
        assert_ne!(
            first[0].chunk, second[0].chunk,
            "the two peers must work on different chunks"
        );
    }

    #[test]
    fn an_empty_ranking_assigns_nothing() {
        let mut scheduler = Scheduler::new(4);
        assert!(scheduler.next_assignments(&[]).is_empty());
        assert_eq!(scheduler.in_flight_count(), 0);
    }

    #[test]
    fn completion_is_only_reported_when_every_chunk_verified() {
        let mut scheduler = Scheduler::new(3);
        assert!(!scheduler.is_complete());
        for chunk in 0..3 {
            scheduler.mark_verified(chunk).expect("verify");
        }
        assert!(scheduler.is_complete());
        assert!(scheduler.outstanding().is_empty());
    }

    #[test]
    fn concurrency_limits_cannot_be_zero() {
        // A zero bound would silently mean "never make progress".
        let scheduler = Scheduler::new(4).with_limits(0, 0);
        assert_eq!(scheduler.max_file_concurrency, 1);
        assert_eq!(scheduler.max_peer_concurrency, 1);
    }
}
