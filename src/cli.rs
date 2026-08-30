//! Command line surface. Everything the TUI can do has a scriptable equivalent
//! here, so cryptocli works the same over ssh, in a cron job, or by hand.

use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand};
use std::collections::BTreeMap;

use crate::config::{Config, EndpointConfig, RigConfig, Wallet, check_address};
use crate::daemon;
use crate::ipc::{Client, Request};
use crate::mining::algo;
use crate::model::{fmt_count, fmt_duration, fmt_hashrate};
use crate::paths;

#[derive(Parser)]
#[command(
    name = "cryptocli",
    version,
    about = "Multi-pool mining with a live TUI and a daemon that keeps going after you close it",
    long_about = None,
    subcommand_negates_reqs = true
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Subcommand)]
pub enum Command {
    /// Open the dashboard (default). Starts the daemon if it is not running.
    Dash,
    /// Print a one-shot status summary.
    Status {
        /// Emit the raw snapshot as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Start a rig, or every enabled rig.
    Start {
        /// Rig name, or `node:rig` for a rig on another machine.
        /// Omit to start all enabled rigs here.
        name: Option<String>,
    },
    /// Stop a rig, or every running rig.
    Stop {
        /// Rig name, or `node:rig` for a rig on another machine.
        /// Omit to stop all rigs here.
        name: Option<String>,
    },
    /// Daemon lifecycle.
    Daemon {
        #[command(subcommand)]
        command: DaemonCommand,
    },
    /// Payout addresses to mine to.
    Wallet {
        #[command(subcommand)]
        command: WalletCommand,
    },
    /// Pool connections.
    Rig {
        #[command(subcommand)]
        command: RigCommand,
    },
    /// HTTP checks polled by the daemon.
    Endpoint {
        #[command(subcommand)]
        command: EndpointCommand,
    },
    /// Other machines, controlled from this dashboard.
    Node {
        #[command(subcommand)]
        command: NodeCommand,
    },
    /// Accept connections from other machines' dashboards.
    Remote {
        #[command(subcommand)]
        command: RemoteCommand,
    },
    /// Configuration file helpers.
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },
    /// Measure local hashrate without touching a pool.
    Bench {
        #[arg(long, default_value = "sha256d")]
        algo: String,
        /// Threads to use; defaults to the configured budget.
        #[arg(long)]
        threads: Option<usize>,
        #[arg(long, default_value_t = 5)]
        seconds: u64,
    },
    /// List the supported algorithms.
    Algos,
}

#[derive(Subcommand)]
pub enum DaemonCommand {
    /// Run the daemon in the foreground (used internally by `daemon start`).
    Run,
    /// Start the daemon in the background.
    Start,
    /// Ask the running daemon to shut down.
    Stop,
    /// Show whether the daemon is up.
    Status,
    /// Tail the daemon log file.
    Log {
        #[arg(long, default_value_t = 50)]
        lines: usize,
    },
    /// Reload the config file without restarting.
    Reload,
}

#[derive(Subcommand)]
pub enum WalletCommand {
    /// Register a payout address.
    Add {
        name: String,
        #[arg(long)]
        coin: String,
        #[arg(long)]
        address: String,
        #[arg(long)]
        label: Option<String>,
        /// Skip the address sanity check.
        #[arg(long)]
        force: bool,
    },
    List,
    /// Remove a wallet by name.
    Rm {
        name: String,
    },
}

#[derive(Args)]
pub struct RigAddArgs {
    pub name: String,
    /// Pool address (stratum+tcp://pool.example:3333) or the name of a
    /// configured endpoint, whose url and credentials are reused.
    #[arg(long, value_name = "URL_OR_ENDPOINT")]
    pub url: String,
    #[arg(long, default_value = "sha256d")]
    pub algo: String,
    /// Coin label, e.g. BTC. Drives the per-coin totals.
    #[arg(long)]
    pub coin: Option<String>,
    /// Wallet name to mine to.
    #[arg(long)]
    pub wallet: Option<String>,
    /// Worker suffix appended to the wallet address.
    #[arg(long)]
    pub worker: Option<String>,
    /// Full stratum username; overrides --wallet/--worker.
    #[arg(long)]
    pub user: Option<String>,
    #[arg(long, default_value = "x")]
    pub pass: String,
    /// Fixed thread count. 0 (default) shares the global budget.
    #[arg(long, default_value_t = 0)]
    pub threads: usize,
    /// Share of the auto-allocated threads relative to other rigs.
    #[arg(long, default_value_t = 1)]
    pub weight: u32,
    /// Do not start this rig automatically.
    #[arg(long)]
    pub disabled: bool,
}

#[derive(Subcommand)]
pub enum RigCommand {
    /// Add a pool connection.
    Add(RigAddArgs),
    List,
    Rm {
        name: String,
    },
    Enable {
        name: String,
    },
    Disable {
        name: String,
    },
    /// Change the thread count of a running rig (and persist it).
    Threads {
        name: String,
        threads: usize,
    },
    /// Mine an additional coin on an existing rig, at the same time. The
    /// rig's threads are divided between its coins by weight.
    Coin(RigCoinArgs),
    /// Stop mining one coin on a rig.
    Uncoin {
        name: String,
        coin: String,
    },
}

#[derive(Args)]
pub struct RigCoinArgs {
    /// Existing rig to add the coin to.
    pub name: String,
    #[arg(long)]
    pub coin: String,
    /// Pool url, or the name of a configured endpoint.
    #[arg(long, value_name = "URL_OR_ENDPOINT")]
    pub url: String,
    #[arg(long)]
    pub algo: Option<String>,
    #[arg(long)]
    pub wallet: Option<String>,
    #[arg(long)]
    pub worker: Option<String>,
    #[arg(long)]
    pub user: Option<String>,
    #[arg(long)]
    pub pass: Option<String>,
    /// Share of the rig's threads relative to its other coins.
    #[arg(long, default_value_t = 1)]
    pub weight: u32,
}

#[derive(Args)]
pub struct EndpointAddArgs {
    pub name: String,
    /// https://... for an HTTP check, or stratum+tcp://host:port to check that
    /// a pool is up and will accept your worker.
    #[arg(long)]
    pub url: String,
    /// Pool worker (e.g. `BTC:youraddress.worker`) or HTTP basic auth user.
    #[arg(long)]
    pub user: Option<String>,
    /// Pool password (usually `x`) or HTTP basic auth password.
    #[arg(long)]
    pub password: Option<String>,
    #[arg(long, default_value = "GET")]
    pub method: String,
    /// Repeatable: --header "Authorization: Bearer x"
    #[arg(long = "header", value_name = "K: V")]
    pub headers: Vec<String>,
    /// Request body for POST/PUT.
    #[arg(long)]
    pub body: Option<String>,
    #[arg(long, default_value_t = 60)]
    pub interval: u64,
    #[arg(long, default_value_t = 10)]
    pub timeout: u64,
    #[arg(long, default_value_t = 200)]
    pub expect_status: u16,
    /// Substring that must be present in the response.
    #[arg(long)]
    pub expect_body: Option<String>,
    /// Repeatable: --field "balance=data.miner.balance"
    #[arg(long = "field", value_name = "LABEL=JSON.PATH")]
    pub fields: Vec<String>,
}

#[derive(Subcommand)]
pub enum EndpointCommand {
    /// Register a check: an HTTP url, or a stratum pool to handshake against.
    Add(Box<EndpointAddArgs>),
    List,
    Rm {
        name: String,
    },
    /// Run a check right now and print the result (no daemon needed).
    Test {
        name: String,
    },
}

#[derive(Subcommand)]
pub enum NodeCommand {
    /// Register another machine. Get its address, token and fingerprint from
    /// `cryptocli remote enable` on that machine.
    Add {
        name: String,
        /// host:port of the peer's remote listener.
        #[arg(long)]
        address: String,
        #[arg(long)]
        token: String,
        /// The peer's TLS certificate fingerprint. If omitted, the one the
        /// peer presents is shown and trusted on first use.
        #[arg(long)]
        fingerprint: Option<String>,
    },
    List,
    Rm {
        name: String,
    },
    /// Connect to a peer right now and print what it reports.
    Test {
        name: String,
    },
}

#[derive(Subcommand)]
pub enum RemoteCommand {
    /// Start accepting remote dashboards on this machine.
    Enable {
        /// Address to listen on. Bind to a LAN or VPN address, not a public one.
        #[arg(long, default_value = "0.0.0.0:9944")]
        listen: String,
        /// Shared secret. Generated if omitted.
        #[arg(long)]
        token: Option<String>,
        /// Name other dashboards will show for this machine.
        #[arg(long)]
        node_name: Option<String>,
    },
    /// Stop accepting remote dashboards.
    Disable,
    /// Show the current remote settings.
    Show,
}

#[derive(Subcommand)]
pub enum ConfigCommand {
    /// Print the config file path.
    Path,
    /// Print the config file.
    Show,
    /// Write a commented starter config.
    Init {
        #[arg(long)]
        force: bool,
    },
}

pub fn run(cli: Cli) -> Result<()> {
    match cli.command.unwrap_or(Command::Dash) {
        Command::Dash => {
            // Opening the dashboard must not start mining by itself.
            let we_started_it = daemon::ensure_running(false)?;
            crate::tui::run(we_started_it)
        }
        Command::Status { json } => status(json),
        Command::Start { name } => {
            // An explicit start does want autostart semantics.
            daemon::ensure_running(true)?;
            let request = match name {
                Some(name) => on_node(&name, |rig| Request::StartRig { name: rig }),
                None => Request::StartAll,
            };
            println!("{}", Client::connect()?.command(&request)?);
            Ok(())
        }
        Command::Stop { name } => {
            let request = match name {
                Some(name) => on_node(&name, |rig| Request::StopRig { name: rig }),
                None => Request::StopAll,
            };
            println!("{}", Client::connect()?.command(&request)?);
            Ok(())
        }
        Command::Daemon { command } => daemon_command(command),
        Command::Wallet { command } => wallet_command(command),
        Command::Rig { command } => rig_command(command),
        Command::Endpoint { command } => endpoint_command(command),
        Command::Node { command } => node_command(command),
        Command::Remote { command } => remote_command(command),
        Command::Config { command } => config_command(command),
        Command::Bench {
            algo: algo_id,
            threads,
            seconds,
        } => bench(&algo_id, threads, seconds),
        Command::Algos => {
            for a in algo::all() {
                println!("{:<10} {}", a.id(), a.description());
            }
            Ok(())
        }
    }
}

fn daemon_command(command: DaemonCommand) -> Result<()> {
    match command {
        DaemonCommand::Run => {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                // The daemon's async side is I/O only; hashing runs on its own
                // dedicated threads, so a small pool is plenty.
                .worker_threads(2)
                .enable_all()
                .build()?;
            runtime.block_on(daemon::run())
        }
        DaemonCommand::Start => {
            if Client::probe() {
                println!("daemon is already running");
                return Ok(());
            }
            daemon::spawn_detached(true)?;
            println!("daemon started");
            Ok(())
        }
        DaemonCommand::Stop => {
            if !Client::probe() {
                println!("daemon is not running");
                return Ok(());
            }
            println!("{}", Client::connect()?.command(&Request::Shutdown)?);
            Ok(())
        }
        DaemonCommand::Status => {
            match Client::connect().and_then(|mut c| c.command(&Request::Ping)) {
                Ok(message) => println!("{message}"),
                Err(_) => println!("daemon is not running"),
            }
            println!("socket: {}", paths::socket_path().display());
            println!("log:    {}", paths::log_path().display());
            println!("config: {}", paths::config_path().display());
            Ok(())
        }
        DaemonCommand::Log { lines } => {
            let path = paths::log_path();
            let text = std::fs::read_to_string(&path)
                .with_context(|| format!("reading {}", path.display()))?;
            let all: Vec<&str> = text.lines().collect();
            for line in all.iter().skip(all.len().saturating_sub(lines)) {
                println!("{line}");
            }
            Ok(())
        }
        DaemonCommand::Reload => {
            println!("{}", Client::connect()?.command(&Request::Reload)?);
            Ok(())
        }
    }
}

fn status(json: bool) -> Result<()> {
    if !Client::probe() {
        if json {
            println!("{{\"daemon\":\"stopped\"}}");
        } else {
            println!("daemon is not running (start it with `cryptocli daemon start`)");
        }
        return Ok(());
    }
    let snapshot = Client::connect()?.snapshot()?;
    if json {
        println!("{}", serde_json::to_string_pretty(&snapshot)?);
        return Ok(());
    }

    let t = &snapshot.totals;
    println!(
        "daemon {} (pid {})  up {}",
        snapshot.daemon.version,
        snapshot.daemon.pid,
        fmt_duration(snapshot.daemon.uptime_secs)
    );
    println!(
        "total  {}  |  {} of {} sessions active  |  {} threads of {}  |  {}",
        fmt_hashrate(t.hashrate),
        t.rigs_active,
        t.rigs_total,
        t.threads_active,
        t.threads_budget,
        t.backend
    );
    println!(
        "shares {} accepted / {} rejected / {} stale  |  {} hashes",
        t.accepted,
        t.rejected,
        t.stale,
        fmt_count(t.hashes_total as f64)
    );
    println!();
    if snapshot.nodes.len() > 1 {
        println!();
        println!(
            "{:<16} {:<8} {:>12} {:>6} {:>8}  ADDRESS",
            "NODE", "STATE", "HASHRATE", "RIGS", "PING"
        );
        for node in &snapshot.nodes {
            println!(
                "{:<16} {:<8} {:>12} {:>6} {:>8}  {}",
                truncate(&node.name, 16),
                if node.online { "online" } else { "offline" },
                fmt_hashrate(node.hashrate),
                format!("{}/{}", node.rigs_active, node.rigs_total),
                node.latency_ms
                    .map(|v| format!("{v}ms"))
                    .unwrap_or_else(|| "-".into()),
                node.address
            );
        }
        println!();
    }
    if snapshot.totals.coins.len() > 1 {
        println!("coins:");
        for coin in &snapshot.totals.coins {
            println!(
                "  {:<8} {:>12}  {} session(s), {} accepted",
                coin.coin,
                fmt_hashrate(coin.hashrate),
                coin.sessions,
                coin.accepted
            );
        }
        println!();
    }
    // The node column only earns its width once there is more than one machine.
    let multi = snapshot.nodes.len() > 1;
    println!(
        "{:<14} {:<18} {:<6} {:<12} {:>12} {:>7} {:>8}  POOL",
        if multi { "NODE" } else { "" },
        "RIG",
        "COIN",
        "STATE",
        "HASHRATE",
        "THREADS",
        "ACCEPT"
    );
    for rig in &snapshot.rigs {
        println!(
            "{:<14} {:<18} {:<6} {:<12} {:>12} {:>7} {:>8}  {}",
            if multi {
                truncate(&rig.node, 14)
            } else {
                String::new()
            },
            truncate(&rig.name, 18),
            truncate(&rig.coin, 6),
            rig.state.label(),
            fmt_hashrate(rig.hashrate),
            rig.threads,
            rig.accepted,
            rig.pool
        );
    }
    if !snapshot.endpoints.is_empty() {
        println!();
        println!("{:<16} {:<8} {:>8}  URL", "ENDPOINT", "STATE", "LATENCY");
        for e in &snapshot.endpoints {
            let state = match e.ok {
                Some(true) => "up",
                Some(false) => "down",
                None => "pending",
            };
            println!(
                "{:<16} {:<8} {:>8}  {}",
                truncate(&e.name, 16),
                state,
                e.latency_ms
                    .map(|v| format!("{v}ms"))
                    .unwrap_or_else(|| "-".into()),
                e.url
            );
        }
    }
    Ok(())
}

fn wallet_command(command: WalletCommand) -> Result<()> {
    let mut config = Config::load()?;
    match command {
        WalletCommand::Add {
            name,
            coin,
            address,
            label,
            force,
        } => {
            if config.wallets.iter().any(|w| w.name == name) {
                bail!("a wallet named `{name}` already exists");
            }
            if !force {
                check_address(&coin, &address)
                    .context("address failed the sanity check (pass --force to store it anyway)")?;
            }
            config.wallets.push(Wallet {
                name: name.clone(),
                coin: coin.to_ascii_uppercase(),
                address,
                label,
            });
            config.save()?;
            nudge_daemon();
            println!("wallet `{name}` added");
        }
        WalletCommand::List => {
            if config.wallets.is_empty() {
                println!("no wallets configured");
            }
            for w in &config.wallets {
                println!("{:<12} {:<6} {}", w.name, w.coin, w.address);
            }
        }
        WalletCommand::Rm { name } => {
            let before = config.wallets.len();
            config.wallets.retain(|w| w.name != name);
            if config.wallets.len() == before {
                bail!("no wallet named `{name}`");
            }
            config.save()?;
            nudge_daemon();
            println!("wallet `{name}` removed");
        }
    }
    Ok(())
}

fn rig_command(command: RigCommand) -> Result<()> {
    let mut config = Config::load()?;
    match command {
        RigCommand::Add(args) => {
            if config.rigs.iter().any(|r| r.name == args.name) {
                bail!("a rig named `{}` already exists", args.name);
            }
            if algo::lookup(&args.algo).is_none() {
                bail!(
                    "unknown algo `{}` (known: {})",
                    args.algo,
                    algo::names().join(", ")
                );
            }
            let inherits_user = config
                .resolve_pool(&args.url)
                .map(|p| p.user.is_some())
                .unwrap_or(false);
            if args.user.is_none() && args.wallet.is_none() && !inherits_user {
                bail!("give either --wallet or --user so the pool knows who to pay");
            }
            if let Some(wallet) = &args.wallet
                && !config.wallets.iter().any(|w| &w.name == wallet)
            {
                bail!("no wallet named `{wallet}` (add one with `cryptocli wallet add`)");
            }
            let pool = config.resolve_pool(&args.url)?;
            if let Some(endpoint) = &pool.from_endpoint {
                println!("using endpoint `{endpoint}` -> {}", pool.url);
            }
            let rig = RigConfig {
                name: args.name.clone(),
                url: pool.url,
                algo: args.algo,
                coin: args.coin.map(|c| c.to_ascii_uppercase()),
                targets: Vec::new(),
                user: args.user.or_else(|| pool.user.clone()).unwrap_or_default(),
                pass: pool.pass.clone().unwrap_or(args.pass),
                wallet: args.wallet,
                worker: args.worker,
                threads: args.threads,
                weight: args.weight,
                enabled: !args.disabled,
            };
            rig.host_port()?;
            config.rigs.push(rig);
            config.save()?;
            nudge_daemon();
            println!("rig `{}` added", args.name);
        }
        RigCommand::List => {
            if config.rigs.is_empty() {
                println!("no rigs configured");
            }
            for r in &config.rigs {
                println!(
                    "{:<14} {:<10} {:<28} threads={} weight={} {}",
                    r.name,
                    r.coins().join("+"),
                    r.url,
                    if r.threads == 0 {
                        "auto".to_string()
                    } else {
                        r.threads.to_string()
                    },
                    r.weight,
                    if r.enabled { "enabled" } else { "disabled" }
                );
            }
        }
        RigCommand::Rm { name } => {
            let before = config.rigs.len();
            config.rigs.retain(|r| r.name != name);
            if config.rigs.len() == before {
                bail!("no rig named `{name}`");
            }
            config.save()?;
            if Client::probe() {
                let _ = Client::connect()?.command(&Request::StopRig { name: name.clone() });
            }
            nudge_daemon();
            println!("rig `{name}` removed");
        }
        RigCommand::Coin(args) => {
            if let Some(wallet) = &args.wallet
                && !config.wallets.iter().any(|w| &w.name == wallet)
            {
                bail!("no wallet named `{wallet}`");
            }
            let wallets = config.wallets.clone();
            let pool = config.resolve_pool(&args.url)?;
            let rig = config
                .rigs
                .iter_mut()
                .find(|r| r.name == args.name)
                .with_context(|| format!("no rig named `{}`", args.name))?;
            let coin = args.coin.to_ascii_uppercase();
            if rig.coins().iter().any(|c| c == &coin) {
                bail!("rig `{}` already mines {coin}", args.name);
            }
            if let Some(algo_id) = &args.algo
                && algo::lookup(algo_id).is_none()
            {
                bail!("unknown algo `{algo_id}`");
            }
            rig.targets.push(crate::config::RigTarget {
                coin: coin.clone(),
                url: pool.url,
                algo: args.algo,
                wallet: args.wallet,
                worker: args.worker,
                user: args.user.or(pool.user),
                pass: args.pass.or(pool.pass),
                weight: args.weight,
            });
            // Surfaces a bad url or a missing wallet before we write it out.
            let targets = rig.expand(&wallets)?;
            let name = args.name.clone();
            config.save()?;
            nudge_daemon();
            println!(
                "rig `{name}` now mines {} coin(s): {}",
                targets.len(),
                targets
                    .iter()
                    .map(|t| t.coin.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            );
            println!(
                "restart it to pick up the change: cryptocli stop {name} && cryptocli start {name}"
            );
        }
        RigCommand::Uncoin { name, coin } => {
            let coin = coin.to_ascii_uppercase();
            let rig = config
                .rigs
                .iter_mut()
                .find(|r| r.name == name)
                .with_context(|| format!("no rig named `{name}`"))?;
            let before = rig.targets.len();
            rig.targets.retain(|t| t.coin.to_ascii_uppercase() != coin);
            if rig.targets.len() == before {
                bail!("rig `{name}` has no extra target for {coin}");
            }
            config.save()?;
            if Client::probe() {
                let _ = Client::connect()?.command(&Request::StopRig {
                    name: format!("{name}/{coin}"),
                });
            }
            nudge_daemon();
            println!("rig `{name}` no longer mines {coin}");
        }
        RigCommand::Enable { name } => set_enabled(&mut config, &name, true)?,
        RigCommand::Disable { name } => set_enabled(&mut config, &name, false)?,
        RigCommand::Threads { name, threads } => {
            let rig = config
                .rigs
                .iter_mut()
                .find(|r| r.name == name)
                .with_context(|| format!("no rig named `{name}`"))?;
            rig.threads = threads;
            config.save()?;
            if Client::probe() {
                let _ = Client::connect()?.command(&Request::SetThreads {
                    name: name.clone(),
                    threads,
                });
            }
            println!("rig `{name}` set to {threads} thread(s)");
        }
    }
    Ok(())
}

fn set_enabled(config: &mut Config, name: &str, enabled: bool) -> Result<()> {
    let rig = config
        .rigs
        .iter_mut()
        .find(|r| r.name == name)
        .with_context(|| format!("no rig named `{name}`"))?;
    rig.enabled = enabled;
    config.save()?;
    nudge_daemon();
    println!(
        "rig `{name}` {}",
        if enabled { "enabled" } else { "disabled" }
    );
    Ok(())
}

fn endpoint_command(command: EndpointCommand) -> Result<()> {
    let mut config = Config::load()?;
    match command {
        EndpointCommand::Add(args) => {
            let args = *args;
            if config.endpoints.iter().any(|e| e.name == args.name) {
                bail!("an endpoint named `{}` already exists", args.name);
            }
            let mut headers = BTreeMap::new();
            for raw in &args.headers {
                let (k, v) = raw
                    .split_once(':')
                    .with_context(|| format!("header `{raw}` should look like `Name: value`"))?;
                headers.insert(k.trim().to_string(), v.trim().to_string());
            }
            let mut fields = BTreeMap::new();
            for raw in &args.fields {
                let (label, path) = raw
                    .split_once('=')
                    .with_context(|| format!("field `{raw}` should look like `label=json.path`"))?;
                fields.insert(label.trim().to_string(), path.trim().to_string());
            }
            if crate::config::endpoint_kind(&args.url) == crate::model::EndpointKind::Stratum {
                // Fails fast on a missing port rather than at poll time.
                crate::config::host_port(&args.url, "endpoint")?;
            }
            config.endpoints.push(EndpointConfig {
                name: args.name.clone(),
                url: args.url,
                method: args.method.to_ascii_uppercase(),
                headers,
                body: args.body,
                interval_secs: args.interval.max(1),
                timeout_secs: args.timeout.max(1),
                expect_status: args.expect_status,
                expect_body: args.expect_body,
                fields,
                user: args.user,
                password: args.password,
                enabled: true,
            });
            config.save()?;
            nudge_daemon();
            println!(
                "endpoint `{}` added ({} check)",
                args.name,
                crate::config::endpoint_kind(&config.endpoints.last().unwrap().url).label()
            );
        }
        EndpointCommand::List => {
            if config.endpoints.is_empty() {
                println!("no endpoints configured");
            }
            for e in &config.endpoints {
                let kind = crate::config::endpoint_kind(&e.url);
                println!(
                    "{:<16} {:<6} {:<6} every {:>4}s  {}",
                    e.name,
                    kind.label(),
                    if kind == crate::model::EndpointKind::Stratum {
                        "-".to_string()
                    } else {
                        e.method.clone()
                    },
                    e.interval_secs,
                    e.url
                );
                if let Some(user) = &e.user {
                    println!("{:<16}   user {user}", "");
                }
                for (label, path) in &e.fields {
                    println!("{:<16}   {label} <- {path}", "");
                }
            }
        }
        EndpointCommand::Rm { name } => {
            let before = config.endpoints.len();
            config.endpoints.retain(|e| e.name != name);
            if config.endpoints.len() == before {
                bail!("no endpoint named `{name}`");
            }
            config.save()?;
            nudge_daemon();
            println!("endpoint `{name}` removed");
        }
        EndpointCommand::Test { name } => {
            let endpoint = config
                .endpoints
                .iter()
                .find(|e| e.name == name)
                .with_context(|| format!("no endpoint named `{name}`"))?;
            let outcome = crate::endpoints::check(endpoint);
            println!(
                "{}  {}  {} ms",
                if outcome.ok { "ok" } else { "FAILED" },
                match outcome.http_status {
                    Some(status) => format!("HTTP {status}"),
                    // A rejected worker is not the same as an unreachable
                    // pool, so say "check" rather than claiming either.
                    None if crate::config::endpoint_kind(&endpoint.url)
                        == crate::model::EndpointKind::Stratum =>
                        if outcome.ok {
                            "pool handshake ok".into()
                        } else {
                            "pool check failed".into()
                        },
                    None => "no response".to_string(),
                },
                outcome.latency_ms
            );
            for (label, value) in &outcome.fields {
                println!("  {label}: {value}");
            }
            if let Some(err) = outcome.error {
                println!("  error: {err}");
            }
        }
    }
    Ok(())
}

fn node_command(command: NodeCommand) -> Result<()> {
    let mut config = Config::load()?;
    match command {
        NodeCommand::Add {
            name,
            address,
            token,
            fingerprint,
        } => {
            if config.nodes.iter().any(|n| n.name == name) {
                bail!("a node named `{name}` already exists");
            }
            if !address.contains(':') {
                bail!("address should be host:port");
            }
            // Verify before saving, so a typo surfaces now rather than as a
            // permanently red row on the dashboard.
            use std::io::Write;
            let timeout = std::time::Duration::from_secs(8);
            let fingerprint = match fingerprint {
                Some(given) => crate::tls::normalize_fingerprint(&given),
                None => {
                    // Trust on first use, but say so loudly: this is the one
                    // moment an interceptor could substitute its own key.
                    let seen = crate::tls::peek_fingerprint(&address, timeout)
                        .with_context(|| format!("cannot reach node `{name}`"))?;
                    println!("no --fingerprint given; the node presents:");
                    println!("  {seen}");
                    println!("Check that against `cryptocli remote show` on that machine.");
                    seen
                }
            };
            print!("connecting to {address}... ");
            std::io::stdout().flush().ok();
            match Client::connect_remote(&address, &token, &fingerprint, timeout)
                .and_then(|mut c| c.snapshot())
            {
                Ok(snapshot) => println!(
                    "ok — cryptocli {}, {} rig(s), {}",
                    snapshot.daemon.version,
                    snapshot.totals.rigs_total,
                    fmt_hashrate(snapshot.totals.hashrate)
                ),
                Err(err) => {
                    println!("failed");
                    return Err(err.context(format!("cannot reach node `{name}`")));
                }
            }
            config.nodes.push(crate::config::NodeConfig {
                name: name.clone(),
                address,
                token,
                fingerprint,
                enabled: true,
            });
            config.save()?;
            nudge_daemon();
            println!("node `{name}` added");
        }
        NodeCommand::List => {
            if config.nodes.is_empty() {
                println!("no other machines configured");
                println!("run `cryptocli remote enable` on another machine, then:");
                println!("  cryptocli node add rig2 --address HOST:9944 --token TOKEN");
            }
            for n in &config.nodes {
                println!(
                    "{:<16} {:<24} {}",
                    n.name,
                    n.address,
                    if n.enabled { "enabled" } else { "disabled" }
                );
                println!("{:<16}   {}", "", n.fingerprint);
            }
        }
        NodeCommand::Rm { name } => {
            let before = config.nodes.len();
            config.nodes.retain(|n| n.name != name);
            if config.nodes.len() == before {
                bail!("no node named `{name}`");
            }
            config.save()?;
            nudge_daemon();
            println!("node `{name}` removed");
        }
        NodeCommand::Test { name } => {
            let node = config
                .nodes
                .iter()
                .find(|n| n.name == name)
                .with_context(|| format!("no node named `{name}`"))?;
            let started = std::time::Instant::now();
            let mut client = Client::connect_remote(
                &node.address,
                &node.token,
                &node.fingerprint,
                std::time::Duration::from_secs(8),
            )?;
            let snapshot = client.snapshot()?;
            println!("ok  {}  {} ms", node.address, started.elapsed().as_millis());
            println!(
                "  cryptocli {}  up {}",
                snapshot.daemon.version,
                fmt_duration(snapshot.daemon.uptime_secs)
            );
            println!(
                "  {}  {} of {} rigs active  {} threads",
                fmt_hashrate(snapshot.totals.hashrate),
                snapshot.totals.rigs_active,
                snapshot.totals.rigs_total,
                snapshot.totals.threads_active
            );
        }
    }
    Ok(())
}

fn remote_command(command: RemoteCommand) -> Result<()> {
    let mut config = Config::load()?;
    match command {
        RemoteCommand::Enable {
            listen,
            token,
            node_name,
        } => {
            let token = token.unwrap_or_else(crate::nodes::generate_token);
            config.settings.remote.listen = Some(listen.clone());
            config.settings.remote.token = Some(token.clone());
            if let Some(name) = node_name {
                config.settings.node_name = Some(name);
            }
            config.save()?;
            let name = config.settings.node_name();
            // Generating the identity here means the fingerprint can be shown
            // now, rather than only after the daemon first starts.
            let fingerprint = crate::tls::local_fingerprint()?;
            println!("remote access enabled on {listen} as node `{name}`");
            println!("fingerprint: {fingerprint}");
            println!();
            println!("On the machine you want to watch from, run:");
            println!(
                "  cryptocli node add {name} --address <THIS_HOST>:{} \\",
                listen.rsplit(':').next().unwrap_or("9944")
            );
            println!("      --token {token} \\");
            println!("      --fingerprint {fingerprint}");
            println!();
            println!("The connection is TLS encrypted and the certificate is pinned,");
            println!("so the token never crosses the network in the clear.");
            println!();
            println!("Restart the daemon to start listening:");
            println!("  cryptocli daemon stop && cryptocli daemon start");
        }
        RemoteCommand::Disable => {
            config.settings.remote.listen = None;
            config.settings.remote.token = None;
            config.save()?;
            println!("remote access disabled (restart the daemon to close the port)");
        }
        RemoteCommand::Show => match config.settings.remote.active() {
            Some((listen, token)) => {
                println!("node name:   {}", config.settings.node_name());
                println!("listening:   {listen}");
                println!("token:       {token}");
                match crate::tls::local_fingerprint() {
                    Ok(fingerprint) => println!("fingerprint: {fingerprint}"),
                    Err(err) => println!("fingerprint: unavailable ({err:#})"),
                }
            }
            None => println!("remote access is off"),
        },
    }
    Ok(())
}

fn config_command(command: ConfigCommand) -> Result<()> {
    match command {
        ConfigCommand::Path => {
            println!("{}", paths::config_path().display());
        }
        ConfigCommand::Show => {
            let path = paths::config_path();
            match std::fs::read_to_string(&path) {
                Ok(text) => print!("{text}"),
                Err(_) => println!("# no config yet at {}", path.display()),
            }
        }
        ConfigCommand::Init { force } => {
            let path = paths::config_path();
            if path.exists() && !force {
                bail!(
                    "{} already exists (pass --force to overwrite)",
                    path.display()
                );
            }
            paths::ensure_dirs()?;
            std::fs::write(&path, STARTER_CONFIG)?;
            println!("wrote {}", path.display());
        }
    }
    Ok(())
}

fn bench(algo_id: &str, threads: Option<usize>, seconds: u64) -> Result<()> {
    let algorithm = algo::lookup(algo_id).with_context(|| {
        format!(
            "unknown algo `{algo_id}` (known: {})",
            algo::names().join(", ")
        )
    })?;
    let threads = threads.unwrap_or_else(|| {
        Config::load()
            .map(|c| c.settings.thread_budget())
            .unwrap_or(1)
    });
    println!("benchmarking {algo_id} on {threads} thread(s) for {seconds}s...");
    let rate = algorithm.bench(threads.max(1), seconds.max(1));
    println!("{}", fmt_hashrate(rate));
    println!("{} per thread", fmt_hashrate(rate / threads.max(1) as f64));
    Ok(())
}

/// Accept `node:rig` to address a rig on another machine. A bare name means
/// the local one, so nothing changes for single-machine use.
fn on_node(name: &str, build: impl Fn(String) -> Request) -> Request {
    match name.split_once(':') {
        Some((node, rig)) if !node.is_empty() && !rig.is_empty() => Request::OnNode {
            node: node.to_string(),
            request: Box::new(build(rig.to_string())),
        },
        _ => build(name.to_string()),
    }
}

/// Config on disk just changed; tell a running daemon to pick it up.
fn nudge_daemon() {
    if Client::probe()
        && let Ok(mut client) = Client::connect()
    {
        let _ = client.command(&Request::Reload);
    }
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        s.chars().take(n.saturating_sub(1)).collect::<String>() + "…"
    }
}

const STARTER_CONFIG: &str = r#"# cryptocli configuration
#
# Edit by hand or with `cryptocli wallet|rig|endpoint` subcommands.
# The daemon picks up changes on `cryptocli daemon reload` (or `r` in the TUI).

[settings]
# 0 = every logical core except one.
max_threads = 0
sample_interval_ms = 1000
log_lines = 500
history_len = 240
# Start enabled rigs as soon as the daemon comes up.
autostart = true

# Where you get paid. No private keys are ever stored or needed.
# [[wallet]]
# name = "main"
# coin = "BTC"
# address = "bc1qexampleexampleexampleexampleexampleexample"

# A pool connection. Add several to mine more than one at a time; threads are
# split between them by `weight` unless you pin a rig to a fixed count.
# [[rig]]
# name = "btc"
# url = "stratum+tcp://pool.example.com:3333"
# algo = "sha256d"
# wallet = "main"
# worker = "rig1"
# pass = "x"
# threads = 0
# weight = 1
# enabled = true

# Anything with a status URL can be watched here: pool APIs, explorers, your
# own boxes. `fields` pulls values out of a JSON response for the dashboard.
# [[endpoint]]
# name = "pool-stats"
# url = "https://pool.example.com/api/worker/rig1"
# method = "GET"
# interval_secs = 60
# timeout_secs = 10
# expect_status = 200
#
# [endpoint.headers]
# Authorization = "Bearer YOUR_TOKEN"
#
# [endpoint.fields]
# hashrate = "data.hashrate"
# balance = "data.balance"
"#;
