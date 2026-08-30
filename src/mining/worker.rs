//! The hashing thread.
//!
//! Design notes, since this is the only genuinely hot code in the project:
//!   * generic over the concrete `Hasher`, so the compression function inlines
//!     into the loop and there is no dynamic dispatch per hash;
//!   * one OS thread per slot, no async, no allocation in the loop;
//!   * work is picked up by comparing a generation counter, so the common case
//!     is a single relaxed atomic load per batch instead of a mutex;
//!   * hashes are flushed to the shared counter once per batch, not per hash;
//!   * a single `u32` comparison settles all but a vanishing fraction of
//!     hashes, and the full digest is never even materialised unless it passes.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use tokio::sync::mpsc::UnboundedSender;

use super::algo::{self, Hasher};
use super::{Share, Work, WorkSlot};

/// Nonces hashed between generation checks. Large enough that the atomic loads
/// disappear into the noise, small enough that a new job is picked up in well
/// under a millisecond on any modern core.
const BATCH: u32 = 2048;

pub fn run<H: Hasher>(
    slot: Arc<WorkSlot>,
    stop: Arc<AtomicBool>,
    shares: UnboundedSender<Share>,
    index: usize,
) {
    let mut current: Option<Arc<Work>> = None;
    let mut hasher: Option<H> = None;
    let mut seen_generation = u64::MAX;
    let mut nonce: u32 = 0;
    let mut out = [0u8; 32];

    while !stop.load(Ordering::Relaxed) {
        let generation = slot.generation.load(Ordering::Acquire);
        if generation != seen_generation {
            seen_generation = generation;
            current = slot.work.lock().unwrap().clone();
            match &current {
                Some(work) => {
                    hasher = Some(H::new(&work.header));
                    nonce = work.nonce_start;
                }
                None => hasher = None,
            }
        }

        let (Some(work), Some(hasher)) = (current.as_ref(), hasher.as_mut()) else {
            // Idle: no work published yet. Ask for some and back off.
            slot.needs_work.store(true, Ordering::Release);
            std::thread::sleep(Duration::from_millis(25));
            continue;
        };

        let remaining = work.nonce_end.saturating_sub(nonce);
        if remaining == 0 {
            // Range exhausted; the stratum task hands us a fresh extranonce2
            // (globally unique, see mining::coordinator) on its next tick.
            slot.needs_work.store(true, Ordering::Release);
            std::thread::sleep(Duration::from_millis(5));
            continue;
        }
        let batch = BATCH.min(remaining);
        let target_top = algo::target_top(&work.target);
        let lanes = algo::LANES as u32;
        let mut tops = [0u32; algo::LANES];
        let mut done = 0u32;

        // Vector path: LANES nonces per step. Only the top word of each lane
        // comes back; a lane that passes is re-hashed scalar to get the full
        // digest, which happens vanishingly rarely.
        while done + lanes <= batch {
            hasher.top_lanes(nonce, &mut tops);
            for (lane, top) in tops.iter().enumerate() {
                if *top <= target_top {
                    let candidate = nonce.wrapping_add(lane as u32);
                    let confirmed = hasher.top(candidate);
                    hasher.digest(&mut out);
                    if confirmed < target_top || algo::meets_target(&out, &work.target) {
                        submit(&shares, work, candidate, &out, index);
                    }
                }
            }
            nonce = nonce.wrapping_add(lanes);
            done += lanes;
        }

        // Whatever is left over when the range does not divide evenly.
        while done < batch {
            let top = hasher.top(nonce);
            if top <= target_top {
                hasher.digest(&mut out);
                if top < target_top || algo::meets_target(&out, &work.target) {
                    submit(&shares, work, nonce, &out, index);
                }
            }
            nonce = nonce.wrapping_add(1);
            done += 1;
        }

        slot.hashes.fetch_add(batch as u64, Ordering::Relaxed);
    }
}

#[inline]
fn submit(
    shares: &UnboundedSender<Share>,
    work: &Work,
    nonce: u32,
    digest: &[u8; 32],
    index: usize,
) {
    let _ = shares.send(Share {
        job_id: work.job_id.clone(),
        extranonce2: work.extranonce2.clone(),
        ntime_hex: work.ntime_hex.clone(),
        nonce,
        difficulty: work.difficulty,
        share_difficulty: algo::hash_difficulty(digest),
        worker: index,
    });
}

/// Standalone hashrate benchmark, exercising the same loop the miner uses.
pub fn bench<H: Hasher + 'static>(threads: usize, seconds: u64) -> f64 {
    let stop = Arc::new(AtomicBool::new(false));
    let counters: Vec<Arc<AtomicU64>> = (0..threads).map(|_| Arc::new(AtomicU64::new(0))).collect();
    let mut handles = Vec::new();
    for (i, counter) in counters.iter().enumerate() {
        let stop = stop.clone();
        let counter = counter.clone();
        handles.push(std::thread::spawn(move || {
            let mut header = [0u8; 76];
            header[0] = i as u8;
            let mut hasher = H::new(&header);
            let mut nonce: u32 = 0;
            let mut sink = 0u32;
            let mut tops = [0u32; algo::LANES];
            while !stop.load(Ordering::Relaxed) {
                for _ in 0..(BATCH / algo::LANES as u32) {
                    hasher.top_lanes(nonce, &mut tops);
                    // Fold the results in so the loop cannot be optimised away.
                    for top in tops.iter() {
                        sink ^= *top;
                    }
                    nonce = nonce.wrapping_add(algo::LANES as u32);
                }
                counter.fetch_add(BATCH as u64, Ordering::Relaxed);
            }
            std::hint::black_box(sink);
        }));
    }
    let started = std::time::Instant::now();
    std::thread::sleep(Duration::from_secs(seconds));
    stop.store(true, Ordering::Relaxed);
    for handle in handles {
        let _ = handle.join();
    }
    let total: u64 = counters.iter().map(|c| c.load(Ordering::Relaxed)).sum();
    total as f64 / started.elapsed().as_secs_f64()
}

#[cfg(test)]
mod tests {
    use crate::mining::algo::{DIFF1, diff_to_target, meets_target, target_top};

    /// The worker's two-step check (top-word compare, then full compare) must
    /// agree with a plain full comparison for every hash, at any difficulty --
    /// including the fractional difficulties some pools hand out.
    #[test]
    fn prefilter_agrees_with_full_comparison() {
        let targets = [
            DIFF1,
            diff_to_target(0.002),
            diff_to_target(1.0),
            diff_to_target(1024.0),
        ];
        let mut hash = [0u8; 32];
        let mut state = 0x243f_6a88_85a3_08d3u64;
        for target in targets {
            let top = target_top(&target);
            for _ in 0..20_000 {
                // xorshift; we only need spread, not cryptographic quality.
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                for (i, byte) in state.to_le_bytes().iter().enumerate() {
                    hash[24 + i] = *byte;
                }
                // Bias the top bytes down so shares actually occur.
                hash[31] &= 0x0f;
                let hash_top = u32::from_le_bytes([hash[28], hash[29], hash[30], hash[31]]);
                let fast = hash_top <= top && (hash_top < top || meets_target(&hash, &target));
                assert_eq!(fast, meets_target(&hash, &target), "target {target:?}");
            }
        }
    }
}
