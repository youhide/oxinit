//! The console shell.
//!
//! M0 has no units, so the only thing PID 1 supervises is a shell. It is also
//! the last-resort recovery path for every later milestone: when the event
//! loop cannot continue, oxinit spawns this and keeps reaping rather than
//! exiting, because PID 1 exiting is a kernel panic.

use std::process::{Child, Command};

use crate::error::{Error, Result};

pub const SHELL: &str = "/bin/sh";

pub fn spawn() -> Result<Child> {
    // std resets the child's signal mask to empty between fork and exec. That
    // matters here: oxinit blocks every signal, and a child that inherited a
    // full block mask would be unable to be interrupted or killed normally.
    Command::new(SHELL).spawn().map_err(|source| Error::Spawn {
        path: SHELL,
        source,
    })
}
