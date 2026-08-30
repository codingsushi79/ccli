//! User-registered HTTP checks.
//!
//! Any site that exposes a status or stats URL — a pool's worker API, an
//! explorer, a balance endpoint, your own box — can be registered and the
//! daemon will poll it on its own interval, record availability, and pull named
//! values out of the JSON response for the dashboard.

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::config::EndpointConfig;
use crate::log::LogSink;
use crate::model::{EndpointKind, EndpointStatus};

#[derive(Default, Clone)]
struct LastResult {
    ok: Option<bool>,
    http_status: Option<u16>,
    latency_ms: Option<u64>,
    checked_at: Option<i64>,
    error: Option<String>,
    fields: Vec<(String, String)>,
}

pub struct EndpointRuntime {
    pub cfg: EndpointConfig,
    checks: AtomicU64,
    failures: AtomicU64,
    last: Mutex<LastResult>,
    next_due: Mutex<Instant>,
}

impl EndpointRuntime {
    pub fn new(cfg: EndpointConfig) -> Self {
        Self {
            cfg,
            checks: AtomicU64::new(0),
            failures: AtomicU64::new(0),
            last: Mutex::new(LastResult::default()),
            next_due: Mutex::new(Instant::now()),
        }
    }

    pub fn due(&self) -> bool {
        self.cfg.enabled && Instant::now() >= *self.next_due.lock().unwrap()
    }

    pub fn schedule_next(&self) {
        let interval = Duration::from_secs(self.cfg.interval_secs.max(1));
        *self.next_due.lock().unwrap() = Instant::now() + interval;
    }

    pub fn status(&self) -> EndpointStatus {
        let last = self.last.lock().unwrap().clone();
        let checks = self.checks.load(Ordering::Relaxed);
        let failures = self.failures.load(Ordering::Relaxed);
        let now = chrono::Utc::now().timestamp();
        let next = self
            .next_due
            .lock()
            .unwrap()
            .saturating_duration_since(Instant::now());
        EndpointStatus {
            // Filled in by the daemon, which knows which machine this is.
            node: String::new(),
            name: self.cfg.name.clone(),
            url: self.cfg.url.clone(),
            kind: crate::config::endpoint_kind(&self.cfg.url),
            enabled: self.cfg.enabled,
            interval_secs: self.cfg.interval_secs,
            ok: last.ok,
            http_status: last.http_status,
            latency_ms: last.latency_ms,
            last_check_secs: last.checked_at.map(|t| (now - t).max(0) as u64),
            next_check_secs: if self.cfg.enabled {
                Some(next.as_secs())
            } else {
                None
            },
            checks,
            failures,
            uptime_pct: if checks == 0 {
                0.0
            } else {
                (checks - failures) as f64 * 100.0 / checks as f64
            },
            last_error: last.error,
            fields: last.fields,
        }
    }

    pub fn record(&self, outcome: CheckOutcome, log: &LogSink) {
        self.checks.fetch_add(1, Ordering::Relaxed);
        let was_ok = self.last.lock().unwrap().ok;
        if !outcome.ok {
            self.failures.fetch_add(1, Ordering::Relaxed);
        }
        // Only log transitions, so a flapping endpoint doesn't drown the log.
        match (was_ok, outcome.ok) {
            (Some(true), false) | (None, false) => log.warn(
                format!("endpoint:{}", self.cfg.name),
                outcome
                    .error
                    .clone()
                    .unwrap_or_else(|| "check failed".into()),
            ),
            (Some(false), true) => log.info(
                format!("endpoint:{}", self.cfg.name),
                format!("recovered ({} ms)", outcome.latency_ms),
            ),
            _ => {}
        }
        *self.last.lock().unwrap() = LastResult {
            ok: Some(outcome.ok),
            http_status: outcome.http_status,
            latency_ms: Some(outcome.latency_ms),
            checked_at: Some(chrono::Utc::now().timestamp()),
            error: outcome.error,
            fields: outcome.fields,
        };
        self.schedule_next();
    }
}

pub struct CheckOutcome {
    pub ok: bool,
    pub http_status: Option<u16>,
    pub latency_ms: u64,
    pub error: Option<String>,
    pub fields: Vec<(String, String)>,
}

/// Perform one check. Blocking — callers run it on a blocking thread.
pub fn check(cfg: &EndpointConfig) -> CheckOutcome {
    match crate::config::endpoint_kind(&cfg.url) {
        EndpointKind::Stratum => check_stratum(cfg),
        EndpointKind::Http => check_http(cfg),
    }
}

/// Connect to a stratum pool, subscribe, and authorize if credentials were
/// given. This answers the question people actually have about a pool — "will
/// it take my worker?" — rather than merely whether the port is open.
fn check_stratum(cfg: &EndpointConfig) -> CheckOutcome {
    use std::io::{BufRead, BufReader, Write};
    use std::net::{TcpStream, ToSocketAddrs};

    let started = Instant::now();
    let timeout = Duration::from_secs(cfg.timeout_secs.max(1));
    let fail = |error: String, latency: u64| CheckOutcome {
        ok: false,
        http_status: None,
        latency_ms: latency,
        error: Some(error),
        fields: Vec::new(),
    };

    let target = match crate::config::host_port(&cfg.url, "endpoint") {
        Ok(target) => target,
        Err(err) => return fail(format!("{err:#}"), 0),
    };
    let address = match target.to_socket_addrs() {
        Ok(mut addresses) => match addresses.next() {
            Some(address) => address,
            None => return fail(format!("{target} did not resolve"), 0),
        },
        Err(err) => return fail(format!("cannot resolve {target}: {err}"), 0),
    };

    let stream = match TcpStream::connect_timeout(&address, timeout) {
        Ok(stream) => stream,
        Err(err) => {
            return fail(
                format!("cannot connect to {target}: {err}"),
                started.elapsed().as_millis() as u64,
            );
        }
    };
    stream.set_read_timeout(Some(timeout)).ok();
    stream.set_write_timeout(Some(timeout)).ok();

    let mut writer = match stream.try_clone() {
        Ok(writer) => writer,
        Err(err) => return fail(format!("{err}"), started.elapsed().as_millis() as u64),
    };
    let subscribe = format!(
        "{{\"id\":1,\"method\":\"mining.subscribe\",\"params\":[\"cryptocli/{}\"]}}\n",
        env!("CARGO_PKG_VERSION")
    );
    if let Err(err) = writer.write_all(subscribe.as_bytes()) {
        return fail(
            format!("cannot talk to {target}: {err}"),
            started.elapsed().as_millis() as u64,
        );
    }
    let user = cfg.user.clone().unwrap_or_default();
    let wants_auth = !user.is_empty();
    if wants_auth {
        let authorize = serde_json::json!({
            "id": 2,
            "method": "mining.authorize",
            "params": [user, cfg.password.clone().unwrap_or_else(|| "x".into())],
        });
        if writer
            .write_all(format!("{authorize}\n").as_bytes())
            .is_err()
        {
            return fail(
                "pool closed the connection during authorize".into(),
                started.elapsed().as_millis() as u64,
            );
        }
    }
    let _ = writer.flush();

    let _ = reader_timeout(&stream, Duration::from_millis(500));
    let mut reader = BufReader::new(stream);
    let mut subscribed = false;
    let mut authorized = None::<bool>;
    let mut errors: Vec<String> = Vec::new();
    let mut fields: Vec<(String, String)> = Vec::new();
    let mut settled: Option<Instant> = None;
    let mut handshake_ms = 0u64;
    let deadline = Instant::now() + timeout;

    while Instant::now() < deadline {
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) => {
                errors.push("pool closed the connection".into());
                break;
            }
            Ok(_) => {}
            Err(err) => {
                if settled.is_none() {
                    errors.push(format!("no reply from pool: {err}"));
                }
                break;
            }
        }
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line.trim()) else {
            continue;
        };

        // Server-initiated notifications tell us the pool is really live.
        match value.get("method").and_then(|m| m.as_str()) {
            Some("mining.set_difficulty") => {
                if let Some(d) = value
                    .get("params")
                    .and_then(|p| p.get(0))
                    .and_then(|d| d.as_f64())
                {
                    fields.push(("difficulty".into(), format!("{d}")));
                }
                continue;
            }
            Some("mining.notify") => {
                if let Some(job) = value
                    .get("params")
                    .and_then(|p| p.get(0))
                    .and_then(|j| j.as_str())
                {
                    fields.push(("job".into(), job.to_string()));
                }
                continue;
            }
            Some(_) => continue,
            None => {}
        }

        let id = value.get("id").and_then(|v| v.as_u64()).unwrap_or(0);
        let error_text = match value.get("error") {
            Some(serde_json::Value::Array(a)) => Some(
                a.get(1)
                    .and_then(|v| v.as_str())
                    .unwrap_or("pool error")
                    .to_string(),
            ),
            _ => None,
        };
        if id == 1 {
            match error_text {
                Some(err) => errors.push(format!("subscribe rejected: {err}")),
                None => {
                    subscribed = true;
                    if let Some(extranonce1) = value
                        .get("result")
                        .and_then(|r| r.as_array())
                        .and_then(|r| r.get(1))
                        .and_then(|e| e.as_str())
                    {
                        fields.push(("extranonce1".into(), extranonce1.to_string()));
                    }
                }
            }
        } else if id == 2 {
            let accepted = value.get("result") == Some(&serde_json::Value::Bool(true));
            authorized = Some(accepted && error_text.is_none());
            if let Some(err) = error_text {
                errors.push(format!("worker rejected: {err}"));
            } else if !accepted {
                errors.push(format!(
                    "pool rejected the worker credentials for `{}`",
                    cfg.user.clone().unwrap_or_default()
                ));
            }
        }

        if subscribed && (!wants_auth || authorized.is_some()) && settled.is_none() {
            // Latency is the handshake, not the extra moment we linger to pick
            // up a job.
            handshake_ms = started.elapsed().as_millis() as u64;
            settled = Some(Instant::now());
        }
        // Give the pool a brief moment to volunteer a difficulty and a job;
        // those make the check genuinely informative rather than a port probe.
        if let Some(at) = settled
            && (at.elapsed() > Duration::from_millis(400) || fields.iter().any(|(k, _)| k == "job"))
        {
            break;
        }
    }

    let latency_ms = if settled.is_some() {
        handshake_ms
    } else {
        started.elapsed().as_millis() as u64
    };
    if !subscribed && errors.is_empty() {
        errors.push("pool did not answer mining.subscribe in time".into());
    }
    if wants_auth {
        fields.push((
            "authorized".into(),
            match authorized {
                Some(true) => "yes".into(),
                Some(false) => "no".into(),
                None => "no answer".into(),
            },
        ));
    } else {
        fields.push(("authorized".into(), "not checked (no user set)".into()));
    }
    fields.sort();

    CheckOutcome {
        ok: errors.is_empty(),
        http_status: None,
        latency_ms,
        error: if errors.is_empty() {
            None
        } else {
            Some(errors.join("; "))
        },
        fields,
    }
}

fn check_http(cfg: &EndpointConfig) -> CheckOutcome {
    let started = Instant::now();
    let timeout = Duration::from_secs(cfg.timeout_secs.max(1));
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(timeout))
        .user_agent(concat!("cryptocli/", env!("CARGO_PKG_VERSION")))
        .build()
        .into();

    // ureq types bodied and body-less requests differently, so the two shapes
    // are built separately.
    let method = cfg.method.to_ascii_uppercase();
    let body = cfg.body.clone().unwrap_or_default();
    let result = match method.as_str() {
        "POST" | "PUT" => {
            let mut request = if method == "POST" {
                agent.post(&cfg.url)
            } else {
                agent.put(&cfg.url)
            };
            for (k, v) in &cfg.headers {
                request = request.header(k.as_str(), v.as_str());
            }
            if let Some(auth) = basic_auth(cfg) {
                request = request.header("Authorization", auth.as_str());
            }
            request.send(body.as_str())
        }
        _ => {
            let mut request = if method == "HEAD" {
                agent.head(&cfg.url)
            } else {
                agent.get(&cfg.url)
            };
            for (k, v) in &cfg.headers {
                request = request.header(k.as_str(), v.as_str());
            }
            if let Some(auth) = basic_auth(cfg) {
                request = request.header("Authorization", auth.as_str());
            }
            request.call()
        }
    };

    let latency_ms = started.elapsed().as_millis() as u64;
    let mut response = match result {
        Ok(r) => r,
        Err(err) => {
            return CheckOutcome {
                ok: false,
                http_status: None,
                latency_ms,
                error: Some(trim_error(&err.to_string())),
                fields: Vec::new(),
            };
        }
    };

    let http_status = response.status().as_u16();
    let body = response.body_mut().read_to_string().unwrap_or_default();

    let mut errors: Vec<String> = Vec::new();
    if http_status != cfg.expect_status {
        errors.push(format!(
            "expected HTTP {} but got {http_status}",
            cfg.expect_status
        ));
    }
    if let Some(needle) = &cfg.expect_body
        && !body.contains(needle.as_str())
    {
        errors.push(format!("response does not contain `{needle}`"));
    }

    let mut fields = Vec::new();
    if !cfg.fields.is_empty() {
        match serde_json::from_str::<Value>(&body) {
            Ok(json) => {
                for (label, path) in &cfg.fields {
                    match extract(&json, path) {
                        Some(v) => fields.push((label.clone(), v)),
                        None => {
                            fields.push((label.clone(), "-".into()));
                            errors.push(format!("no value at `{path}`"));
                        }
                    }
                }
            }
            Err(e) => errors.push(format!("response is not JSON: {e}")),
        }
    }

    CheckOutcome {
        ok: errors.is_empty(),
        http_status: Some(http_status),
        latency_ms,
        error: if errors.is_empty() {
            None
        } else {
            Some(errors.join("; "))
        },
        fields,
    }
}

/// Shorten the socket read timeout once the handshake is under way, so the
/// opportunistic wait for a job cannot hold the check open for the full
/// configured timeout.
fn reader_timeout(stream: &std::net::TcpStream, timeout: Duration) -> std::io::Result<()> {
    stream.set_read_timeout(Some(timeout))
}

/// `user`/`password` on an http endpoint mean HTTP basic auth.
fn basic_auth(cfg: &EndpointConfig) -> Option<String> {
    let user = cfg.user.as_deref().filter(|u| !u.is_empty())?;
    let password = cfg.password.clone().unwrap_or_default();
    Some(format!(
        "Basic {}",
        base64(format!("{user}:{password}").as_bytes())
    ))
}

/// Standard base64. Small enough that a dependency isn't worth it.
fn base64(input: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(ALPHABET[(n >> 18) as usize & 63] as char);
        out.push(ALPHABET[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

fn trim_error(msg: &str) -> String {
    let msg = msg.trim();
    if msg.len() > 160 {
        format!("{}...", &msg[..157])
    } else {
        msg.to_string()
    }
}

/// Resolve a dotted path with optional array indices: `data.workers[0].hashrate`.
pub fn extract(root: &Value, path: &str) -> Option<String> {
    let mut current = root;
    for raw in path.split('.') {
        if raw.is_empty() {
            continue;
        }
        let (key, indices) = split_indices(raw);
        if !key.is_empty() {
            current = current.get(key)?;
        }
        for index in indices {
            current = current.get(index)?;
        }
    }
    Some(match current {
        Value::String(s) => s.clone(),
        Value::Null => "null".into(),
        other => other.to_string(),
    })
}

fn split_indices(segment: &str) -> (&str, Vec<usize>) {
    let Some(open) = segment.find('[') else {
        return (segment, Vec::new());
    };
    let (key, rest) = segment.split_at(open);
    let indices = rest
        .split(']')
        .filter_map(|part| part.trim_start_matches('[').parse::<usize>().ok())
        .collect();
    (key, indices)
}

pub type SharedEndpoint = Arc<EndpointRuntime>;

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn extracts_nested_values() {
        let v = json!({"data": {"workers": [{"hashrate": 1234.5, "name": "rig1"}]}});
        assert_eq!(
            extract(&v, "data.workers[0].hashrate").as_deref(),
            Some("1234.5")
        );
        assert_eq!(extract(&v, "data.workers[0].name").as_deref(), Some("rig1"));
        assert_eq!(extract(&v, "data.workers[3].name"), None);
        assert_eq!(extract(&v, "nope.here"), None);
    }

    #[test]
    fn basic_auth_encodes_credentials() {
        // Vectors from RFC 4648.
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
        assert_eq!(base64(b"user:pass"), "dXNlcjpwYXNz");
    }

    #[test]
    fn extracts_top_level_scalars() {
        let v = json!({"status": "ok", "count": 7});
        assert_eq!(extract(&v, "status").as_deref(), Some("ok"));
        assert_eq!(extract(&v, "count").as_deref(), Some("7"));
    }
}
