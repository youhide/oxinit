//! When a timer next elapses.
//!
//! Two kinds of schedule, and a unit may declare both:
//!
//! - **Monotonic** — `on-boot` for the first firing and `interval` for every
//!   one after it, each measured from an event rather than from a clock.
//! - **Calendar** — a wall-clock time from a closed vocabulary.
//!
//! Nothing here makes a syscall or reads a clock. Every function takes the
//! current time as an argument, which is what makes "what does this schedule
//! do at 23:59:59 on the 28th of February in a leap year" a test rather than a
//! thing you find out in production.
//!
//! `oxinit` depends on this crate, so a panic here is a panic in PID 1.

#![forbid(unsafe_code)]
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::fmt;
use std::time::Duration;

/// Seconds since the Unix epoch, UTC.
pub type Unix = u64;

const MINUTE: u64 = 60;
const HOUR: u64 = 60 * MINUTE;
const DAY: u64 = 24 * HOUR;

/// A wall-clock schedule.
///
/// The vocabulary is closed, the way the `%` specifier set is closed. It is
/// not a cron expression and will not grow into one: a schedule nobody can
/// misread is worth more than one that can say anything. `hourly` and a
/// literal time cover what units actually ask for, and anything past that is
/// an argument for a real scheduler rather than a bigger grammar.
///
/// **UTC.** oxinit has no timezone database and is not going to carry one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Calendar {
    /// Every hour, on the hour.
    Hourly,
    /// Every day at 00:00:00.
    Daily,
    /// Every Monday at 00:00:00.
    Weekly,
    /// Every day at this time.
    At { hour: u32, minute: u32, second: u32 },
}

impl Calendar {
    /// `hourly`, `daily`, `weekly`, `HH:MM`, or `HH:MM:SS`.
    pub fn parse(text: &str) -> Result<Self, CalendarError> {
        match text {
            "hourly" => return Ok(Calendar::Hourly),
            "daily" => return Ok(Calendar::Daily),
            "weekly" => return Ok(Calendar::Weekly),
            _ => {}
        }

        let mut parts = text.split(':');
        let hour = field(parts.next(), 23, text)?;
        let minute = field(parts.next(), 59, text)?;

        // Absent rather than invalid: `HH:MM` is a time, and the second it
        // means is zero.
        let second = match parts.next() {
            Some(field) => number(field, 59, text)?,
            None => 0,
        };

        if parts.next().is_some() {
            return Err(CalendarError {
                expression: text.to_owned(),
            });
        }

        Ok(Calendar::At {
            hour,
            minute,
            second,
        })
    }

    /// The first moment at or after `now` that this schedule names.
    ///
    /// At or after, not strictly after: a caller that has just fired passes
    /// `now + 1`, and one that is arming for the first time wants the very
    /// next occurrence even if that is this second.
    pub fn next_at(self, now: Unix) -> Unix {
        match self {
            Calendar::Hourly => round_up(now, HOUR),
            Calendar::Daily => round_up(now, DAY),
            Calendar::Weekly => {
                // The epoch was a Thursday, so days-since-epoch mod 7 is 0 on
                // a Thursday and 4 on a Monday.
                const MONDAY_OFFSET: u64 = 4 * DAY;
                round_up(now + (7 * DAY - MONDAY_OFFSET), 7 * DAY) - (7 * DAY - MONDAY_OFFSET)
            }
            Calendar::At {
                hour,
                minute,
                second,
            } => {
                let offset =
                    u64::from(hour) * HOUR + u64::from(minute) * MINUTE + u64::from(second);
                let midnight = now - now % DAY;

                if midnight + offset >= now {
                    midnight + offset
                } else {
                    midnight + DAY + offset
                }
            }
        }
    }
}

impl fmt::Display for Calendar {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Calendar::Hourly => f.write_str("hourly"),
            Calendar::Daily => f.write_str("daily"),
            Calendar::Weekly => f.write_str("weekly"),
            Calendar::At {
                hour,
                minute,
                second,
            } => write!(f, "{hour:02}:{minute:02}:{second:02}"),
        }
    }
}

/// The smallest multiple of `step` that is at least `now`.
fn round_up(now: Unix, step: u64) -> Unix {
    match now % step {
        0 => now,
        past => now + (step - past),
    }
}

fn field(part: Option<&str>, max: u32, text: &str) -> Result<u32, CalendarError> {
    let part = part.ok_or_else(|| CalendarError {
        expression: text.to_owned(),
    })?;

    number(part, max, text)
}

fn number(part: &str, max: u32, text: &str) -> Result<u32, CalendarError> {
    // Rejected rather than trimmed. ` 9:00` and `9 :00` are typos, and a
    // parser that accepts them teaches an operator a syntax the document does
    // not describe.
    let value: u32 = part.parse().map_err(|_| CalendarError {
        expression: text.to_owned(),
    })?;

    if value > max {
        return Err(CalendarError {
            expression: text.to_owned(),
        });
    }

    Ok(value)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CalendarError {
    pub expression: String,
}

impl fmt::Display for CalendarError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "`{}` is not a schedule: expected hourly, daily, weekly, HH:MM or HH:MM:SS",
            self.expression
        )
    }
}

impl std::error::Error for CalendarError {}

/// Everything a `[timer]` says about when to fire.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Schedule {
    /// The first firing, measured from when the timer unit started.
    pub on_boot: Option<Duration>,
    /// Every firing after the first, measured from the previous one.
    ///
    /// Measured from the previous *firing*, not from the service's last
    /// activation. That is what the name says, it does not depend on a
    /// transition the service may never make — a `oneshot` never reaches
    /// `Active` at all — and it is what anyone writing "every hour" means.
    pub interval: Option<Duration>,
    pub calendar: Option<Calendar>,
}

/// When to fire, and *which clock says so*.
///
/// The distinction is the whole of what a clock step does to a schedule. A
/// monotonic delay means "this long from now" and must not move when the clock
/// is corrected; an absolute wall-clock moment means "at 03:30" and must stay
/// at 03:30 however the clock gets there. Collapsing the second into the first
/// — computing a delay once and arming a monotonic timer with it — is what
/// made an NTP correction move a nightly job by however much the clock moved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Next {
    /// A delay on the monotonic clock.
    After(Duration),
    /// A moment on the wall clock, in seconds since the epoch.
    At(Unix),
}

impl Next {
    /// Roughly how far away this is, for a countdown. `now` is the wall clock.
    ///
    /// Approximate for the calendar half by exactly the amount the clock is
    /// about to be corrected by, which is the honest answer to "how long until
    /// 03:30" when nobody knows what time it really is.
    pub fn in_seconds(self, now: Unix) -> u64 {
        match self {
            Next::After(delay) => delay.as_secs(),
            Next::At(at) => at.saturating_sub(now),
        }
    }
}

impl Schedule {
    /// Nothing to fire on. A load-time error, not a timer that sits forever.
    pub fn is_empty(&self) -> bool {
        self.on_boot.is_none() && self.interval.is_none() && self.calendar.is_none()
    }

    /// The delay to arm with for the first firing after the unit started.
    ///
    /// `interval` with no `on-boot` means the first firing is one interval
    /// away, which is what "every ten minutes" means to the person who wrote
    /// it.
    pub fn first(&self, now: Unix) -> Option<Next> {
        Self::sooner(self.on_boot.or(self.interval), self.calendar_at(now), now)
    }

    /// The delay to arm with after a firing.
    ///
    /// Both halves of the schedule are considered and the sooner wins. A unit
    /// declaring an `interval` and an `on-calendar` fires on whichever comes
    /// first, which is the only reading of "and" that does not silently ignore
    /// one of them.
    ///
    /// `None` when the timer is done: an `on-boot` with no `interval` and no
    /// calendar is a one-shot delayed task, and re-arming it would turn a
    /// deliberate single firing into a loop.
    pub fn next(&self, now: Unix) -> Option<Next> {
        Self::sooner(self.interval, self.calendar_at(now), now)
    }

    /// The next wall-clock moment this schedule names.
    ///
    /// `now + 1` because [`Calendar::next_at`] answers "at or after", and
    /// asking at the exact second a schedule names would otherwise return that
    /// same second and fire again immediately.
    fn calendar_at(&self, now: Unix) -> Option<Unix> {
        Some(self.calendar?.next_at(now.saturating_add(1)))
    }

    /// Whichever comes first, keeping the clock it belongs to.
    ///
    /// Compared in wall-clock seconds because that is the only scale both are
    /// expressible on. Sub-second precision is lost in the comparison and not
    /// in the result: a monotonic delay that wins is returned as the delay it
    /// was, not as a rounded moment.
    fn sooner(delay: Option<Duration>, at: Option<Unix>, now: Unix) -> Option<Next> {
        match (delay, at) {
            (Some(delay), Some(at)) => {
                if now.saturating_add(delay.as_secs()) <= at {
                    Some(Next::After(delay))
                } else {
                    Some(Next::At(at))
                }
            }
            (Some(delay), None) => Some(Next::After(delay)),
            (None, Some(at)) => Some(Next::At(at)),
            (None, None) => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 2026-08-09 12:34:56 UTC, a Sunday afternoon.
    ///
    /// Written as the number rather than computed, because a test that derives
    /// its own expectations from the code it is testing agrees with itself no
    /// matter what either says.
    const NOW: Unix = 1786278896;

    fn secs(n: u64) -> Option<Duration> {
        Some(Duration::from_secs(n))
    }

    fn after(n: u64) -> Option<Next> {
        Some(Next::After(Duration::from_secs(n)))
    }

    #[test]
    fn parses_the_named_schedules() {
        assert_eq!(Calendar::parse("hourly"), Ok(Calendar::Hourly));
        assert_eq!(Calendar::parse("daily"), Ok(Calendar::Daily));
        assert_eq!(Calendar::parse("weekly"), Ok(Calendar::Weekly));
    }

    #[test]
    fn parses_a_literal_time() {
        assert_eq!(
            Calendar::parse("03:30"),
            Ok(Calendar::At {
                hour: 3,
                minute: 30,
                second: 0
            }),
            "HH:MM means that minute exactly"
        );
        assert_eq!(
            Calendar::parse("23:59:59"),
            Ok(Calendar::At {
                hour: 23,
                minute: 59,
                second: 59
            })
        );
    }

    #[test]
    fn refuses_everything_else() {
        for bad in [
            "",
            "3",
            "24:00",
            "12:60",
            "12:00:60",
            "12:00:00:00",
            " 9:00",
            "9:00 ",
            "*/5",
            "0 * * * *",
            "Mon 09:00",
            "hourly ",
        ] {
            assert!(
                Calendar::parse(bad).is_err(),
                "`{bad}` should not parse — the vocabulary is closed"
            );
        }
    }

    #[test]
    fn hourly_lands_on_the_hour() {
        let at = Calendar::Hourly.next_at(NOW);
        assert_eq!(at % HOUR, 0);
        assert_eq!(at - NOW, 25 * MINUTE + 4, "12:34:56 to 13:00:00");
    }

    #[test]
    fn daily_lands_at_midnight() {
        let at = Calendar::Daily.next_at(NOW);
        assert_eq!(at % DAY, 0);
        assert_eq!(at - NOW, 11 * HOUR + 25 * MINUTE + 4);
    }

    #[test]
    fn weekly_lands_on_a_monday_midnight() {
        let at = Calendar::Weekly.next_at(NOW);

        assert_eq!(at % DAY, 0, "midnight");
        // The epoch was a Thursday: day 0 is Thursday, so a Monday is day 4
        // modulo 7.
        assert_eq!((at / DAY) % 7, 4, "Monday");
        // NOW is a Sunday afternoon, so the next Monday midnight is hours away
        // rather than a week.
        assert!(at - NOW < DAY, "the very next Monday, not the one after");
    }

    #[test]
    fn a_literal_time_rolls_over_to_tomorrow_once_it_has_passed() {
        let morning = Calendar::At {
            hour: 3,
            minute: 0,
            second: 0,
        };
        let evening = Calendar::At {
            hour: 23,
            minute: 0,
            second: 0,
        };

        // NOW is 12:34:56, so 03:00 is tomorrow and 23:00 is today.
        assert_eq!(morning.next_at(NOW) - NOW, 14 * HOUR + 25 * MINUTE + 4);
        assert_eq!(evening.next_at(NOW) - NOW, 10 * HOUR + 25 * MINUTE + 4);
    }

    #[test]
    fn asking_at_the_exact_second_a_schedule_names_returns_that_second() {
        // `next_at` is "at or after". A caller that has just fired is what
        // passes `now + 1`, and `Schedule` is where that happens.
        let midnight = NOW - NOW % DAY;
        assert_eq!(Calendar::Daily.next_at(midnight), midnight);
    }

    #[test]
    fn a_calendar_schedule_never_re_fires_on_the_same_second() {
        // The bug this guards: firing `daily` at exactly midnight, then asking
        // what is next and being told "now", forever.
        let midnight = NOW - NOW % DAY;
        let schedule = Schedule {
            calendar: Some(Calendar::Daily),
            ..Schedule::default()
        };

        assert_eq!(schedule.next(midnight), Some(Next::At(midnight + DAY)));
    }

    #[test]
    fn interval_alone_first_fires_one_interval_in() {
        let schedule = Schedule {
            interval: secs(600),
            ..Schedule::default()
        };

        assert_eq!(schedule.first(NOW), after(600));
        assert_eq!(schedule.next(NOW), after(600));
    }

    #[test]
    fn on_boot_alone_fires_once() {
        let schedule = Schedule {
            on_boot: secs(30),
            ..Schedule::default()
        };

        assert_eq!(schedule.first(NOW), after(30));
        assert_eq!(
            schedule.next(NOW),
            None,
            "a delayed one-shot must not become a loop"
        );
    }

    #[test]
    fn on_boot_sets_the_first_firing_and_interval_the_rest() {
        let schedule = Schedule {
            on_boot: secs(5),
            interval: secs(3600),
            ..Schedule::default()
        };

        assert_eq!(schedule.first(NOW), after(5));
        assert_eq!(schedule.next(NOW), after(3600));
    }

    #[test]
    fn the_sooner_of_the_two_halves_wins() {
        // The only reading of "an interval and a calendar" that does not
        // silently ignore one of them.
        let schedule = Schedule {
            interval: secs(DAY),
            calendar: Some(Calendar::Hourly),
            ..Schedule::default()
        };

        assert_eq!(
            schedule.next(NOW),
            Some(Next::At(NOW + 25 * MINUTE + 4)),
            "the hour comes before the day, and stays on the wall clock"
        );
    }

    #[test]
    fn a_calendar_firing_keeps_the_wall_clock() {
        // The bug this exists to prevent: collapsing "at 03:30" into "in
        // fourteen hours" and arming a monotonic timer with it, so that an NTP
        // correction moves the job by however much the clock moved.
        let schedule = Schedule {
            calendar: Some(Calendar::At {
                hour: 3,
                minute: 30,
                second: 0,
            }),
            ..Schedule::default()
        };

        assert!(matches!(schedule.next(NOW), Some(Next::At(_))));
    }

    #[test]
    fn a_monotonic_firing_keeps_its_sub_second_precision() {
        // Comparison happens in whole seconds; the result must not be rounded
        // to them.
        let schedule = Schedule {
            interval: Some(Duration::from_millis(1500)),
            calendar: Some(Calendar::Daily),
            ..Schedule::default()
        };

        assert_eq!(
            schedule.next(NOW),
            Some(Next::After(Duration::from_millis(1500)))
        );
    }

    #[test]
    fn an_empty_schedule_is_recognised() {
        assert!(Schedule::default().is_empty());
        assert!(!Schedule {
            on_boot: secs(1),
            ..Schedule::default()
        }
        .is_empty());
    }
}
