//! The service state machine and restart policy.
//!
//! States and transitions are specified in ARCHITECTURE.md. This is that
//! diagram in code.
//!
//! No Linux dependencies: the policy decisions here — when to restart, how far
//! to back off, when to forget a failure — are where the bugs are, and they
//! are tested on the host without a VM.
//!
//! This runs inside PID 1, so a panic here is a panic in PID 1.

#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

pub mod notify;

pub use notify::Message;

use std::fmt;
use std::process::Child;
use std::time::{Duration, Instant};

use oxinit_unit::{Restart, Unit};

/// Backoff never grows past this, or a service that fails once a week ends up
/// taking hours to come back.
const MAX_BACKOFF: Duration = Duration::from_secs(300);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Inactive,
    Activating,
    Active,
    Deactivating,
    Failed,
    Restarting,
}

impl fmt::Display for State {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            State::Inactive => "inactive",
            State::Activating => "activating",
            State::Active => "active",
            State::Deactivating => "deactivating",
            State::Failed => "failed",
            State::Restarting => "restarting",
        };
        f.write_str(name)
    }
}

/// How a service's main process ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exit {
    /// Exited with status 0.
    Clean,
    /// Exited with a non-zero status.
    Code(i32),
    /// Killed by a signal, or otherwise ended abnormally.
    Abnormal,
}

impl Exit {
    pub fn from_status(code: Option<i32>, signal: Option<i32>) -> Self {
        match (code, signal) {
            (Some(0), _) => Exit::Clean,
            (Some(code), _) => Exit::Code(code),
            (None, Some(_)) => Exit::Abnormal,
            (None, None) => Exit::Abnormal,
        }
    }
}

impl fmt::Display for Exit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Exit::Clean => f.write_str("exited cleanly"),
            Exit::Code(code) => write!(f, "exited with status {code}"),
            Exit::Abnormal => f.write_str("terminated abnormally"),
        }
    }
}

/// One unit's runtime state.
pub struct Instance {
    pub unit: Unit,
    pub state: State,
    pub child: Option<Child>,
    /// Consecutive restarts. Reset when the service stays up.
    pub restarts: u32,
    /// Next backoff delay. Doubles per consecutive restart.
    backoff: Duration,
    /// When the service last entered `Active`.
    active_since: Option<Instant>,
    /// Last `STATUS=` from the service. Free text, shown by `oxctl status`.
    pub status: Option<String>,
    /// Last `WATCHDOG=1`. `None` when the service never pinged.
    pub last_ping: Option<Instant>,
}

impl Instance {
    pub fn new(unit: Unit) -> Self {
        let backoff = unit
            .service()
            .map_or(Duration::from_millis(100), |s| s.restart_sec);

        Self {
            unit,
            state: State::Inactive,
            child: None,
            restarts: 0,
            backoff,
            active_since: None,
            status: None,
            last_ping: None,
        }
    }

    pub fn name(&self) -> &str {
        &self.unit.name
    }

    pub fn pid(&self) -> Option<u32> {
        self.child.as_ref().map(Child::id)
    }

    /// The base delay, from the unit's `restart-sec`.
    fn base_backoff(&self) -> Duration {
        self.unit
            .service()
            .map_or(Duration::from_millis(100), |s| s.restart_sec)
    }

    pub fn entered_activating(&mut self, child: Child) {
        self.child = Some(child);
        self.state = State::Activating;
    }

    /// exec or fork failed. Never reached Activating in any useful sense.
    pub fn failed_to_start(&mut self) {
        self.child = None;
        self.active_since = None;
        self.state = State::Failed;
    }

    pub fn entered_active(&mut self) {
        self.state = State::Active;
        self.active_since = Some(Instant::now());
        self.last_ping = Some(Instant::now());
    }

    /// A `WATCHDOG=1` ping.
    pub fn pinged(&mut self) {
        self.last_ping = Some(Instant::now());
    }

    /// Whether the service has missed its watchdog deadline.
    pub fn watchdog_expired(&self, interval: Duration) -> bool {
        self.state == State::Active && self.last_ping.is_some_and(|last| last.elapsed() > interval)
    }

    /// Decide what happens after the main process exits.
    ///
    /// Returns the backoff delay when a restart applies.
    pub fn on_exit(&mut self, exit: Exit) -> Option<Duration> {
        self.child = None;

        // A service that stayed up longer than its own backoff is working;
        // forget the failure history. Without this a service failing once a
        // day would eventually take the full cap to come back.
        if let Some(since) = self.active_since {
            if since.elapsed() > self.backoff {
                self.restarts = 0;
                self.backoff = self.base_backoff();
            }
        }
        self.active_since = None;

        let policy = self.unit.service().map_or(Restart::No, |s| s.restart);
        if !should_restart(policy, exit) {
            self.state = if exit == Exit::Clean {
                State::Inactive
            } else {
                State::Failed
            };
            return None;
        }

        let delay = self.backoff;
        self.restarts = self.restarts.saturating_add(1);
        // Saturating so a long-running failure cannot wrap the duration.
        self.backoff = self.backoff.saturating_mul(2).min(MAX_BACKOFF);
        self.state = State::Restarting;

        Some(delay)
    }

    /// An explicit stop overrides the restart policy.
    pub fn stopped(&mut self) {
        self.child = None;
        self.active_since = None;
        self.status = None;
        self.state = State::Inactive;
    }
}

/// Whether `policy` restarts after `exit`.
pub fn should_restart(policy: Restart, exit: Exit) -> bool {
    match policy {
        Restart::No => false,
        Restart::Always => true,
        Restart::OnFailure => !matches!(exit, Exit::Clean),
        // Not a non-zero exit: that is a deliberate, meaningful failure the
        // service is reporting, and retrying it just repeats it.
        Restart::OnAbnormal => matches!(exit, Exit::Abnormal),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unit(restart: &str, restart_sec: &str) -> Unit {
        let text = format!(
            "[service]\nexec = \"/bin/true\"\nrestart = \"{restart}\"\nrestart-sec = \"{restart_sec}\"\n"
        );
        oxinit_unit::parse("test", &text, "host").unwrap()
    }

    #[test]
    fn restart_policies_follow_the_table() {
        use Exit::{Abnormal, Clean, Code};

        assert!(!should_restart(Restart::No, Abnormal));
        assert!(!should_restart(Restart::No, Clean));

        assert!(should_restart(Restart::Always, Clean));
        assert!(should_restart(Restart::Always, Code(1)));
        assert!(should_restart(Restart::Always, Abnormal));

        assert!(!should_restart(Restart::OnFailure, Clean));
        assert!(should_restart(Restart::OnFailure, Code(1)));
        assert!(should_restart(Restart::OnFailure, Abnormal));

        // on-abnormal deliberately does not retry a non-zero exit.
        assert!(!should_restart(Restart::OnAbnormal, Clean));
        assert!(!should_restart(Restart::OnAbnormal, Code(1)));
        assert!(should_restart(Restart::OnAbnormal, Abnormal));
    }

    #[test]
    fn clean_exit_without_a_policy_is_inactive_not_failed() {
        let mut instance = Instance::new(unit("no", "1s"));
        assert_eq!(instance.on_exit(Exit::Clean), None);
        assert_eq!(instance.state, State::Inactive);
    }

    #[test]
    fn failure_without_a_policy_is_failed() {
        let mut instance = Instance::new(unit("no", "1s"));
        assert_eq!(instance.on_exit(Exit::Code(1)), None);
        assert_eq!(instance.state, State::Failed);
    }

    #[test]
    fn backoff_doubles_and_caps() {
        let mut instance = Instance::new(unit("always", "1s"));

        assert_eq!(
            instance.on_exit(Exit::Code(1)),
            Some(Duration::from_secs(1))
        );
        assert_eq!(instance.state, State::Restarting);
        assert_eq!(
            instance.on_exit(Exit::Code(1)),
            Some(Duration::from_secs(2))
        );
        assert_eq!(
            instance.on_exit(Exit::Code(1)),
            Some(Duration::from_secs(4))
        );
        assert_eq!(
            instance.on_exit(Exit::Code(1)),
            Some(Duration::from_secs(8))
        );

        for _ in 0..20 {
            instance.on_exit(Exit::Code(1));
        }
        assert_eq!(instance.on_exit(Exit::Code(1)), Some(MAX_BACKOFF));
    }

    #[test]
    fn restart_count_accumulates() {
        let mut instance = Instance::new(unit("always", "1s"));
        for _ in 0..3 {
            instance.on_exit(Exit::Code(1));
        }
        assert_eq!(instance.restarts, 3);
    }

    #[test]
    fn backoff_resets_after_the_service_stays_up() {
        let mut instance = Instance::new(unit("always", "1ms"));

        instance.on_exit(Exit::Code(1));
        instance.on_exit(Exit::Code(1));
        assert!(instance.restarts > 0);

        // Active for longer than the current backoff counts as working.
        instance.entered_active();
        std::thread::sleep(Duration::from_millis(20));

        assert_eq!(
            instance.on_exit(Exit::Code(1)),
            Some(Duration::from_millis(1)),
            "backoff should be back at restart-sec"
        );
        assert_eq!(instance.restarts, 1, "counted from zero again");
    }

    #[test]
    fn a_brief_active_period_does_not_reset_backoff() {
        let mut instance = Instance::new(unit("always", "10s"));

        instance.on_exit(Exit::Code(1));
        instance.entered_active();
        // Nowhere near the 10s backoff, so the history stands.
        assert_eq!(
            instance.on_exit(Exit::Code(1)),
            Some(Duration::from_secs(20))
        );
    }

    #[test]
    fn exit_classification() {
        assert_eq!(Exit::from_status(Some(0), None), Exit::Clean);
        assert_eq!(Exit::from_status(Some(3), None), Exit::Code(3));
        assert_eq!(Exit::from_status(None, Some(9)), Exit::Abnormal);
    }
}
