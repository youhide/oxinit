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

/// Why a unit is being stopped.
///
/// It is not bookkeeping: it decides what happens when the cgroup finally
/// empties. An operator's stop overrides the restart policy, and a watchdog
/// kill deliberately does not — the whole point of a watchdog is to turn a
/// hang into something the restart policy can act on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopCause {
    /// Asked for, by `oxctl`, a `conflicts`, or shutdown. No restart.
    Requested,
    /// The watchdog deadline was missed. Treated exactly like a crash.
    Watchdog,
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
    /// Set while `Deactivating`. Decides the outcome once the cgroup empties.
    stop_cause: Option<StopCause>,
    /// Whether the stop timeout expired and `cgroup.kill` was used.
    killed: bool,
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
            stop_cause: None,
            killed: false,
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

    /// A `forking` service's initial process exited and left its cgroup
    /// populated.
    ///
    /// That is this type's readiness condition, so the unit is `Active` — but
    /// with no process oxinit forked left to watch. From here the cgroup is
    /// the only thing that knows whether the service is still running.
    pub fn forked_away(&mut self) {
        self.child = None;
        self.entered_active();
    }

    /// A `WATCHDOG=1` ping.
    pub fn pinged(&mut self) {
        self.last_ping = Some(Instant::now());
    }

    /// Whether the service has missed its watchdog deadline.
    pub fn watchdog_expired(&self, interval: Duration) -> bool {
        self.state == State::Active && self.last_ping.is_some_and(|last| last.elapsed() > interval)
    }

    /// `SIGTERM` has been sent; the unit is waiting for its cgroup to empty
    /// or for the stop timeout to expire.
    pub fn entered_deactivating(&mut self, cause: StopCause) {
        self.stop_cause = Some(cause);
        self.killed = false;
        self.state = State::Deactivating;
    }

    /// The stop timeout expired and `cgroup.kill` was used.
    pub fn stop_timed_out(&mut self) {
        self.killed = true;
    }

    /// Abandon a pending restart.
    ///
    /// A unit waiting out its backoff has no process to signal, so a stop has
    /// nothing to do to it — but leaving it `Restarting` would have a shutdown
    /// wait forever for a unit that was never going to come back.
    pub fn cancel_restart(&mut self) {
        if self.state == State::Restarting {
            self.child = None;
            self.active_since = None;
            self.state = State::Inactive;
        }
    }

    /// The cgroup emptied, which ends a stop.
    ///
    /// Returns the backoff delay when a restart applies — which it does only
    /// for a watchdog kill. Nothing else that reaches here restarts.
    pub fn deactivated(&mut self) -> Option<Duration> {
        self.child = None;
        let killed = std::mem::take(&mut self.killed);

        match self.stop_cause.take() {
            // The unit failed; the policy decides, exactly as for a crash.
            Some(StopCause::Watchdog) => self.schedule_restart(Exit::Abnormal),

            // An explicit stop overrides the restart policy. Having had to
            // reach for cgroup.kill is still a failure, and is reported as
            // one — the unit did not stop when it was asked to.
            Some(StopCause::Requested) | None => {
                self.active_since = None;
                self.status = None;
                self.state = if killed {
                    State::Failed
                } else {
                    State::Inactive
                };
                None
            }
        }
    }

    /// Decide what happens after the main process exits.
    ///
    /// Returns the backoff delay when a restart applies.
    pub fn on_exit(&mut self, exit: Exit) -> Option<Duration> {
        self.child = None;

        // Mid-stop, the exit of the main process is not the end of the unit:
        // the cgroup may still hold children it forked. `deactivated` owns
        // the transition, and the cgroup emptying is what triggers it.
        if self.state == State::Deactivating {
            return None;
        }

        self.schedule_restart(exit)
    }

    /// Apply the restart policy to a unit that has stopped running.
    fn schedule_restart(&mut self, exit: Exit) -> Option<Duration> {
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
    fn an_explicit_stop_overrides_the_restart_policy() {
        let mut instance = Instance::new(unit("always", "1s"));
        instance.entered_active();

        instance.entered_deactivating(StopCause::Requested);
        assert_eq!(instance.state, State::Deactivating);

        // The main process exiting mid-stop does not end the unit: its cgroup
        // may still hold children, and it must not restart either.
        assert_eq!(instance.on_exit(Exit::Abnormal), None);
        assert_eq!(instance.state, State::Deactivating);

        assert_eq!(instance.deactivated(), None);
        assert_eq!(instance.state, State::Inactive);
        assert_eq!(instance.restarts, 0);
    }

    #[test]
    fn a_stop_that_needed_cgroup_kill_is_a_failure() {
        let mut instance = Instance::new(unit("no", "1s"));
        instance.entered_active();
        instance.entered_deactivating(StopCause::Requested);

        // SIGTERM was ignored and the stop timeout expired.
        instance.stop_timed_out();

        assert_eq!(instance.deactivated(), None, "still no restart");
        assert_eq!(
            instance.state,
            State::Failed,
            "a unit that had to be killed did not stop when it was asked to"
        );
    }

    #[test]
    fn a_watchdog_kill_still_restarts() {
        let mut instance = Instance::new(unit("on-failure", "1s"));
        instance.entered_active();

        instance.entered_deactivating(StopCause::Watchdog);
        instance.on_exit(Exit::Abnormal);
        instance.stop_timed_out();

        // The point of a watchdog is to turn a hang into a failure the
        // restart policy acts on.
        assert_eq!(instance.deactivated(), Some(Duration::from_secs(1)));
        assert_eq!(instance.state, State::Restarting);
        assert_eq!(instance.restarts, 1);
    }

    #[test]
    fn a_watchdog_kill_on_a_unit_that_never_restarts_is_failed() {
        let mut instance = Instance::new(unit("no", "1s"));
        instance.entered_active();
        instance.entered_deactivating(StopCause::Watchdog);

        assert_eq!(instance.deactivated(), None);
        assert_eq!(instance.state, State::Failed);
    }

    #[test]
    fn the_kill_flag_does_not_survive_the_next_stop() {
        let mut instance = Instance::new(unit("no", "1s"));

        instance.entered_deactivating(StopCause::Requested);
        instance.stop_timed_out();
        instance.deactivated();
        assert_eq!(instance.state, State::Failed);

        // A later, well-behaved stop must not inherit the earlier failure.
        instance.entered_deactivating(StopCause::Requested);
        instance.deactivated();
        assert_eq!(instance.state, State::Inactive);
    }

    #[test]
    fn exit_classification() {
        assert_eq!(Exit::from_status(Some(0), None), Exit::Clean);
        assert_eq!(Exit::from_status(Some(3), None), Exit::Code(3));
        assert_eq!(Exit::from_status(None, Some(9)), Exit::Abnormal);
    }
}
