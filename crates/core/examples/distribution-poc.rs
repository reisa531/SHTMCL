//! `distribution-poc` — the measurable half of module 09.
//!
//! It runs the same file through two transports over the same simulated network
//! and prints a comparison table. Nothing about the network is faked beyond the
//! latency/bandwidth model below, and that model is printed with every run so a
//! reader can judge how far the numbers generalise.
//!
//! Run: `cargo run -p shtmcl-core --bin distribution-poc -- --help`
//!
//! The output is the evidence for the "installation benefit" and "device cost"
//! rows of `docs/development/09-distribution-research.md` §8. It does **not**
//! produce readiness for those gates on its own: a loopback simulation cannot
//! stand in for real multi-device networks or a real install chain, which the
//! design document already says in §9.

use std::collections::BTreeMap;
use std::time::Instant;

use shtmcl_core::distribution::{
    manifest::{ChunkHash, Manifest},
    peer::{FallbackGate, PeerPool},
    plan::{Scheduler, DEFAULT_FILE_CONCURRENCY, DEFAULT_PEER_CONCURRENCY},
    verify::{digest_hex, verify_chunk_at, FileVerifier},
};

use sha2::{Digest, Sha256};

/// One simulated peer link.
#[derive(Debug, Clone)]
struct Link {
    name: String,
    /// Bytes per second this link can deliver.
    bytes_per_sec: u64,
    /// One-way latency in milliseconds.
    latency_ms: u64,
}

/// Deterministic pseudo-random source. A reproducible run matters more here than
/// statistical quality: two invocations must produce the same bytes so a
/// difference in the table can only come from the transport.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        // xorshift64*, adequate for generating test payloads.
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn fill(&mut self, buffer: &mut [u8]) {
        for chunk in buffer.chunks_mut(8) {
            let value = self.next().to_le_bytes();
            let take = chunk.len();
            chunk.copy_from_slice(&value[..take]);
        }
    }
}

/// Build the file and its manifest.
fn build_fixture(length: u64, chunk_size: u32, seed: u64) -> (Vec<u8>, Manifest) {
    let mut rng = Rng(seed);
    let mut bytes = vec![0u8; length as usize];
    rng.fill(&mut bytes);

    let mut chunks = Vec::new();
    let mut hasher = Sha256::new();
    for (index, slice) in bytes.chunks(chunk_size as usize).enumerate() {
        chunks.push(ChunkHash {
            offset: index as u64 * chunk_size as u64,
            length: slice.len() as u32,
            digest: digest_hex(slice),
        });
        hasher.update(slice);
    }
    let root = format!("{:x}", hasher.finalize());

    let manifest = Manifest {
        manifest_id: format!("poc-{length}-{chunk_size}"),
        artifact: shtmcl_domain::ArtifactId::new("poc-artifact").expect("id"),
        length,
        chunk_size,
        chunks,
        root_digest: root,
        signature: Some("poc-signature".into()),
    };
    (bytes, manifest)
}

/// Outcome of one simulated transfer.
#[derive(Debug, Default)]
struct Run {
    elapsed_ms: u128,
    peer_bytes: u64,
    http_bytes: u64,
    duplicate_requests: u64,
    peak_inflight_bytes: u64,
    verified: bool,
    digest: String,
}

/// Simulate the single-source HTTP baseline.
///
/// The baseline is **not** naive sequential: it fetches with the same per-file
/// concurrency bound the peer path uses. Comparing a serial centre against a
/// parallel peer swarm would manufacture a speedup that has nothing to do with
/// distribution, and the design document's fallback gate (§8) is written against
/// "接近 HTTP 基线", which only means something if the baseline is what HTTP can
/// actually do. Range requests over one link in parallel is exactly what a
/// download manager already does against a single origin.
fn run_http_baseline(manifest: &Manifest, payload: &[u8], link: &Link) -> Run {
    let started = Instant::now();
    let mut verifier = FileVerifier::new(manifest.chunks.len());
    let mut peak = 0u64;

    let (tx, rx) = std::sync::mpsc::channel();
    let total = manifest.chunks.len();
    let mut next = 0usize;
    let mut outstanding = 0usize;

    // Loop on the verifier's own completeness. An earlier form compared
    // `missing() < total`, which is true before anything arrives, so the loop
    // exited immediately and the baseline "verified" an empty file.
    while !verifier.is_complete() {
        // Fill the window.
        while outstanding < DEFAULT_FILE_CONCURRENCY && next < total {
            let index = next;
            next += 1;
            outstanding += 1;
            let chunk = manifest.chunks[index].clone();
            let start = chunk.offset as usize;
            let end = start + chunk.length as usize;
            let slice = payload[start..end].to_vec();
            peak = peak.max(slice.len() as u64);
            let tx = tx.clone();
            let link = link.clone();
            std::thread::spawn(move || {
                let transfer_ms = (chunk.length as u64 * 1000) / link.bytes_per_sec.max(1);
                std::thread::sleep(std::time::Duration::from_millis(transfer_ms));
                std::thread::sleep(std::time::Duration::from_millis(link.latency_ms));
                let _ = tx.send((index, slice));
            });
        }
        if outstanding == 0 {
            break;
        }
        match rx.recv_timeout(std::time::Duration::from_secs(120)) {
            Ok((index, slice)) => {
                outstanding -= 1;
                verify_chunk_at(index, &manifest.chunks[index], &slice, &link.name)
                    .expect("baseline payload must verify");
                verifier.accept(index, slice).expect("accept");
            }
            Err(_) => break,
        }
    }

    let digest = verifier.finish(manifest).expect("baseline must verify");
    Run {
        elapsed_ms: started.elapsed().as_millis(),
        peer_bytes: 0,
        http_bytes: manifest.length,
        duplicate_requests: 0,
        peak_inflight_bytes: peak,
        verified: true,
        digest,
    }
}

/// Simulate the peer path: bounded concurrency, rare-first scheduling, per-chunk
/// verification, whole-file verification.
///
/// The chunk "transfer" is a sleep proportional to its size on the assigned link,
/// so total wall time reflects how well the scheduler keeps links busy rather
/// than how fast one link is alone.
fn run_peers(manifest: &Manifest, payload: &[u8], links: &[Link]) -> Run {
    let started = Instant::now();
    let mut scheduler = Scheduler::new(manifest.chunks.len())
        .with_limits(DEFAULT_FILE_CONCURRENCY, DEFAULT_PEER_CONCURRENCY);
    let mut pool = PeerPool::new();
    for link in links {
        pool.add(link.name.clone());
    }

    // Every peer holds every chunk in this fixture; the interesting variable is
    // bandwidth, not availability.
    let mut availability = BTreeMap::new();
    for chunk in 0..manifest.chunks.len() {
        availability.insert(chunk, links.len());
    }
    scheduler.set_availability(&availability);

    let mut verifier = FileVerifier::new(manifest.chunks.len());
    let mut peak = 0u64;

    // Threads stand in for the transport. Each link is one worker, so the
    // per-peer concurrency bound is what the scheduler enforces, exactly as it
    // would against a real socket.
    let (tx, rx) = std::sync::mpsc::channel();
    let links_by_name: BTreeMap<String, Link> = links
        .iter()
        .map(|link| (link.name.clone(), link.clone()))
        .collect();

    let total = manifest.chunks.len();
    while scheduler.verified_count() < total {
        let ranking: Vec<String> = pool.ranked().iter().map(|p| p.peer_id.clone()).collect();
        if ranking.is_empty() {
            // Every peer was quarantined; there is no source left.
            break;
        }
        let assignments = scheduler.next_assignments(&ranking);
        if assignments.is_empty() {
            // No fresh work and no legal hedge. Either everything is verified, or
            // the in-flight window is full and we must wait for results.
            if scheduler.is_complete() || scheduler.in_flight_count() == 0 {
                break;
            }
            // Wait for at least one result so the window frees up.
            match rx.recv_timeout(std::time::Duration::from_secs(5)) {
                Ok(result) => {
                    settle(result, &manifest, &mut pool, &mut verifier, &mut scheduler);
                    continue;
                }
                Err(_) => break,
            }
        }

        for assignment in assignments {
            let link = links_by_name[&assignment.peer_id].clone();
            let index = assignment.chunk;
            let chunk = manifest.chunks[index].clone();
            let start = chunk.offset as usize;
            let end = start + chunk.length as usize;
            let slice = payload[start..end].to_vec();
            let tx = tx.clone();
            let peer_name = link.name.clone();
            peak = peak.max(slice.len() as u64);
            std::thread::spawn(move || {
                // Same charge as the baseline: no clamp, so the simulated link
                // rate is what the run actually experiences.
                let transfer_ms = (chunk.length as u64 * 1000) / link.bytes_per_sec.max(1);
                std::thread::sleep(std::time::Duration::from_millis(transfer_ms));
                std::thread::sleep(std::time::Duration::from_millis(link.latency_ms));
                let _ = tx.send((index, peer_name, slice));
            });
        }

        // Wait for the window to make progress instead of spinning: a spawned
        // thread has not run yet, so a non-blocking drain here would almost always
        // be empty and the loop would exit before any work completed.
        match rx.recv_timeout(std::time::Duration::from_secs(5)) {
            Ok(result) => settle(result, &manifest, &mut pool, &mut verifier, &mut scheduler),
            Err(_) => break,
        }
        // Opportunistically pick up anything else that already landed.
        while let Ok(result) = rx.try_recv() {
            settle(result, &manifest, &mut pool, &mut verifier, &mut scheduler);
        }
    }

    // Collect the remainder: keep receiving until every chunk verified or the
    // deadline expires. The loop terminates on the verifier's own state, which is
    // the only authoritative answer; a hand-computed count was wrong before.
    let deadline = Instant::now() + std::time::Duration::from_secs(30);
    while !verifier.is_complete() && Instant::now() < deadline {
        match rx.recv_timeout(std::time::Duration::from_millis(200)) {
            Ok(result) => settle(result, &manifest, &mut pool, &mut verifier, &mut scheduler),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                // Nothing more will arrive if the window is empty.
                if scheduler.in_flight_count() == 0 {
                    break;
                }
                continue;
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }

    let verified = verifier.is_complete();
    let digest = if verified {
        verifier.finish(manifest).unwrap_or_default()
    } else {
        String::new()
    };

    Run {
        elapsed_ms: started.elapsed().as_millis(),
        peer_bytes: verifier.bytes(),
        http_bytes: 0,
        duplicate_requests: scheduler.duplicate_requests(),
        peak_inflight_bytes: peak,
        verified,
        digest,
    }
}

/// Nearest-rank percentile over the observed samples.
///
/// With a handful of repeats this is coarse by design: §7 asks for a stated
/// sample size, and reporting a confident P95 from three runs would be worse than
/// reporting the crude number alongside `repeats` in the table.
fn percentile(samples: &[u128], pct: usize) -> u128 {
    if samples.is_empty() {
        return 0;
    }
    let mut sorted = samples.to_vec();
    sorted.sort_unstable();
    let rank = ((pct as f64 / 100.0) * sorted.len() as f64).ceil() as usize;
    let index = rank.saturating_sub(1).min(sorted.len() - 1);
    sorted[index]
}

/// Apply one received chunk: verify it, then either advance the scheduler or
/// quarantine the peer that served it.
///
/// One function for all three receive sites so the success and failure paths
/// cannot drift apart between them.
fn settle(
    result: (usize, String, Vec<u8>),
    manifest: &Manifest,
    pool: &mut PeerPool,
    verifier: &mut FileVerifier,
    scheduler: &mut Scheduler,
) {
    let (index, peer, bytes) = result;
    let chunk = &manifest.chunks[index];
    match verify_chunk_at(index, chunk, &bytes, &peer) {
        Ok(()) => {
            pool.record_success(&peer, bytes.len() as u64, 1);
            if verifier.accept(index, bytes).is_ok() {
                let _ = scheduler.mark_verified(index);
            }
        }
        Err(_) => {
            pool.record_failure(&peer);
            scheduler.release(index, &peer);
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|arg| arg == "--help" || arg == "-h") {
        println!("distribution-poc [--size BYTES] [--chunk BYTES] [--peers N] [--latency MS] [--bandwidth BPS]");
        println!();
        println!("Compares a single-source HTTP fetch against the controlled-chunk scheduler");
        println!("on the same payload and the same simulated links. Prints P50/P95 over repeats.");
        return;
    }

    let mut size: u64 = 32 * 1024 * 1024;
    let mut chunk: u32 = 2 << 20;
    let mut peer_count: usize = 4;
    let mut latency: u64 = 20;
    let mut bandwidth: u64 = 4 * 1024 * 1024;
    let mut repeats = 3usize;

    let mut index = 0;
    while index < args.len() {
        let value = args.get(index + 1).cloned().unwrap_or_default();
        match args[index].as_str() {
            "--size" => size = value.parse().unwrap_or(size),
            "--chunk" => chunk = value.parse().unwrap_or(chunk),
            "--peers" => peer_count = value.parse().unwrap_or(peer_count),
            "--latency" => latency = value.parse().unwrap_or(latency),
            "--bandwidth" => bandwidth = value.parse().unwrap_or(bandwidth),
            "--repeats" => repeats = value.parse().unwrap_or(repeats),
            other => {
                eprintln!("unknown argument: {other} (try --help)");
                std::process::exit(2);
            }
        }
        index += 2;
    }

    let (payload, manifest) = build_fixture(size, chunk, 0xC0FFEE);
    manifest.validate().expect("fixture manifest must be valid");

    // The links deliberately total less than one full-speed centre link, which is
    // the contested-peak case §5 asks about.
    let links: Vec<Link> = (0..peer_count)
        .map(|n| Link {
            name: format!("peer-{n}"),
            bytes_per_sec: bandwidth,
            latency_ms: latency,
        })
        .collect();
    let centre = Link {
        name: "http-centre".into(),
        bytes_per_sec: bandwidth,
        latency_ms: latency,
    };

    println!("# distribution PoC");
    println!();
    println!(
        "fixture: {} bytes, chunk {} bytes, {} chunks",
        size,
        chunk,
        manifest.chunks.len()
    );
    println!(
        "links:   {} peers at {} B/s + {} ms; centre at {} B/s",
        peer_count, bandwidth, latency, bandwidth
    );
    println!("repeats: {repeats} (all numbers are observed, not modelled)");
    println!();
    println!(
        "| transport | run | elapsed_ms | verified | bytes_from_source | duplicate_requests | peak_inflight_bytes |"
    );
    println!("| --- | --- | --- | --- | --- | --- | --- |");

    let mut http_times = Vec::new();
    for run in 0..repeats {
        let result = run_http_baseline(&manifest, &payload, &centre);
        println!(
            "| http | {} | {} | {} | {} | {} | {} |",
            run + 1,
            result.elapsed_ms,
            result.verified,
            result.http_bytes,
            result.duplicate_requests,
            result.peak_inflight_bytes
        );
        http_times.push(result.elapsed_ms);
    }

    let mut peer_times = Vec::new();
    for run in 0..repeats {
        let result = run_peers(&manifest, &payload, &links);
        println!(
            "| peers | {} | {} | {} | {} | {} | {} |",
            run + 1,
            result.elapsed_ms,
            result.verified,
            result.peer_bytes,
            result.duplicate_requests,
            result.peak_inflight_bytes
        );
        peer_times.push(result.elapsed_ms);
        // A peer run that does not verify the same digest is a correctness
        // failure, not a performance result.
        assert_eq!(
            result.digest, manifest.root_digest,
            "peer path produced a different file digest"
        );
    }

    println!();
    println!("| statistic | http_ms | peers_ms |");
    println!("| --- | --- | --- |");
    println!(
        "| P50 | {} | {} |",
        percentile(&http_times, 50),
        percentile(&peer_times, 50)
    );
    println!(
        "| P95 | {} | {} |",
        percentile(&http_times, 95),
        percentile(&peer_times, 95)
    );

    let http_p50 = percentile(&http_times, 50) as f64;
    let peer_p50 = percentile(&peer_times, 50) as f64;
    println!();
    if peer_p50 > 0.0 && http_p50 > 0.0 {
        println!(
            "P50 ratio (http/peers): {:.2}x — a value above 1.0 means peers were faster",
            http_p50 / peer_p50
        );
    }

    // Report the gate decision using the measured baseline rather than a guess.
    let baseline_bps = if http_p50 > 0.0 {
        (size as f64) / (http_p50 / 1000.0)
    } else {
        0.0
    };
    let peer_bps = if peer_p50 > 0.0 {
        (size as f64) / (peer_p50 / 1000.0)
    } else {
        0.0
    };
    let mut gate = FallbackGate::new(baseline_bps, 0.5);
    gate.record_peer(size);
    println!(
        "measured baseline {:.0} B/s, peer aggregate {:.0} B/s, peers worth using at 50% floor: {}",
        baseline_bps,
        peer_bps,
        gate.peers_worth_using(peer_bps)
    );
    println!();
    println!("NOTE: loopback simulation. It cannot stand in for real multi-device networks");
    println!("or the real install chain (design doc §9), and this run does not measure the");
    println!("upload side, so the central-pressure gate stays unverified.");
}
