//! Wire stdio to `/dev/console`.
//!
//! Until this runs, everything oxinit prints goes nowhere, which makes early
//! boot failures invisible. It is the third thing that happens, right after
//! the mounts that make `/dev/console` exist.

use rustix::fs::{Mode, OFlags};

use crate::error::{Error, Result};

pub fn attach() -> Result<()> {
    let console = rustix::fs::open("/dev/console", OFlags::RDWR | OFlags::NOCTTY, Mode::empty())
        .map_err(Error::Console)?;

    rustix::stdio::dup2_stdin(&console).map_err(Error::Console)?;
    rustix::stdio::dup2_stdout(&console).map_err(Error::Console)?;
    rustix::stdio::dup2_stderr(&console).map_err(Error::Console)?;

    Ok(())
}
