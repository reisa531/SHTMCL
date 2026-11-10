//! Peer scoring and HTTP fallback.
//!
//! Two mechanisms live here and both exist for the same reason: a peer that
//! misbehaves must lose work share *before* it can slow the whole file down.
//!
//! - [`PeerScore`] — short-window smoothed throughput and failure rate. A single
//!   slow sample does not banish a good peer, but a sustained one does.
//! - [`FallbackGate`] — decides when peer delivery stops paying and HTTP should
//!   take over. The threshold is expressed as a ratio against a *measured*
//!   single-source baseline, never a hard-coded constant, because §5 requires the
//!   threshold to come from the same-conditions baseline.

use std::collections::BTreeMap;

use crate::distribution::error::DistributionError;

/// Smoothing factor for the exponential moving average (1/8, the conventional
/// value: responsive enough to notice a collapse, slow enough to ignore a dip).
const EMA_ALPHA: f64 = 0.125;

/// Minimum samples before a peer's score is trusted for scheduling decisions.
pub const MIN_SAMPLES: u32 = 3;

/// Failure rate above which a peer is quarantined outright.
pub const FAILURE_QUARANTINE: f64 = 0.5;

/// One peer's observed behaviour.
#[derive(Debug, Clone, PartialEq)]
pub struct PeerScore {
    pub peer_id: String,
    /// Smoothed bytes per second.
    throughput: f64,
    /// Smoothed failure ratio in `0..=1`.
    failure_rate: f64,
    samples: u32,
    consecutive_failures: u32,
    quarantined: bool,
}

impl PeerScore {
    pub fn new(peer_id: impl Into<String>) -> Self {
        Self {
            peer_id: peer_id.into(),
            throughput: 0.0,
            failure_rate: 0.0,
            samples: 0,
            consecutive_failures: 0,
            quarantined: false,
        }
    }

    /// Record one successful chunk transfer.
    pub fn record_success(&mut self, bytes: u64, elapsed_ms: u64) {
        let rate = if elapsed_ms == 0 {
            // A zero-duration transfer would produce an infinite rate, which
            // would let one cached response dominate scheduling forever.
            bytes as f64
        } else {
            bytes as f64 * 1000.0 / elapsed_ms as f64
        };
        self.throughput = if self.samples == 0 {
            rate
        } else {
            self.throughput * (1.0 - EMA_ALPHA) + rate * EMA_ALPHA
        };
        self.failure_rate = self.failure_rate * (1.0 - EMA_ALPHA);
        self.samples = self.samples.saturating_add(1);
        self.consecutive_failures = 0;
    }

    /// Record one failure (corrupt chunk, transport error, timeout).
    ///
    /// Two consecutive failures quarantine the peer: a corrupt chunk means the
    /// source cannot be trusted for this manifest, and continuing to ask it is
    /// what turns one bad peer into a stalled download.
    pub fn record_failure(&mut self) {
        self.failure_rate = self.failure_rate * (1.0 - EMA_ALPHA) + EMA_ALPHA;
        self.samples = self.samples.saturating_add(1);
        self.consecutive_failures = self.consecutive_failures.saturating_add(1);
        if self.consecutive_failures >= 2 || self.failure_rate > FAILURE_QUARANTINE {
            self.quarantined = true;
        }
    }

    pub fn throughput(&self) -> f64 {
        self.throughput
    }

    pub fn samples(&self) -> u32 {
        self.samples
    }

    pub fn is_quarantined(&self) -> bool {
        self.quarantined
    }

    /// Whether this peer may be asked for another chunk.
    pub fn is_available(&self) -> bool {
        !self.quarantined
    }

    /// Whether enough has been observed to rank this peer against others.
    pub fn is_rated(&self) -> bool {
        self.samples >= MIN_SAMPLES
    }
}

/// A pool of peers, ranked for scheduling and quarantining on failure.
#[derive(Debug, Clone, Default)]
pub struct PeerPool {
    peers: BTreeMap<String, PeerScore>,
}

impl PeerPool {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add(&mut self, peer_id: impl Into<String>) {
        let peer_id = peer_id.into();
        self.peers
            .entry(peer_id.clone())
            .or_insert_with(|| PeerScore::new(peer_id));
    }

    pub fn score(&self, peer_id: &str) -> Option<&PeerScore> {
        self.peers.get(peer_id)
    }

    pub fn len(&self) -> usize {
        self.peers.len()
    }

    pub fn is_empty(&self) -> bool {
        self.peers.is_empty()
    }

    pub fn available(&self) -> impl Iterator<Item = &PeerScore> {
        self.peers.values().filter(|peer| peer.is_available())
    }

    /// Available peers ordered by decreasing smoothed throughput.
    ///
    /// Unrated peers (fewer than [`MIN_SAMPLES`]) sort last rather than first:
    /// an unknown peer must not outrank a proven one on the strength of a single
    /// fast sample, which is exactly what a peer trying to get work would produce.
    pub fn ranked(&self) -> Vec<&PeerScore> {
        let mut peers: Vec<&PeerScore> = self.available().collect();
        peers.sort_by(|a, b| {
            b.is_rated()
                .cmp(&a.is_rated())
                .then_with(|| {
                    b.throughput()
                        .partial_cmp(&a.throughput())
                        .unwrap_or(std::cmp::Ordering::Equal)
                })
                .then_with(|| a.peer_id.cmp(&b.peer_id))
        });
        peers
    }

    pub fn record_success(&mut self, peer_id: &str, bytes: u64, elapsed_ms: u64) {
        if let Some(peer) = self.peers.get_mut(peer_id) {
            peer.record_success(bytes, elapsed_ms);
        }
    }

    pub fn record_failure(&mut self, peer_id: &str) {
        if let Some(peer) = self.peers.get_mut(peer_id) {
            peer.record_failure();
        }
    }

    /// Whether any peer may still be asked. `false` means HTTP only.
    pub fn any_available(&self) -> bool {
        self.available().next().is_some()
    }

    /// Refuse to proceed when nothing is available, so a caller cannot silently
    /// treat "no peers" as "no source".
    pub fn require_available(&self) -> Result<(), DistributionError> {
        if self.any_available() {
            Ok(())
        } else {
            Err(DistributionError::PeerNoneAvailable)
        }
    }
}

/// Decides when peer delivery has stopped paying and HTTP must take over.
///
/// The baseline is measured, and the comparison is a ratio: a fixed bytes/sec
/// threshold would be wrong on every machine whose network differs from the one
/// it was chosen on.
#[derive(Debug, Clone, PartialEq)]
pub struct FallbackGate {
    /// Throughput a plain single-source HTTP fetch achieved, in bytes/sec.
    baseline_bps: f64,
    /// Peer aggregate throughput must stay above this fraction of the baseline.
    floor_ratio: f64,
    /// Bytes transferred so far this session, for reporting.
    peer_bytes: u64,
    http_bytes: u64,
}

impl FallbackGate {
    /// `floor_ratio` defaults to 0.5: peers must be at least half of what the
    /// centre achieved, otherwise the added verification and bookkeeping is not
    /// paid for. The PoC reports the measured ratio so this can be re-chosen.
    pub fn new(baseline_bps: f64, floor_ratio: f64) -> Self {
        Self {
            baseline_bps,
            floor_ratio,
            peer_bytes: 0,
            http_bytes: 0,
        }
    }

    pub fn baseline_bps(&self) -> f64 {
        self.baseline_bps
    }

    pub fn peer_bytes(&self) -> u64 {
        self.peer_bytes
    }

    pub fn http_bytes(&self) -> u64 {
        self.http_bytes
    }

    pub fn record_peer(&mut self, bytes: u64) {
        self.peer_bytes = self.peer_bytes.saturating_add(bytes);
    }

    pub fn record_http(&mut self, bytes: u64) {
        self.http_bytes = self.http_bytes.saturating_add(bytes);
    }

    /// Whether peers are still worth using at `observed_bps`.
    ///
    /// The comparison is **strictly greater** than the floor. Exactly at the floor
    /// means peers deliver the same throughput as the centre while adding
    /// verification cost, extra memory and an upload surface — i.e. no benefit at
    /// real cost. Treating that as "worth using" would turn a break-even
    /// measurement into a positive recommendation.
    pub fn peers_worth_using(&self, observed_bps: f64) -> bool {
        if self.baseline_bps <= 0.0 {
            // No baseline measured means no evidence that peers help. Refusing
            // here keeps an unmeasured assumption from becoming a product claim.
            return false;
        }
        observed_bps > self.baseline_bps * self.floor_ratio
    }

    /// How much of the transfer came from peers, in `0..=1`.
    ///
    /// Zero when nothing has moved, which is the honest answer: a ratio of a
    /// zero-byte transfer is undefined and must not be reported as 100 %.
    pub fn peer_share(&self) -> f64 {
        let total = self.peer_bytes + self.http_bytes;
        if total == 0 {
            0.0
        } else {
            self.peer_bytes as f64 / total as f64
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fast_peer_smooths_toward_its_observed_rate() {
        let mut peer = PeerScore::new("p1");
        peer.record_success(1_000, 100); // 10 000 B/s
        assert_eq!(peer.throughput(), 10_000.0);
        peer.record_success(1_000, 100);
        // After the first sample the value is an EMA, so it stays at 10 000 when
        // the rate does not change.
        assert!((peer.throughput() - 10_000.0).abs() < 0.001);
    }

    #[test]
    fn a_slow_peer_decays_but_is_not_banished_by_one_sample() {
        let mut peer = PeerScore::new("p1");
        for _ in 0..5 {
            peer.record_success(1_000, 100);
        }
        let before = peer.throughput();
        peer.record_success(1_000, 10_000); // one very slow chunk
        assert!(peer.throughput() < before);
        assert!(peer.is_available(), "one slow sample must not quarantine");
    }

    #[test]
    fn two_consecutive_failures_quarantine_a_peer() {
        let mut peer = PeerScore::new("p1");
        peer.record_failure();
        assert!(peer.is_available());
        peer.record_failure();
        assert!(peer.is_quarantined());
        assert!(!peer.is_available());
    }

    #[test]
    fn a_success_after_a_failure_does_not_undo_quarantine() {
        // Quarantine is one-way within a manifest: a source that served a corrupt
        // chunk does not become trustworthy by serving one good one.
        let mut peer = PeerScore::new("p1");
        peer.record_failure();
        peer.record_failure();
        peer.record_success(1_000, 100);
        assert!(peer.is_quarantined());
    }

    #[test]
    fn unrated_peers_rank_below_proven_ones() {
        let mut pool = PeerPool::new();
        pool.add("proven");
        pool.add("newcomer");
        for _ in 0..MIN_SAMPLES {
            pool.record_success("proven", 1_000, 1_000); // slow but proven
        }
        // One spectacular sample from the newcomer.
        pool.record_success("newcomer", 1_000_000, 1);
        let ranked = pool.ranked();
        assert_eq!(ranked[0].peer_id, "proven");
        assert_eq!(ranked[1].peer_id, "newcomer");
    }

    #[test]
    fn quarantined_peers_leave_the_ranking() {
        let mut pool = PeerPool::new();
        pool.add("p1");
        pool.add("p2");
        pool.record_failure("p1");
        pool.record_failure("p1");
        assert_eq!(pool.ranked().len(), 1);
        assert_eq!(pool.ranked()[0].peer_id, "p2");
    }

    #[test]
    fn an_empty_pool_reports_no_available_peer() {
        let pool = PeerPool::new();
        assert!(!pool.any_available());
        assert!(matches!(
            pool.require_available(),
            Err(DistributionError::PeerNoneAvailable)
        ));
    }

    #[test]
    fn peers_are_only_worth_using_above_the_measured_floor() {
        let gate = FallbackGate::new(1_000.0, 0.5);
        assert!(gate.peers_worth_using(600.0));
        assert!(!gate.peers_worth_using(400.0));
        // Exactly at the floor is break-even with extra cost on top, so it does
        // not qualify. This is the case a PoC run actually hit, and calling it a
        // pass would have turned a tie into a benefit claim.
        assert!(!gate.peers_worth_using(500.0));
    }

    #[test]
    fn an_unmeasured_baseline_never_enables_peers() {
        // No baseline means no evidence; the gate must not guess in favour of
        // the feature it is supposed to evaluate.
        let gate = FallbackGate::new(0.0, 0.5);
        assert!(!gate.peers_worth_using(1_000_000.0));
    }

    #[test]
    fn peer_share_is_zero_before_any_bytes_move() {
        let gate = FallbackGate::new(1_000.0, 0.5);
        assert_eq!(gate.peer_share(), 0.0);
    }

    #[test]
    fn peer_share_is_the_peer_fraction_of_moved_bytes() {
        let mut gate = FallbackGate::new(1_000.0, 0.5);
        gate.record_peer(300);
        gate.record_http(700);
        assert!((gate.peer_share() - 0.3).abs() < 1e-9);
    }
}
