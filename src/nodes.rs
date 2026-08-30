//! Peer machines.
//!
//! The local daemon doubles as a hub: it holds a client connection to every
//! configured peer, polls their snapshots on its own timer, and merges them
//! into the one the dashboard renders. Commands aimed at a peer's rig are
//! forwarded over the same connection.
//!
//! Polling is deliberately done by the daemon rather than the TUI, so the
//! aggregate view exists whether or not anyone is looking at it, and a slow or
//! dead peer can never stall the dashboard.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::config::NodeConfig;
use crate::ipc::{Client, Request};
use crate::model::{NodeStatus, Snapshot};

/// How long to wait on a peer before treating it as down.
const TIMEOUT: Duration = Duration::from_secs(5);

pub struct PeerNode {
    pub cfg: NodeConfig,
    /// Reconnected lazily; `None` means "not currently connected".
    client: Mutex<Option<Client>>,
    state: Mutex<PeerState>,
}

#[derive(Default)]
struct PeerState {
    snapshot: Option<Snapshot>,
    latency_ms: Option<u64>,
    last_error: Option<String>,
    online: bool,
}

impl PeerNode {
    pub fn new(cfg: NodeConfig) -> Self {
        Self {
            cfg,
            client: Mutex::new(None),
            state: Mutex::new(PeerState::default()),
        }
    }

    /// Fetch a fresh snapshot. Blocking; callers run it off the async runtime.
    pub fn poll(&self) {
        let started = Instant::now();
        let result = self.with_client(|client| client.snapshot());
        let mut state = self.state.lock().unwrap();
        match result {
            Ok(snapshot) => {
                state.snapshot = Some(snapshot);
                state.latency_ms = Some(started.elapsed().as_millis() as u64);
                state.last_error = None;
                state.online = true;
            }
            Err(err) => {
                state.online = false;
                state.latency_ms = None;
                state.last_error = Some(format!("{err:#}"));
                // Keep the last snapshot so the dashboard can still show what
                // the peer was doing when it went away.
            }
        }
    }

    /// Send a command to the peer.
    pub fn command(&self, request: &Request) -> anyhow::Result<String> {
        self.with_client(|client| client.command(request))
    }

    /// Run `f` against a live connection, reconnecting once if needed.
    fn with_client<T>(&self, f: impl Fn(&mut Client) -> anyhow::Result<T>) -> anyhow::Result<T> {
        let mut guard = self.client.lock().unwrap();
        if let Some(client) = guard.as_mut()
            && let Ok(value) = f(client)
        {
            return Ok(value);
        }
        // Either we had no connection or the existing one just failed; a peer
        // restart is the common case, so reconnect and try once more.
        *guard = None;
        let mut client = Client::connect_remote(
            &self.cfg.address,
            &self.cfg.token,
            &self.cfg.fingerprint,
            TIMEOUT,
        )?;
        let value = f(&mut client)?;
        *guard = Some(client);
        Ok(value)
    }

    /// Last known snapshot, if we ever got one.
    pub fn snapshot(&self) -> Option<Snapshot> {
        self.state.lock().unwrap().snapshot.clone()
    }

    pub fn status(&self) -> NodeStatus {
        let state = self.state.lock().unwrap();
        let snapshot = state.snapshot.as_ref();
        NodeStatus {
            name: self.cfg.name.clone(),
            address: self.cfg.address.clone(),
            local: false,
            online: state.online,
            version: snapshot
                .map(|s| s.daemon.version.clone())
                .unwrap_or_default(),
            latency_ms: state.latency_ms,
            uptime_secs: snapshot.map(|s| s.daemon.uptime_secs).unwrap_or(0),
            hashrate: if state.online {
                snapshot.map(|s| s.totals.hashrate).unwrap_or(0.0)
            } else {
                0.0
            },
            rigs_active: if state.online {
                snapshot.map(|s| s.totals.rigs_active).unwrap_or(0)
            } else {
                0
            },
            rigs_total: snapshot.map(|s| s.totals.rigs_total).unwrap_or(0),
            threads: if state.online {
                snapshot.map(|s| s.totals.threads_active).unwrap_or(0)
            } else {
                0
            },
            accepted: snapshot.map(|s| s.totals.accepted).unwrap_or(0),
            rejected: snapshot.map(|s| s.totals.rejected).unwrap_or(0),
            cpu_usage: snapshot.map(|s| s.hardware.cpu_usage).unwrap_or(0.0),
            hottest_c: snapshot.and_then(|s| s.hardware.temps.first().map(|t| t.celsius)),
            last_error: state.last_error.clone(),
        }
    }
}

/// Compare tokens without leaking their contents through timing.
pub fn token_matches(expected: &str, presented: &str) -> bool {
    if expected.len() != presented.len() {
        return false;
    }
    let mut difference = 0u8;
    for (a, b) in expected.bytes().zip(presented.bytes()) {
        difference |= a ^ b;
    }
    difference == 0
}

/// Generate a token for `cryptocli remote enable`.
pub fn generate_token() -> String {
    // 160 bits from the OS, hex encoded. No dependency needed for this.
    // Note: read_exact, not fs::read — /dev/urandom never reaches EOF.
    use std::io::Read;
    let mut bytes = [0u8; 20];
    if let Ok(mut file) = std::fs::File::open("/dev/urandom") {
        let _ = file.read_exact(&mut bytes);
    }
    if bytes.iter().all(|b| *b == 0) {
        // Fallback: still unpredictable enough to not be guessable offline,
        // and this path should never be taken on a working system.
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        for (i, b) in now.to_le_bytes().iter().enumerate() {
            bytes[i] = *b;
        }
    }
    hex::encode(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_must_match_exactly() {
        assert!(token_matches("secret", "secret"));
        assert!(!token_matches("secret", "secrez"));
        assert!(!token_matches("secret", "secretx"));
        assert!(!token_matches("secret", ""));
        assert!(!token_matches("", "secret"));
        assert!(token_matches("", ""));
    }

    #[test]
    fn generated_tokens_are_long_and_distinct() {
        let a = generate_token();
        let b = generate_token();
        assert_eq!(a.len(), 40);
        assert_ne!(a, b, "tokens must not repeat");
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
    }
}
