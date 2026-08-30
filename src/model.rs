//! Wire types shared by the daemon and every client (TUI, one-shot CLI).

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RigState {
    Stopped,
    Connecting,
    Authorizing,
    Mining,
    Retrying,
    Error,
    /// The machine reporting this rig is unreachable, so its real state is
    /// genuinely unknown rather than any of the above.
    Unknown,
}

impl RigState {
    pub fn label(&self) -> &'static str {
        match self {
            RigState::Stopped => "stopped",
            RigState::Connecting => "connecting",
            RigState::Authorizing => "authorizing",
            RigState::Mining => "mining",
            RigState::Retrying => "retrying",
            RigState::Error => "error",
            RigState::Unknown => "unknown",
        }
    }

    pub fn is_live(&self) -> bool {
        !matches!(self, RigState::Stopped)
    }

    pub fn from_u8(v: u8) -> Self {
        match v {
            1 => RigState::Connecting,
            2 => RigState::Authorizing,
            3 => RigState::Mining,
            4 => RigState::Retrying,
            5 => RigState::Error,
            6 => RigState::Unknown,
            _ => RigState::Stopped,
        }
    }

    pub fn as_u8(&self) -> u8 {
        match self {
            RigState::Stopped => 0,
            RigState::Connecting => 1,
            RigState::Authorizing => 2,
            RigState::Mining => 3,
            RigState::Retrying => 4,
            RigState::Error => 5,
            RigState::Unknown => 6,
        }
    }
}

/// One machine in the dashboard: this one, or a configured peer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeStatus {
    pub name: String,
    pub address: String,
    /// True for the daemon the dashboard is attached to.
    pub local: bool,
    pub online: bool,
    pub version: String,
    pub latency_ms: Option<u64>,
    pub uptime_secs: u64,
    pub hashrate: f64,
    pub rigs_active: usize,
    pub rigs_total: usize,
    pub threads: usize,
    pub accepted: u64,
    pub rejected: u64,
    pub cpu_usage: f32,
    pub hottest_c: Option<f32>,
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RigStatus {
    /// Machine this rig runs on.
    #[serde(default)]
    pub node: String,
    pub name: String,
    /// Owning rig; equals `name` unless the rig mines several coins.
    pub group: String,
    pub coin: String,
    pub enabled: bool,
    pub state: RigState,
    pub algo: String,
    pub pool: String,
    pub user: String,
    pub threads: usize,
    /// Hashes per second, smoothed.
    pub hashrate: f64,
    pub hashrate_avg: f64,
    pub history: Vec<u64>,
    pub hashes_total: u64,
    pub accepted: u64,
    pub rejected: u64,
    pub stale: u64,
    pub difficulty: f64,
    pub best_share: f64,
    pub job_id: String,
    pub last_share_secs: Option<u64>,
    pub uptime_secs: u64,
    pub latency_ms: Option<u64>,
    pub reconnects: u64,
    pub last_error: Option<String>,
}

/// Aggregated across every session mining the same coin.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CoinTotals {
    pub coin: String,
    pub hashrate: f64,
    pub accepted: u64,
    pub rejected: u64,
    pub sessions: usize,
    pub active: usize,
    pub threads: usize,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Totals {
    pub hashrate: f64,
    pub hashrate_avg: f64,
    pub history: Vec<u64>,
    pub accepted: u64,
    pub rejected: u64,
    pub stale: u64,
    pub hashes_total: u64,
    pub rigs_active: usize,
    pub rigs_total: usize,
    pub threads_active: usize,
    pub threads_budget: usize,
    /// One entry per coin being mined, however the rigs are arranged.
    #[serde(default)]
    pub coins: Vec<CoinTotals>,
    /// Active hashing backend, e.g. "AVX2 8-way".
    #[serde(default)]
    pub backend: String,
    /// Work units handed out by the coordinator, and how many distinct search
    /// spaces they cover.
    #[serde(default)]
    pub work_units: u64,
    #[serde(default)]
    pub work_spaces: usize,
    /// Times a session joined a space another session already held — each one
    /// is duplicated work that did not happen.
    #[serde(default)]
    pub shared_spaces: usize,
    /// Peers online / configured, counting this machine.
    #[serde(default)]
    pub nodes_online: usize,
    #[serde(default)]
    pub nodes_total: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TempSensor {
    pub label: String,
    pub celsius: f32,
    pub max: Option<f32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GpuInfo {
    pub index: usize,
    pub name: String,
    pub util_percent: Option<f32>,
    pub mem_used_mb: Option<u64>,
    pub mem_total_mb: Option<u64>,
    pub temp_c: Option<f32>,
    pub power_w: Option<f32>,
    pub fan_percent: Option<f32>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HardwareSnapshot {
    pub cpu_brand: String,
    pub cpu_arch: String,
    pub cores_physical: usize,
    pub cores_logical: usize,
    pub cpu_usage: f32,
    pub per_core: Vec<f32>,
    pub freq_mhz: u64,
    pub mem_used: u64,
    pub mem_total: u64,
    pub swap_used: u64,
    pub swap_total: u64,
    pub load_avg: [f64; 3],
    pub temps: Vec<TempSensor>,
    pub gpus: Vec<GpuInfo>,
    pub host_uptime_secs: u64,
    pub proc_cpu: f32,
    pub proc_mem: u64,
    pub os: String,
}

/// What kind of thing an endpoint points at, inferred from its url.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EndpointKind {
    Http,
    /// A stratum pool: checked by connecting, subscribing and authorizing.
    Stratum,
}

impl EndpointKind {
    pub fn label(&self) -> &'static str {
        match self {
            EndpointKind::Http => "http",
            EndpointKind::Stratum => "pool",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EndpointStatus {
    #[serde(default)]
    pub node: String,
    pub name: String,
    pub url: String,
    #[serde(default = "default_kind")]
    pub kind: EndpointKind,
    pub enabled: bool,
    pub interval_secs: u64,
    pub ok: Option<bool>,
    pub http_status: Option<u16>,
    pub latency_ms: Option<u64>,
    pub last_check_secs: Option<u64>,
    pub next_check_secs: Option<u64>,
    pub checks: u64,
    pub failures: u64,
    pub uptime_pct: f64,
    pub last_error: Option<String>,
    /// Extracted `label -> value` pairs, in config order.
    pub fields: Vec<(String, String)>,
}

fn default_kind() -> EndpointKind {
    EndpointKind::Http
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WalletView {
    #[serde(default)]
    pub node: String,
    pub name: String,
    pub coin: String,
    pub address: String,
    pub label: Option<String>,
    pub rigs: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogLevel {
    Debug,
    Info,
    Warn,
    Error,
    Share,
}

impl LogLevel {
    pub fn label(&self) -> &'static str {
        match self {
            LogLevel::Debug => "DBG",
            LogLevel::Info => "INF",
            LogLevel::Warn => "WRN",
            LogLevel::Error => "ERR",
            LogLevel::Share => "SHR",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogLine {
    #[serde(default)]
    pub node: String,
    pub seq: u64,
    pub ts: i64,
    pub level: LogLevel,
    pub source: String,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DaemonInfo {
    pub pid: u32,
    pub version: String,
    pub started_at: i64,
    pub uptime_secs: u64,
    pub config_path: String,
    pub socket_path: String,
    pub log_path: String,
    pub config_error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Snapshot {
    pub daemon: DaemonInfo,
    /// This machine plus every configured peer.
    #[serde(default)]
    pub nodes: Vec<NodeStatus>,
    pub totals: Totals,
    pub rigs: Vec<RigStatus>,
    pub hardware: HardwareSnapshot,
    pub endpoints: Vec<EndpointStatus>,
    pub wallets: Vec<WalletView>,
    pub logs: Vec<LogLine>,
}

/// Human formatting helpers, shared by the TUI and the plain CLI output.
pub fn fmt_hashrate(hs: f64) -> String {
    const UNITS: [&str; 6] = ["H/s", "kH/s", "MH/s", "GH/s", "TH/s", "PH/s"];
    if !hs.is_finite() || hs <= 0.0 {
        return "0.00 H/s".into();
    }
    let mut v = hs;
    let mut i = 0;
    while v >= 1000.0 && i < UNITS.len() - 1 {
        v /= 1000.0;
        i += 1;
    }
    format!("{v:.2} {}", UNITS[i])
}

pub fn fmt_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = bytes as f64;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    format!("{v:.1} {}", UNITS[i])
}

pub fn fmt_duration(secs: u64) -> String {
    let d = secs / 86400;
    let h = (secs % 86400) / 3600;
    let m = (secs % 3600) / 60;
    let s = secs % 60;
    if d > 0 {
        format!("{d}d {h:02}h {m:02}m")
    } else if h > 0 {
        format!("{h}h {m:02}m {s:02}s")
    } else if m > 0 {
        format!("{m}m {s:02}s")
    } else {
        format!("{s}s")
    }
}

pub fn fmt_count(n: f64) -> String {
    if !n.is_finite() {
        return "-".into();
    }
    if n >= 1e12 {
        format!("{:.2}T", n / 1e12)
    } else if n >= 1e9 {
        format!("{:.2}G", n / 1e9)
    } else if n >= 1e6 {
        format!("{:.2}M", n / 1e6)
    } else if n >= 1e3 {
        format!("{:.2}k", n / 1e3)
    } else if n >= 10.0 {
        format!("{n:.0}")
    } else if n > 0.0 {
        // Sub-unit values matter for share difficulty on low-diff pools.
        format!("{n:.4}")
    } else {
        "0".into()
    }
}

/// Pool difficulty, which ranges from thousandths on test pools to millions on
/// real ones.
pub fn fmt_difficulty(d: f64) -> String {
    if !d.is_finite() || d <= 0.0 {
        "-".into()
    } else if d >= 1000.0 {
        fmt_count(d)
    } else if d >= 1.0 {
        format!("{d:.2}")
    } else {
        format!("{d:.4}")
    }
}
