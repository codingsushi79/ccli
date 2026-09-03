//! Address handling for node-to-node connections.
//!
//! Two things kept biting people setting up a fleet, and both live here now.
//!
//! The first is that `ToSocketAddrs` hands back *several* addresses and the
//! code used to take only the first. A hostname on a dual-stack machine
//! usually resolves to its AAAA record first, while a daemon told to listen on
//! `0.0.0.0:9944` is bound to IPv4 only — so the very first address tried is
//! the one that cannot work, and the connection is refused before the IPv4
//! address is ever reached. We try every address and only give up once they
//! have all failed.
//!
//! The second is that a bare `os error 61` (or 60, or 63) is not something a
//! user can act on. Every failure here is reported with the address that was
//! tried and a sentence about what usually causes it.

use std::io;
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::time::Duration;

use anyhow::{Result, bail};

/// The port `cryptocli remote enable` listens on unless told otherwise.
pub const DEFAULT_PORT: u16 = 9944;

/// Clean up an address typed or pasted into `node add` or the dashboard form.
///
/// Accepts `host`, `host:port`, `[::1]:port` and a pasted `tcp://host:port`,
/// and fills in [`DEFAULT_PORT`] when no port was given — the port is the part
/// people leave off, and there is only one sensible answer for it.
pub fn normalize_address(input: &str) -> Result<String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        bail!("no address given — use host:port, e.g. 10.0.0.7:{DEFAULT_PORT}");
    }
    let raw = trimmed
        .trim_start_matches("tcp://")
        .trim_start_matches("cryptocli://")
        .trim_end_matches('/')
        .trim();
    if raw.is_empty() {
        bail!("`{trimmed}` has no host in it");
    }
    if raw.contains(char::is_whitespace) {
        bail!("`{raw}` contains a space — an address is just host:port");
    }
    if raw.starts_with("http://") || raw.starts_with("https://") {
        bail!("`{raw}` is a web url; a node address is just host:port");
    }

    // An IPv6 literal has to be bracketed before a port can be told apart from
    // the address's own colons.
    let bare_ipv6 = raw.parse::<std::net::Ipv6Addr>().is_ok();
    let has_port = if bare_ipv6 {
        false
    } else if raw.starts_with('[') {
        raw.rsplit_once("]:").is_some_and(|(_, p)| !p.is_empty())
    } else {
        raw.matches(':').count() == 1 && !raw.ends_with(':')
    };

    let with_port = if has_port {
        raw.to_string()
    } else if bare_ipv6 {
        format!("[{raw}]:{DEFAULT_PORT}")
    } else {
        format!("{}:{DEFAULT_PORT}", raw.trim_end_matches(':'))
    };

    // Reject a bad port here rather than at connect time, where the message
    // would be about name resolution instead.
    if let Some((_, port)) = with_port.rsplit_once(':')
        && port.parse::<u16>().is_err()
    {
        bail!("`{port}` is not a valid port number");
    }
    Ok(with_port)
}

/// Clean up the address the daemon should listen on.
///
/// A bare port is accepted and means "every interface", which is what people
/// mean when they type `--listen 9944`.
pub fn normalize_listen(input: &str) -> Result<String> {
    let trimmed = input.trim();
    if let Ok(port) = trimmed.parse::<u16>() {
        return Ok(format!("0.0.0.0:{port}"));
    }
    let address = normalize_address(trimmed)?;
    // A listen address has to be one this machine actually holds, so resolve
    // it now: `--listen rig2:9944` on the wrong machine is otherwise a mystery
    // at daemon start, hours later.
    resolve(&address)?;
    Ok(address)
}

/// Every address `address` resolves to, in the order the resolver gave them.
pub fn resolve(address: &str) -> Result<Vec<SocketAddr>> {
    let addresses: Vec<SocketAddr> = match address.to_socket_addrs() {
        Ok(iter) => iter.collect(),
        Err(err) => bail!(
            "cannot look up `{address}`: {} — check the host name, \
             or use the machine's IP address instead",
            err.to_string().trim_start_matches("failed to lookup ")
        ),
    };
    if addresses.is_empty() {
        bail!("`{address}` did not resolve to any address");
    }
    Ok(addresses)
}

/// Connect to the first address that answers.
///
/// `timeout` applies per address, so a host with both an A and a AAAA record
/// can take up to twice as long before it is declared unreachable. That is the
/// right trade: a fleet that works slowly beats one that never connects.
pub fn connect(address: &str, timeout: Duration) -> Result<TcpStream> {
    let addresses = resolve(address)?;
    let mut last: Option<(SocketAddr, io::Error)> = None;
    for candidate in addresses {
        match TcpStream::connect_timeout(&candidate, timeout) {
            Ok(stream) => {
                stream.set_read_timeout(Some(timeout))?;
                stream.set_write_timeout(Some(timeout))?;
                stream.set_nodelay(true).ok();
                return Ok(stream);
            }
            Err(err) => last = Some((candidate, err)),
        }
    }
    let (candidate, err) = last.expect("resolve returns at least one address");
    bail!("{}", explain(address, candidate, &err))
}

/// Turn a connection failure into something the user can act on.
fn explain(address: &str, tried: SocketAddr, err: &io::Error) -> String {
    let hint = match err.kind() {
        io::ErrorKind::ConnectionRefused => {
            "nothing is listening there. Run `cryptocli remote enable` on that machine \
             and restart its daemon, and check the port matches"
        }
        io::ErrorKind::TimedOut => {
            "no reply at all, which usually means a firewall in the way, or a daemon \
             bound to a different address than the one you dialled"
        }
        io::ErrorKind::HostUnreachable | io::ErrorKind::NetworkUnreachable => {
            "there is no route to that machine. Check it is on this network or VPN"
        }
        io::ErrorKind::PermissionDenied => "the local firewall blocked the connection",
        _ => "check the address, the port, and that the peer's daemon is running",
    };
    // Naming the address actually dialled matters when a hostname resolves to
    // more than one: "cannot connect to rig2:9944" hides which family failed.
    let dialled = if tried.to_string() == address {
        String::new()
    } else {
        format!(" ({tried})")
    };
    format!("cannot connect to {address}{dialled}: {err} — {hint}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_port_gets_the_default() {
        assert_eq!(normalize_address("10.0.0.7").unwrap(), "10.0.0.7:9944");
        assert_eq!(normalize_address("rig2").unwrap(), "rig2:9944");
        assert_eq!(normalize_address("  rig2  ").unwrap(), "rig2:9944");
        assert_eq!(normalize_address("::1").unwrap(), "[::1]:9944");
    }

    #[test]
    fn an_explicit_port_is_kept() {
        assert_eq!(normalize_address("10.0.0.7:1234").unwrap(), "10.0.0.7:1234");
        assert_eq!(normalize_address("[::1]:1234").unwrap(), "[::1]:1234");
        assert_eq!(
            normalize_address("tcp://rig2:1234/").unwrap(),
            "rig2:1234",
            "a pasted scheme should not become part of the host"
        );
    }

    #[test]
    fn nonsense_is_rejected_with_a_reason() {
        for bad in ["", "   ", "rig2:notaport", "https://rig2:9944", "a b:9944"] {
            assert!(
                normalize_address(bad).is_err(),
                "`{bad}` should be rejected"
            );
        }
    }

    #[test]
    fn a_bare_port_listens_everywhere() {
        assert_eq!(normalize_listen("9944").unwrap(), "0.0.0.0:9944");
        assert_eq!(normalize_listen(" 0.0.0.0:9944 ").unwrap(), "0.0.0.0:9944");
        assert_eq!(normalize_listen("127.0.0.1").unwrap(), "127.0.0.1:9944");
        assert!(normalize_listen("not a port").is_err());
    }

    #[test]
    fn resolution_yields_every_address() {
        let found = resolve("127.0.0.1:9944").unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].port(), 9944);
        assert!(resolve("no-such-host.invalid:9944").is_err());
    }
}
