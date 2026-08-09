//! A socket-activated service, and the client that pokes it.
//!
//! Not part of the running system. `cargo xtask boot` installs it in the test
//! image so descriptor passing can be verified from inside a guest — nothing
//! in busybox both speaks `LISTEN_FDS` and reports what it was handed.
//!
//! Usage:
//!
//! ```text
//! listen-probe --serve NAME     # the activated service
//! listen-probe --connect PATH   # the client that triggers it
//! ```
//!
//! `--serve NAME` looks its descriptor up by name in `LISTEN_FDNAMES` rather
//! than assuming fd 3, which is what the variable is for: a service given
//! descriptors by more than one socket unit has to be able to tell them apart.

use std::io::{Read as _, Write as _};
use std::net::Shutdown;
use std::os::fd::FromRawFd as _;
use std::os::unix::net::{UnixListener, UnixStream};
use std::process::ExitCode;

/// Where the protocol says the first inherited descriptor is.
const FIRST_LISTEN_FD: i32 = 3;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();

    let result = match (flag(&args, "--serve"), flag(&args, "--connect")) {
        (Some(name), _) => serve(name),
        (None, Some(path)) => connect(path),
        (None, None) => Err("usage: listen-probe --serve NAME | --connect PATH".to_owned()),
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("listen-probe: {message}");
            ExitCode::FAILURE
        }
    }
}

/// Serve exactly one connection on the descriptor named `name`.
fn serve(name: &str) -> Result<(), String> {
    let count: usize = var("LISTEN_FDS")?
        .parse()
        .map_err(|_| "LISTEN_FDS is not a number".to_owned())?;

    let announced: u32 = var("LISTEN_PID")?
        .parse()
        .map_err(|_| "LISTEN_PID is not a number".to_owned())?;

    // The protocol's one hard rule. These variables are inherited by every
    // descendant, and a child that skipped this check would believe it owns
    // descriptors belonging to its parent.
    let mine = std::process::id();
    if announced != mine {
        return Err(format!("LISTEN_PID is {announced}, but I am {mine}"));
    }

    let names = var("LISTEN_FDNAMES")?;
    let index = names
        .split(':')
        .position(|candidate| candidate == name)
        .ok_or_else(|| format!("no descriptor named `{name}` in `{names}`"))?;

    println!(
        "listen-probe: {count} descriptor(s) as `{names}`, LISTEN_PID={announced} is mine, \
         serving `{name}` on fd {}",
        FIRST_LISTEN_FD + index as i32
    );

    // SAFETY: the descriptor was inherited from oxinit at this number, is not
    // owned by anything else in this process, and the protocol says it is a
    // listening socket. This is a test fixture, not PID 1.
    let listener = unsafe { UnixListener::from_raw_fd(FIRST_LISTEN_FD + index as i32) };

    let (mut stream, _) = listener.accept().map_err(|e| format!("accept: {e}"))?;

    let mut request = [0u8; 64];
    let read = stream
        .read(&mut request)
        .map_err(|e| format!("read: {e}"))?;
    let request = String::from_utf8_lossy(request.get(..read).unwrap_or_default()).into_owned();

    println!("listen-probe: serving a connection, request {request:?}");

    stream
        .write_all(format!("echo: {request}").as_bytes())
        .map_err(|e| format!("write: {e}"))?;

    Ok(())
}

/// Connect, which is what makes oxinit start the service.
fn connect(path: &str) -> Result<(), String> {
    let mut stream = UnixStream::connect(path).map_err(|e| format!("connect {path}: {e}"))?;

    stream
        .write_all(b"hello")
        .map_err(|e| format!("write: {e}"))?;
    let _ = stream.shutdown(Shutdown::Write);

    // Blocks until the service oxinit just started gets around to answering,
    // which is the whole thing being tested.
    let mut reply = String::new();
    stream
        .read_to_string(&mut reply)
        .map_err(|e| format!("read: {e}"))?;

    println!("listen-probe: connected to {path}, got {reply:?}");
    Ok(())
}

fn var(name: &str) -> Result<String, String> {
    std::env::var(name).map_err(|_| format!("{name} is not set"))
}

/// The value of `--name VALUE`.
fn flag<'a>(args: &'a [String], name: &str) -> Option<&'a str> {
    args.iter()
        .position(|arg| arg == name)
        .and_then(|at| args.get(at + 1))
        .map(String::as_str)
}
