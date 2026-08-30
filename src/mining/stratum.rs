//! Stratum v1 client (`mining.subscribe` / `notify` / `submit`).
//!
//! One task per rig owns the socket, builds work units from `mining.notify`,
//! hands them to the hashing threads, and submits whatever comes back. Byte
//! ordering follows the usual stratum conventions: the header is assembled
//! little-endian, prevhash arrives word-swapped, and the submitted nonce is the
//! byte-reverse of the nonce in the header.

use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::sync::mpsc;

use super::algo;
use super::{RigRuntime, Share, Work, WorkSlot};
use crate::model::RigState;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const TICK: Duration = Duration::from_millis(200);
/// No `mining.notify` for this long means the connection is wedged.
const JOB_TIMEOUT: Duration = Duration::from_secs(360);

pub fn sha256d(data: &[u8]) -> [u8; 32] {
    let first = Sha256::digest(data);
    let second = Sha256::digest(first);
    let mut out = [0u8; 32];
    out.copy_from_slice(&second);
    out
}

#[derive(Clone)]
struct Job {
    id: String,
    prevhash: Vec<u8>,
    coinb1: Vec<u8>,
    coinb2: Vec<u8>,
    merkle_branch: Vec<[u8; 32]>,
    version: [u8; 4],
    nbits: [u8; 4],
    ntime: [u8; 4],
    ntime_hex: String,
}

struct Pending {
    sent: Instant,
    share_difficulty: f64,
    /// Pool difficulty in force when the share was found, which may since have
    /// changed.
    pool_difficulty: f64,
    worker: usize,
}

/// Supervisor: keeps a session alive, with backoff, until the rig is stopped.
pub async fn run(rig: Arc<RigRuntime>) {
    let mut backoff = 1u64;
    while !rig.stopping() {
        rig.stats.set_state(RigState::Connecting);
        let started = Instant::now();
        match session(&rig).await {
            Ok(()) => {}
            Err(err) => {
                let msg = format!("{err:#}");
                rig.stats.set_error(Some(msg.clone()));
                rig.log.error(rig.name.clone(), msg);
            }
        }
        rig.stats.set_hashrate(0.0);
        rig.stats.connected_at.store(0, Ordering::Relaxed);
        rig.stats.threads.store(0, Ordering::Relaxed);
        if rig.stopping() {
            break;
        }
        // A session that lasted a while is a transient drop, not a bad config.
        if started.elapsed() > Duration::from_secs(60) {
            backoff = 1;
        }
        rig.stats.reconnects.fetch_add(1, Ordering::Relaxed);
        rig.stats.set_state(RigState::Retrying);
        rig.log
            .warn(rig.name.clone(), format!("reconnecting in {backoff}s"));
        for _ in 0..(backoff * 5) {
            if rig.stopping() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        backoff = (backoff * 2).min(30);
    }
    rig.stats.set_state(RigState::Stopped);
    rig.stats.set_hashrate(0.0);
    rig.log.info(rig.name.clone(), "rig stopped");
}

async fn session(rig: &Arc<RigRuntime>) -> Result<()> {
    rig.log
        .info(rig.name.clone(), format!("connecting to {}", rig.host_port));
    let stream = tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(&rig.host_port))
        .await
        .map_err(|_| anyhow!("connection to {} timed out", rig.host_port))?
        .with_context(|| format!("connecting to {}", rig.host_port))?;
    stream.set_nodelay(true).ok();

    let (read_half, mut writer) = stream.into_split();
    let mut lines = BufReader::new(read_half).lines();

    let mut state = SessionState::new(rig.clone());

    // Subscribe, then authorize. Some pools answer out of order, so both are
    // tracked by id.
    let subscribe_id = state.next_id();
    send(
        &mut writer,
        &json!({"id": subscribe_id, "method": "mining.subscribe",
                "params": [format!("cryptocli/{}", env!("CARGO_PKG_VERSION"))]}),
    )
    .await?;
    rig.stats.set_state(RigState::Authorizing);
    let authorize_id = state.next_id();
    send(
        &mut writer,
        &json!({"id": authorize_id, "method": "mining.authorize",
                "params": [rig.user, rig.pass]}),
    )
    .await?;

    let (share_tx, mut share_rx) = mpsc::unbounded_channel::<Share>();
    let mut ticker = tokio::time::interval(TICK);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut last_job = Instant::now();

    loop {
        tokio::select! {
            line = lines.next_line() => {
                let Some(line) = line.context("reading from pool")? else {
                    bail!("pool closed the connection");
                };
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }
                let value: Value = match serde_json::from_str(line) {
                    Ok(v) => v,
                    Err(e) => {
                        rig.log.warn(rig.name.clone(), format!("unparseable line from pool: {e}"));
                        continue;
                    }
                };
                if value.get("method").is_some() {
                    if state.handle_notification(&value, &mut writer).await? {
                        last_job = Instant::now();
                    }
                } else {
                    state.handle_response(&value, subscribe_id, authorize_id)?;
                    if state.subscribed && state.authorized && !state.workers_started {
                        state.start_workers(share_tx.clone());
                    }
                }
            }
            Some(share) = share_rx.recv() => {
                state.submit(share, &mut writer).await?;
            }
            _ = ticker.tick() => {
                if rig.stopping() {
                    return Ok(());
                }
                state.collect_hashes();
                state.refill_slots();
                state.resize_workers(share_tx.clone());
                state.expire_pending();
                if state.workers_started && last_job.elapsed() > JOB_TIMEOUT {
                    bail!("no job from pool in {}s", JOB_TIMEOUT.as_secs());
                }
            }
        }
    }
}

async fn send<W: AsyncWriteExt + Unpin>(writer: &mut W, value: &Value) -> Result<()> {
    let mut line = serde_json::to_string(value)?;
    line.push('\n');
    writer.write_all(line.as_bytes()).await?;
    writer.flush().await?;
    Ok(())
}

struct SessionState {
    rig: Arc<RigRuntime>,
    id_counter: u64,
    subscribed: bool,
    authorized: bool,
    workers_started: bool,
    extranonce1: Vec<u8>,
    extranonce2_size: usize,
    /// Claimed once the pool tells us our extranonce1; shared with any other
    /// session addressing the same search space.
    space: Option<Arc<super::coordinator::SearchSpace>>,
    difficulty: f64,
    target: [u8; 32],
    job: Option<Job>,
    slots: Vec<Arc<WorkSlot>>,
    worker_stop: Arc<AtomicBool>,
    workers: Vec<std::thread::JoinHandle<()>>,
    last_hashes: u64,
    pending: HashMap<u64, Pending>,
}

impl SessionState {
    fn new(rig: Arc<RigRuntime>) -> Self {
        Self {
            rig,
            id_counter: 0,
            subscribed: false,
            authorized: false,
            workers_started: false,
            extranonce1: Vec::new(),
            extranonce2_size: 4,
            space: None,
            difficulty: 1.0,
            target: algo::DIFF1,
            job: None,
            slots: Vec::new(),
            worker_stop: Arc::new(AtomicBool::new(false)),
            workers: Vec::new(),
            last_hashes: 0,
            pending: HashMap::new(),
        }
    }

    fn next_id(&mut self) -> u64 {
        self.id_counter += 1;
        self.id_counter
    }

    fn handle_response(
        &mut self,
        value: &Value,
        subscribe_id: u64,
        authorize_id: u64,
    ) -> Result<()> {
        let id = value.get("id").and_then(|v| v.as_u64()).unwrap_or(0);
        let error = value.get("error");
        let error_text = match error {
            Some(Value::Null) | None => None,
            Some(Value::Array(a)) => Some(
                a.get(1)
                    .and_then(|v| v.as_str())
                    .unwrap_or("pool error")
                    .to_string(),
            ),
            Some(other) => Some(other.to_string()),
        };

        if id == subscribe_id {
            if let Some(err) = error_text {
                bail!("mining.subscribe rejected: {err}");
            }
            let result = value
                .get("result")
                .and_then(|v| v.as_array())
                .ok_or_else(|| anyhow!("malformed mining.subscribe result"))?;
            let e1 = result
                .get(1)
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow!("mining.subscribe result has no extranonce1"))?;
            self.extranonce1 = hex::decode(e1).context("decoding extranonce1")?;
            self.extranonce2_size = result
                .get(2)
                .and_then(|v| v.as_u64())
                .unwrap_or(4)
                .clamp(0, 32) as usize;
            let space = self
                .rig
                .coordinator
                .space(&self.rig.host_port, &self.extranonce1);
            let holders = space.holders();
            self.space = Some(space);
            if holders > 1 {
                self.rig.log.info(
                    self.rig.name.clone(),
                    format!(
                        "sharing a work space with {} other session(s) on this pool; \
                         extranonce2 allocation is coordinated so no work is repeated",
                        holders - 1
                    ),
                );
            }
            self.subscribed = true;
            self.rig.log.info(
                self.rig.name.clone(),
                format!(
                    "subscribed (extranonce1={e1}, extranonce2 size={})",
                    self.extranonce2_size
                ),
            );
            return Ok(());
        }

        if id == authorize_id {
            if let Some(err) = error_text {
                bail!("pool rejected worker `{}`: {err}", self.rig.user);
            }
            if value.get("result") == Some(&Value::Bool(false)) {
                bail!("pool rejected worker `{}`", self.rig.user);
            }
            self.authorized = true;
            self.rig
                .stats
                .connected_at
                .store(chrono::Utc::now().timestamp(), Ordering::Relaxed);
            self.rig.stats.set_error(None);
            self.rig.log.info(
                self.rig.name.clone(),
                format!("authorized as {}", self.rig.user),
            );
            return Ok(());
        }

        // Anything else with an id we sent is a share result.
        if let Some(pending) = self.pending.remove(&id) {
            let latency = pending.sent.elapsed().as_millis() as u64;
            self.rig.stats.latency_ms.store(latency, Ordering::Relaxed);
            let accepted =
                matches!(value.get("result"), Some(Value::Bool(true))) && error_text.is_none();
            if accepted {
                self.rig.stats.accepted.fetch_add(1, Ordering::Relaxed);
                self.rig
                    .stats
                    .last_share_at
                    .store(chrono::Utc::now().timestamp(), Ordering::Relaxed);
                self.rig.stats.record_best(pending.share_difficulty);
                self.rig.log.share(
                    self.rig.name.clone(),
                    format!(
                        "accepted  share diff {:.4} / pool {:.4}  worker {}  {}ms",
                        pending.share_difficulty, pending.pool_difficulty, pending.worker, latency
                    ),
                );
            } else {
                let reason = error_text.unwrap_or_else(|| "rejected".into());
                let stale = reason.to_ascii_lowercase().contains("stale")
                    || reason.to_ascii_lowercase().contains("job not found")
                    || reason.to_ascii_lowercase().contains("duplicate");
                if stale {
                    self.rig.stats.stale.fetch_add(1, Ordering::Relaxed);
                } else {
                    self.rig.stats.rejected.fetch_add(1, Ordering::Relaxed);
                }
                self.rig
                    .log
                    .warn(self.rig.name.clone(), format!("share rejected: {reason}"));
            }
        }
        Ok(())
    }

    /// Returns true if the notification was a new job.
    async fn handle_notification<W: AsyncWriteExt + Unpin>(
        &mut self,
        value: &Value,
        writer: &mut W,
    ) -> Result<bool> {
        let method = value.get("method").and_then(|v| v.as_str()).unwrap_or("");
        let params = value.get("params").and_then(|v| v.as_array());
        match method {
            "mining.notify" => {
                let params = params.ok_or_else(|| anyhow!("mining.notify without params"))?;
                let job = parse_job(params)?;
                let clean = params.get(8).and_then(|v| v.as_bool()).unwrap_or(true);
                *self.rig.stats.job_id.lock().unwrap() = job.id.clone();
                self.job = Some(job);
                self.rig.stats.set_state(RigState::Mining);
                self.republish(clean);
                Ok(true)
            }
            "mining.set_difficulty" => {
                let d = params
                    .and_then(|p| p.first())
                    .and_then(|v| v.as_f64())
                    .unwrap_or(1.0);
                if d > 0.0 {
                    self.difficulty = d;
                    self.target = algo::diff_to_target(d * self.rig.algo.diff_multiplier());
                    self.rig.stats.set_difficulty(d);
                    self.rig
                        .log
                        .info(self.rig.name.clone(), format!("difficulty set to {d}"));
                    self.republish(false);
                }
                Ok(false)
            }
            "mining.set_extranonce" => {
                if let Some(p) = params {
                    if let Some(e1) = p.first().and_then(|v| v.as_str()) {
                        self.extranonce1 = hex::decode(e1).unwrap_or_default();
                    }
                    if let Some(size) = p.get(1).and_then(|v| v.as_u64()) {
                        self.extranonce2_size = size.clamp(0, 32) as usize;
                    }
                    self.republish(true);
                }
                Ok(false)
            }
            "client.reconnect" => {
                bail!("pool asked us to reconnect");
            }
            "client.show_message" => {
                if let Some(msg) = params.and_then(|p| p.first()).and_then(|v| v.as_str()) {
                    self.rig
                        .log
                        .info(self.rig.name.clone(), format!("pool: {msg}"));
                }
                Ok(false)
            }
            "mining.ping" => {
                let id = value.get("id").cloned().unwrap_or(Value::Null);
                send(writer, &json!({"id": id, "result": "pong", "error": null})).await?;
                Ok(false)
            }
            other => {
                self.rig
                    .log
                    .debug(self.rig.name.clone(), format!("ignoring `{other}`"));
                Ok(false)
            }
        }
    }

    fn start_workers(&mut self, shares: mpsc::UnboundedSender<Share>) {
        let threads = self.rig.threads_wanted.load(Ordering::Relaxed).max(1);
        self.spawn_workers(threads, shares);
        self.workers_started = true;
    }

    fn spawn_workers(&mut self, threads: usize, shares: mpsc::UnboundedSender<Share>) {
        self.stop_workers();
        self.worker_stop = Arc::new(AtomicBool::new(false));
        self.slots = (0..threads)
            .map(|_| Arc::new(WorkSlot::default()))
            .collect();
        self.last_hashes = 0;
        let algorithm = self.rig.algo;
        for (index, slot) in self.slots.iter().enumerate() {
            let slot = slot.clone();
            let stop = self.worker_stop.clone();
            let shares = shares.clone();
            let name = format!("{}-w{index}", self.rig.name);
            let handle = std::thread::Builder::new()
                .name(name)
                .stack_size(256 * 1024)
                .spawn(move || algorithm.run_worker(slot, stop, shares, index))
                .expect("spawning hashing thread");
            self.workers.push(handle);
        }
        self.rig.stats.threads.store(threads, Ordering::Relaxed);
        self.rig.log.info(
            self.rig.name.clone(),
            format!(
                "{threads} hashing thread{} running",
                if threads == 1 { "" } else { "s" }
            ),
        );
        self.republish(true);
    }

    fn stop_workers(&mut self) {
        if self.workers.is_empty() {
            return;
        }
        self.collect_hashes();
        self.worker_stop.store(true, Ordering::Relaxed);
        for handle in self.workers.drain(..) {
            let _ = handle.join();
        }
        self.slots.clear();
        self.rig.stats.threads.store(0, Ordering::Relaxed);
    }

    /// Apply a thread-count change requested while the rig is running.
    fn resize_workers(&mut self, shares: mpsc::UnboundedSender<Share>) {
        if !self.workers_started {
            return;
        }
        let wanted = self.rig.threads_wanted.load(Ordering::Relaxed).max(1);
        if wanted != self.slots.len() {
            self.rig.log.info(
                self.rig.name.clone(),
                format!("resizing {} -> {wanted} threads", self.slots.len()),
            );
            self.spawn_workers(wanted, shares);
        }
    }

    /// Roll per-slot counters into the rig total. Called on every tick so the
    /// hot loop only ever touches its own cache line.
    fn collect_hashes(&mut self) {
        let total: u64 = self
            .slots
            .iter()
            .map(|s| s.hashes.load(Ordering::Relaxed))
            .sum();
        let delta = total.saturating_sub(self.last_hashes);
        self.last_hashes = total;
        if delta > 0 {
            self.rig
                .stats
                .hashes_total
                .fetch_add(delta, Ordering::Relaxed);
        }
    }

    fn refill_slots(&mut self) {
        if self.job.is_none() {
            return;
        }
        let total = self.slots.len();
        for index in 0..total {
            if self.slots[index].needs_work.load(Ordering::Acquire)
                && let Some(work) = self.build_work(index, total)
            {
                self.slots[index].publish(Arc::new(work));
            }
        }
    }

    fn republish(&mut self, _clean: bool) {
        let total = self.slots.len();
        for index in 0..total {
            match self.build_work(index, total) {
                Some(work) => self.slots[index].publish(Arc::new(work)),
                None => self.slots[index].clear(),
            }
        }
    }

    fn next_extranonce2(&mut self) -> Vec<u8> {
        match &self.space {
            Some(space) => space.next_extranonce2(self.extranonce2_size),
            None => vec![0u8; self.extranonce2_size],
        }
    }

    fn build_work(&mut self, index: usize, total: usize) -> Option<Work> {
        let job = self.job.clone()?;
        let extranonce2 = self.next_extranonce2();

        // coinbase = coinb1 || extranonce1 || extranonce2 || coinb2
        let mut coinbase = Vec::with_capacity(
            job.coinb1.len() + self.extranonce1.len() + extranonce2.len() + job.coinb2.len(),
        );
        coinbase.extend_from_slice(&job.coinb1);
        coinbase.extend_from_slice(&self.extranonce1);
        coinbase.extend_from_slice(&extranonce2);
        coinbase.extend_from_slice(&job.coinb2);

        let mut merkle_root = sha256d(&coinbase);
        let mut buf = [0u8; 64];
        for branch in &job.merkle_branch {
            buf[0..32].copy_from_slice(&merkle_root);
            buf[32..64].copy_from_slice(branch);
            merkle_root = sha256d(&buf);
        }

        // 76-byte header prefix, little-endian, minus the nonce.
        let mut header = [0u8; 76];
        // Byte-order shuffles: the indices on both sides carry the meaning.
        #[allow(clippy::needless_range_loop)]
        for i in 0..4 {
            header[i] = job.version[3 - i];
        }
        for word in 0..8 {
            for byte in 0..4 {
                header[4 + word * 4 + byte] = job.prevhash[word * 4 + (3 - byte)];
            }
        }
        header[36..68].copy_from_slice(&merkle_root);
        for i in 0..4 {
            header[68 + i] = job.ntime[3 - i];
            header[72 + i] = job.nbits[3 - i];
        }

        // Split the nonce space so threads never duplicate work, even when the
        // pool gives us a zero-length extranonce2.
        let total = total.max(1) as u64;
        let span = (0x1_0000_0000u64 / total) as u32;
        let nonce_start = (index as u32).wrapping_mul(span);
        let nonce_end = if index as u64 + 1 == total {
            u32::MAX
        } else {
            nonce_start.saturating_add(span)
        };

        Some(Work {
            job_id: job.id,
            extranonce2,
            ntime_hex: job.ntime_hex,
            header,
            target: self.target,
            difficulty: self.difficulty,
            nonce_start,
            nonce_end,
        })
    }

    async fn submit<W: AsyncWriteExt + Unpin>(
        &mut self,
        share: Share,
        writer: &mut W,
    ) -> Result<()> {
        let id = self.next_id();
        // The header carries the nonce big-endian; the pool wants the reverse.
        let nonce_hex = hex::encode(share.nonce.to_le_bytes());
        let params = json!([
            self.rig.user,
            share.job_id,
            hex::encode(&share.extranonce2),
            share.ntime_hex,
            nonce_hex
        ]);
        self.pending.insert(
            id,
            Pending {
                sent: Instant::now(),
                share_difficulty: share.share_difficulty,
                pool_difficulty: share.difficulty,
                worker: share.worker,
            },
        );
        send(
            writer,
            &json!({"id": id, "method": "mining.submit", "params": params}),
        )
        .await
    }

    /// Pools occasionally never answer a submit; don't leak the entry.
    fn expire_pending(&mut self) {
        self.pending
            .retain(|_, p| p.sent.elapsed() < Duration::from_secs(120));
    }
}

impl Drop for SessionState {
    fn drop(&mut self) {
        self.stop_workers();
        if let Some(space) = self.space.take() {
            self.rig.coordinator.release(&space);
        }
    }
}

fn parse_job(params: &[Value]) -> Result<Job> {
    let s = |i: usize| -> Result<&str> {
        params
            .get(i)
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("mining.notify param {i} missing or not a string"))
    };
    let fixed = |i: usize| -> Result<[u8; 4]> {
        let bytes = hex::decode(s(i)?).with_context(|| format!("decoding notify param {i}"))?;
        if bytes.len() != 4 {
            bail!(
                "mining.notify param {i} should be 4 bytes, got {}",
                bytes.len()
            );
        }
        Ok([bytes[0], bytes[1], bytes[2], bytes[3]])
    };

    let prevhash = hex::decode(s(1)?).context("decoding prevhash")?;
    if prevhash.len() != 32 {
        bail!("prevhash should be 32 bytes, got {}", prevhash.len());
    }
    let merkle_branch = params
        .get(4)
        .and_then(|v| v.as_array())
        .ok_or_else(|| anyhow!("mining.notify has no merkle branch"))?
        .iter()
        .map(|v| {
            let raw = v
                .as_str()
                .ok_or_else(|| anyhow!("bad merkle branch entry"))?;
            let bytes = hex::decode(raw).context("decoding merkle branch")?;
            if bytes.len() != 32 {
                bail!("merkle branch entry should be 32 bytes");
            }
            let mut out = [0u8; 32];
            out.copy_from_slice(&bytes);
            Ok(out)
        })
        .collect::<Result<Vec<_>>>()?;

    Ok(Job {
        id: s(0)?.to_string(),
        prevhash,
        coinb1: hex::decode(s(2)?).context("decoding coinb1")?,
        coinb2: hex::decode(s(3)?).context("decoding coinb2")?,
        merkle_branch,
        version: fixed(5)?,
        nbits: fixed(6)?,
        ntime: fixed(7)?,
        ntime_hex: s(7)?.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merkle_root_with_no_branches_is_the_coinbase_hash() {
        let coinbase = b"coinbase";
        assert_eq!(sha256d(coinbase), sha256d(&coinbase[..]));
    }

    #[test]
    fn job_parsing_rejects_short_prevhash() {
        let params: Vec<Value> = vec![
            json!("job1"),
            json!("00ff"),
            json!(""),
            json!(""),
            json!([]),
            json!("20000000"),
            json!("1a2b3c4d"),
            json!("5f5e1000"),
            json!(true),
        ];
        assert!(parse_job(&params).is_err());
    }

    #[test]
    fn job_parsing_accepts_a_well_formed_notify() {
        let params: Vec<Value> = vec![
            json!("job1"),
            json!("00".repeat(32)),
            json!("01000000"),
            json!("ffffffff"),
            json!([]),
            json!("20000000"),
            json!("1a2b3c4d"),
            json!("5f5e1000"),
            json!(true),
        ];
        let job = parse_job(&params).unwrap();
        assert_eq!(job.id, "job1");
        assert_eq!(job.ntime_hex, "5f5e1000");
        assert_eq!(job.version, [0x20, 0, 0, 0]);
    }
}
