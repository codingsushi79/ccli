//! Newline-delimited JSON over a Unix socket. One request, one response.
//!
//! The client half is blocking on purpose: the TUI drives it from its own event
//! loop and a full snapshot round-trip is well under a millisecond.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::os::unix::net::UnixStream;
use std::time::Duration;

use crate::model::Snapshot;
use crate::paths;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum Request {
    Ping,
    Snapshot,
    StartRig {
        name: String,
    },
    StopRig {
        name: String,
    },
    StartAll,
    StopAll,
    SetThreads {
        name: String,
        threads: usize,
    },
    CheckEndpoint {
        name: String,
    },
    Reload,
    Shutdown,
    /// First message on a TCP connection. Local Unix clients skip it.
    Auth {
        token: String,
    },
    /// Run `request` on `node`; the hub forwards it if the node is a peer.
    OnNode {
        node: String,
        request: Box<Request>,
    },
    // Config mutation, so the dashboard can add things without dropping to a
    // shell. The daemon validates, persists and applies in one step.
    AddWallet {
        name: String,
        coin: String,
        address: String,
        label: Option<String>,
    },
    RemoveWallet {
        name: String,
    },
    AddRig {
        name: String,
        url: String,
        coin: String,
        algo: String,
        wallet: Option<String>,
        worker: Option<String>,
        user: Option<String>,
        pass: String,
        threads: usize,
        weight: u32,
    },
    AddRigCoin {
        rig: String,
        coin: String,
        url: String,
        wallet: Option<String>,
        worker: Option<String>,
        weight: u32,
    },
    RemoveRig {
        name: String,
    },
    SetRigEnabled {
        name: String,
        enabled: bool,
    },
    AddEndpoint {
        name: String,
        url: String,
        method: String,
        interval_secs: u64,
        timeout_secs: u64,
        expect_status: u16,
        headers: Vec<(String, String)>,
        fields: Vec<(String, String)>,
        user: Option<String>,
        password: Option<String>,
    },
    RemoveEndpoint {
        name: String,
    },
    AddNode {
        name: String,
        address: String,
        token: String,
        /// Empty means trust the certificate the peer presents right now.
        fingerprint: String,
    },
    RemoveNode {
        name: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum Response {
    Ok {
        message: String,
    },
    Pong {
        pid: u32,
        version: String,
        uptime_secs: u64,
    },
    Snapshot(Box<Snapshot>),
    Error {
        message: String,
    },
}

/// Anything we can run the line protocol over: a Unix socket, a TCP socket,
/// or a TLS session on top of one.
pub trait Transport: Read + Write + Send {}
impl<T: Read + Write + Send> Transport for T {}

/// A blocking connection to a daemon, local (Unix socket) or remote (TLS).
///
/// The reader and writer are the *same* object rather than two clones, because
/// a TLS session cannot be split in half.
pub struct Client {
    stream: BufReader<Box<dyn Transport>>,
}

impl Client {
    pub fn connect() -> Result<Self> {
        let path = paths::socket_path();
        let stream = UnixStream::connect(&path)
            .with_context(|| format!("connecting to daemon at {}", path.display()))?;
        stream.set_read_timeout(Some(Duration::from_secs(10)))?;
        stream.set_write_timeout(Some(Duration::from_secs(10)))?;
        Ok(Self {
            stream: BufReader::new(Box::new(stream)),
        })
    }

    /// Connect to another machine over TLS and authenticate.
    ///
    /// `fingerprint` pins the peer's certificate; the token is only sent once
    /// the encrypted channel is up, so it never crosses the network in clear.
    pub fn connect_remote(
        address: &str,
        token: &str,
        fingerprint: &str,
        timeout: Duration,
    ) -> Result<Self> {
        use std::net::ToSocketAddrs;
        let resolved = address
            .to_socket_addrs()
            .with_context(|| format!("resolving {address}"))?
            .next()
            .with_context(|| format!("{address} did not resolve"))?;
        let stream = TcpStream::connect_timeout(&resolved, timeout)
            .with_context(|| format!("connecting to {address}"))?;
        stream.set_read_timeout(Some(timeout))?;
        stream.set_write_timeout(Some(timeout))?;
        stream.set_nodelay(true).ok();

        let config = crate::tls::client_config(fingerprint)?;
        let server_name = rustls::pki_types::ServerName::try_from("cryptocli-node")
            .expect("static name is valid");
        let connection = rustls::ClientConnection::new(config, server_name)
            .context("starting the TLS handshake")?;
        let tls = rustls::StreamOwned::new(connection, stream);

        let mut client = Self {
            stream: BufReader::new(Box::new(tls)),
        };
        match client.send(&Request::Auth {
            token: token.to_string(),
        })? {
            Response::Ok { .. } => Ok(client),
            Response::Error { message } => bail!("{address} rejected the token: {message}"),
            other => bail!("unexpected auth reply from {address}: {other:?}"),
        }
    }

    /// True if a daemon is listening and answering.
    pub fn probe() -> bool {
        match Client::connect() {
            Ok(mut c) => matches!(c.send(&Request::Ping), Ok(Response::Pong { .. })),
            Err(_) => false,
        }
    }

    pub fn send(&mut self, req: &Request) -> Result<Response> {
        let mut line = serde_json::to_string(req)?;
        line.push('\n');
        {
            let writer = self.stream.get_mut();
            writer.write_all(line.as_bytes())?;
            writer.flush()?;
        }
        let mut buf = String::new();
        let n = self.stream.read_line(&mut buf)?;
        if n == 0 {
            bail!("daemon closed the connection");
        }
        Ok(serde_json::from_str(&buf)?)
    }

    pub fn snapshot(&mut self) -> Result<Snapshot> {
        match self.send(&Request::Snapshot)? {
            Response::Snapshot(s) => Ok(*s),
            Response::Error { message } => bail!(message),
            other => bail!("unexpected response: {other:?}"),
        }
    }

    /// Send a command and reduce the reply to a one-line message.
    pub fn command(&mut self, req: &Request) -> Result<String> {
        match self.send(req)? {
            Response::Ok { message } => Ok(message),
            Response::Error { message } => bail!(message),
            Response::Pong {
                pid,
                version,
                uptime_secs,
            } => Ok(format!(
                "daemon {version} (pid {pid}) up {}",
                crate::model::fmt_duration(uptime_secs)
            )),
            Response::Snapshot(_) => Ok("snapshot".into()),
        }
    }
}
