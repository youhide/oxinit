//! Deadlines.
//!
//! One timerfd and a heap, not one fd per timer. A restart storm across a
//! hundred services would otherwise mean a hundred descriptors and an epoll
//! registration per restart; here it means a hundred heap entries and one
//! `timerfd_settime`.

use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::os::fd::{AsFd, OwnedFd};
use std::time::{Duration, Instant};

use rustix::time::{Itimerspec, TimerfdClockId, TimerfdFlags, TimerfdTimerFlags, Timespec};

use crate::error::{Error, Result};

/// What a deadline means when it fires.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Alarm {
    /// A unit's backoff has elapsed and it may start again.
    Restart { unit: String },
    /// Time to check whether a unit has missed its watchdog deadline.
    WatchdogCheck { unit: String },
    /// A unit's `stop-sec` has elapsed since `SIGTERM`. Whatever is left in
    /// its cgroup gets killed.
    StopTimeout { unit: String },
    /// A timer unit elapsed. Start what it names, then arm it again.
    TimerElapsed { unit: String },
    /// A unit's `start-sec` has elapsed and it is still `Activating`.
    StartTimeout { unit: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Entry {
    at: Instant,
    alarm: Alarm,
}

// Ordered by time only, so the heap is a priority queue over `at`. Wrapped in
// `Reverse` at the call site to make it a min-heap.
impl Ord for Entry {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.at.cmp(&other.at)
    }
}

impl PartialOrd for Entry {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

pub struct Timers {
    fd: OwnedFd,
    heap: BinaryHeap<Reverse<Entry>>,
}

impl Timers {
    pub fn new() -> Result<Self> {
        let fd = rustix::time::timerfd_create(
            TimerfdClockId::Monotonic,
            TimerfdFlags::CLOEXEC | TimerfdFlags::NONBLOCK,
        )
        .map_err(Error::Timer)?;

        Ok(Self {
            fd,
            heap: BinaryHeap::new(),
        })
    }

    pub fn as_fd(&self) -> impl AsFd + '_ {
        self.fd.as_fd()
    }

    /// Schedule `alarm` for `delay` from now, and rearm if it is the earliest.
    pub fn schedule(&mut self, delay: Duration, alarm: Alarm) -> Result<()> {
        self.heap.push(Reverse(Entry {
            at: Instant::now() + delay,
            alarm,
        }));
        self.arm()
    }

    /// Everything whose deadline has passed, and rearm for whatever is next.
    pub fn expired(&mut self) -> Result<Vec<Alarm>> {
        // Draining the fd matters: it is level-triggered, so an unread
        // expiry would wake the loop forever.
        let mut buf = [0u8; 8];
        let _ = rustix::io::read(&self.fd, buf.as_mut_slice());

        let now = Instant::now();
        let mut fired = Vec::new();

        while let Some(Reverse(entry)) = self.heap.peek() {
            if entry.at > now {
                break;
            }
            if let Some(Reverse(entry)) = self.heap.pop() {
                fired.push(entry.alarm);
            }
        }

        self.arm()?;
        Ok(fired)
    }

    /// Point the timerfd at the earliest deadline, or disarm it.
    fn arm(&self) -> Result<()> {
        // A relative one-shot rather than an absolute time: `Instant` has no
        // documented conversion to a `CLOCK_MONOTONIC` timespec, and the
        // rearm-on-every-change pattern makes the drift irrelevant.
        let delay = match self.heap.peek() {
            Some(Reverse(entry)) => entry.at.saturating_duration_since(Instant::now()),
            // An all-zero itimerspec disarms the timer.
            None => Duration::ZERO,
        };

        // Zero would also mean "disarmed", so an already-expired deadline is
        // nudged to the smallest non-zero value instead of being lost.
        let delay = if self.heap.is_empty() {
            Duration::ZERO
        } else {
            delay.max(Duration::from_nanos(1))
        };

        let spec = Itimerspec {
            it_interval: Timespec {
                tv_sec: 0,
                tv_nsec: 0,
            },
            it_value: Timespec {
                tv_sec: delay.as_secs() as i64,
                tv_nsec: i64::from(delay.subsec_nanos()),
            },
        };

        rustix::time::timerfd_settime(&self.fd, TimerfdTimerFlags::empty(), &spec)
            .map(|_| ())
            .map_err(Error::Timer)
    }
}
