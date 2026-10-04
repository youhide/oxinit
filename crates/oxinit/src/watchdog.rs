//! The hardware watchdog, fed from the event loop.
//!
//! A hung PID 1 is the one failure nothing on the machine can recover from:
//! there is no supervisor above it. A hardware watchdog is outside it, so it
//! can — it resets the machine unless it is written to within its timeout.
//! oxinit writes to it from the loop itself rather than from a handler or a
//! timer alarm, so what keeps the machine alive is exactly the loop turning:
//! a handler that never returns, a loop that has fallen back to the console
//! shell, or a deadline heap that has stopped firing all end in a reset.
//!
//! When to write, when to stop, and what to do on the way down are decided in
//! `oxinit-watchdog`, which is tested on the host. This module is the device.

use std::io;
use std::time::{Duration, Instant};

use std::os::fd::{AsFd, OwnedFd};

use rustix::fs::{Mode, OFlags};

use oxinit_watchdog::{Action, Event, Policy, Release, Settings, WatchdogError};

use crate::container::Environment;
use crate::shutdown;
use crate::sys::raw;

pub struct Watchdog {
    policy: Policy,
    device: Option<OwnedFd>,
    /// What every decision is measured from. Also the start of the boot
    /// deadline.
    started: Instant,
    /// The device was missing and that has been said. Said once rather than
    /// at every retry: a driver that is a module may take a while to load.
    reported_missing: bool,
}

impl Watchdog {
    /// The watchdog the configuration asks for, if any.
    ///
    /// `None` when none is configured, in a container, or when the
    /// configuration is broken — which is said, and is not fatal. A machine
    /// whose watchdog was already running before oxinit, as an initramfs may
    /// leave it, is then reset when nothing takes it over; one where nothing
    /// started it boots without that protection. Either way the console says
    /// why.
    pub fn configure(environment: &Environment) -> Option<Self> {
        let config = match load().and_then(Settings::resolve) {
            Ok(config) => config?,
            Err(e) => {
                eprintln!("oxinit: watchdog: {e}; not opening the watchdog");
                return None;
            }
        };

        // The hardware is the host's. A privileged container can open the
        // host's /dev/watchdog, and feeding — or failing to feed — the host's
        // reset line is not a container's call.
        if environment.is_container() {
            println!(
                "oxinit: watchdog: {} is configured, but this is {environment}; \
                 leaving the host's hardware alone",
                config.device
            );
            return None;
        }

        match config.boot.as_ref() {
            Some(boot) => println!(
                "oxinit: watchdog: {}, {} s timeout; the boot has {} s to reach {}",
                config.device,
                config.timeout,
                boot.within.as_secs(),
                boot.unit
            ),
            None => println!(
                "oxinit: watchdog: {}, {} s timeout",
                config.device, config.timeout
            ),
        }

        Some(Self {
            policy: Policy::new(config),
            device: None,
            started: Instant::now(),
            reported_missing: false,
        })
    }

    /// The unit the boot deadline is waiting for, while it is.
    pub fn awaiting(&self) -> Option<&str> {
        self.policy.awaiting()
    }

    /// Said once, after the units have loaded: a boot unit that does not
    /// exist is a boot nothing can confirm. The deadline still applies — the
    /// configuration asked for a confirmed boot, and running without one is a
    /// different configuration, not a smaller version of it.
    pub fn check_boot_unit(&self, known: bool) {
        if let (Some(unit), false) = (self.awaiting(), known) {
            eprintln!(
                "oxinit: watchdog: boot-unit `{unit}` is not a loaded unit; \
                 nothing can confirm this boot, and the watchdog will reset it"
            );
        }
    }

    /// How long the loop may sleep before there is something to do here.
    pub fn wait_bound(&self) -> Option<Duration> {
        self.policy.next_wake(self.now())
    }

    /// Called once per wake-up of the loop, after everything else.
    ///
    /// `booted` is whether the unit [`Watchdog::awaiting`] names has come up.
    /// `shutting_down` suspends the boot deadline; asked every time rather
    /// than told once, so no path into a shutdown can forget to say so.
    pub fn tick(&mut self, booted: bool, shutting_down: bool) {
        if shutting_down {
            self.policy.begin_shutdown();
        }

        let now = self.now();
        let tick = self.policy.tick(now, booted);

        match tick.event {
            Some(Event::BootConfirmed) => {
                println!("oxinit: watchdog: the boot is confirmed");
            }
            Some(Event::BootOverdue) => {
                let config = self.policy.config();
                eprintln!(
                    "oxinit: watchdog: the boot did not reach {} within {} s; \
                     no longer feeding {}, which resets the machine when its \
                     {} s timeout runs out",
                    config.boot.as_ref().map_or("?", |boot| boot.unit.as_str()),
                    config.boot.as_ref().map_or(0, |boot| boot.within.as_secs()),
                    config.device,
                    config.timeout
                );
            }
            None => {}
        }

        match tick.action {
            Action::Open => self.open(now),
            Action::Pet => self.pet(),
            Action::Nothing => {}
        }
    }

    /// The end of a shutdown, just before `reboot(2)`.
    pub fn release(&mut self, action: shutdown::Action) {
        match oxinit_watchdog::release(action == shutdown::Action::Reboot) {
            Release::Disarm => {
                let Some(device) = self.device.take() else {
                    return;
                };
                // The magic close: `V` is the last thing written, and the
                // close that follows stops the timer. A driver without magic
                // close stops on any close; one built `nowayout` does not stop
                // at all, and says so in the kernel log.
                match rustix::io::write(&device, b"V") {
                    Ok(_) => println!("oxinit: watchdog: stopped"),
                    Err(e) => eprintln!("oxinit: watchdog: stop: {e}"),
                }
                drop(device);
            }
            Release::KeepRunning => {
                if self.device.is_some() {
                    self.pet();
                    println!("oxinit: watchdog: left running through the reboot");
                }
            }
        }
    }

    fn now(&self) -> Duration {
        self.started.elapsed()
    }

    fn open(&mut self, now: Duration) {
        let path = self.policy.config().device.clone();

        // CLOEXEC above all: a service that inherited this descriptor could
        // keep the machine alive after PID 1 had hung, or reset it by
        // closing it.
        let device = match rustix::fs::open(&path, OFlags::WRONLY | OFlags::CLOEXEC, Mode::empty())
        {
            Ok(device) => device,
            Err(e) => {
                if !self.reported_missing {
                    eprintln!(
                        "oxinit: watchdog: open {path}: {e}; looking again as it may yet appear"
                    );
                    self.reported_missing = true;
                }
                self.policy.open_failed(now);
                return;
            }
        };

        let wanted = i32::try_from(self.policy.config().timeout).unwrap_or(i32::MAX);
        let applied = match raw::watchdog_set_timeout(device.as_fd(), wanted) {
            Ok(applied) => Some(applied),
            Err(e) => {
                eprintln!("oxinit: watchdog: {path} would not take a {wanted} s timeout: {e}");
                raw::watchdog_get_timeout(device.as_fd()).ok()
            }
        };
        let applied = applied.and_then(|secs| u32::try_from(secs).ok());

        match applied {
            Some(secs) => println!("oxinit: watchdog: feeding {path}, {secs} s timeout"),
            None => println!(
                "oxinit: watchdog: feeding {path} every second; it will not say its timeout"
            ),
        }

        self.policy.opened(now, applied);
        self.device = Some(device);
    }

    fn pet(&self) {
        let Some(device) = self.device.as_ref() else {
            return;
        };

        // Any byte but `V` is a keepalive. Not the ioctl: a write is what
        // every driver has understood since before the ioctl existed.
        if let Err(e) = rustix::io::write(device, b"\0") {
            eprintln!("oxinit: watchdog: {e}");
        }
    }
}

/// The configuration, from the files and the kernel command line.
///
/// `/etc` replaces `/usr/lib` wholly, as it does for units. The command line
/// then wins key by key, because it is one boot's exception to whichever
/// file was in force.
fn load() -> Result<Settings, WatchdogError> {
    let file = match read(oxinit_watchdog::ETC_FILE)? {
        Some(settings) => settings,
        None => read(oxinit_watchdog::VENDOR_FILE)?.unwrap_or_default(),
    };

    // No /proc means no command line to read, which is not an error here:
    // the mounts have already said so if it matters.
    let cmdline = std::fs::read_to_string("/proc/cmdline").unwrap_or_default();

    Ok(Settings::from_cmdline(&cmdline)?.over(file))
}

fn read(path: &str) -> Result<Option<Settings>, WatchdogError> {
    match std::fs::read_to_string(path) {
        Ok(text) => Settings::parse(path, &text).map(Some),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(WatchdogError::Toml {
            origin: path.to_owned(),
            message: e.to_string(),
        }),
    }
}
