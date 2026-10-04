//! The hardware watchdog: what is configured, and when to pet it.
//!
//! A hardware watchdog resets the machine unless something writes to it
//! within its timeout. oxinit is that something, from its event loop — so a
//! PID 1 that stops turning its loop is a machine that resets, which is the
//! only recovery there is from a hung PID 1. See ARCHITECTURE.md.
//!
//! On top of that, a **boot deadline**: until a named unit has come up, oxinit
//! pets only for so long. A boot that never gets there stops being fed and is
//! reset, which is what lets a boot manager count a hang as a failed attempt.
//!
//! Nothing here opens a device or reads a clock. Every decision takes the time
//! since oxinit started as an argument, so "what happens if the boot unit comes
//! up one second after the deadline" is a host test.
//!
//! `oxinit` depends on this crate, so a panic here is a panic in PID 1.

#![forbid(unsafe_code)]
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::time::Duration;

use oxinit_unit::DurationValue;
use serde::Deserialize;
use thiserror::Error;

/// Packaged configuration. Lowest precedence.
pub const VENDOR_FILE: &str = "/usr/lib/oxinit/watchdog.toml";

/// Operator configuration. Replaces the vendor file wholly, as a unit file in
/// `/etc` replaces one in `/usr/lib`: the effective configuration is always a
/// file you can `cat`, plus whatever the kernel command line says.
pub const ETC_FILE: &str = "/etc/oxinit/watchdog.toml";

/// The device when the configuration names none.
///
/// `watchdog0` rather than the legacy `/dev/watchdog`: both are the first
/// watchdog, but only the numbered node exists for every one of them.
pub const DEFAULT_DEVICE: &str = "/dev/watchdog0";

/// Kernel command line keys are this, followed by a key from the file.
pub const CMDLINE_PREFIX: &str = "oxinit.watchdog.";

/// Pet interval when the driver would neither take a timeout nor say what its
/// own is. Short enough for any hardware worth having.
const UNKNOWN_TIMEOUT_INTERVAL: Duration = Duration::from_secs(1);

/// The largest timeout the ioctl can carry: it is a C `int`.
const MAX_TIMEOUT_SECS: u64 = i32::MAX as u64;

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum WatchdogError {
    #[error("{origin}: {message}")]
    Toml { origin: String, message: String },

    #[error("{origin}: `{key}` is not a key oxinit knows")]
    UnknownKey { origin: String, key: String },

    #[error("{origin}: {key}: {message}")]
    Value {
        origin: String,
        key: String,
        message: String,
    },

    #[error(
        "timeout-sec must be whole seconds, between 1s and {MAX_TIMEOUT_SECS}s, \
         or 0s for no watchdog; the hardware counts seconds"
    )]
    Timeout,

    #[error(
        "boot-sec needs boot-unit: a boot deadline is a deadline for something, \
         and boot-unit names what"
    )]
    BootWithoutUnit,

    #[error(
        "boot-unit needs boot-sec: naming the unit that confirms a boot means \
         nothing without a deadline for it"
    )]
    UnitWithoutBoot,
}

/// What a file or the command line said, key by key.
///
/// Every key optional, so the two sources can be layered: the command line
/// wins key by key over the file. That differs from how the files themselves
/// combine, and on purpose — a file is an installation's configuration and is
/// replaced whole; a command line entry is one boot's exception to it, which
/// is what a boot manager entry or a test image wants to say.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Settings {
    pub device: Option<String>,
    pub timeout: Option<Duration>,
    pub boot_unit: Option<String>,
    pub boot: Option<Duration>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
struct RawFile {
    device: Option<String>,
    timeout_sec: Option<DurationValue>,
    boot_unit: Option<String>,
    boot_sec: Option<DurationValue>,
}

impl Settings {
    /// A `watchdog.toml`. `origin` is what to call it in an error.
    pub fn parse(origin: &str, text: &str) -> Result<Self, WatchdogError> {
        let raw: RawFile = basic_toml::from_str(text).map_err(|e| WatchdogError::Toml {
            origin: origin.to_owned(),
            message: e.to_string(),
        })?;

        Ok(Self {
            device: raw.device,
            timeout: raw.timeout_sec.map(|value| value.0),
            boot_unit: raw.boot_unit,
            boot: raw.boot_sec.map(|value| value.0),
        })
    }

    /// The `oxinit.watchdog.*` entries of a kernel command line.
    ///
    /// Everything else on the line is somebody else's and is ignored. An
    /// unknown key under the prefix is an error rather than ignored, for the
    /// reason an unknown key in a unit is: a typo must not quietly mean the
    /// default.
    pub fn from_cmdline(cmdline: &str) -> Result<Self, WatchdogError> {
        let origin = "kernel command line";
        let mut settings = Self::default();

        for word in cmdline.split_whitespace() {
            let Some(rest) = word.strip_prefix(CMDLINE_PREFIX) else {
                continue;
            };
            let (key, value) = rest.split_once('=').unwrap_or((rest, ""));

            let duration = |value: &str| {
                oxinit_unit::parse_duration(value).map_err(|e| WatchdogError::Value {
                    origin: origin.to_owned(),
                    key: format!("{CMDLINE_PREFIX}{key}"),
                    message: e.to_string(),
                })
            };

            match key {
                "device" => settings.device = Some(value.to_owned()),
                "timeout-sec" => settings.timeout = Some(duration(value)?),
                "boot-unit" => settings.boot_unit = Some(value.to_owned()),
                "boot-sec" => settings.boot = Some(duration(value)?),
                _ => {
                    return Err(WatchdogError::UnknownKey {
                        origin: origin.to_owned(),
                        key: format!("{CMDLINE_PREFIX}{key}"),
                    })
                }
            }
        }

        Ok(settings)
    }

    /// `self`, with anything it leaves unsaid taken from `base`.
    pub fn over(self, base: Settings) -> Settings {
        Settings {
            device: self.device.or(base.device),
            timeout: self.timeout.or(base.timeout),
            boot_unit: self.boot_unit.or(base.boot_unit),
            boot: self.boot.or(base.boot),
        }
    }

    /// What to run with. `None` is no watchdog: no timeout configured, or a
    /// timeout of zero.
    ///
    /// Zero rather than a separate switch, so that the one key a command line
    /// needs to turn the watchdog off for a boot is the key that turns it on.
    pub fn resolve(self) -> Result<Option<Config>, WatchdogError> {
        let timeout = match self.timeout {
            None => return Ok(None),
            Some(timeout) if timeout.is_zero() => return Ok(None),
            Some(timeout) => timeout,
        };

        if timeout.subsec_nanos() != 0 || timeout.as_secs() > MAX_TIMEOUT_SECS {
            return Err(WatchdogError::Timeout);
        }
        let timeout = u32::try_from(timeout.as_secs()).map_err(|_| WatchdogError::Timeout)?;

        // A zero deadline is no deadline, for the same reason a zero timeout
        // is no watchdog: it is how one boot turns it off.
        let boot = match (self.boot.filter(|boot| !boot.is_zero()), self.boot_unit) {
            (None, None) => None,
            (None, Some(_)) if self.boot.is_some() => None,
            (None, Some(_)) => return Err(WatchdogError::UnitWithoutBoot),
            (Some(_), None) => return Err(WatchdogError::BootWithoutUnit),
            (Some(within), Some(unit)) => Some(Boot { unit, within }),
        };

        Ok(Some(Config {
            device: self.device.unwrap_or_else(|| DEFAULT_DEVICE.to_owned()),
            timeout,
            boot,
        }))
    }
}

/// A watchdog to feed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub device: String,
    /// Seconds, as the driver is asked for them. It may round.
    pub timeout: u32,
    pub boot: Option<Boot>,
}

/// How long a boot has to confirm itself, and what confirms it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Boot {
    /// The boot is good once this unit has come up: become `Active`, or — a
    /// `oneshot` — run to a clean exit.
    pub unit: String,
    /// Measured from oxinit's own start. What came before it — firmware,
    /// kernel, initramfs — is whoever armed the watchdog first's to bound.
    pub within: Duration,
}

/// How often to pet a watchdog with this timeout: twice per timeout.
///
/// Half rather than something tighter because the loop is woken for exactly
/// this, so the margin is scheduling latency, not the length of a handler.
pub fn pet_interval(timeout_secs: u32) -> Duration {
    Duration::from_secs(u64::from(timeout_secs)) / 2
}

/// Where the boot deadline stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BootState {
    /// No deadline configured, or the unit came up in time.
    Confirmed,
    /// Waiting for the unit, inside the deadline.
    Pending,
    /// The deadline passed first. The watchdog is no longer fed, and the
    /// machine resets one timeout from now. Final.
    Overdue,
}

/// What the loop should do with the device, now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Nothing,
    /// Try to open it. It was not there, or never tried.
    Open,
    /// Write to it.
    Pet,
}

/// Something worth a line on the console.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    BootConfirmed,
    BootOverdue,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Tick {
    pub action: Action,
    pub event: Option<Event>,
}

/// What to do with the device at the end of a shutdown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Release {
    /// Write the magic `V` and close: the driver stops the timer. A machine
    /// told to power off or halt must not be reset afterwards.
    Disarm,
    /// Pet it one last time and keep it running into `reboot(2)`. If the
    /// kernel hangs on the way down, the watchdog finishes the reboot it was
    /// asked for. A driver that stops itself at reboot does so after the
    /// point where that hang could happen.
    KeepRunning,
}

/// The decision at the end of a shutdown, by whether it ends in a reboot.
pub fn release(reboot: bool) -> Release {
    if reboot {
        Release::KeepRunning
    } else {
        Release::Disarm
    }
}

/// When to pet, and when to stop.
///
/// Times are durations since oxinit started, on the monotonic clock.
#[derive(Debug, Clone)]
pub struct Policy {
    config: Config,
    boot: BootState,
    /// Whether the device is held.
    open: bool,
    /// How often to pet, from the timeout the driver settled on.
    interval: Duration,
    last_pet: Option<Duration>,
    last_try: Option<Duration>,
    shutting_down: bool,
}

impl Policy {
    pub fn new(config: Config) -> Self {
        let boot = match config.boot {
            Some(_) => BootState::Pending,
            None => BootState::Confirmed,
        };
        let interval = pet_interval(config.timeout);

        Self {
            config,
            boot,
            open: false,
            interval,
            last_pet: None,
            last_try: None,
            shutting_down: false,
        }
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    pub fn boot_state(&self) -> BootState {
        self.boot
    }

    /// The unit whose coming up confirms the boot, while that is still being
    /// waited for.
    pub fn awaiting(&self) -> Option<&str> {
        match (self.boot, self.config.boot.as_ref()) {
            (BootState::Pending, Some(boot)) => Some(&boot.unit),
            _ => None,
        }
    }

    /// The device is open. Opening it pings it, so that counts as a pet.
    ///
    /// `applied` is the timeout the driver settled on, which may differ from
    /// the one asked for; `None` when it would neither take one nor report
    /// its own, and then the interval is short enough for any hardware.
    pub fn opened(&mut self, now: Duration, applied: Option<u32>) {
        self.open = true;
        self.last_pet = Some(now);
        self.interval = match applied {
            Some(secs) if secs > 0 => pet_interval(secs),
            _ => UNKNOWN_TIMEOUT_INTERVAL,
        };
    }

    /// Opening failed — most likely the device does not exist yet, because
    /// its driver is a module nothing has loaded. Tried again one pet
    /// interval later.
    pub fn open_failed(&mut self, now: Duration) {
        self.last_try = Some(now);
    }

    /// The machine is going down. The boot deadline stops applying: a boot
    /// interrupted by a shutdown is not a boot that hung, and the shutdown has
    /// a deadline of its own. Petting continues until the end, because a
    /// shutdown that hangs is exactly a hung PID 1.
    pub fn begin_shutdown(&mut self) {
        self.shutting_down = true;
    }

    /// Decide, and record the decision as done.
    ///
    /// `booted` is whether the boot unit has come up. Called after every
    /// wake-up of the loop, including the ones [`Policy::next_wake`] asked
    /// for.
    pub fn tick(&mut self, now: Duration, booted: bool) -> Tick {
        let event = self.update_boot(now, booted);

        let action = if !self.open {
            if self.due(self.last_try, now) {
                self.last_try = Some(now);
                Action::Open
            } else {
                Action::Nothing
            }
        } else if self.boot == BootState::Overdue {
            // Starved on purpose. Nothing is ever written again, and the
            // device stays open so that nothing else can feed it either.
            Action::Nothing
        } else if self.due(self.last_pet, now) {
            self.last_pet = Some(now);
            Action::Pet
        } else {
            Action::Nothing
        };

        Tick { action, event }
    }

    /// How long the loop may sleep before [`Policy::tick`] has something to
    /// do. `None` when nothing will ever be due again.
    pub fn next_wake(&self, now: Duration) -> Option<Duration> {
        let mut soonest: Option<Duration> = None;
        let mut consider = |at: Duration| {
            let wait = at.saturating_sub(now);
            soonest = Some(soonest.map_or(wait, |s| s.min(wait)));
        };

        if !self.open {
            consider(self.last_try.map_or(now, |last| last + self.interval));
        } else if self.boot != BootState::Overdue {
            consider(self.last_pet.map_or(now, |last| last + self.interval));
        }

        if let (BootState::Pending, false, Some(boot)) =
            (self.boot, self.shutting_down, self.config.boot.as_ref())
        {
            consider(boot.within);
        }

        soonest
    }

    fn due(&self, last: Option<Duration>, now: Duration) -> bool {
        last.is_none_or(|last| now.saturating_sub(last) >= self.interval)
    }

    fn update_boot(&mut self, now: Duration, booted: bool) -> Option<Event> {
        if self.boot != BootState::Pending {
            return None;
        }

        // Asked first: a unit that came up in the same wake-up as the
        // deadline came up in time.
        if booted {
            self.boot = BootState::Confirmed;
            return Some(Event::BootConfirmed);
        }

        let within = self.config.boot.as_ref().map(|boot| boot.within)?;
        if !self.shutting_down && now >= within {
            self.boot = BootState::Overdue;
            return Some(Event::BootOverdue);
        }

        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secs(n: u64) -> Duration {
        Duration::from_secs(n)
    }

    fn config(timeout: u32, boot: Option<(&str, u64)>) -> Config {
        Config {
            device: DEFAULT_DEVICE.to_owned(),
            timeout,
            boot: boot.map(|(unit, within)| Boot {
                unit: unit.to_owned(),
                within: secs(within),
            }),
        }
    }

    fn resolve(text: &str) -> Result<Option<Config>, WatchdogError> {
        Settings::parse("test", text).and_then(Settings::resolve)
    }

    #[test]
    fn no_timeout_is_no_watchdog() {
        assert_eq!(resolve(""), Ok(None));
        assert_eq!(resolve("device = \"/dev/watchdog1\"\n"), Ok(None));
    }

    #[test]
    fn a_zero_timeout_is_no_watchdog() {
        assert_eq!(resolve("timeout-sec = \"0s\"\n"), Ok(None));
    }

    #[test]
    fn a_timeout_alone_feeds_the_default_device() {
        let config = resolve("timeout-sec = \"30s\"\n").unwrap().unwrap();
        assert_eq!(config.device, DEFAULT_DEVICE);
        assert_eq!(config.timeout, 30);
        assert_eq!(config.boot, None);
    }

    #[test]
    fn every_key() {
        let config = resolve(
            "device = \"/dev/watchdog1\"\ntimeout-sec = \"1min\"\n\
             boot-unit = \"boot-ok\"\nboot-sec = \"3min\"\n",
        )
        .unwrap()
        .unwrap();
        assert_eq!(config.device, "/dev/watchdog1");
        assert_eq!(config.timeout, 60);
        assert_eq!(
            config.boot,
            Some(Boot {
                unit: "boot-ok".to_owned(),
                within: secs(180)
            })
        );
    }

    #[test]
    fn the_hardware_counts_whole_seconds() {
        assert_eq!(
            resolve("timeout-sec = \"1500ms\"\n"),
            Err(WatchdogError::Timeout)
        );
        assert_eq!(
            resolve("timeout-sec = \"100000days\"\n"),
            Err(WatchdogError::Timeout)
        );
    }

    #[test]
    fn a_deadline_and_its_unit_come_together() {
        assert_eq!(
            resolve("timeout-sec = \"30s\"\nboot-sec = \"3min\"\n"),
            Err(WatchdogError::BootWithoutUnit)
        );
        assert_eq!(
            resolve("timeout-sec = \"30s\"\nboot-unit = \"boot-ok\"\n"),
            Err(WatchdogError::UnitWithoutBoot)
        );
    }

    #[test]
    fn a_zero_deadline_is_no_deadline_even_with_a_unit() {
        let config = resolve("timeout-sec = \"30s\"\nboot-unit = \"x\"\nboot-sec = \"0s\"\n")
            .unwrap()
            .unwrap();
        assert_eq!(config.boot, None);
    }

    #[test]
    fn unknown_keys_and_bare_numbers_are_refused() {
        assert!(matches!(
            resolve("timeout = \"30s\"\n"),
            Err(WatchdogError::Toml { .. })
        ));
        assert!(matches!(
            resolve("timeout-sec = 30\n"),
            Err(WatchdogError::Toml { .. })
        ));
    }

    #[test]
    fn the_command_line_wins_key_by_key() {
        let file = Settings::parse(
            "file",
            "timeout-sec = \"30s\"\nboot-unit = \"boot-ok\"\nboot-sec = \"3min\"\n",
        )
        .unwrap();
        let cmdline = Settings::from_cmdline(
            "root=/dev/vda quiet oxinit.watchdog.boot-sec=45s console=ttyS0",
        )
        .unwrap();

        let config = cmdline.over(file).resolve().unwrap().unwrap();
        assert_eq!(config.timeout, 30, "from the file");
        assert_eq!(
            config.boot.unwrap().within,
            secs(45),
            "from the command line"
        );
    }

    #[test]
    fn the_command_line_can_turn_it_off() {
        let file = Settings::parse("file", "timeout-sec = \"30s\"\n").unwrap();
        let cmdline = Settings::from_cmdline("oxinit.watchdog.timeout-sec=0s").unwrap();
        assert_eq!(cmdline.over(file).resolve(), Ok(None));
    }

    #[test]
    fn the_command_line_alone_can_turn_it_on() {
        let cmdline = Settings::from_cmdline(
            "oxinit.watchdog.timeout-sec=10s oxinit.watchdog.device=/dev/watchdog1",
        )
        .unwrap();
        let config = cmdline
            .over(Settings::default())
            .resolve()
            .unwrap()
            .unwrap();
        assert_eq!(config.device, "/dev/watchdog1");
        assert_eq!(config.timeout, 10);
    }

    #[test]
    fn the_command_line_refuses_typos_under_its_prefix() {
        assert!(matches!(
            Settings::from_cmdline("oxinit.watchdog.timout-sec=10s"),
            Err(WatchdogError::UnknownKey { .. })
        ));
        assert!(matches!(
            Settings::from_cmdline("oxinit.watchdog.timeout-sec=10"),
            Err(WatchdogError::Value { .. })
        ));
        // Somebody else's prefix is somebody else's business.
        assert_eq!(
            Settings::from_cmdline("hideos.watchdog=45 oxinit.debug"),
            Ok(Settings::default())
        );
    }

    #[test]
    fn pets_twice_per_timeout() {
        assert_eq!(pet_interval(30), secs(15));
        assert_eq!(pet_interval(1), Duration::from_millis(500));
    }

    #[test]
    fn opens_first_then_pets_on_the_interval() {
        let mut policy = Policy::new(config(10, None));

        assert_eq!(policy.tick(secs(0), false).action, Action::Open);
        policy.opened(secs(0), Some(10));

        assert_eq!(policy.tick(secs(1), false).action, Action::Nothing);
        assert_eq!(policy.next_wake(secs(1)), Some(secs(4)));
        assert_eq!(policy.tick(secs(5), false).action, Action::Pet);
        assert_eq!(policy.tick(secs(6), false).action, Action::Nothing);
        assert_eq!(policy.tick(secs(10), false).action, Action::Pet);
    }

    #[test]
    fn the_interval_follows_the_timeout_the_driver_settled_on() {
        let mut policy = Policy::new(config(60, None));
        policy.tick(secs(0), false);
        // Asked for 60, the hardware could only do 20.
        policy.opened(secs(0), Some(20));
        assert_eq!(policy.next_wake(secs(0)), Some(secs(10)));
    }

    #[test]
    fn an_unknown_timeout_pets_every_second() {
        let mut policy = Policy::new(config(60, None));
        policy.tick(secs(0), false);
        policy.opened(secs(0), None);
        assert_eq!(policy.next_wake(secs(0)), Some(secs(1)));
    }

    #[test]
    fn a_missing_device_is_tried_again_an_interval_later() {
        let mut policy = Policy::new(config(10, None));
        assert_eq!(policy.tick(secs(0), false).action, Action::Open);
        policy.open_failed(secs(0));

        assert_eq!(policy.tick(secs(2), false).action, Action::Nothing);
        assert_eq!(policy.next_wake(secs(2)), Some(secs(3)));
        assert_eq!(policy.tick(secs(5), false).action, Action::Open);
    }

    #[test]
    fn a_boot_that_confirms_in_time_is_fed_for_ever() {
        let mut policy = Policy::new(config(10, Some(("boot-ok", 60))));
        policy.tick(secs(0), false);
        policy.opened(secs(0), Some(10));
        assert_eq!(policy.awaiting(), Some("boot-ok"));

        let tick = policy.tick(secs(30), true);
        assert_eq!(tick.event, Some(Event::BootConfirmed));
        assert_eq!(policy.boot_state(), BootState::Confirmed);
        assert_eq!(policy.awaiting(), None);

        // Long after the deadline, still fed.
        assert_eq!(policy.tick(secs(3600), false).action, Action::Pet);
    }

    #[test]
    fn a_boot_that_misses_its_deadline_stops_being_fed() {
        let mut policy = Policy::new(config(10, Some(("boot-ok", 60))));
        policy.tick(secs(0), false);
        policy.opened(secs(0), Some(10));

        assert_eq!(policy.tick(secs(55), false).action, Action::Pet);
        // The loop is told to wake at the deadline, not a pet later.
        assert_eq!(policy.next_wake(secs(58)), Some(secs(2)));

        let tick = policy.tick(secs(60), false);
        assert_eq!(tick.event, Some(Event::BootOverdue));
        assert_eq!(tick.action, Action::Nothing);

        // Final: the unit turning up afterwards does not rescue it.
        let tick = policy.tick(secs(61), true);
        assert_eq!(tick.event, None);
        assert_eq!(tick.action, Action::Nothing);
        assert_eq!(policy.tick(secs(70), true).action, Action::Nothing);
        assert_eq!(policy.next_wake(secs(70)), None);
    }

    #[test]
    fn coming_up_at_the_deadline_is_coming_up_in_time() {
        let mut policy = Policy::new(config(10, Some(("boot-ok", 60))));
        policy.tick(secs(0), false);
        policy.opened(secs(0), Some(10));
        assert_eq!(
            policy.tick(secs(60), true).event,
            Some(Event::BootConfirmed)
        );
    }

    #[test]
    fn a_shutdown_suspends_the_boot_deadline_but_not_the_feeding() {
        let mut policy = Policy::new(config(10, Some(("boot-ok", 60))));
        policy.tick(secs(0), false);
        policy.opened(secs(0), Some(10));

        policy.begin_shutdown();
        let tick = policy.tick(secs(65), false);
        assert_eq!(tick.event, None);
        assert_eq!(tick.action, Action::Pet);
        assert_eq!(policy.next_wake(secs(65)), Some(secs(5)));
    }

    #[test]
    fn the_deadline_counts_while_the_device_is_missing() {
        let mut policy = Policy::new(config(10, Some(("boot-ok", 20))));
        policy.tick(secs(0), false);
        policy.open_failed(secs(0));

        assert_eq!(policy.tick(secs(20), false).event, Some(Event::BootOverdue));

        // A device that turns up afterwards is opened — which starts it —
        // and then never fed.
        assert_eq!(policy.tick(secs(25), false).action, Action::Open);
        policy.opened(secs(25), Some(10));
        assert_eq!(policy.tick(secs(40), false).action, Action::Nothing);
    }

    #[test]
    fn power_off_and_halt_disarm_and_a_reboot_keeps_it_running() {
        assert_eq!(release(false), Release::Disarm);
        assert_eq!(release(true), Release::KeepRunning);
    }
}
