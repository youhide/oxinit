//! `oxlogd` — reads service output and writes it.
//!
//! It connects to `/run/oxinit/log.sock`, and oxinit hands it one descriptor
//! per service start: a message carrying the unit name, with the read end of
//! that service's pipe attached as `SCM_RIGHTS`. Everything that arrives on
//! that descriptor is that unit's output, and it goes to
//! `/var/log/oxinit/<unit>.log` a line at a time, timestamped.
//!
//! **PID 1 does not do this.** A service writing a megabyte a second must not
//! be able to make PID 1 do work, and a bug in a log writer must kill a log
//! writer. That is the same argument that put `oxctl` out of process, and it
//! is the only reason this program exists.
//!
//! It is also why oxinit keeps its own copy of every read end. If this process
//! dies, the pipes stay open, the services writing into them notice nothing,
//! and the replacement is handed every descriptor again on connect.
//!
//! `poll`, not `epoll`. The descriptor set is small and changes on nearly
//! every event — a service starts and one is added, a service exits and one is
//! dropped. `epoll` pays for a registration call per change to make waiting
//! cheap on a large stable set, and there is neither here.

#![forbid(unsafe_code)]

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{IoSliceMut, Write as _};
use std::mem::MaybeUninit;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use rustix::event::{poll, PollFd, PollFlags};
use rustix::net::{
    AddressFamily, RecvAncillaryBuffer, RecvAncillaryMessage, RecvFlags, SocketAddrUnix,
    SocketFlags, SocketType,
};

use oxinit_log::{Move, Rotation, Splitter, Timestamp, LOG_DIR, MAX_MESSAGE, SOCKET_PATH};

/// How much to take from a pipe at once.
///
/// One page over the default pipe capacity would be pointless; this is a
/// buffer, not a promise, and a short read is the normal case.
const READ_SIZE: usize = 16 * 1024;

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("oxlogd: {message}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), String> {
    if std::env::args().any(|arg| arg == "--version" || arg == "-V") {
        println!("oxlogd {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }

    let dir = std::env::args()
        .skip_while(|arg| arg != "--dir")
        .nth(1)
        .map_or_else(|| PathBuf::from(LOG_DIR), PathBuf::from);

    std::fs::create_dir_all(&dir).map_err(|e| format!("create {}: {e}", dir.display()))?;

    let socket = connect()?;
    println!("oxlogd: connected, writing to {}", dir.display());

    // Only once the socket is up. A `type = "notify"` unit that said it was
    // ready before it could receive anything would let services ordered after
    // it start with nowhere for their output to go, which is exactly the thing
    // the ordering was declared for.
    ready();

    Shipper::new(dir).serve(socket)
}

/// oxinit is the server. If it is not listening, there is nothing to wait for
/// — the restart policy is what retries, at a backoff oxinit already knows how
/// to compute.
fn connect() -> Result<OwnedFd, String> {
    let socket = rustix::net::socket_with(
        AddressFamily::UNIX,
        SocketType::SEQPACKET,
        SocketFlags::CLOEXEC,
        None,
    )
    .map_err(|e| format!("socket: {e}"))?;

    let addr = SocketAddrUnix::new(SOCKET_PATH).map_err(|e| format!("{SOCKET_PATH}: {e}"))?;

    rustix::net::connect(&socket, &addr).map_err(|e| {
        format!(
            "connect {SOCKET_PATH}: {e}\n\
             is oxinit running as PID 1, and are you root?"
        )
    })?;

    Ok(socket)
}

/// `READY=1`, if oxinit asked for it.
///
/// Best-effort and unconditional: a unit declared `simple` gets no
/// `NOTIFY_SOCKET`, and this is then correctly a no-op rather than an error.
fn ready() {
    let Ok(path) = std::env::var("NOTIFY_SOCKET") else {
        return;
    };

    if let Ok(socket) = std::os::unix::net::UnixDatagram::unbound() {
        let _ = socket.send_to(b"READY=1\nSTATUS=shipping\n", path);
    }
}

/// One service's pipe.
struct Source {
    fd: OwnedFd,
    unit: String,
    splitter: Splitter,
}

/// One unit's file.
///
/// Kept open across restarts of the service, so its output stays in one file
/// and one ordering. The size is tracked rather than asked of the filesystem
/// on every write, because rotation is decided per record.
struct Sink {
    file: File,
    size: u64,
}

struct Shipper {
    dir: PathBuf,
    rotation: Rotation,
    sources: Vec<Source>,
    sinks: HashMap<String, Sink>,
    /// Reused across records so a busy service does not allocate per line.
    scratch: Vec<u8>,
}

impl Shipper {
    fn new(dir: PathBuf) -> Self {
        Self {
            dir,
            rotation: Rotation::default(),
            sources: Vec::new(),
            sinks: HashMap::new(),
            scratch: Vec::new(),
        }
    }

    /// Wait, then handle whatever became readable. Returns when oxinit closes
    /// the socket, which for PID 1 means it is going down.
    fn serve(&mut self, socket: OwnedFd) -> Result<(), String> {
        loop {
            // Scoped, because every `PollFd` borrows a source, and the sources
            // have to be borrowed mutably below to be read from. What survives
            // the scope is one bool per descriptor: index 0 is the socket,
            // index n + 1 is `sources[n]`.
            let ready: Vec<bool> = {
                let mut fds = Vec::with_capacity(self.sources.len() + 1);
                fds.push(PollFd::new(&socket, PollFlags::IN));
                for source in &self.sources {
                    fds.push(PollFd::new(&source.fd, PollFlags::IN));
                }

                match poll(&mut fds, None) {
                    Ok(_) => {}
                    // Nothing is blocked here, so this is only reachable if a
                    // signal is delivered mid-wait.
                    Err(rustix::io::Errno::INTR) => continue,
                    Err(e) => return Err(format!("poll: {e}")),
                }

                // Not just `IN`: when the last write end closes, the kernel
                // reports `HUP`, and the read that follows is what returns the
                // zero that means the service is gone.
                fds.iter().map(|fd| !fd.revents().is_empty()).collect()
            };

            // Sources before the socket. When a service exits, its pipe goes
            // readable with the last of its output and then reports EOF, and
            // draining first keeps that output ahead of whatever start oxinit
            // is about to announce.
            let mut closed = Vec::new();
            for (index, source) in self.sources.iter_mut().enumerate() {
                if ready.get(index + 1) != Some(&true) {
                    continue;
                }

                let alive = drain(
                    source,
                    &mut self.scratch,
                    &self.dir,
                    self.rotation,
                    &mut self.sinks,
                );
                if !alive {
                    closed.push(index);
                }
            }

            // Descending, so removing one does not shift the next.
            for index in closed.into_iter().rev() {
                if index < self.sources.len() {
                    self.sources.remove(index);
                }
            }

            if ready.first() == Some(&true) {
                match self.accept(socket.as_fd())? {
                    Accepted::Source => {}
                    Accepted::Closed => {
                        println!("oxlogd: oxinit closed the socket; draining and exiting");
                        self.drain_all();
                        return Ok(());
                    }
                }
            }
        }
    }

    /// One message: a unit name, with a descriptor attached.
    fn accept(&mut self, socket: BorrowedFd<'_>) -> Result<Accepted, String> {
        let mut payload = [0u8; MAX_MESSAGE];
        let mut space = [MaybeUninit::<u8>::uninit(); 128];
        let mut ancillary = RecvAncillaryBuffer::new(&mut space);

        let received = rustix::net::recvmsg(
            socket,
            &mut [IoSliceMut::new(&mut payload)],
            &mut ancillary,
            RecvFlags::empty(),
        )
        .map_err(|e| format!("recvmsg: {e}"))?;

        if received.bytes == 0 {
            return Ok(Accepted::Closed);
        }

        let mut fd = None;
        for message in ancillary.drain() {
            if let RecvAncillaryMessage::ScmRights(rights) = message {
                for right in rights {
                    fd = Some(right);
                }
            }
        }

        let Some(fd) = fd else {
            eprintln!("oxlogd: a message arrived with no descriptor; ignoring");
            return Ok(Accepted::Source);
        };

        let unit =
            String::from_utf8_lossy(payload.get(..received.bytes).unwrap_or_default()).into_owned();

        println!("oxlogd: shipping {unit}");
        self.sources.push(Source {
            fd,
            unit,
            splitter: Splitter::new(),
        });

        Ok(Accepted::Source)
    }

    /// Everything still buffered, on the way out.
    fn drain_all(&mut self) {
        for source in &mut self.sources {
            if let Some(last) = source.splitter.flush() {
                write(
                    &self.dir,
                    self.rotation,
                    &mut self.sinks,
                    &mut self.scratch,
                    &source.unit,
                    &last,
                );
            }
        }
    }
}

enum Accepted {
    Source,
    Closed,
}

/// Read what is there and write it out. `false` once the writer is gone.
fn drain(
    source: &mut Source,
    scratch: &mut Vec<u8>,
    dir: &Path,
    rotation: Rotation,
    sinks: &mut HashMap<String, Sink>,
) -> bool {
    let mut buf = [0u8; READ_SIZE];

    let read = match rustix::io::read(&source.fd, buf.as_mut_slice()) {
        Ok(0) => {
            // EOF: every write end is closed, so the service and everything it
            // forked are done with this pipe. A partial last line is still
            // something the service said, and is usually the interesting one.
            if let Some(last) = source.splitter.flush() {
                write(dir, rotation, sinks, scratch, &source.unit, &last);
            }
            return false;
        }
        Ok(read) => read,
        Err(rustix::io::Errno::INTR | rustix::io::Errno::AGAIN) => return true,
        Err(e) => {
            eprintln!("oxlogd: {}: read: {e}", source.unit);
            return false;
        }
    };

    // Collected rather than written inside the closure, because writing needs
    // `sinks` and the closure already borrows the splitter that owns it.
    let mut lines = Vec::new();
    source
        .splitter
        .push(buf.get(..read).unwrap_or_default(), |line| {
            lines.push(line.to_vec());
        });

    for line in &lines {
        write(dir, rotation, sinks, scratch, &source.unit, line);
    }

    true
}

/// One record into one unit's file, rotating first if it would not fit.
fn write(
    dir: &Path,
    rotation: Rotation,
    sinks: &mut HashMap<String, Sink>,
    scratch: &mut Vec<u8>,
    unit: &str,
    line: &[u8],
) {
    scratch.clear();
    oxinit_log::record(scratch, Timestamp::now(), line);

    let Some(size) = open(dir, sinks, unit).map(|sink| sink.size) else {
        return;
    };

    if rotation.should_rotate(size, scratch.len()) {
        rotate(dir, rotation, unit);

        // The old file was renamed out from under the open handle, which would
        // go on writing into it by inode — a rotation that renamed the file
        // and changed nothing else would produce one growing `.log.1` and an
        // empty `.log`. Dropping the handle is what starts the new file, and
        // reopening reads its size back as zero from the filesystem rather
        // than assuming it.
        sinks.remove(unit);
    }

    let Some(sink) = open(dir, sinks, unit) else {
        return;
    };

    match sink.file.write_all(scratch) {
        Ok(()) => sink.size = sink.size.saturating_add(scratch.len() as u64),
        Err(e) => eprintln!("oxlogd: {unit}: write: {e}"),
    }
}

fn open<'a>(dir: &Path, sinks: &'a mut HashMap<String, Sink>, unit: &str) -> Option<&'a mut Sink> {
    if !sinks.contains_key(unit) {
        let path = oxinit_log::path(dir, unit);

        let file = match OpenOptions::new().create(true).append(true).open(&path) {
            Ok(file) => file,
            Err(e) => {
                eprintln!("oxlogd: open {}: {e}", path.display());
                return None;
            }
        };

        let size = file.metadata().map(|meta| meta.len()).unwrap_or(0);
        sinks.insert(unit.to_owned(), Sink { file, size });
    }

    sinks.get_mut(unit)
}

/// Shift the generations down and drop the oldest.
///
/// Every step is best-effort: a generation that does not exist yet is the
/// normal case for the first few rotations, and a rename that fails costs one
/// old file rather than the live one.
fn rotate(dir: &Path, rotation: Rotation, unit: &str) {
    for step in rotation.plan(dir, unit) {
        match step {
            Move::Remove(path) => {
                let _ = std::fs::remove_file(path);
            }
            Move::Rename { from, to } => {
                let _ = std::fs::rename(from, to);
            }
        }
    }
}
