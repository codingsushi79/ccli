//! Mining engine: one `RigRuntime` per pool connection, each owning its own
//! stratum session and pool of hashing threads. Rigs are fully independent, so
//! multi-mining is just "run more than one".

pub mod algo;
pub mod coordinator;
#[cfg(target_arch = "x86_64")]
pub mod sha256_avx2;
pub mod stratum;
pub mod worker;

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use crate::config::ResolvedTarget;
use crate::log::LogSink;
use crate::mining::coordinator::WorkCoordinator;
use crate::model::{RigState, RigStatus};

/// A unit of work handed to a single hashing thread: a header prefix plus the
/// nonce sub-range that thread owns.
#[derive(Clone)]
pub struct Work {
    pub job_id: String,
    pub extranonce2: Vec<u8>,
    pub ntime_hex: String,
    pub header: [u8; 76],
    pub target: [u8; 32],
    pub difficulty: f64,
    pub nonce_start: u32,
    pub nonce_end: u32,
}

/// A hash that met the pool target, on its way to `mining.submit`.
pub struct Share {
    pub job_id: String,
    pub extranonce2: Vec<u8>,
    pub ntime_hex: String,
    pub nonce: u32,
    pub difficulty: f64,
    pub share_difficulty: f64,
    pub worker: usize,
}

/// The handoff point between the stratum task and one hashing thread.
/// Publishing new work is a lock + a generation bump; the thread checks the
/// generation once per batch, so the hot loop never touches the mutex.
pub struct WorkSlot {
    pub generation: AtomicU64,
    pub work: Mutex<Option<Arc<Work>>>,
    pub needs_work: AtomicBool,
    pub hashes: AtomicU64,
}

impl Default for WorkSlot {
    fn default() -> Self {
        Self {
            generation: AtomicU64::new(0),
            work: Mutex::new(None),
            needs_work: AtomicBool::new(true),
            hashes: AtomicU64::new(0),
        }
    }
}

impl WorkSlot {
    pub fn publish(&self, work: Arc<Work>) {
        *self.work.lock().unwrap() = Some(work);
        self.needs_work.store(false, Ordering::Release);
        self.generation.fetch_add(1, Ordering::Release);
    }

    pub fn clear(&self) {
        *self.work.lock().unwrap() = None;
        self.needs_work.store(true, Ordering::Release);
        self.generation.fetch_add(1, Ordering::Release);
    }
}

/// Live counters for one rig. Everything the TUI shows is read out of here, so
/// it is all atomic and lock-light.
pub struct RigStats {
    pub state: AtomicUsize,
    pub hashes_total: AtomicU64,
    pub accepted: AtomicU64,
    pub rejected: AtomicU64,
    pub stale: AtomicU64,
    pub reconnects: AtomicU64,
    /// f64 bit patterns.
    pub hashrate: AtomicU64,
    pub difficulty: AtomicU64,
    pub best_share: AtomicU64,
    pub latency_ms: AtomicU64,
    pub threads: AtomicUsize,
    pub last_share_at: AtomicI64,
    pub connected_at: AtomicI64,
    pub job_id: Mutex<String>,
    pub last_error: Mutex<Option<String>>,
    pub history: Mutex<VecDeque<u64>>,
}

impl Default for RigStats {
    fn default() -> Self {
        Self {
            state: AtomicUsize::new(RigState::Stopped.as_u8() as usize),
            hashes_total: AtomicU64::new(0),
            accepted: AtomicU64::new(0),
            rejected: AtomicU64::new(0),
            stale: AtomicU64::new(0),
            reconnects: AtomicU64::new(0),
            hashrate: AtomicU64::new(0),
            difficulty: AtomicU64::new(0),
            best_share: AtomicU64::new(0),
            latency_ms: AtomicU64::new(0),
            threads: AtomicUsize::new(0),
            last_share_at: AtomicI64::new(0),
            connected_at: AtomicI64::new(0),
            job_id: Mutex::new(String::new()),
            last_error: Mutex::new(None),
            history: Mutex::new(VecDeque::new()),
        }
    }
}

impl RigStats {
    pub fn set_state(&self, s: RigState) {
        self.state.store(s.as_u8() as usize, Ordering::Relaxed);
    }
    pub fn state(&self) -> RigState {
        RigState::from_u8(self.state.load(Ordering::Relaxed) as u8)
    }
    pub fn set_hashrate(&self, hs: f64) {
        self.hashrate.store(hs.to_bits(), Ordering::Relaxed);
    }
    pub fn hashrate(&self) -> f64 {
        f64::from_bits(self.hashrate.load(Ordering::Relaxed))
    }
    pub fn set_difficulty(&self, d: f64) {
        self.difficulty.store(d.to_bits(), Ordering::Relaxed);
    }
    pub fn difficulty(&self) -> f64 {
        f64::from_bits(self.difficulty.load(Ordering::Relaxed))
    }
    pub fn best_share(&self) -> f64 {
        f64::from_bits(self.best_share.load(Ordering::Relaxed))
    }
    pub fn record_best(&self, d: f64) {
        let mut cur = self.best_share.load(Ordering::Relaxed);
        loop {
            if f64::from_bits(cur) >= d {
                return;
            }
            match self.best_share.compare_exchange_weak(
                cur,
                d.to_bits(),
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => return,
                Err(actual) => cur = actual,
            }
        }
    }
    pub fn set_error(&self, msg: Option<String>) {
        *self.last_error.lock().unwrap() = msg;
    }
}

/// Everything one rig needs at runtime. Shared between the daemon, the stratum
/// task and the hashing threads.
pub struct RigRuntime {
    /// Unique id: the rig name, or `rig/COIN` for one coin of a multi-coin rig.
    pub name: String,
    /// Owning rig, which may run several of these at once.
    pub group: String,
    pub coin: String,
    pub user: String,
    pub pass: String,
    pub host_port: String,
    pub algo: &'static dyn algo::Algorithm,
    pub stats: RigStats,
    pub stop: AtomicBool,
    pub threads_wanted: AtomicUsize,
    pub log: LogSink,
    pub history_len: usize,
    /// Shared with every other rig so no two ever search the same space.
    pub coordinator: Arc<WorkCoordinator>,
}

impl RigRuntime {
    pub fn new(
        target: &ResolvedTarget,
        algo: &'static dyn algo::Algorithm,
        log: LogSink,
        threads: usize,
        history_len: usize,
        coordinator: Arc<WorkCoordinator>,
    ) -> Self {
        Self {
            name: target.id.clone(),
            group: target.group.clone(),
            coin: target.coin.clone(),
            user: target.user.clone(),
            pass: target.pass.clone(),
            host_port: target.host_port.clone(),
            algo,
            stats: RigStats::default(),
            stop: AtomicBool::new(false),
            threads_wanted: AtomicUsize::new(threads.max(1)),
            log,
            history_len,
            coordinator,
        }
    }

    pub fn stopping(&self) -> bool {
        self.stop.load(Ordering::Relaxed)
    }

    /// Sample the hashrate. Called once per tick by the daemon; `elapsed` is
    /// the time since the previous call and `delta` the hashes done since.
    pub fn sample(&self, delta: u64, elapsed_secs: f64) {
        let instant = if elapsed_secs > 0.0 {
            delta as f64 / elapsed_secs
        } else {
            0.0
        };
        // Exponential smoothing keeps the sparkline readable without lagging.
        let prev = self.hashrate_or_zero();
        let smoothed = if prev > 0.0 && self.stats.state() == RigState::Mining {
            prev * 0.7 + instant * 0.3
        } else {
            instant
        };
        self.stats.set_hashrate(smoothed);
        let mut hist = self.stats.history.lock().unwrap();
        if hist.len() >= self.history_len {
            hist.pop_front();
        }
        hist.push_back(smoothed as u64);
    }

    fn hashrate_or_zero(&self) -> f64 {
        let v = self.stats.hashrate();
        if v.is_finite() { v } else { 0.0 }
    }

    pub fn status(&self, enabled: bool) -> RigStatus {
        let now = chrono::Utc::now().timestamp();
        let history: Vec<u64> = self.stats.history.lock().unwrap().iter().copied().collect();
        let avg = if history.is_empty() {
            0.0
        } else {
            history.iter().map(|v| *v as f64).sum::<f64>() / history.len() as f64
        };
        let last_share = self.stats.last_share_at.load(Ordering::Relaxed);
        let connected = self.stats.connected_at.load(Ordering::Relaxed);
        let latency = self.stats.latency_ms.load(Ordering::Relaxed);
        RigStatus {
            node: String::new(),
            name: self.name.clone(),
            group: self.group.clone(),
            coin: self.coin.clone(),
            enabled,
            state: self.stats.state(),
            algo: self.algo.id().to_string(),
            pool: self.host_port.clone(),
            user: self.user.clone(),
            threads: self.stats.threads.load(Ordering::Relaxed),
            hashrate: self.hashrate_or_zero(),
            hashrate_avg: avg,
            history,
            hashes_total: self.stats.hashes_total.load(Ordering::Relaxed),
            accepted: self.stats.accepted.load(Ordering::Relaxed),
            rejected: self.stats.rejected.load(Ordering::Relaxed),
            stale: self.stats.stale.load(Ordering::Relaxed),
            difficulty: self.stats.difficulty(),
            best_share: self.stats.best_share(),
            job_id: self.stats.job_id.lock().unwrap().clone(),
            last_share_secs: if last_share > 0 {
                Some((now - last_share).max(0) as u64)
            } else {
                None
            },
            uptime_secs: if connected > 0 {
                (now - connected).max(0) as u64
            } else {
                0
            },
            latency_ms: if latency > 0 { Some(latency) } else { None },
            reconnects: self.stats.reconnects.load(Ordering::Relaxed),
            last_error: self.stats.last_error.lock().unwrap().clone(),
        }
    }
}
