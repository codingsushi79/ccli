//! Global work coordination.
//!
//! Every hash a miner computes is only worth doing once. Within a single
//! stratum session the nonce space is partitioned across threads, but that is
//! not enough on its own: when several rigs mine the *same* pool — two coins on
//! one pool, a split rig, or simply two rigs pointed at the same host — pools
//! commonly hand the same `extranonce1` to every connection of the same
//! account. Two sessions each rolling their own extranonce2 counter would then
//! search overlapping space and throw away half the work.
//!
//! The coordinator makes the extranonce2 sequence a process-wide resource keyed
//! by the search space it actually addresses — `(pool, extranonce1)`. Sessions
//! sharing a key share one counter, so no two threads anywhere in the process
//! ever cover the same `(extranonce2, nonce)` pair.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

/// Identifies a distinct search space: the pool we are talking to and the
/// extranonce1 it assigned us.
type SpaceKey = (String, Vec<u8>);

#[derive(Default)]
pub struct WorkCoordinator {
    spaces: Mutex<HashMap<SpaceKey, Arc<SearchSpace>>>,
    issued: Arc<AtomicU64>,
    shared: Arc<AtomicUsize>,
}

impl WorkCoordinator {
    pub fn new() -> Self {
        Self::default()
    }

    /// Claim the search space for a session. Sessions that address the same
    /// space get the same allocator back, which is the whole point.
    pub fn space(&self, pool: &str, extranonce1: &[u8]) -> Arc<SearchSpace> {
        let key = (pool.to_string(), extranonce1.to_vec());
        let mut spaces = self.spaces.lock().unwrap();
        match spaces.get(&key) {
            Some(existing) => {
                existing.holders.fetch_add(1, Ordering::Relaxed);
                self.shared.fetch_add(1, Ordering::Relaxed);
                existing.clone()
            }
            None => {
                let space = Arc::new(SearchSpace {
                    // Start somewhere unpredictable so a restart does not
                    // repeat the space we already searched this block.
                    next: AtomicU64::new(
                        (chrono::Utc::now().timestamp_micros() as u64) & 0xffff_ffff,
                    ),
                    holders: AtomicUsize::new(1),
                    issued: self.issued.clone(),
                });
                spaces.insert(key, space.clone());
                space
            }
        }
    }

    /// Total work units handed out since the daemon started.
    pub fn issued(&self) -> u64 {
        self.issued.load(Ordering::Relaxed)
    }

    /// Number of distinct search spaces in play.
    pub fn spaces(&self) -> usize {
        self.spaces.lock().unwrap().len()
    }

    /// How many times a session joined a space another session already held —
    /// i.e. how often coordination actually prevented duplicated work.
    pub fn shared_claims(&self) -> usize {
        self.shared.load(Ordering::Relaxed)
    }

    /// Drop spaces nobody is mining any more.
    pub fn release(&self, space: &Arc<SearchSpace>) {
        space.holders.fetch_sub(1, Ordering::Relaxed);
        let mut spaces = self.spaces.lock().unwrap();
        spaces.retain(|_, s| s.holders.load(Ordering::Relaxed) > 0);
    }
}

pub struct SearchSpace {
    next: AtomicU64,
    holders: AtomicUsize,
    issued: Arc<AtomicU64>,
}

impl SearchSpace {
    /// Hand out the next extranonce2, encoded big-endian in `size` bytes.
    /// Unique across every caller sharing this space.
    pub fn next_extranonce2(&self, size: usize) -> Vec<u8> {
        let value = self.next.fetch_add(1, Ordering::Relaxed);
        self.issued.fetch_add(1, Ordering::Relaxed);
        encode(value, size)
    }

    pub fn holders(&self) -> usize {
        self.holders.load(Ordering::Relaxed)
    }
}

/// Big-endian encoding of `value` into `size` bytes, truncating from the left
/// when the pool asks for fewer than eight.
fn encode(value: u64, size: usize) -> Vec<u8> {
    let mut out = vec![0u8; size];
    if size == 0 {
        return out;
    }
    let bytes = value.to_be_bytes();
    let n = size.min(8);
    let start = size - n;
    out[start..].copy_from_slice(&bytes[8 - n..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn sessions_on_the_same_pool_share_one_space() {
        let coordinator = WorkCoordinator::new();
        let a = coordinator.space("pool:3333", &[1, 2, 3, 4]);
        let b = coordinator.space("pool:3333", &[1, 2, 3, 4]);
        assert!(
            Arc::ptr_eq(&a, &b),
            "same pool+extranonce1 must share a space"
        );
        assert_eq!(coordinator.spaces(), 1);
        assert_eq!(a.holders(), 2);
        assert_eq!(coordinator.shared_claims(), 1);
    }

    #[test]
    fn different_pools_get_independent_spaces() {
        let coordinator = WorkCoordinator::new();
        let a = coordinator.space("pool-a:3333", &[1, 2, 3, 4]);
        let b = coordinator.space("pool-b:3333", &[1, 2, 3, 4]);
        let c = coordinator.space("pool-a:3333", &[9, 9, 9, 9]);
        assert!(!Arc::ptr_eq(&a, &b));
        assert!(!Arc::ptr_eq(&a, &c));
        assert_eq!(coordinator.spaces(), 3);
    }

    /// The guarantee that matters: concurrent rigs sharing a pool never
    /// receive the same extranonce2.
    #[test]
    fn concurrent_rigs_never_duplicate_work() {
        let coordinator = Arc::new(WorkCoordinator::new());
        let mut handles = Vec::new();
        for _ in 0..8 {
            let coordinator = coordinator.clone();
            handles.push(std::thread::spawn(move || {
                // Every thread stands in for a separate rig on the same pool.
                let space = coordinator.space("pool:3333", &[0xab, 0xcd]);
                (0..2000)
                    .map(|_| space.next_extranonce2(8))
                    .collect::<Vec<_>>()
            }));
        }
        let mut all = HashSet::new();
        let mut total = 0;
        for handle in handles {
            for value in handle.join().unwrap() {
                total += 1;
                assert!(all.insert(value), "duplicated extranonce2 across rigs");
            }
        }
        assert_eq!(total, 8 * 2000);
        assert_eq!(all.len(), total);
        assert_eq!(coordinator.issued(), total as u64);
    }

    #[test]
    fn extranonce2_encoding_matches_the_requested_width() {
        assert_eq!(encode(0x1122_3344, 4), vec![0x11, 0x22, 0x33, 0x44]);
        assert_eq!(encode(0xff, 2), vec![0x00, 0xff]);
        assert_eq!(
            encode(0x1122_3344_5566_7788, 4),
            vec![0x55, 0x66, 0x77, 0x88]
        );
        assert_eq!(encode(1, 0), Vec::<u8>::new());
    }

    #[test]
    fn releasing_the_last_holder_drops_the_space() {
        let coordinator = WorkCoordinator::new();
        let a = coordinator.space("pool:3333", &[1]);
        let b = coordinator.space("pool:3333", &[1]);
        coordinator.release(&a);
        assert_eq!(coordinator.spaces(), 1, "still held by b");
        coordinator.release(&b);
        assert_eq!(coordinator.spaces(), 0);
    }
}
