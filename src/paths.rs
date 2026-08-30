//! Filesystem locations. Everything is overridable with `CRYPTOCLI_HOME`, which
//! keeps test runs and multiple daemons from colliding.

use std::path::PathBuf;

fn home() -> PathBuf {
    if let Ok(dir) = std::env::var("CRYPTOCLI_HOME") {
        return PathBuf::from(dir);
    }
    dirs::data_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("cryptocli")
}

/// Where runtime state lives: socket, pid, log, and the node's TLS identity.
pub fn data_dir() -> PathBuf {
    home()
}

pub fn config_path() -> PathBuf {
    if let Ok(p) = std::env::var("CRYPTOCLI_CONFIG") {
        return PathBuf::from(p);
    }
    if std::env::var("CRYPTOCLI_HOME").is_ok() {
        return home().join("config.toml");
    }
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("cryptocli")
        .join("config.toml")
}

/// `sockaddr_un.sun_path` is 108 bytes on Linux and 104 on macOS, so a deep
/// data directory (or a long $HOME) would make `bind` fail. When the natural
/// path is too long we fall back to a short, deterministic name under the temp
/// directory — deterministic so the client derives the same path as the daemon.
pub fn socket_path() -> PathBuf {
    const MAX_SUN_PATH: usize = 100;
    let preferred = home().join("daemon.sock");
    if preferred.as_os_str().len() <= MAX_SUN_PATH {
        return preferred;
    }
    let fallback = std::env::temp_dir().join(format!(
        "cryptocli-{:016x}.sock",
        fnv1a(preferred.to_string_lossy().as_bytes())
    ));
    if fallback.as_os_str().len() <= MAX_SUN_PATH {
        fallback
    } else {
        PathBuf::from(format!(
            "/tmp/cryptocli-{:016x}.sock",
            fnv1a(preferred.to_string_lossy().as_bytes())
        ))
    }
}

fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

pub fn pid_path() -> PathBuf {
    home().join("daemon.pid")
}

pub fn log_path() -> PathBuf {
    home().join("daemon.log")
}

pub fn ensure_dirs() -> std::io::Result<()> {
    std::fs::create_dir_all(home())?;
    if let Some(parent) = config_path().parent() {
        std::fs::create_dir_all(parent)?;
    }
    Ok(())
}
