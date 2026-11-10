//! Tunnel control protocol and the in-memory transport used to verify it.
//!
//! Scope of this round (stated plainly, and repeated in the handoff): the real
//! tunnel process (`apps/tunnel`) and cross-network P2P are **not** part of this
//! milestone. What is implemented and tested here is the control protocol and the
//! byte-forwarding state machine under a bounded buffer:
//!
//! - frames carry a monotonic `sequence` and a `frameId` dedup key, so a
//!   retransmitted or reordered frame can never be applied twice,
//! - the buffer is bounded (2 MiB by default) and reaching the limit **pauses the
//!   upstream read** instead of growing the queue,
//! - `transport` is never implied by a successful hole punch: only a completed
//!   open with a real path reports `direct` or `relay`.

use std::collections::{BTreeSet, VecDeque};

use serde_json::{json, Value};

use super::error::RoomsError;
use super::lease::CONNECTION_BUFFER_LIMIT;
use super::state::TunnelBinding;

pub const TUNNEL_PROTOCOL_VERSION: u32 = 1;
/// Buffer level that clears a pause: draining below half re-enables the read.
pub const BACKPRESSURE_RESUME_DIVISOR: usize = 2;
/// Bounded memory of already-applied frame ids (dedup window).
pub const DEDUP_WINDOW: usize = 256;

/// Control frames of one tunnel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TunnelFrameKind {
    /// Version + connection identity.
    Hello,
    /// One opaque candidate token (never an address).
    Candidate,
    /// The path is chosen, forwarding may start.
    Open,
    /// Credit/backpressure report so the peer stops reading.
    Backpressure,
    /// Half-close/teardown.
    Close,
}

impl TunnelFrameKind {
    pub const ALL: [Self; 5] = [
        Self::Hello,
        Self::Candidate,
        Self::Open,
        Self::Backpressure,
        Self::Close,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Hello => "hello",
            Self::Candidate => "candidate",
            Self::Open => "open",
            Self::Backpressure => "backpressure",
            Self::Close => "close",
        }
    }

    pub fn parse(value: &str) -> Result<Self, RoomsError> {
        match value {
            "hello" => Ok(Self::Hello),
            "candidate" => Ok(Self::Candidate),
            "open" => Ok(Self::Open),
            "backpressure" => Ok(Self::Backpressure),
            "close" => Ok(Self::Close),
            other => Err(RoomsError::TunnelFrameInvalid {
                detail: format!("unknown frame kind {other}"),
            }),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TunnelFrame {
    pub sequence: u64,
    pub frame_id: String,
    pub kind: TunnelFrameKind,
}

impl TunnelFrame {
    pub fn new(
        sequence: u64,
        frame_id: impl Into<String>,
        kind: TunnelFrameKind,
    ) -> Result<Self, RoomsError> {
        let frame_id = frame_id.into();
        if frame_id.is_empty() || frame_id.len() > 128 {
            return Err(RoomsError::TunnelFrameInvalid {
                detail: "frameId must be 1-128 bytes".to_string(),
            });
        }
        if sequence == 0 {
            return Err(RoomsError::TunnelFrameInvalid {
                detail: "sequence starts at 1".to_string(),
            });
        }
        Ok(Self {
            sequence,
            frame_id,
            kind,
        })
    }

    pub fn to_value(&self) -> Value {
        json!({
            "protocol": TUNNEL_PROTOCOL_VERSION,
            "sequence": self.sequence,
            "frameId": self.frame_id,
            "kind": self.kind.as_str(),
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameOutcome {
    /// In order and first time seen.
    Accepted,
    /// Already applied: duplicate `frameId`.
    Duplicate,
    /// Skipped, repeated or future sequence: the stream is not ordered.
    OutOfOrder { expected: u64, actual: u64 },
}

/// Receiver side of one tunnel: ordering plus id-based deduplication.
#[derive(Debug, Clone)]
pub struct FrameReceiver {
    next_sequence: u64,
    seen: BTreeSet<String>,
    seen_order: VecDeque<String>,
    closed: bool,
    accepted: u64,
    duplicates: u64,
    out_of_order: u64,
}

impl Default for FrameReceiver {
    fn default() -> Self {
        Self::new()
    }
}

impl FrameReceiver {
    pub fn new() -> Self {
        Self {
            next_sequence: 1,
            seen: BTreeSet::new(),
            seen_order: VecDeque::new(),
            closed: false,
            accepted: 0,
            duplicates: 0,
            out_of_order: 0,
        }
    }

    pub fn next_sequence(&self) -> u64 {
        self.next_sequence
    }

    pub fn is_closed(&self) -> bool {
        self.closed
    }

    /// Mark the stream closed without consuming a frame (used by the local side).
    pub fn mark_closed(&mut self) {
        self.closed = true;
    }

    pub fn accepted(&self) -> u64 {
        self.accepted
    }

    pub fn duplicates(&self) -> u64 {
        self.duplicates
    }

    pub fn out_of_order(&self) -> u64 {
        self.out_of_order
    }

    pub fn accept(&mut self, frame: &TunnelFrame) -> Result<FrameOutcome, RoomsError> {
        if self.closed {
            return Err(RoomsError::TunnelClosed);
        }
        // Dedup first: a retransmission of the already-applied frame is not an
        // ordering error, it is the reliable stream doing its job.
        if self.seen.contains(&frame.frame_id) {
            self.duplicates += 1;
            return Ok(FrameOutcome::Duplicate);
        }
        if frame.sequence != self.next_sequence {
            self.out_of_order += 1;
            return Ok(FrameOutcome::OutOfOrder {
                expected: self.next_sequence,
                actual: frame.sequence,
            });
        }
        self.next_sequence += 1;
        self.accepted += 1;
        self.seen.insert(frame.frame_id.clone());
        self.seen_order.push_back(frame.frame_id.clone());
        while self.seen_order.len() > DEDUP_WINDOW {
            if let Some(oldest) = self.seen_order.pop_front() {
                self.seen.remove(&oldest);
            }
        }
        if frame.kind == TunnelFrameKind::Close {
            self.closed = true;
        }
        Ok(FrameOutcome::Accepted)
    }
}

/// Bounded byte buffer with an explicit pause/resume signal.
#[derive(Debug, Clone)]
pub struct MemoryTransport {
    limit: usize,
    buffer: VecDeque<u8>,
    paused: bool,
    peak: usize,
    pushed: u64,
    dropped: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PushOutcome {
    /// Bytes accepted into the buffer this call.
    pub accepted: usize,
    /// `true` while the limit is reached: the caller must stop reading upstream.
    pub paused: bool,
    /// `true` only on the transition from reading to paused.
    pub just_paused: bool,
    /// Bytes the caller still holds because the buffer was full.
    pub rejected: usize,
}

impl MemoryTransport {
    pub fn new(limit: usize) -> Self {
        Self {
            limit: limit.max(1),
            buffer: VecDeque::new(),
            paused: false,
            peak: 0,
            pushed: 0,
            dropped: 0,
        }
    }

    pub fn with_default_limit() -> Self {
        Self::new(CONNECTION_BUFFER_LIMIT)
    }

    pub fn limit(&self) -> usize {
        self.limit
    }

    pub fn buffered(&self) -> usize {
        self.buffer.len()
    }

    pub fn capacity_left(&self) -> usize {
        self.limit.saturating_sub(self.buffer.len())
    }

    pub fn is_paused(&self) -> bool {
        self.paused
    }

    pub fn peak(&self) -> usize {
        self.peak
    }

    pub fn pushed(&self) -> u64 {
        self.pushed
    }

    pub fn dropped(&self) -> u64 {
        self.dropped
    }

    /// Accept bytes until the limit. Reaching the limit pauses the upstream read;
    /// nothing is ever queued beyond `limit`.
    pub fn push(&mut self, bytes: &[u8]) -> PushOutcome {
        let room = self.capacity_left();
        let accepted = room.min(bytes.len());
        for byte in &bytes[..accepted] {
            self.buffer.push_back(*byte);
        }
        self.pushed += accepted as u64;
        self.dropped += (bytes.len() - accepted) as u64;
        self.peak = self.peak.max(self.buffer.len());

        let was_paused = self.paused;
        if self.buffer.len() >= self.limit {
            self.paused = true;
        }
        PushOutcome {
            accepted,
            paused: self.paused,
            just_paused: self.paused && !was_paused,
            rejected: bytes.len() - accepted,
        }
    }

    /// Downstream read. Draining below half the limit resumes the upstream read.
    pub fn drain(&mut self, max: usize) -> Vec<u8> {
        let count = max.min(self.buffer.len());
        let drained: Vec<u8> = self.buffer.drain(..count).collect();
        if self.paused && self.buffer.len() <= self.limit / BACKPRESSURE_RESUME_DIVISOR {
            self.paused = false;
        }
        drained
    }

    pub fn drain_all(&mut self) -> Vec<u8> {
        self.drain(self.buffer.len())
    }
}

/// Two bounded directions plus the frame receiver of one connection.
#[derive(Debug, Clone)]
pub struct TunnelChannel {
    pub binding: TunnelBinding,
    upstream: MemoryTransport,
    downstream: MemoryTransport,
    receiver: FrameReceiver,
    out_sequence: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PumpReport {
    pub accepted: usize,
    pub rejected: usize,
    pub buffered: usize,
    /// The pump must stop reading the game socket until the buffer drains.
    pub read_paused: bool,
    /// A backpressure control frame the caller must send to the peer.
    pub backpressure_frame: Option<TunnelFrame>,
}

impl TunnelChannel {
    pub fn new(binding: TunnelBinding, limit: usize) -> Self {
        Self {
            binding,
            upstream: MemoryTransport::new(limit),
            downstream: MemoryTransport::new(limit),
            receiver: FrameReceiver::new(),
            out_sequence: 1,
        }
    }

    pub fn with_default_limit(binding: TunnelBinding) -> Self {
        Self::new(binding, CONNECTION_BUFFER_LIMIT)
    }

    pub fn upstream_buffered(&self) -> usize {
        self.upstream.buffered()
    }

    pub fn downstream_buffered(&self) -> usize {
        self.downstream.buffered()
    }

    pub fn upstream_peak(&self) -> usize {
        self.upstream.peak()
    }

    pub fn upstream_read_allowed(&self) -> bool {
        !self.upstream.is_paused() && !self.receiver.is_closed()
    }

    pub fn is_closed(&self) -> bool {
        self.receiver.is_closed()
    }

    pub fn receiver(&self) -> &FrameReceiver {
        &self.receiver
    }

    /// Local control frame with the next outbound sequence and a dedup id.
    pub fn local_frame(&mut self, kind: TunnelFrameKind) -> Result<TunnelFrame, RoomsError> {
        let sequence = self.out_sequence;
        self.out_sequence += 1;
        TunnelFrame::new(
            sequence,
            format!("{}:{}", self.binding.connection_id.as_str(), sequence),
            kind,
        )
    }

    pub fn on_frame(&mut self, frame: &TunnelFrame) -> Result<FrameOutcome, RoomsError> {
        self.receiver.accept(frame)
    }

    /// Control frames must be strictly ordered. A gap is fatal for the tunnel
    /// instead of being silently skipped, because `open`/`close` change ownership of
    /// the byte stream: `rooms.tunnel.sequence_gap` is what a real transport reports
    /// so the caller can tear the path down and renegotiate.
    pub fn on_control_frame(&mut self, frame: &TunnelFrame) -> Result<FrameOutcome, RoomsError> {
        match self.receiver.accept(frame)? {
            FrameOutcome::OutOfOrder { expected, actual } => {
                Err(RoomsError::TunnelSequenceGap { expected, actual })
            }
            other => Ok(other),
        }
    }

    /// Forward application bytes from the upstream (game) socket.
    ///
    /// When the buffer is full the accepted count is smaller than the input and
    /// `read_paused` is set: the caller must pause the upstream read instead of
    /// buffering without bound.
    pub fn pump(&mut self, incoming: &[u8]) -> Result<PumpReport, RoomsError> {
        if self.receiver.is_closed() {
            return Err(RoomsError::TunnelClosed);
        }
        if self.upstream.is_paused() {
            return Ok(PumpReport {
                accepted: 0,
                rejected: incoming.len(),
                buffered: self.upstream.buffered(),
                read_paused: true,
                backpressure_frame: None,
            });
        }
        let outcome = self.upstream.push(incoming);
        let backpressure_frame = if outcome.just_paused {
            Some(self.local_frame(TunnelFrameKind::Backpressure)?)
        } else {
            None
        };
        Ok(PumpReport {
            accepted: outcome.accepted,
            rejected: outcome.rejected,
            buffered: self.upstream.buffered(),
            read_paused: outcome.paused,
            backpressure_frame,
        })
    }

    /// Downstream consumer reads forwarded bytes; this is what clears the pause.
    pub fn drain(&mut self, max: usize) -> Vec<u8> {
        self.upstream.drain(max)
    }

    /// Peer bytes handed to the game socket (reverse direction).
    pub fn push_downstream(&mut self, bytes: &[u8]) -> PushOutcome {
        self.downstream.push(bytes)
    }

    pub fn take_downstream(&mut self, max: usize) -> Vec<u8> {
        self.downstream.drain(max)
    }

    /// Half-close: emit the closing frame and refuse further frames.
    pub fn close(&mut self) -> Result<TunnelFrame, RoomsError> {
        let frame = self.local_frame(TunnelFrameKind::Close)?;
        // The outbound sequence and the inbound sequence are independent counters,
        // so the local close is applied directly instead of through the ordering
        // check that governs peer frames.
        self.receiver.mark_closed();
        Ok(frame)
    }

    pub fn to_value(&self) -> Value {
        json!({
            "binding": self.binding.to_value(),
            "upstreamBuffered": self.upstream.buffered(),
            "upstreamLimit": self.upstream.limit(),
            "upstreamPeak": self.upstream.peak(),
            "upstreamPaused": self.upstream.is_paused(),
            "closed": self.receiver.is_closed(),
            "framesAccepted": self.receiver.accepted(),
            "framesDuplicate": self.receiver.duplicates(),
            "framesOutOfOrder": self.receiver.out_of_order(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use shtmcl_domain::Revision;

    fn channel(limit: usize) -> TunnelChannel {
        let binding = TunnelBinding::new(
            super::super::state::ConnectionId::new("connection-1").expect("id"),
            41_000,
            25_565,
            "fixture-tunnel",
            Revision::new(2),
        )
        .expect("binding");
        TunnelChannel::new(binding, limit)
    }

    #[test]
    fn frames_require_a_sequence_and_a_dedup_id() {
        assert!(TunnelFrame::new(0, "f", TunnelFrameKind::Hello).is_err());
        assert!(TunnelFrame::new(1, "", TunnelFrameKind::Hello).is_err());
        let frame = TunnelFrame::new(1, "connection-1:1", TunnelFrameKind::Hello).expect("frame");
        assert_eq!(frame.to_value()["protocol"], json!(TUNNEL_PROTOCOL_VERSION));
        assert_eq!(
            TunnelFrameKind::parse("open").expect("parse"),
            TunnelFrameKind::Open
        );
        assert!(TunnelFrameKind::parse("blast").is_err());
    }

    #[test]
    fn duplicate_frames_are_deduplicated_before_ordering_is_judged() {
        let mut receiver = FrameReceiver::new();
        let hello = TunnelFrame::new(1, "c:1", TunnelFrameKind::Hello).expect("frame");
        assert_eq!(receiver.accept(&hello).expect("ok"), FrameOutcome::Accepted);
        // Same frameId again (retransmission): duplicate, not out of order.
        assert_eq!(
            receiver.accept(&hello).expect("ok"),
            FrameOutcome::Duplicate
        );
        assert_eq!(receiver.next_sequence(), 2);

        let skip = TunnelFrame::new(4, "c:4", TunnelFrameKind::Open).expect("frame");
        assert_eq!(
            receiver.accept(&skip).expect("ok"),
            FrameOutcome::OutOfOrder {
                expected: 2,
                actual: 4
            }
        );
        assert_eq!(receiver.out_of_order(), 1);
        assert!(!receiver.is_closed());

        let close = TunnelFrame::new(2, "c:2", TunnelFrameKind::Close).expect("frame");
        assert_eq!(receiver.accept(&close).expect("ok"), FrameOutcome::Accepted);
        assert!(receiver.is_closed());
        assert_eq!(
            receiver
                .accept(&TunnelFrame::new(3, "c:3", TunnelFrameKind::Candidate).expect("frame"))
                .expect_err("closed")
                .code(),
            super::super::error::codes::TUNNEL_CLOSED
        );
    }

    #[test]
    fn reaching_the_buffer_limit_pauses_the_upstream_read() {
        let mut channel = channel(16);
        let first = channel.pump(&[7u8; 10]).expect("pump");
        assert_eq!(first.accepted, 10);
        assert!(!first.read_paused);
        assert!(first.backpressure_frame.is_none());
        assert!(channel.upstream_read_allowed());

        let second = channel.pump(&[9u8; 10]).expect("pump");
        assert_eq!(second.accepted, 6, "the buffer must stop at its limit");
        assert_eq!(second.rejected, 4);
        assert!(second.read_paused);
        assert_eq!(
            second.backpressure_frame.map(|frame| frame.kind),
            Some(TunnelFrameKind::Backpressure)
        );
        assert!(!channel.upstream_read_allowed(), "upstream read is paused");
        assert_eq!(channel.upstream_buffered(), 16);
        assert_eq!(channel.upstream_peak(), 16);

        // While paused the pump consumes nothing: no unbounded growth.
        let third = channel.pump(&[1u8; 8]).expect("pump");
        assert_eq!(third.accepted, 0);
        assert_eq!(third.rejected, 8);
        assert!(third.backpressure_frame.is_none());
        assert_eq!(channel.upstream_buffered(), 16);

        // Draining below half resumes the read.
        let drained = channel.drain(12);
        assert_eq!(drained.len(), 12);
        assert!(channel.upstream_read_allowed());
        assert_eq!(channel.pump(&[3u8; 4]).expect("pump").accepted, 4);
    }

    #[test]
    fn a_closed_channel_refuses_more_bytes() {
        let mut channel = channel(64);
        let close = channel.close().expect("close");
        assert_eq!(close.kind, TunnelFrameKind::Close);
        assert!(channel.is_closed());
        assert_eq!(
            channel.pump(b"abc").expect_err("closed").code(),
            super::super::error::codes::TUNNEL_CLOSED
        );
    }
}
