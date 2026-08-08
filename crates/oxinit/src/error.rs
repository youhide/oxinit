use std::io;

use thiserror::Error;

/// Everything that can go wrong during boot.
///
/// Nothing here is fatal by itself. PID 1 logs and continues; see the failure
/// policy in ARCHITECTURE.md.
#[derive(Debug, Error)]
pub enum Error {
    #[error("mount {target}: {source}")]
    Mount {
        target: &'static str,
        #[source]
        source: rustix::io::Errno,
    },

    #[error("create directory {path}: {source}")]
    Mkdir {
        path: &'static str,
        #[source]
        source: rustix::io::Errno,
    },

    #[error("open /dev/console: {0}")]
    Console(#[source] rustix::io::Errno),

    #[error("block signals: {0}")]
    BlockSignals(#[source] rustix::io::Errno),

    #[error("create signalfd: {0}")]
    SignalFd(#[source] rustix::io::Errno),

    #[error("read signalfd: {0}")]
    ReadSignalFd(#[source] rustix::io::Errno),

    #[error("set hostname: {0}")]
    Hostname(#[source] rustix::io::Errno),

    #[error("epoll: {0}")]
    Epoll(#[source] rustix::io::Errno),

    #[error("notify socket: {0}")]
    Notify(#[source] rustix::io::Errno),

    #[error("timerfd: {0}")]
    Timer(#[source] rustix::io::Errno),

    #[error("spawn {path}: {source}")]
    Spawn {
        path: &'static str,
        #[source]
        source: io::Error,
    },
}

pub type Result<T> = std::result::Result<T, Error>;
