//! The console shell.
//!
//! M0 has no units, so the only thing PID 1 supervises is a shell. It is also
//! the last-resort recovery path for every later milestone: when the event
//! loop cannot continue, oxinit spawns this and keeps reaping rather than
//! exiting, because PID 1 exiting is a kernel panic.

use std::process::{Child, Command};

use crate::error::{Error, Result};
use crate::sys::raw::{setup_child, ChildSetup};

pub const SHELL: &str = "/bin/sh";

pub fn spawn() -> Result<Child> {
    let mut command = Command::new(SHELL);

    // For the signal mask reset, which oxinit does itself. A recovery shell
    // that inherited PID 1's full block mask could not be interrupted, could
    // not be suspended, and would pass that mask on to everything an operator
    // ran from it.
    setup_child(&mut command, ChildSetup::default());

    command.spawn().map_err(|source| Error::Spawn {
        path: SHELL,
        source,
    })
}
