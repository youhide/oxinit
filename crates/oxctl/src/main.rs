//! `oxctl` — the command-line client for oxinit.
//!
//! Connects to `/run/oxinit/control.sock`, sends one request, prints one
//! reply, exits. There is no daemon here and no state: the socket is
//! `SOCK_SEQPACKET`, so a request is one `send` and a reply is one `recv`.
//!
//! Not PID 1. This process may panic, may exit, and is held to none of the
//! rules the `oxinit` crate carries.

#![forbid(unsafe_code)]

use std::path::Path;
use std::process::ExitCode;

use rustix::net::{AddressFamily, SendFlags, SocketAddrUnix, SocketFlags, SocketType};

use oxinit_ipc::{Request, Response, UnitStatus, CONTROL_PATH, MAX_MESSAGE};

const USAGE: &str = "\
usage: oxctl <command> [unit]

commands:
  list              every unit oxinit knows about
  status [unit]     one unit in detail, or all of them
  start <unit>      start a unit now
  stop <unit>       stop a unit; the restart policy does not apply
  restart <unit>    stop, then start once it is down
  logs <unit>       what the unit has written, from /var/log/oxinit
  reload            re-read the unit directories
  --version         which build this is

`logs` reads the file directly. It does not go through oxinit: the control
socket has to stay responsive, and bulk data is what would stop it being.

oxinit answers immediately. `stop` means the stop was asked for, not that
it has finished — a unit is not stopped until its cgroup is empty.";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();

    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("oxctl: {message}");
            ExitCode::FAILURE
        }
    }
}

fn run(args: &[String]) -> Result<(), String> {
    let command = args.first().map(String::as_str).unwrap_or("");
    let unit = args.get(1).cloned();

    let request = match (command, unit) {
        ("list", _) => Request::List,
        ("status", None) => Request::List,
        ("status", Some(unit)) => Request::Status { unit },
        ("reload", _) => Request::Reload,

        // Not a request at all. `oxlogd` writes files and this reads them,
        // because putting a service's whole output through the one socket
        // that has to stay responsive is exactly what would stop it being.
        ("logs", Some(unit)) => return logs(&unit, tail(args)),
        ("logs", None) => return Err(format!("logs needs a unit name\n\n{USAGE}")),

        ("start", Some(unit)) => Request::Start { unit },
        ("stop", Some(unit)) => Request::Stop { unit },
        ("restart", Some(unit)) => Request::Restart { unit },

        ("start" | "stop" | "restart", None) => {
            return Err(format!("{command} needs a unit name\n\n{USAGE}"))
        }

        ("--version" | "-V", _) => {
            println!("oxctl {}", env!("CARGO_PKG_VERSION"));
            return Ok(());
        }

        ("" | "help" | "--help" | "-h", _) => {
            println!("{USAGE}");
            return Ok(());
        }

        (other, _) => return Err(format!("unknown command `{other}`\n\n{USAGE}")),
    };

    match exchange(&request)? {
        Response::Units(units) => {
            print_units(&units, matches!(request, Request::List));
            Ok(())
        }
        Response::Accepted { message } => {
            println!("{message}");
            Ok(())
        }
        Response::Error { message } => Err(message),
    }
}

/// `-n N`, if given.
fn tail(args: &[String]) -> Option<usize> {
    args.iter()
        .position(|arg| arg == "-n")
        .and_then(|at| args.get(at + 1))
        .and_then(|value| value.parse().ok())
}

/// Print what a unit has written.
///
/// Only the live file. The rotated generations are `<unit>.log.1` and up in
/// the same directory, and reading them is `cat` — there is no index and no
/// merge to do, which is most of the reason the format is plain text.
fn logs(unit: &str, tail: Option<usize>) -> Result<(), String> {
    let path = oxinit_log::path(Path::new(oxinit_log::LOG_DIR), unit);

    let text = std::fs::read_to_string(&path).map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => format!(
            "no logs for `{unit}` at {}\n\
             is oxlogd running, and does the unit declare `output = \"log\"`?",
            path.display()
        ),
        _ => format!("read {}: {e}", path.display()),
    })?;

    let lines: Vec<&str> = text.lines().collect();
    let from = tail.map_or(0, |n| lines.len().saturating_sub(n));

    for line in lines.get(from..).unwrap_or_default() {
        println!("{line}");
    }

    Ok(())
}

/// One request, one reply.
fn exchange(request: &Request) -> Result<Response, String> {
    let body = oxinit_ipc::encode(request).map_err(|e| e.to_string())?;

    let socket = rustix::net::socket_with(
        AddressFamily::UNIX,
        SocketType::SEQPACKET,
        SocketFlags::CLOEXEC,
        None,
    )
    .map_err(|e| format!("socket: {e}"))?;

    let addr = SocketAddrUnix::new(CONTROL_PATH).map_err(|e| format!("{CONTROL_PATH}: {e}"))?;

    rustix::net::connect(&socket, &addr).map_err(|e| {
        format!(
            "connect {CONTROL_PATH}: {e}\n\
             is oxinit running as PID 1, and are you root?"
        )
    })?;

    rustix::net::send(&socket, &body, SendFlags::empty()).map_err(|e| format!("send: {e}"))?;

    let mut buf = vec![0u8; MAX_MESSAGE];
    let (read, sent) =
        rustix::net::recv(&socket, buf.as_mut_slice(), rustix::net::RecvFlags::empty())
            .map_err(|e| format!("recv: {e}"))?;

    if read == 0 {
        return Err("oxinit closed the connection without answering".to_owned());
    }
    if sent > buf.len() {
        return Err(format!(
            "oxinit sent {sent} bytes, over the {MAX_MESSAGE} limit"
        ));
    }

    oxinit_ipc::decode(&buf[..read]).map_err(|e| e.to_string())
}

/// One line per unit for a list, and the whole of it for a single unit.
fn print_units(units: &[UnitStatus], compact: bool) {
    if units.is_empty() {
        println!("no units");
        return;
    }

    if compact {
        let width = units.iter().map(|u| u.name.len()).max().unwrap_or(4);

        for unit in units {
            println!(
                "{:<width$}  {:<8}  {:<12}  {}",
                unit.name,
                unit.kind,
                unit.state,
                unit.description,
                width = width
            );
        }
        return;
    }

    for unit in units {
        println!("{} ({})", unit.name, unit.kind);
        println!("  description  {}", unit.description);
        println!("  state        {}", unit.state);

        if let Some(pid) = unit.pid {
            println!("  pid          {pid}");
        }
        if unit.restarts > 0 {
            println!("  restarts     {}", unit.restarts);
        }
        if let Some(status) = unit.status.as_deref() {
            println!("  status       {status}");
        }
        if let Some(next) = unit.next_elapse {
            println!("  next         in {next}s");
        }
        if let Some(memory) = unit.memory {
            println!("  memory       {memory} bytes");
        }
        if let Some(tasks) = unit.tasks {
            println!("  tasks        {tasks}");
        }
    }
}
