//! In-memory ring buffer of log lines, mirrored to the daemon's stderr (which
//! is redirected to `daemon.log` when it detaches).

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::model::{LogLevel, LogLine};

#[derive(Clone)]
pub struct LogSink {
    inner: Arc<Inner>,
}

struct Inner {
    ring: Mutex<VecDeque<LogLine>>,
    seq: AtomicU64,
    cap: usize,
    echo: bool,
}

impl LogSink {
    pub fn new(cap: usize, echo: bool) -> Self {
        Self {
            inner: Arc::new(Inner {
                ring: Mutex::new(VecDeque::with_capacity(cap.min(4096))),
                seq: AtomicU64::new(0),
                cap: cap.max(32),
                echo,
            }),
        }
    }

    pub fn log(&self, level: LogLevel, source: impl Into<String>, message: impl Into<String>) {
        let line = LogLine {
            node: String::new(),
            seq: self.inner.seq.fetch_add(1, Ordering::Relaxed),
            ts: chrono::Utc::now().timestamp(),
            level,
            source: source.into(),
            message: message.into(),
        };
        if self.inner.echo {
            eprintln!(
                "{} {} [{}] {}",
                chrono::Local::now().format("%H:%M:%S"),
                line.level.label(),
                line.source,
                line.message
            );
        }
        let mut ring = self.inner.ring.lock().unwrap();
        if ring.len() == self.inner.cap {
            ring.pop_front();
        }
        ring.push_back(line);
    }

    pub fn info(&self, source: impl Into<String>, message: impl Into<String>) {
        self.log(LogLevel::Info, source, message);
    }
    pub fn warn(&self, source: impl Into<String>, message: impl Into<String>) {
        self.log(LogLevel::Warn, source, message);
    }
    pub fn error(&self, source: impl Into<String>, message: impl Into<String>) {
        self.log(LogLevel::Error, source, message);
    }
    pub fn share(&self, source: impl Into<String>, message: impl Into<String>) {
        self.log(LogLevel::Share, source, message);
    }
    pub fn debug(&self, source: impl Into<String>, message: impl Into<String>) {
        self.log(LogLevel::Debug, source, message);
    }

    pub fn tail(&self, n: usize) -> Vec<LogLine> {
        let ring = self.inner.ring.lock().unwrap();
        let skip = ring.len().saturating_sub(n);
        ring.iter().skip(skip).cloned().collect()
    }
}
