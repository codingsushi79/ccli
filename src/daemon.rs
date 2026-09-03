//! The long-lived half of cryptocli.
//!
//! The daemon owns every rig, the hardware sampler and the endpoint poller. It
//! is deliberately independent of any terminal: the TUI attaches over a Unix
//! socket and detaching (quit, Ctrl-C, closing the terminal) leaves mining
//! untouched. Stopping is always an explicit action.

use anyhow::{Context, Result, bail};
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;

use crate::config::{Config, EndpointConfig, ResolvedTarget, RigConfig, RigTarget, Wallet};
use crate::endpoints::{EndpointRuntime, SharedEndpoint};
use crate::hardware::HardwareMonitor;
use crate::ipc::{Request, Response};
use crate::log::LogSink;
use crate::mining::coordinator::WorkCoordinator;
use crate::mining::{RigRuntime, algo, stratum};
use crate::model::{
    DaemonInfo, HardwareSnapshot, NodeStatus, RigState, RigStatus, Snapshot, Totals, WalletView,
};
use crate::nodes::PeerNode;
use crate::paths;

pub struct Daemon {
    started: Instant,
    started_at: i64,
    log: LogSink,
    config: RwLock<Config>,
    config_error: RwLock<Option<String>>,
    rigs: RwLock<HashMap<String, Arc<RigRuntime>>>,
    order: RwLock<Vec<String>>,
    endpoints: RwLock<Vec<SharedEndpoint>>,
    hardware: Mutex<HardwareMonitor>,
    hardware_snapshot: RwLock<HardwareSnapshot>,
    history: Mutex<VecDeque<u64>>,
    rig_hash_marks: Mutex<HashMap<String, u64>>,
    last_sample: Mutex<Instant>,
    shutdown: Arc<tokio::sync::Notify>,
    shutting_down: AtomicBool,
    coordinator: Arc<WorkCoordinator>,
    peers: RwLock<Vec<Arc<PeerNode>>>,
}

impl Daemon {
    fn new(config: Config, config_error: Option<String>) -> Self {
        let log = LogSink::new(config.settings.log_lines, true);
        Self {
            started: Instant::now(),
            started_at: chrono::Utc::now().timestamp(),
            log,
            config: RwLock::new(config),
            config_error: RwLock::new(config_error),
            rigs: RwLock::new(HashMap::new()),
            order: RwLock::new(Vec::new()),
            endpoints: RwLock::new(Vec::new()),
            hardware: Mutex::new(HardwareMonitor::new()),
            hardware_snapshot: RwLock::new(HardwareSnapshot::default()),
            history: Mutex::new(VecDeque::new()),
            rig_hash_marks: Mutex::new(HashMap::new()),
            last_sample: Mutex::new(Instant::now()),
            shutdown: Arc::new(tokio::sync::Notify::new()),
            shutting_down: AtomicBool::new(false),
            coordinator: Arc::new(WorkCoordinator::new()),
            peers: RwLock::new(Vec::new()),
        }
    }

    fn settings(&self) -> crate::config::Settings {
        self.config.read().unwrap().settings.clone()
    }

    // -------------------------------------------------------------- rigs ---

    /// Thread budget split, in two stages: between rigs, then between the
    /// coins a rig mines. A rig with a fixed `threads` takes what it asks for;
    /// the rest share what's left, weighted, with a floor of one thread each.
    /// Returns a map keyed by session id (`rig` or `rig/COIN`).
    fn allocate_threads(&self, active_groups: &[String]) -> HashMap<String, usize> {
        let config = self.config.read().unwrap();
        let budget = config.settings.thread_budget();
        let wallets = config.wallets.clone();

        let mut per_group: HashMap<String, usize> = HashMap::new();
        let mut explicit = 0usize;
        let mut auto: Vec<(String, u32)> = Vec::new();
        for name in active_groups {
            let Some(rig) = config.rig(name) else {
                continue;
            };
            if rig.threads > 0 {
                explicit += rig.threads;
                per_group.insert(rig.name.clone(), rig.threads);
            } else {
                auto.push((rig.name.clone(), rig.weight.max(1)));
            }
        }
        let remaining = budget.saturating_sub(explicit).max(auto.len());
        let total_weight: u32 = auto.iter().map(|(_, w)| *w).sum::<u32>().max(1);
        for (name, weight) in &auto {
            let share = (remaining as f64 * *weight as f64 / total_weight as f64).floor() as usize;
            per_group.insert(name.clone(), share.max(1));
        }

        let mut out = HashMap::new();
        for name in active_groups {
            let Some(rig) = config.rig(name) else {
                continue;
            };
            let Ok(targets) = rig.expand(&wallets) else {
                continue;
            };
            let share = per_group.get(name).copied().unwrap_or(1);
            let weights: u32 = targets.iter().map(|t| t.weight).sum::<u32>().max(1);
            for target in &targets {
                let threads =
                    (share as f64 * target.weight as f64 / weights as f64).floor() as usize;
                out.insert(target.id.clone(), threads.max(1));
            }
        }
        out
    }

    fn rebalance(self: &Arc<Self>) {
        let mut active: Vec<String> = self
            .rigs
            .read()
            .unwrap()
            .values()
            .filter(|r| !r.stopping())
            .map(|r| r.group.clone())
            .collect();
        active.sort();
        active.dedup();
        let allocation = self.allocate_threads(&active);
        let rigs = self.rigs.read().unwrap();
        for (name, threads) in allocation {
            if let Some(rig) = rigs.get(&name) {
                rig.threads_wanted.store(threads.max(1), Ordering::Relaxed);
            }
        }
    }

    /// Resolve a name that may be a rig (all of its coins) or a single
    /// session id like `rig/LTC`.
    fn resolve(&self, name: &str) -> Result<(Vec<ResolvedTarget>, crate::config::RigConfig)> {
        let config = self.config.read().unwrap();
        if let Some(rig) = config.rig(name) {
            let targets = rig.expand(&config.wallets)?;
            return Ok((targets, rig.clone()));
        }
        for rig in &config.rigs {
            let targets = rig.expand(&config.wallets).unwrap_or_default();
            if let Some(target) = targets.into_iter().find(|t| t.id == name) {
                return Ok((vec![target], rig.clone()));
            }
        }
        bail!("no rig or coin named `{name}` in the config")
    }

    pub fn start_rig(self: &Arc<Self>, name: &str) -> Result<String> {
        let (targets, _cfg) = self.resolve(name)?;
        let history_len = self.settings().history_len;

        let mut started = Vec::new();
        let mut already = 0;
        for target in &targets {
            {
                let rigs = self.rigs.read().unwrap();
                if let Some(existing) = rigs.get(&target.id)
                    && !existing.stopping()
                    && existing.stats.state() != RigState::Stopped
                {
                    already += 1;
                    continue;
                }
            }
            let algorithm = algo::lookup(&target.algo)
                .with_context(|| format!("unknown algo `{}`", target.algo))?;
            let runtime = Arc::new(RigRuntime::new(
                target,
                algorithm,
                self.log.clone(),
                1,
                history_len,
                self.coordinator.clone(),
            ));
            self.rigs
                .write()
                .unwrap()
                .insert(target.id.clone(), runtime.clone());
            {
                let mut order = self.order.write().unwrap();
                if !order.iter().any(|n| n == &target.id) {
                    order.push(target.id.clone());
                }
            }
            self.rig_hash_marks
                .lock()
                .unwrap()
                .insert(target.id.clone(), 0);
            started.push(target.id.clone());
            tokio::spawn(stratum::run(runtime));
        }

        if started.is_empty() {
            bail!("`{name}` is already running");
        }
        self.rebalance();
        self.log
            .info("daemon", format!("started {}", started.join(", ")));
        let suffix = if already > 0 {
            format!(" ({already} already running)")
        } else {
            String::new()
        };
        Ok(if started.len() == 1 {
            format!("`{}` started{suffix}", started[0])
        } else {
            format!(
                "{} coins started: {}{suffix}",
                started.len(),
                started.join(", ")
            )
        })
    }

    pub fn stop_rig(self: &Arc<Self>, name: &str) -> Result<String> {
        let rigs = self.rigs.read().unwrap();
        let matching: Vec<Arc<RigRuntime>> = rigs
            .values()
            .filter(|r| r.name == name || r.group == name)
            .cloned()
            .collect();
        drop(rigs);
        if matching.is_empty() {
            bail!("`{name}` is not running");
        }
        let mut stopped = Vec::new();
        for rig in matching {
            if !rig.stopping() {
                rig.stop.store(true, Ordering::Relaxed);
                stopped.push(rig.name.clone());
            }
        }
        if stopped.is_empty() {
            bail!("`{name}` is already stopping");
        }
        self.log
            .info("daemon", format!("stopping {}", stopped.join(", ")));
        Ok(format!("stopping {}", stopped.join(", ")))
    }

    pub fn start_all(self: &Arc<Self>) -> String {
        let names: Vec<String> = self
            .config
            .read()
            .unwrap()
            .rigs
            .iter()
            .filter(|r| r.enabled)
            .map(|r| r.name.clone())
            .collect();
        if names.is_empty() {
            return "no enabled rigs in the config".into();
        }
        let mut started = 0;
        for name in &names {
            if self.start_rig(name).is_ok() {
                started += 1;
            }
        }
        format!("{started} of {} enabled rig(s) started", names.len())
    }

    pub fn stop_all(self: &Arc<Self>) -> String {
        let rigs = self.rigs.read().unwrap();
        let mut stopped = 0;
        for rig in rigs.values() {
            if !rig.stopping() {
                rig.stop.store(true, Ordering::Relaxed);
                stopped += 1;
            }
        }
        format!("{stopped} rig(s) stopping")
    }

    fn set_threads(self: &Arc<Self>, name: &str, threads: usize) -> Result<String> {
        let rigs = self.rigs.read().unwrap();
        let rig = rigs
            .get(name)
            .with_context(|| format!("rig `{name}` is not running"))?;
        let threads = threads.clamp(1, 4096);
        rig.threads_wanted.store(threads, Ordering::Relaxed);
        Ok(format!("rig `{name}` will use {threads} thread(s)"))
    }

    // ------------------------------------------------------------ config ---

    fn reload(self: &Arc<Self>) -> Result<String> {
        let config = Config::load()?;
        let rig_count = config.rigs.len();
        let endpoint_count = config.endpoints.len();
        self.apply_config(config);
        self.log.info("daemon", "configuration reloaded");
        Ok(format!(
            "reloaded: {rig_count} rig(s), {endpoint_count} endpoint(s)"
        ))
    }

    /// Apply a change to the config: mutate a draft, validate it, write it to
    /// disk, then swap it in. Nothing is persisted or applied unless the whole
    /// thing validates, so a bad edit from the dashboard cannot wedge mining.
    fn mutate<F>(self: &Arc<Self>, change: F) -> Result<String>
    where
        F: FnOnce(&mut Config) -> Result<String>,
    {
        let mut draft = self.config.read().unwrap().clone();
        let message = change(&mut draft)?;
        draft.validate()?;
        draft.save()?;
        self.apply_config(draft);
        self.log.info("daemon", message.clone());
        Ok(message)
    }

    /// Swap in a new config, keeping the live state of endpoints that did not
    /// change so their uptime history survives an unrelated edit.
    fn apply_config(self: &Arc<Self>, config: Config) {
        let mut endpoints = Vec::new();
        {
            let existing = self.endpoints.read().unwrap();
            for cfg in &config.endpoints {
                match existing
                    .iter()
                    .find(|e| e.cfg.name == cfg.name && e.cfg.url == cfg.url)
                {
                    Some(kept) => endpoints.push(kept.clone()),
                    None => endpoints.push(Arc::new(EndpointRuntime::new(cfg.clone()))),
                }
            }
        }
        // Reuse existing peer connections where the address and token are
        // unchanged, so an unrelated edit doesn't drop every link.
        let mut peers = Vec::new();
        {
            let existing = self.peers.read().unwrap();
            for cfg in config.nodes.iter().filter(|n| n.enabled) {
                match existing.iter().find(|p| {
                    p.cfg.name == cfg.name
                        && p.cfg.address == cfg.address
                        && p.cfg.token == cfg.token
                }) {
                    Some(kept) => peers.push(kept.clone()),
                    None => peers.push(Arc::new(PeerNode::new(cfg.clone()))),
                }
            }
        }
        *self.peers.write().unwrap() = peers;
        *self.endpoints.write().unwrap() = endpoints;
        *self.config.write().unwrap() = config;
        *self.config_error.write().unwrap() = None;
        self.prune_stale_rigs();
        self.rebalance();
    }

    /// Forget stopped sessions whose id no longer exists in the config. Ids
    /// change when a rig gains or loses a coin (`rig` becomes `rig/BTC`), and
    /// without this the old id would linger in the dashboard forever.
    fn prune_stale_rigs(self: &Arc<Self>) {
        let live: std::collections::HashSet<String> = {
            let config = self.config.read().unwrap();
            config
                .rigs
                .iter()
                .flat_map(|rig| rig.expand(&config.wallets).unwrap_or_default())
                .map(|target| target.id)
                .collect()
        };
        let mut rigs = self.rigs.write().unwrap();
        rigs.retain(|id, runtime| live.contains(id) || runtime.stats.state() != RigState::Stopped);
        let ids: std::collections::HashSet<String> = rigs.keys().cloned().collect();
        drop(rigs);
        self.order.write().unwrap().retain(|id| ids.contains(id));
        self.rig_hash_marks
            .lock()
            .unwrap()
            .retain(|id, _| ids.contains(id));
    }

    // ---------------------------------------------------------- sampling ---

    fn sample(self: &Arc<Self>) {
        let elapsed = {
            let mut last = self.last_sample.lock().unwrap();
            let elapsed = last.elapsed().as_secs_f64();
            *last = Instant::now();
            elapsed
        };

        let rigs: Vec<Arc<RigRuntime>> = self.rigs.read().unwrap().values().cloned().collect();
        let mut marks = self.rig_hash_marks.lock().unwrap();
        let mut total = 0.0;
        for rig in &rigs {
            let hashes = rig.stats.hashes_total.load(Ordering::Relaxed);
            let mark = marks.entry(rig.name.clone()).or_insert(0);
            let delta = hashes.saturating_sub(*mark);
            *mark = hashes;
            if rig.stats.state() == RigState::Mining {
                rig.sample(delta, elapsed);
            } else {
                rig.sample(0, elapsed);
            }
            total += rig.stats.hashrate();
        }
        drop(marks);

        let history_len = self.settings().history_len;
        let mut history = self.history.lock().unwrap();
        if history.len() >= history_len {
            history.pop_front();
        }
        history.push_back(total as u64);
        drop(history);

        let snapshot = self.hardware.lock().unwrap().sample();
        *self.hardware_snapshot.write().unwrap() = snapshot;
        self.prune_stale_rigs();
    }

    /// Refresh every peer. Each poll is independent, so one slow machine
    /// cannot delay the others or the local snapshot.
    async fn poll_peers(self: &Arc<Self>) {
        let peers: Vec<Arc<PeerNode>> = self.peers.read().unwrap().clone();
        for peer in peers {
            tokio::task::spawn_blocking(move || peer.poll());
        }
    }

    async fn poll_endpoints(self: &Arc<Self>) {
        let due: Vec<SharedEndpoint> = self
            .endpoints
            .read()
            .unwrap()
            .iter()
            .filter(|e| e.due())
            .cloned()
            .collect();
        for endpoint in due {
            // Claim the slot immediately so a slow check isn't started twice.
            endpoint.schedule_next();
            let log = self.log.clone();
            tokio::task::spawn_blocking(move || {
                let outcome = crate::endpoints::check(&endpoint.cfg);
                endpoint.record(outcome, &log);
            });
        }
    }

    fn check_endpoint_now(self: &Arc<Self>, name: &str) -> Result<String> {
        let endpoint = self
            .endpoints
            .read()
            .unwrap()
            .iter()
            .find(|e| e.cfg.name == name)
            .cloned()
            .with_context(|| format!("no endpoint named `{name}`"))?;
        let log = self.log.clone();
        tokio::task::spawn_blocking(move || {
            let outcome = crate::endpoints::check(&endpoint.cfg);
            endpoint.record(outcome, &log);
        });
        Ok(format!("checking `{name}`"))
    }

    // ---------------------------------------------------------- snapshot ---

    /// Snapshot of this machine only, with everything attributed to it.
    /// Peers receive exactly this over the wire.
    pub fn local_snapshot(self: &Arc<Self>) -> Snapshot {
        let config = self.config.read().unwrap().clone();
        let rigs_map = self.rigs.read().unwrap();
        let order = self.order.read().unwrap().clone();

        let mut rigs: Vec<RigStatus> = Vec::new();
        let mut seen = std::collections::HashSet::new();
        // Configured rigs first, in config order, so the list doesn't jump
        // around as rigs start and stop. A multi-coin rig contributes one row
        // per coin.
        for cfg in &config.rigs {
            for target in cfg.expand(&config.wallets).unwrap_or_default() {
                seen.insert(target.id.clone());
                match rigs_map.get(&target.id) {
                    Some(runtime) => rigs.push(runtime.status(cfg.enabled)),
                    None => rigs.push(idle_status(&target, cfg.enabled)),
                }
            }
        }
        // Then anything running that is no longer in the config.
        for name in order {
            if seen.contains(&name) {
                continue;
            }
            if let Some(runtime) = rigs_map.get(&name) {
                rigs.push(runtime.status(false));
            }
        }
        drop(rigs_map);

        let history: Vec<u64> = self.history.lock().unwrap().iter().copied().collect();
        let avg = if history.is_empty() {
            0.0
        } else {
            history.iter().map(|v| *v as f64).sum::<f64>() / history.len() as f64
        };
        let totals = Totals {
            hashrate: rigs.iter().map(|r| r.hashrate).sum(),
            hashrate_avg: avg,
            history,
            accepted: rigs.iter().map(|r| r.accepted).sum(),
            rejected: rigs.iter().map(|r| r.rejected).sum(),
            stale: rigs.iter().map(|r| r.stale).sum(),
            hashes_total: rigs.iter().map(|r| r.hashes_total).sum(),
            rigs_active: rigs.iter().filter(|r| r.state.is_live()).count(),
            rigs_total: rigs.len(),
            threads_active: rigs.iter().map(|r| r.threads).sum(),
            threads_budget: config.settings.thread_budget(),
            nodes_online: 1,
            nodes_total: 1,
            coins: coin_totals(&rigs),
            backend: algo::backend_name().to_string(),
            work_units: self.coordinator.issued(),
            work_spaces: self.coordinator.spaces(),
            shared_spaces: self.coordinator.shared_claims(),
        };

        let wallets: Vec<WalletView> = config
            .wallets
            .iter()
            .map(|w| WalletView {
                node: String::new(),
                name: w.name.clone(),
                coin: w.coin.clone(),
                address: w.address.clone(),
                label: w.label.clone(),
                rigs: config
                    .rigs
                    .iter()
                    .filter(|r| r.wallet.as_deref() == Some(w.name.as_str()))
                    .map(|r| r.name.clone())
                    .collect(),
            })
            .collect();

        let endpoints = self
            .endpoints
            .read()
            .unwrap()
            .iter()
            .map(|e| e.status())
            .collect();

        Snapshot {
            daemon: DaemonInfo {
                pid: std::process::id(),
                version: env!("CARGO_PKG_VERSION").to_string(),
                started_at: self.started_at,
                uptime_secs: self.started.elapsed().as_secs(),
                config_path: paths::config_path().display().to_string(),
                socket_path: paths::socket_path().display().to_string(),
                log_path: paths::log_path().display().to_string(),
                config_error: self.config_error.read().unwrap().clone(),
            },
            totals,
            rigs,
            hardware: self.hardware_snapshot.read().unwrap().clone(),
            endpoints,
            wallets,
            logs: self.log.tail(self.settings().log_lines),
            nodes: Vec::new(),
        }
    }

    /// The view the dashboard gets: this machine plus every peer, merged.
    pub fn snapshot(self: &Arc<Self>) -> Snapshot {
        let node_name = self.settings().node_name();
        let mut merged = self.local_snapshot();

        // Tag local rows so the dashboard can tell machines apart.
        for rig in &mut merged.rigs {
            rig.node = node_name.clone();
        }
        for endpoint in &mut merged.endpoints {
            endpoint.node = node_name.clone();
        }
        for wallet in &mut merged.wallets {
            wallet.node = node_name.clone();
        }
        for line in &mut merged.logs {
            line.node = node_name.clone();
        }

        let mut nodes = vec![NodeStatus {
            name: node_name.clone(),
            address: self
                .config
                .read()
                .unwrap()
                .settings
                .remote
                .listen
                .clone()
                .unwrap_or_else(|| "local".into()),
            local: true,
            online: true,
            version: merged.daemon.version.clone(),
            latency_ms: Some(0),
            uptime_secs: merged.daemon.uptime_secs,
            hashrate: merged.totals.hashrate,
            rigs_active: merged.totals.rigs_active,
            rigs_total: merged.totals.rigs_total,
            threads: merged.totals.threads_active,
            accepted: merged.totals.accepted,
            rejected: merged.totals.rejected,
            cpu_usage: merged.hardware.cpu_usage,
            hottest_c: merged.hardware.temps.first().map(|t| t.celsius),
            last_error: None,
        }];

        let peers: Vec<Arc<PeerNode>> = self.peers.read().unwrap().clone();
        for peer in peers {
            let status = peer.status();
            let online = status.online;
            nodes.push(status);
            let Some(remote) = peer.snapshot() else {
                continue;
            };

            for mut rig in remote.rigs {
                rig.node = peer.cfg.name.clone();
                if !online {
                    // Don't present a dead peer's last reading as current: we
                    // do not know what that machine is doing any more.
                    rig.hashrate = 0.0;
                    rig.state = RigState::Unknown;
                    rig.last_error = Some(format!("node `{}` is unreachable", peer.cfg.name));
                }
                merged.rigs.push(rig);
            }
            for mut endpoint in remote.endpoints {
                endpoint.node = peer.cfg.name.clone();
                merged.endpoints.push(endpoint);
            }
            for mut wallet in remote.wallets {
                wallet.node = peer.cfg.name.clone();
                merged.wallets.push(wallet);
            }
            for mut line in remote.logs {
                line.node = peer.cfg.name.clone();
                merged.logs.push(line);
            }
            if online {
                merged.totals.hashrate += remote.totals.hashrate;
                merged.totals.threads_active += remote.totals.threads_active;
                merged.totals.rigs_active += remote.totals.rigs_active;
            }
            merged.totals.accepted += remote.totals.accepted;
            merged.totals.rejected += remote.totals.rejected;
            merged.totals.stale += remote.totals.stale;
            merged.totals.hashes_total += remote.totals.hashes_total;
            merged.totals.rigs_total += remote.totals.rigs_total;
            merged.totals.threads_budget += remote.totals.threads_budget;
            merged.totals.work_units += remote.totals.work_units;
        }

        // Interleave logs from every machine chronologically.
        merged.logs.sort_by_key(|line| line.ts);
        let keep = self.settings().log_lines;
        if merged.logs.len() > keep {
            merged.logs.drain(..merged.logs.len() - keep);
        }

        merged.totals.coins = coin_totals(&merged.rigs);
        merged.totals.nodes_online = nodes.iter().filter(|n| n.online).count();
        merged.totals.nodes_total = nodes.len();
        merged.nodes = nodes;
        merged
    }

    /// Look up a peer by name.
    fn peer(&self, name: &str) -> Option<Arc<PeerNode>> {
        self.peers
            .read()
            .unwrap()
            .iter()
            .find(|p| p.cfg.name == name)
            .cloned()
    }

    fn is_local_node(&self, name: &str) -> bool {
        name.is_empty() || name == "local" || name == self.settings().node_name()
    }

    fn handle(self: &Arc<Self>, request: Request) -> Response {
        let result = match request {
            Request::Auth { .. } => Ok("already authenticated".to_string()),
            Request::OnNode { node, request } => {
                // Only reached for the local node; `serve` intercepts the
                // remote case so forwarding can block off the runtime.
                if self.is_local_node(&node) {
                    return self.handle(*request);
                }
                Err(anyhow::anyhow!("unknown node `{node}`"))
            }
            Request::Ping => {
                return Response::Pong {
                    pid: std::process::id(),
                    version: env!("CARGO_PKG_VERSION").to_string(),
                    uptime_secs: self.started.elapsed().as_secs(),
                };
            }
            Request::Snapshot => return Response::Snapshot(Box::new(self.snapshot())),
            Request::StartRig { name } => self.start_rig(&name),
            Request::StopRig { name } => self.stop_rig(&name),
            Request::StartAll => Ok(self.start_all()),
            Request::StopAll => Ok(self.stop_all()),
            Request::SetThreads { name, threads } => self.set_threads(&name, threads),
            Request::CheckEndpoint { name } => self.check_endpoint_now(&name),
            Request::Reload => self.reload(),
            Request::AddWallet {
                name,
                coin,
                address,
                label,
            } => self.mutate(|config| {
                if config.wallets.iter().any(|w| w.name == name) {
                    bail!("a wallet named `{name}` already exists");
                }
                crate::config::check_address(&coin, &address)?;
                config.wallets.push(Wallet {
                    name: name.clone(),
                    coin: coin.to_ascii_uppercase(),
                    address,
                    label,
                });
                Ok(format!("wallet `{name}` added"))
            }),
            Request::RemoveWallet { name } => self.mutate(|config| {
                let before = config.wallets.len();
                config.wallets.retain(|w| w.name != name);
                if config.wallets.len() == before {
                    bail!("no wallet named `{name}`");
                }
                Ok(format!("wallet `{name}` removed"))
            }),
            Request::AddRig {
                name,
                url,
                coin,
                algo,
                wallet,
                worker,
                user,
                pass,
                threads,
                weight,
            } => {
                let result = self.mutate(|config| {
                    if config.rigs.iter().any(|r| r.name == name) {
                        bail!("a rig named `{name}` already exists");
                    }
                    // `url` may name an endpoint, in which case its url and
                    // credentials come along for free.
                    let pool = config.resolve_pool(&url)?;
                    let user = user.or_else(|| pool.user.clone());
                    let pass = pool.pass.clone().unwrap_or(pass);
                    if user.is_none() && wallet.is_none() {
                        bail!("set a wallet or a stratum user so the pool knows who to pay");
                    }
                    config.rigs.push(RigConfig {
                        name: name.clone(),
                        url: pool.url,
                        algo,
                        coin: if coin.is_empty() {
                            None
                        } else {
                            Some(coin.to_ascii_uppercase())
                        },
                        user: user.unwrap_or_default(),
                        pass,
                        wallet,
                        worker,
                        threads,
                        weight,
                        enabled: true,
                        targets: Vec::new(),
                    });
                    Ok(match pool.from_endpoint {
                        Some(endpoint) => {
                            format!("rig `{name}` added from endpoint `{endpoint}`")
                        }
                        None => format!("rig `{name}` added"),
                    })
                });
                // A rig added from the dashboard should just start mining.
                if result.is_ok() {
                    let _ = self.start_rig(&name);
                }
                result
            }
            Request::AddRigCoin {
                rig,
                coin,
                url,
                wallet,
                worker,
                weight,
            } => {
                let result = self.mutate(|config| {
                    let wallets = config.wallets.clone();
                    let pool = config.resolve_pool(&url)?;
                    let target = config
                        .rigs
                        .iter_mut()
                        .find(|r| r.name == rig)
                        .with_context(|| format!("no rig named `{rig}`"))?;
                    let coin = coin.to_ascii_uppercase();
                    if target.coins().iter().any(|c| c == &coin) {
                        bail!("rig `{rig}` already mines {coin}");
                    }
                    target.targets.push(RigTarget {
                        coin: coin.clone(),
                        url: pool.url,
                        algo: None,
                        wallet,
                        worker,
                        user: pool.user,
                        pass: pool.pass,
                        weight,
                    });
                    target.expand(&wallets)?;
                    Ok(format!("rig `{rig}` now also mines {coin}"))
                });
                // Restart so the new coin gets a session and a thread share.
                if result.is_ok() {
                    let _ = self.stop_rig(&rig);
                    let daemon = self.clone();
                    let rig = rig.clone();
                    tokio::spawn(async move {
                        tokio::time::sleep(Duration::from_millis(600)).await;
                        let _ = daemon.start_rig(&rig);
                    });
                }
                result
            }
            Request::RemoveRig { name } => {
                let _ = self.stop_rig(&name);
                self.mutate(|config| {
                    let before = config.rigs.len();
                    config.rigs.retain(|r| r.name != name);
                    if config.rigs.len() == before {
                        bail!("no rig named `{name}`");
                    }
                    Ok(format!("rig `{name}` removed"))
                })
            }
            Request::SetRigEnabled { name, enabled } => self.mutate(|config| {
                let rig = config
                    .rigs
                    .iter_mut()
                    .find(|r| r.name == name)
                    .with_context(|| format!("no rig named `{name}`"))?;
                rig.enabled = enabled;
                Ok(format!(
                    "rig `{name}` {}",
                    if enabled { "enabled" } else { "disabled" }
                ))
            }),
            Request::AddEndpoint {
                name,
                url,
                method,
                interval_secs,
                timeout_secs,
                expect_status,
                headers,
                fields,
                user,
                password,
            } => self.mutate(|config| {
                if config.endpoints.iter().any(|e| e.name == name) {
                    bail!("an endpoint named `{name}` already exists");
                }
                if crate::config::endpoint_kind(&url) == crate::model::EndpointKind::Stratum {
                    crate::config::host_port(&url, "endpoint")?;
                }
                config.endpoints.push(EndpointConfig {
                    name: name.clone(),
                    url,
                    method: method.to_ascii_uppercase(),
                    headers: headers.into_iter().collect(),
                    body: None,
                    interval_secs: interval_secs.max(1),
                    timeout_secs: timeout_secs.max(1),
                    expect_status,
                    expect_body: None,
                    fields: fields.into_iter().collect(),
                    user: user.filter(|u: &String| !u.is_empty()),
                    password: password.filter(|p: &String| !p.is_empty()),
                    enabled: true,
                });
                Ok(format!("endpoint `{name}` added"))
            }),
            Request::AddNode {
                name,
                address,
                token,
                fingerprint,
            } => {
                // Verify before persisting: a node that cannot be reached is
                // almost always a typo, and a red row is a poor way to say so.
                //
                // Two dials at 6s each stay inside the client's own 30s budget
                // even when the address is a hostname with both an A and a AAAA
                // record, where every attempt is tried before giving up.
                let timeout = Duration::from_secs(6);
                let address = match crate::net::normalize_address(&address) {
                    Ok(address) => address,
                    Err(err) => {
                        return Response::Error {
                            message: format!("{err:#}"),
                        };
                    }
                };
                let pin = if fingerprint.trim().is_empty() {
                    match crate::tls::peek_fingerprint(&address, timeout) {
                        Ok(seen) => seen,
                        Err(err) => {
                            return Response::Error {
                                message: format!("{err:#}"),
                            };
                        }
                    }
                } else {
                    crate::tls::normalize_fingerprint(&fingerprint)
                };
                match crate::ipc::Client::connect_remote(&address, &token, &pin, timeout)
                    .and_then(|mut c| c.snapshot())
                {
                    Ok(_) => {}
                    Err(err) => {
                        return Response::Error {
                            message: format!("{err:#}"),
                        };
                    }
                }
                self.mutate(|config| {
                    if config.nodes.iter().any(|n| n.name == name) {
                        bail!("a node named `{name}` already exists");
                    }
                    config.nodes.push(crate::config::NodeConfig {
                        name: name.clone(),
                        address,
                        token,
                        fingerprint: pin,
                        enabled: true,
                    });
                    Ok(format!("node `{name}` added"))
                })
            }
            Request::CheckNode { name } => {
                let Some(peer) = self.peer(&name) else {
                    return Response::Error {
                        message: format!("no node named `{name}`"),
                    };
                };
                peer.poll_now();
                let status = peer.status();
                if status.online {
                    Ok(format!(
                        "node `{name}` is up — cryptocli {}, {} ms",
                        status.version,
                        status.latency_ms.unwrap_or(0)
                    ))
                } else {
                    Err(anyhow::anyhow!(status.last_error.unwrap_or_else(
                        || format!("node `{name}` is not answering")
                    )))
                }
            }
            Request::RemoveNode { name } => self.mutate(|config| {
                let before = config.nodes.len();
                config.nodes.retain(|n| n.name != name);
                if config.nodes.len() == before {
                    bail!("no node named `{name}`");
                }
                Ok(format!("node `{name}` removed"))
            }),
            Request::RemoveEndpoint { name } => self.mutate(|config| {
                let before = config.endpoints.len();
                config.endpoints.retain(|e| e.name != name);
                if config.endpoints.len() == before {
                    bail!("no endpoint named `{name}`");
                }
                Ok(format!("endpoint `{name}` removed"))
            }),
            Request::Shutdown => {
                self.log.info("daemon", "shutdown requested");
                self.shutting_down.store(true, Ordering::Relaxed);
                self.stop_all();
                self.shutdown.notify_waiters();
                Ok("daemon shutting down".to_string())
            }
        };
        match result {
            Ok(message) => Response::Ok { message },
            Err(err) => Response::Error {
                message: format!("{err:#}"),
            },
        }
    }
}

/// Aggregate every session by the coin it mines, however the rigs are split.
fn coin_totals(rigs: &[RigStatus]) -> Vec<crate::model::CoinTotals> {
    let mut out: Vec<crate::model::CoinTotals> = Vec::new();
    for rig in rigs {
        let coin = if rig.coin.is_empty() {
            "-".to_string()
        } else {
            rig.coin.clone()
        };
        match out.iter_mut().find(|c| c.coin == coin) {
            Some(entry) => {
                entry.hashrate += rig.hashrate;
                entry.accepted += rig.accepted;
                entry.rejected += rig.rejected;
                entry.sessions += 1;
                entry.threads += rig.threads;
                entry.active += usize::from(rig.state.is_live());
            }
            None => out.push(crate::model::CoinTotals {
                coin,
                hashrate: rig.hashrate,
                accepted: rig.accepted,
                rejected: rig.rejected,
                sessions: 1,
                active: usize::from(rig.state.is_live()),
                threads: rig.threads,
            }),
        }
    }
    out.sort_by(|a, b| {
        b.hashrate
            .partial_cmp(&a.hashrate)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    out
}

fn idle_status(target: &ResolvedTarget, enabled: bool) -> RigStatus {
    RigStatus {
        node: String::new(),
        name: target.id.clone(),
        group: target.group.clone(),
        coin: target.coin.clone(),
        enabled,
        state: RigState::Stopped,
        algo: target.algo.clone(),
        pool: target.host_port.clone(),
        user: target.user.clone(),
        threads: 0,
        hashrate: 0.0,
        hashrate_avg: 0.0,
        history: Vec::new(),
        hashes_total: 0,
        accepted: 0,
        rejected: 0,
        stale: 0,
        difficulty: 0.0,
        best_share: 0.0,
        job_id: String::new(),
        last_share_secs: None,
        uptime_secs: 0,
        latency_ms: None,
        reconnects: 0,
        last_error: None,
    }
}

// ------------------------------------------------------------ connections ---

/// Serve one client connection.
///
/// `authenticated` starts true for the Unix socket (reaching it already means
/// local filesystem access) and false for TCP, where the first message must be
/// a matching `Auth` before anything else is answered.
async fn serve<S>(daemon: Arc<Daemon>, stream: S, mut authenticated: bool, peer: String)
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Send + 'static,
{
    let (read_half, mut writer) = tokio::io::split(stream);
    let mut lines = BufReader::new(read_half).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let response = match serde_json::from_str::<Request>(line) {
            Ok(Request::Auth { token }) => {
                let expected = daemon
                    .config
                    .read()
                    .unwrap()
                    .settings
                    .remote
                    .token
                    .clone()
                    .unwrap_or_default();
                if !expected.is_empty() && crate::nodes::token_matches(&expected, &token) {
                    authenticated = true;
                    daemon.log.info("remote", format!("{peer} authenticated"));
                    Response::Ok {
                        message: "authenticated".into(),
                    }
                } else {
                    daemon
                        .log
                        .warn("remote", format!("{peer} presented a bad token"));
                    Response::Error {
                        message: "invalid token".into(),
                    }
                }
            }
            _ if !authenticated => Response::Error {
                message: "authenticate first".into(),
            },
            Ok(Request::OnNode { node, request }) => {
                // Forwarding talks to a peer with a blocking client, so it has
                // to happen off the async runtime.
                if daemon.is_local_node(&node) {
                    daemon.handle(*request)
                } else {
                    match daemon.peer(&node) {
                        Some(peer) => {
                            let result =
                                tokio::task::spawn_blocking(move || peer.command(&request)).await;
                            match result {
                                Ok(Ok(message)) => Response::Ok { message },
                                Ok(Err(err)) => Response::Error {
                                    message: format!("{err:#}"),
                                },
                                Err(err) => Response::Error {
                                    message: format!("forwarding failed: {err}"),
                                },
                            }
                        }
                        None => Response::Error {
                            message: format!("unknown node `{node}`"),
                        },
                    }
                }
            }
            // Adding a node makes the daemon dial the new machine, which can
            // take seconds. Doing that inline would park a runtime worker and
            // stall sampling and peer polling with it.
            Ok(request @ (Request::AddNode { .. } | Request::CheckNode { .. })) => {
                let daemon = daemon.clone();
                match tokio::task::spawn_blocking(move || daemon.handle(request)).await {
                    Ok(response) => response,
                    Err(err) => Response::Error {
                        message: format!("the daemon dropped the request: {err}"),
                    },
                }
            }
            Ok(Request::Snapshot) if !peer.is_empty() => {
                // A peer asking for a snapshot wants this machine only;
                // returning the merged view would double-count in a mesh.
                Response::Snapshot(Box::new(daemon.local_snapshot()))
            }
            Ok(request) => daemon.handle(request),
            Err(err) => Response::Error {
                message: format!("bad request: {err}"),
            },
        };
        let Ok(mut payload) = serde_json::to_string(&response) else {
            break;
        };
        payload.push('\n');
        if writer.write_all(payload.as_bytes()).await.is_err() {
            break;
        }
        let _ = writer.flush().await;
    }
}

// ------------------------------------------------------------- lifecycle ---

/// Run the daemon in this process until shutdown.
pub async fn run() -> Result<()> {
    paths::ensure_dirs()?;
    let socket = paths::socket_path();

    if crate::ipc::Client::probe() {
        bail!(
            "a cryptocli daemon is already running on {}",
            socket.display()
        );
    }
    if socket.exists() {
        std::fs::remove_file(&socket).ok();
    }

    let (config, config_error) = match Config::load() {
        Ok(c) => (c, None),
        Err(err) => (Config::default(), Some(format!("{err:#}"))),
    };
    // A daemon started implicitly by the dashboard comes up idle: opening a
    // dashboard should never be what starts mining.
    let autostart = config.settings.autostart && std::env::var("CRYPTOCLI_NO_AUTOSTART").is_err();
    let sample_interval = Duration::from_millis(config.settings.sample_interval_ms.max(100));
    let daemon = Arc::new(Daemon::new(config.clone(), config_error.clone()));
    // Build endpoints and peer links through the same path a reload uses, so
    // startup and reload can never drift apart.
    daemon.apply_config(config);
    if let Some(err) = &config_error {
        *daemon.config_error.write().unwrap() = Some(err.clone());
    }

    let listener =
        UnixListener::bind(&socket).with_context(|| format!("binding {}", socket.display()))?;
    std::fs::write(paths::pid_path(), std::process::id().to_string()).ok();

    daemon.log.info(
        "daemon",
        format!(
            "cryptocli {} listening on {} (pid {})",
            env!("CARGO_PKG_VERSION"),
            socket.display(),
            std::process::id()
        ),
    );
    if let Some(err) = config_error {
        daemon.log.error("daemon", format!("config: {err}"));
    }
    if autostart {
        daemon.log.info("daemon", daemon.start_all());
    }

    // Accept loop.
    {
        let daemon = daemon.clone();
        tokio::spawn(async move {
            loop {
                match listener.accept().await {
                    Ok((stream, _)) => {
                        tokio::spawn(serve(daemon.clone(), stream, true, String::new()));
                    }
                    Err(err) => {
                        daemon.log.error("daemon", format!("accept failed: {err}"));
                        tokio::time::sleep(Duration::from_millis(200)).await;
                    }
                }
            }
        });
    }

    // Optional remote listener, for other machines' dashboards.
    {
        let remote = daemon.config.read().unwrap().settings.remote.clone();
        match remote.active() {
            Some((listen, _)) => {
                let listen = listen.to_string();
                let acceptor = match crate::tls::server_config() {
                    Ok(config) => tokio_rustls::TlsAcceptor::from(config),
                    Err(err) => {
                        daemon
                            .log
                            .error("remote", format!("cannot set up TLS: {err:#}"));
                        return Err(err);
                    }
                };
                match tokio::net::TcpListener::bind(&listen).await {
                    Ok(listener) => {
                        daemon.log.info(
                            "remote",
                            format!(
                                "listening on {listen} as node `{}` (TLS, token required)",
                                daemon.settings().node_name()
                            ),
                        );
                        let daemon = daemon.clone();
                        tokio::spawn(async move {
                            loop {
                                match listener.accept().await {
                                    Ok((stream, addr)) => {
                                        stream.set_nodelay(true).ok();
                                        let acceptor = acceptor.clone();
                                        let daemon = daemon.clone();
                                        tokio::spawn(async move {
                                            match acceptor.accept(stream).await {
                                                Ok(tls) => {
                                                    serve(daemon, tls, false, addr.to_string())
                                                        .await
                                                }
                                                Err(err) => daemon.log.warn(
                                                    "remote",
                                                    format!(
                                                        "TLS handshake with {addr} failed: {err}"
                                                    ),
                                                ),
                                            }
                                        });
                                    }
                                    Err(err) => {
                                        daemon.log.error("remote", format!("accept failed: {err}"));
                                        tokio::time::sleep(Duration::from_millis(500)).await;
                                    }
                                }
                            }
                        });
                    }
                    Err(err) => {
                        let hint = match err.kind() {
                            std::io::ErrorKind::AddrInUse => {
                                "something else already holds that port — pick another with \
                                 `cryptocli remote enable --listen 0.0.0.0:PORT`"
                            }
                            std::io::ErrorKind::AddrNotAvailable => {
                                "this machine has no such address — use 0.0.0.0 to listen on \
                                 every interface"
                            }
                            std::io::ErrorKind::PermissionDenied => {
                                "ports below 1024 need root — pick a higher one"
                            }
                            _ => "check the address in `cryptocli remote show`",
                        };
                        daemon.log.error(
                            "remote",
                            format!("cannot listen on {listen}: {err} — {hint}"),
                        );
                    }
                }
            }
            None => {
                if remote.listen.is_some() {
                    daemon.log.warn(
                        "remote",
                        "listen address set but no token; remote access stays off",
                    );
                }
            }
        }
    }

    // Sampling + endpoint polling.
    {
        let daemon = daemon.clone();
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(sample_interval);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                ticker.tick().await;
                daemon.sample();
                daemon.poll_endpoints().await;
                daemon.poll_peers().await;
            }
        });
    }

    let shutdown = daemon.shutdown.clone();
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    tokio::select! {
        _ = shutdown.notified() => {}
        _ = tokio::signal::ctrl_c() => {
            daemon.log.info("daemon", "SIGINT received");
        }
        _ = sigterm.recv() => {
            daemon.log.info("daemon", "SIGTERM received");
        }
    }

    daemon.stop_all();
    // Give sessions a moment to unwind their threads cleanly.
    for _ in 0..25 {
        let all_stopped = daemon
            .rigs
            .read()
            .unwrap()
            .values()
            .all(|r| r.stats.state() == RigState::Stopped);
        if all_stopped {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    std::fs::remove_file(&socket).ok();
    std::fs::remove_file(paths::pid_path()).ok();
    daemon.log.info("daemon", "stopped");
    Ok(())
}

/// Fork off a detached daemon and wait until it answers on the socket.
///
/// `autostart` false brings the daemon up without starting any rigs.
pub fn spawn_detached(autostart: bool) -> Result<()> {
    use std::os::unix::process::CommandExt;

    paths::ensure_dirs()?;
    if crate::ipc::Client::probe() {
        return Ok(());
    }
    let socket = paths::socket_path();
    if socket.exists() {
        std::fs::remove_file(&socket).ok();
    }

    let exe = std::env::current_exe().context("locating the cryptocli binary")?;
    // The daemon can run for weeks; roll the log over rather than letting it
    // grow without bound.
    const MAX_LOG_BYTES: u64 = 8 * 1024 * 1024;
    if let Ok(meta) = std::fs::metadata(paths::log_path())
        && meta.len() > MAX_LOG_BYTES
    {
        let previous = paths::log_path().with_extension("log.1");
        std::fs::rename(paths::log_path(), previous).ok();
    }
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(paths::log_path())
        .with_context(|| format!("opening {}", paths::log_path().display()))?;
    let mut command = std::process::Command::new(exe);
    if !autostart {
        command.env("CRYPTOCLI_NO_AUTOSTART", "1");
    }
    command
        .arg("daemon")
        .arg("run")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::from(log.try_clone()?))
        .stderr(std::process::Stdio::from(log));
    // Detach from the controlling terminal so Ctrl-C in the TUI, or closing the
    // terminal altogether, never reaches the miner.
    unsafe {
        command.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    let mut child = command.spawn().context("spawning the daemon")?;

    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline {
        if crate::ipc::Client::probe() {
            return Ok(());
        }
        if let Ok(Some(status)) = child.try_wait() {
            bail!(
                "daemon exited immediately ({status}); see {}",
                paths::log_path().display()
            );
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    bail!(
        "daemon did not come up within 15s; see {}",
        paths::log_path().display()
    )
}

/// Ensure a daemon is running, starting one if needed.
///
/// Returns true if this call is what started it, so a caller that merely wanted
/// to look at the dashboard can put things back as it found them.
pub fn ensure_running(autostart: bool) -> Result<bool> {
    if crate::ipc::Client::probe() {
        return Ok(false);
    }
    spawn_detached(autostart)?;
    Ok(true)
}
