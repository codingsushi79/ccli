//! On-disk configuration: wallets, rigs (pool connections), and check endpoints.
//!
//! The daemon reads this file at startup and on `reload`; the CLI edits it in
//! place. Everything is plain TOML so it stays hand-editable.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;

use crate::paths;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub settings: Settings,
    #[serde(default, rename = "wallet")]
    pub wallets: Vec<Wallet>,
    #[serde(default, rename = "rig")]
    pub rigs: Vec<RigConfig>,
    #[serde(default, rename = "endpoint")]
    pub endpoints: Vec<EndpointConfig>,
    /// Other machines running cryptocli, shown and controlled from this one.
    #[serde(default, rename = "node")]
    pub nodes: Vec<NodeConfig>,
}

/// A peer machine. The local daemon connects to it as a client and merges its
/// snapshot into the dashboard.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeConfig {
    pub name: String,
    /// `host:port` of the peer's remote listener.
    pub address: String,
    /// Shared secret configured on the peer with `cryptocli remote enable`.
    pub token: String,
    /// The peer's TLS certificate fingerprint, pinned when the node was added.
    #[serde(default)]
    pub fingerprint: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
}

/// Settings for accepting connections from other machines. Off unless a
/// listen address and token are both set.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RemoteSettings {
    /// e.g. `0.0.0.0:9944`. Unset means local-only.
    #[serde(default)]
    pub listen: Option<String>,
    /// Shared secret peers must present.
    #[serde(default)]
    pub token: Option<String>,
}

impl RemoteSettings {
    pub fn active(&self) -> Option<(&str, &str)> {
        match (&self.listen, &self.token) {
            (Some(listen), Some(token)) if !listen.is_empty() && !token.is_empty() => {
                Some((listen, token))
            }
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Settings {
    /// Upper bound on hashing threads across every rig. 0 = all logical cores
    /// minus one (leaves a core for the UI and the OS).
    #[serde(default)]
    pub max_threads: usize,
    /// How often the daemon samples hashrate and hardware, in milliseconds.
    #[serde(default = "default_sample_ms")]
    pub sample_interval_ms: u64,
    /// Ring buffer size for the in-memory log the TUI renders.
    #[serde(default = "default_log_lines")]
    pub log_lines: usize,
    /// Sparkline history depth (samples).
    #[serde(default = "default_history")]
    pub history_len: usize,
    /// Start rigs marked `enabled` as soon as the daemon comes up.
    #[serde(default = "default_true")]
    pub autostart: bool,
    /// How this machine identifies itself to other dashboards.
    #[serde(default)]
    pub node_name: Option<String>,
    /// Accepting connections from other machines.
    #[serde(default)]
    pub remote: RemoteSettings,
}

fn default_sample_ms() -> u64 {
    1000
}
fn default_log_lines() -> usize {
    500
}
fn default_history() -> usize {
    240
}
fn default_true() -> bool {
    true
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            max_threads: 0,
            sample_interval_ms: default_sample_ms(),
            log_lines: default_log_lines(),
            history_len: default_history(),
            autostart: default_true(),
            node_name: None,
            remote: RemoteSettings::default(),
        }
    }
}

impl Settings {
    /// This machine's name in a multi-node dashboard.
    pub fn node_name(&self) -> String {
        self.node_name.clone().unwrap_or_else(hostname)
    }

    pub fn thread_budget(&self) -> usize {
        if self.max_threads > 0 {
            return self.max_threads;
        }
        let cores = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1);
        cores.saturating_sub(1).max(1)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Wallet {
    pub name: String,
    pub coin: String,
    pub address: String,
    #[serde(default)]
    pub label: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RigConfig {
    pub name: String,
    /// `stratum+tcp://host:port` or bare `host:port`. May be empty if the rig
    /// only uses `[[rig.target]]` entries.
    #[serde(default)]
    pub url: String,
    /// Algorithm id, see `mining::algo`.
    #[serde(default = "default_algo")]
    pub algo: String,
    /// Label for what this rig is mining, e.g. "BTC". Purely informational,
    /// but it drives the per-coin totals on the dashboard.
    #[serde(default)]
    pub coin: Option<String>,
    /// Pool username. Usually `<wallet address>.<worker>`; if `wallet` is set
    /// and this is empty it is derived from the wallet.
    #[serde(default)]
    pub user: String,
    #[serde(default = "default_pass")]
    pub pass: String,
    /// Name of a configured wallet to mine to.
    #[serde(default)]
    pub wallet: Option<String>,
    #[serde(default)]
    pub worker: Option<String>,
    /// Hashing threads. 0 = share the global budget with the other active rigs.
    #[serde(default)]
    pub threads: usize,
    /// Weight used when threads are auto-allocated across rigs.
    #[serde(default = "default_weight")]
    pub weight: u32,
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Additional coins mined by this same rig, at the same time. Each target
    /// becomes its own pool session; the rig's threads are divided between
    /// them by weight. This is the "both coins on one rig" arrangement — use
    /// separate `[[rig]]` entries instead if you want them managed separately.
    #[serde(default, rename = "target")]
    pub targets: Vec<RigTarget>,
}

/// One coin mined by a rig. Anything left unset falls back to the rig.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RigTarget {
    pub coin: String,
    pub url: String,
    #[serde(default)]
    pub algo: Option<String>,
    #[serde(default)]
    pub wallet: Option<String>,
    #[serde(default)]
    pub worker: Option<String>,
    #[serde(default)]
    pub user: Option<String>,
    #[serde(default)]
    pub pass: Option<String>,
    #[serde(default = "default_weight")]
    pub weight: u32,
}

/// A pool url plus any credentials inherited from the endpoint it came from.
#[derive(Debug, Clone)]
pub struct PoolRef {
    pub url: String,
    pub user: Option<String>,
    pub pass: Option<String>,
    /// Set when the reference named an endpoint rather than a url.
    pub from_endpoint: Option<String>,
}

/// A rig target flattened into something the engine can run directly.
#[derive(Debug, Clone)]
pub struct ResolvedTarget {
    /// Unique id: the rig name, or `rig/COIN` when a rig mines several coins.
    pub id: String,
    /// Owning rig name.
    pub group: String,
    pub coin: String,
    pub host_port: String,
    pub algo: String,
    pub user: String,
    pub pass: String,
    pub weight: u32,
}

fn default_algo() -> String {
    "sha256d".into()
}
fn default_pass() -> String {
    "x".into()
}
fn default_weight() -> u32 {
    1
}

/// Strip the scheme off a pool url and check it has a port.
pub fn host_port(url: &str, context: &str) -> Result<String> {
    // Accepting an ssl scheme and then connecting in plaintext would fail in a
    // confusing way, so say plainly that TLS is not implemented.
    if url
        .trim()
        .to_ascii_lowercase()
        .starts_with("stratum+ssl://")
    {
        bail!(
            "{context} uses stratum+ssl:// but TLS pools are not supported yet — \
             use the pool's plain stratum+tcp:// port"
        );
    }
    let raw = url
        .trim()
        .trim_start_matches("stratum+tcp://")
        .trim_start_matches("stratum://")
        .trim_start_matches("tcp://")
        .trim_end_matches('/');
    if raw.is_empty() {
        bail!("{context} has an empty pool url");
    }
    if !raw.contains(':') {
        bail!("{context} pool url `{url}` is missing a port");
    }
    Ok(raw.to_string())
}

/// Combine a wallet address and worker name into a stratum login.
fn login(
    explicit: Option<&str>,
    wallet: Option<&str>,
    worker: Option<&str>,
    wallets: &[Wallet],
    context: &str,
) -> Result<String> {
    if let Some(user) = explicit
        && !user.is_empty()
    {
        return Ok(user.to_string());
    }
    let Some(name) = wallet else {
        bail!("{context} has neither `user` nor `wallet` set");
    };
    let w = wallets
        .iter()
        .find(|w| w.name == name)
        .with_context(|| format!("{context} references unknown wallet `{name}`"))?;
    Ok(match worker {
        Some(worker) if !worker.is_empty() => format!("{}.{}", w.address, worker),
        _ => w.address.clone(),
    })
}

impl RigConfig {
    /// Every coin this rig mines, flattened. A rig with no extra targets
    /// yields exactly one entry, so single-coin rigs cost nothing extra.
    pub fn expand(&self, wallets: &[Wallet]) -> Result<Vec<ResolvedTarget>> {
        let mut raw: Vec<RigTarget> = Vec::new();
        if !self.url.trim().is_empty() {
            raw.push(RigTarget {
                coin: self.coin.clone().unwrap_or_default(),
                url: self.url.clone(),
                algo: Some(self.algo.clone()),
                wallet: self.wallet.clone(),
                worker: self.worker.clone(),
                user: if self.user.is_empty() {
                    None
                } else {
                    Some(self.user.clone())
                },
                pass: Some(self.pass.clone()),
                weight: self.weight.max(1),
            });
        }
        raw.extend(self.targets.iter().cloned());
        if raw.is_empty() {
            bail!("rig `{}` has no pool url and no targets", self.name);
        }

        let multi = raw.len() > 1;
        let mut out = Vec::with_capacity(raw.len());
        for target in raw {
            let coin = if target.coin.is_empty() {
                "-".to_string()
            } else {
                target.coin.to_ascii_uppercase()
            };
            let id = if multi {
                format!("{}/{}", self.name, coin)
            } else {
                self.name.clone()
            };
            let context = format!("rig `{id}`");
            out.push(ResolvedTarget {
                host_port: host_port(&target.url, &context)?,
                user: login(
                    target.user.as_deref(),
                    target.wallet.as_deref().or(self.wallet.as_deref()),
                    target.worker.as_deref().or(self.worker.as_deref()),
                    wallets,
                    &context,
                )?,
                pass: target.pass.unwrap_or_else(|| self.pass.clone()),
                algo: target.algo.unwrap_or_else(|| self.algo.clone()),
                weight: target.weight.max(1),
                coin,
                group: self.name.clone(),
                id,
            });
        }

        let mut seen = std::collections::HashSet::new();
        for target in &out {
            if !seen.insert(target.id.clone()) {
                bail!(
                    "rig `{}` mines `{}` more than once; give each target a distinct coin",
                    self.name,
                    target.coin
                );
            }
        }
        Ok(out)
    }

    /// `host:port` of the rig's primary pool, for display before it starts.
    pub fn host_port(&self) -> Result<String> {
        if !self.url.trim().is_empty() {
            return host_port(&self.url, &format!("rig `{}`", self.name));
        }
        match self.targets.first() {
            Some(target) => host_port(&target.url, &format!("rig `{}`", self.name)),
            None => bail!("rig `{}` has no pool url", self.name),
        }
    }

    /// Coins this rig mines, for display.
    pub fn coins(&self) -> Vec<String> {
        let mut out = Vec::new();
        if !self.url.trim().is_empty() {
            out.push(
                self.coin
                    .clone()
                    .unwrap_or_else(|| "-".into())
                    .to_ascii_uppercase(),
            );
        }
        for target in &self.targets {
            out.push(target.coin.to_ascii_uppercase());
        }
        out
    }
}

/// A user-registered HTTP check. Pools, explorers, or any site that exposes a
/// status/stats URL can be dropped in here and the daemon will poll it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EndpointConfig {
    pub name: String,
    pub url: String,
    #[serde(default = "default_method")]
    pub method: String,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    #[serde(default)]
    pub body: Option<String>,
    #[serde(default = "default_interval")]
    pub interval_secs: u64,
    #[serde(default = "default_timeout")]
    pub timeout_secs: u64,
    /// Expected HTTP status. Anything else counts as a failure.
    #[serde(default = "default_status")]
    pub expect_status: u16,
    /// Substring that must appear in the response body, if set.
    #[serde(default)]
    pub expect_body: Option<String>,
    /// `label = json.path` pairs pulled out of the response and shown in the
    /// dashboard, e.g. `balance = "data.miner.balance"`.
    #[serde(default)]
    pub fields: BTreeMap<String, String>,
    /// Credentials. For a stratum url these are the pool login (for
    /// unmineable and friends, something like `BTC:youraddress.worker` and
    /// `x`); for an http url they become HTTP basic auth.
    #[serde(default)]
    pub user: Option<String>,
    #[serde(default)]
    pub password: Option<String>,
    #[serde(default = "default_true")]
    pub enabled: bool,
}

fn default_method() -> String {
    "GET".into()
}
fn default_interval() -> u64 {
    60
}
fn default_timeout() -> u64 {
    10
}
fn default_status() -> u16 {
    200
}

impl Config {
    pub fn load() -> Result<Self> {
        Self::load_from(&paths::config_path())
    }

    pub fn load_from(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let raw =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let cfg: Config =
            toml::from_str(&raw).with_context(|| format!("parsing {}", path.display()))?;
        cfg.validate()?;
        Ok(cfg)
    }

    pub fn save(&self) -> Result<()> {
        paths::ensure_dirs()?;
        let path = paths::config_path();
        let text = toml::to_string_pretty(self)?;
        std::fs::write(&path, text).with_context(|| format!("writing {}", path.display()))?;
        Ok(())
    }

    pub fn validate(&self) -> Result<()> {
        let mut seen = std::collections::HashSet::new();
        for r in &self.rigs {
            if !seen.insert(&r.name) {
                bail!("duplicate rig name `{}`", r.name);
            }
            for target in r.expand(&self.wallets).unwrap_or_default() {
                if crate::mining::algo::lookup(&target.algo).is_none() {
                    bail!(
                        "rig `{}` uses unknown algo `{}` (known: {})",
                        target.id,
                        target.algo,
                        crate::mining::algo::names().join(", ")
                    );
                }
            }
            r.host_port()?;
        }
        let mut seen = std::collections::HashSet::new();
        for e in &self.endpoints {
            if !seen.insert(&e.name) {
                bail!("duplicate endpoint name `{}`", e.name);
            }
        }
        let mut seen = std::collections::HashSet::new();
        for n in &self.nodes {
            if !seen.insert(&n.name) {
                bail!("duplicate node name `{}`", n.name);
            }
            if !n.address.contains(':') {
                bail!("node `{}` address `{}` needs a port", n.name, n.address);
            }
        }
        let mut seen = std::collections::HashSet::new();
        for w in &self.wallets {
            if !seen.insert(&w.name) {
                bail!("duplicate wallet name `{}`", w.name);
            }
        }
        Ok(())
    }

    pub fn rig(&self, name: &str) -> Option<&RigConfig> {
        self.rigs.iter().find(|r| r.name == name)
    }

    pub fn endpoint(&self, name: &str) -> Option<&EndpointConfig> {
        self.endpoints.iter().find(|e| e.name == name)
    }

    /// Resolve a pool reference, which may be a literal url *or* the name of a
    /// configured stratum endpoint. Referencing an endpoint reuses its url and
    /// credentials, so a pool you already checked doesn't have to be retyped.
    pub fn resolve_pool(&self, reference: &str) -> Result<PoolRef> {
        let reference = reference.trim();
        if reference.is_empty() {
            bail!("give a pool url or the name of an endpoint");
        }
        if let Some(endpoint) = self.endpoint(reference) {
            if endpoint_kind(&endpoint.url) != crate::model::EndpointKind::Stratum {
                bail!(
                    "endpoint `{reference}` is an HTTP check ({}), not a pool — \
                     pass a stratum url instead",
                    endpoint.url
                );
            }
            host_port(&endpoint.url, &format!("endpoint `{reference}`"))?;
            return Ok(PoolRef {
                url: endpoint.url.clone(),
                user: endpoint.user.clone().filter(|u| !u.is_empty()),
                pass: endpoint.password.clone().filter(|p| !p.is_empty()),
                from_endpoint: Some(reference.to_string()),
            });
        }
        // Not a known endpoint, so it has to stand on its own as a url. A bare
        // word is much more likely to be a typo'd endpoint name than a pool.
        if !reference.contains(':') {
            bail!(
                "`{reference}` is neither a pool url nor a configured endpoint \
                 (add one with `cryptocli endpoint add`, or pass stratum+tcp://host:port)"
            );
        }
        host_port(reference, "rig")?;
        Ok(PoolRef {
            url: reference.to_string(),
            user: None,
            pass: None,
            from_endpoint: None,
        })
    }
}

/// Best-effort hostname, used as the default node name.
pub fn hostname() -> String {
    std::fs::read_to_string("/etc/hostname")
        .ok()
        .map(|h| h.trim().to_string())
        .filter(|h| !h.is_empty())
        .or_else(|| std::env::var("HOSTNAME").ok())
        .unwrap_or_else(|| "local".into())
}

/// Which sort of check a url implies. Stratum pool addresses are recognised
/// by their scheme, or by looking like a bare `host:port`.
pub fn endpoint_kind(url: &str) -> crate::model::EndpointKind {
    use crate::model::EndpointKind;
    let url = url.trim();
    let lower = url.to_ascii_lowercase();
    if lower.starts_with("http://") || lower.starts_with("https://") {
        return EndpointKind::Http;
    }
    if lower.starts_with("stratum+tcp://")
        || lower.starts_with("stratum+ssl://")
        || lower.starts_with("stratum://")
        || lower.starts_with("tcp://")
    {
        return EndpointKind::Stratum;
    }
    // A bare `host:port` with no path is a pool address, not a web url.
    if !url.contains('/')
        && url
            .rsplit(':')
            .next()
            .is_some_and(|p| p.parse::<u16>().is_ok())
    {
        return EndpointKind::Stratum;
    }
    EndpointKind::Http
}

/// Very light sanity check on a payout address. We deliberately do not try to
/// be a full validator for every chain — this catches typos and pasted
/// whitespace, nothing more.
pub fn check_address(coin: &str, address: &str) -> Result<()> {
    let a = address.trim();
    if a.len() != address.len() {
        bail!("address has leading or trailing whitespace");
    }
    if a.len() < 20 || a.len() > 128 {
        bail!("address length {} looks wrong", a.len());
    }
    if a.chars().any(|c| c.is_whitespace()) {
        bail!("address contains whitespace");
    }
    let coin = coin.to_ascii_uppercase();
    if (coin == "BTC" || coin == "BCH" || coin == "LTC" || coin == "DOGE")
        && !a.chars().all(|c| c.is_ascii_alphanumeric())
    {
        bail!("{coin} address should be alphanumeric");
    }
    if coin == "ETH" && !(a.starts_with("0x") && a.len() == 42) {
        bail!("ETH address should be 0x followed by 40 hex characters");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::EndpointKind;

    #[test]
    fn pool_urls_are_recognised_as_stratum_checks() {
        for url in [
            "stratum+tcp://sha256.unmineable.com:3333",
            "stratum+ssl://pool.example.com:443",
            "stratum://pool.example.com:3333",
            "tcp://127.0.0.1:3333",
            "127.0.0.1:13333",
            "pool.example.com:3333",
        ] {
            assert_eq!(endpoint_kind(url), EndpointKind::Stratum, "{url}");
        }
    }

    #[test]
    fn web_urls_stay_http_checks() {
        for url in [
            "https://pool.example.com/api/worker/rig1",
            "http://127.0.0.1:8899/",
            "https://example.com:8443/stats",
        ] {
            assert_eq!(endpoint_kind(url), EndpointKind::Http, "{url}");
        }
    }

    fn config_with_endpoint() -> Config {
        let mut config = Config::default();
        config.endpoints.push(EndpointConfig {
            name: "unmineable".into(),
            url: "stratum+tcp://sha256.unmineable.com:3333".into(),
            method: "GET".into(),
            headers: Default::default(),
            body: None,
            interval_secs: 60,
            timeout_secs: 10,
            expect_status: 200,
            expect_body: None,
            fields: Default::default(),
            user: Some("BTC:addr.worker".into()),
            password: Some("x".into()),
            enabled: true,
        });
        config.endpoints.push(EndpointConfig {
            name: "stats".into(),
            url: "https://pool.example.com/api".into(),
            method: "GET".into(),
            headers: Default::default(),
            body: None,
            interval_secs: 60,
            timeout_secs: 10,
            expect_status: 200,
            expect_body: None,
            fields: Default::default(),
            user: None,
            password: None,
            enabled: true,
        });
        config
    }

    #[test]
    fn a_rig_can_name_an_endpoint_instead_of_a_url() {
        let config = config_with_endpoint();
        let pool = config.resolve_pool("unmineable").unwrap();
        assert_eq!(pool.url, "stratum+tcp://sha256.unmineable.com:3333");
        assert_eq!(pool.user.as_deref(), Some("BTC:addr.worker"));
        assert_eq!(pool.pass.as_deref(), Some("x"));
        assert_eq!(pool.from_endpoint.as_deref(), Some("unmineable"));
    }

    #[test]
    fn a_literal_url_still_works() {
        let config = config_with_endpoint();
        let pool = config.resolve_pool("stratum+tcp://other:3333").unwrap();
        assert_eq!(pool.url, "stratum+tcp://other:3333");
        assert!(pool.from_endpoint.is_none());
        assert!(pool.user.is_none());
    }

    #[test]
    fn http_endpoints_are_not_pools() {
        let config = config_with_endpoint();
        let err = config.resolve_pool("stats").unwrap_err().to_string();
        assert!(err.contains("HTTP check"), "{err}");
    }

    #[test]
    fn an_unknown_bare_name_is_a_helpful_error() {
        let config = config_with_endpoint();
        let err = config.resolve_pool("unmineble").unwrap_err().to_string();
        assert!(
            err.contains("neither a pool url nor a configured endpoint"),
            "{err}"
        );
    }

    #[test]
    fn tls_pools_are_rejected_rather_than_silently_downgraded() {
        let err = host_port("stratum+ssl://pool.example.com:443", "rig `x`")
            .unwrap_err()
            .to_string();
        assert!(err.contains("TLS"), "{err}");
    }

    #[test]
    fn pool_urls_keep_their_host_and_port() {
        assert_eq!(
            host_port("stratum+tcp://sha256.unmineable.com:3333", "t").unwrap(),
            "sha256.unmineable.com:3333"
        );
        assert_eq!(host_port("127.0.0.1:3333", "t").unwrap(), "127.0.0.1:3333");
        assert!(host_port("pool.example.com", "t").is_err(), "port required");
    }
}
