//! Where a log lives, what a line in it looks like, and when to rotate it.
//!
//! Shared by `oxinit`, `oxlogd` and `oxctl`. All three need to agree, and the
//! way to make them agree is to give them one definition rather than three
//! that look alike.
//!
//! Nothing here makes a syscall. Splitting a read into records, bounding a
//! line that never ends, and planning a rotation are the parts that can be
//! wrong in a way a test can catch, so they are separated from the descriptors
//! that feed them.
//!
//! `oxinit` depends on this crate, so a panic here is a panic in PID 1.

#![forbid(unsafe_code)]
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Where `oxlogd` writes, and where `oxctl logs` reads.
pub const LOG_DIR: &str = "/var/log/oxinit";

/// Where `oxinit` listens for `oxlogd`.
///
/// PID 1 is the server. It already owns a listening socket and an event loop;
/// the other direction would have PID 1 connecting to a service it supervises,
/// at every service start, and deciding what to do when that connect fails.
pub const SOCKET_PATH: &str = "/run/oxinit/log.sock";

/// The largest message on that socket.
///
/// The message is a unit name and nothing else — the descriptor rides beside
/// it as `SCM_RIGHTS` — so this is a generous bound on a name, not a buffer
/// size anything is expected to reach.
pub const MAX_MESSAGE: usize = 256;

/// The live file for a unit.
pub fn path(dir: &Path, unit: &str) -> PathBuf {
    dir.join(format!("{unit}.log"))
}

/// A moment, as the two numbers a record carries.
///
/// Deliberately not a formatted date. Rendering one needs either a calendar
/// implementation or a dependency, and every reader of these files already has
/// something better at it than a log writer would be.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Timestamp {
    pub secs: u64,
    pub micros: u32,
}

impl Timestamp {
    pub fn now() -> Self {
        let since = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();

        Self {
            secs: since.as_secs(),
            micros: since.subsec_micros(),
        }
    }
}

/// Append one record: `<seconds>.<microseconds> <line>` and a newline.
///
/// The fixed six digits are what make the files sort as text, which is the
/// whole reason the fraction is padded rather than printed as it comes.
///
/// The line is written as the bytes that arrived. A service that emits
/// something that is not UTF-8 gets it back byte for byte rather than replaced
/// with question marks, because a log is evidence.
pub fn record(out: &mut Vec<u8>, at: Timestamp, line: &[u8]) {
    out.extend_from_slice(format!("{}.{:06} ", at.secs, at.micros).as_bytes());
    out.extend_from_slice(line);
    out.push(b'\n');
}

/// Longest line kept whole.
///
/// A service that writes without ever sending a newline must not grow the
/// buffer holding its partial line to match. At this point the partial is
/// emitted as a record of its own and the buffer starts again — the output is
/// broken in a place the service did not choose, which is the lesser of the
/// two problems.
pub const MAX_LINE: usize = 16 * 1024;

/// Turns reads into lines.
///
/// A read from a pipe ends wherever the pipe happened to have data, which is
/// almost never on a line boundary. What is left over is held until the rest
/// of the line arrives.
#[derive(Debug, Default)]
pub struct Splitter {
    partial: Vec<u8>,
}

impl Splitter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed one read, and call `emit` for every complete line in it.
    ///
    /// A trailing partial line stays here. Carriage returns are left alone:
    /// stripping them would be guessing that the writer meant a line ending
    /// rather than a progress bar.
    pub fn push(&mut self, chunk: &[u8], mut emit: impl FnMut(&[u8])) {
        let mut rest = chunk;

        while let Some(at) = rest.iter().position(|byte| *byte == b'\n') {
            let (line, tail) = rest.split_at(at);

            if self.partial.is_empty() {
                emit(line);
            } else {
                self.partial.extend_from_slice(line);
                emit(&self.partial);
                self.partial.clear();
            }

            rest = tail.get(1..).unwrap_or_default();
        }

        self.partial.extend_from_slice(rest);

        // Only after appending, so the check is against what is actually held
        // rather than against what was about to be.
        if self.partial.len() >= MAX_LINE {
            emit(&self.partial);
            self.partial.clear();
        }
    }

    /// What is left when the writer is gone for good.
    ///
    /// A service that exits without a final newline still wrote that text, and
    /// dropping it would lose exactly the last thing it said — which is
    /// usually the interesting one.
    pub fn flush(&mut self) -> Option<Vec<u8>> {
        if self.partial.is_empty() {
            return None;
        }

        Some(std::mem::take(&mut self.partial))
    }
}

/// When to start a new file, and how many old ones to keep.
///
/// By size, not by time. Bounded disk per unit is the property that matters,
/// and a time-based policy bounds nothing: a service that says one thing an
/// hour and a service that floods produce the same rotation schedule and very
/// different amounts of disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rotation {
    pub max_size: u64,
    /// How many `<unit>.log.N` files survive. `0` means rotation deletes the
    /// old file rather than keeping any of it.
    pub keep: usize,
}

impl Default for Rotation {
    fn default() -> Self {
        Self {
            max_size: 1024 * 1024,
            keep: 3,
        }
    }
}

/// One step of a rotation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Move {
    Remove(PathBuf),
    Rename { from: PathBuf, to: PathBuf },
}

impl Rotation {
    /// Whether writing `adding` more bytes to a file of `current` bytes should
    /// happen in a new file instead.
    ///
    /// Asked before the write, so `max_size` is a size the file does not
    /// exceed rather than one it overshoots by however long the last record
    /// was. A single record larger than `max_size` still gets written — it
    /// goes into a file of its own, which is the only alternative to dropping
    /// it.
    pub fn should_rotate(&self, current: u64, adding: usize) -> bool {
        current > 0 && current.saturating_add(adding as u64) > self.max_size
    }

    /// Every move needed to free `<unit>.log`, in the order they must happen.
    ///
    /// Highest generation first. Done the other way round, `.1` would be
    /// renamed onto `.2` before `.2` had been moved out of the way, and the
    /// rotation would keep one generation no matter what `keep` said.
    pub fn plan(&self, dir: &Path, unit: &str) -> Vec<Move> {
        let live = path(dir, unit);

        if self.keep == 0 {
            return vec![Move::Remove(live)];
        }

        let generation = |n: usize| dir.join(format!("{unit}.log.{n}"));
        let mut moves = vec![Move::Remove(generation(self.keep))];

        for n in (1..self.keep).rev() {
            moves.push(Move::Rename {
                from: generation(n),
                to: generation(n + 1),
            });
        }

        moves.push(Move::Rename {
            from: live,
            to: generation(1),
        });

        moves
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(chunks: &[&[u8]]) -> Vec<String> {
        let mut splitter = Splitter::new();
        let mut out = Vec::new();

        for chunk in chunks {
            splitter.push(chunk, |line| {
                out.push(String::from_utf8_lossy(line).into_owned());
            });
        }

        out
    }

    #[test]
    fn splits_on_newlines() {
        assert_eq!(lines(&[b"one\ntwo\n"]), ["one", "two"]);
    }

    #[test]
    fn holds_a_partial_line_until_the_rest_arrives() {
        // The case that motivates the type: a pipe read ends where the pipe
        // had data, not where the service ended a line.
        assert_eq!(lines(&[b"he", b"llo", b" world\n"]), ["hello world"]);
    }

    #[test]
    fn a_trailing_partial_is_not_emitted_as_a_line() {
        assert_eq!(lines(&[b"done\nstarted"]), ["done"]);
    }

    #[test]
    fn flush_returns_the_last_line_without_a_newline() {
        let mut splitter = Splitter::new();
        splitter.push(b"no newline here", |_| {});

        assert_eq!(splitter.flush().as_deref(), Some(&b"no newline here"[..]));
        assert_eq!(splitter.flush(), None, "flushing twice yields nothing");
    }

    #[test]
    fn empty_lines_survive() {
        assert_eq!(lines(&[b"a\n\nb\n"]), ["a", "", "b"]);
    }

    #[test]
    fn a_line_that_never_ends_is_broken_rather_than_buffered_forever() {
        let flood = vec![b'x'; MAX_LINE + 10];
        let emitted = lines(&[&flood]);

        assert_eq!(emitted.len(), 1);
        assert_eq!(emitted.first().map(String::len), Some(MAX_LINE + 10));

        // And the buffer is empty afterwards, so the next flood does not
        // start from where this one left off.
        let mut splitter = Splitter::new();
        splitter.push(&flood, |_| {});
        assert_eq!(splitter.flush(), None);
    }

    #[test]
    fn a_record_is_a_padded_timestamp_a_space_and_the_line() {
        let mut out = Vec::new();
        record(
            &mut out,
            Timestamp {
                secs: 1754743123,
                micros: 4567,
            },
            b"probe up",
        );

        assert_eq!(out, b"1754743123.004567 probe up\n");
    }

    #[test]
    fn records_sort_as_text() {
        // The reason the fraction is padded to six digits rather than printed
        // as it comes: `.9` would sort after `.10`.
        let stamp = |micros| Timestamp { secs: 1, micros };
        let mut early = Vec::new();
        let mut late = Vec::new();

        record(&mut early, stamp(9), b"first");
        record(&mut late, stamp(10), b"second");

        assert!(early < late);
    }

    #[test]
    fn rotates_only_once_the_write_would_pass_the_limit() {
        let rotation = Rotation {
            max_size: 100,
            keep: 3,
        };

        assert!(!rotation.should_rotate(90, 10), "exactly at the limit fits");
        assert!(rotation.should_rotate(90, 11));
        assert!(
            !rotation.should_rotate(0, 4096),
            "an empty file takes the record whatever its size, or it is lost"
        );
    }

    #[test]
    fn the_plan_moves_the_oldest_generation_first() {
        let rotation = Rotation {
            max_size: 1,
            keep: 3,
        };
        let dir = Path::new("/var/log/oxinit");

        assert_eq!(
            rotation.plan(dir, "probe"),
            [
                Move::Remove(dir.join("probe.log.3")),
                Move::Rename {
                    from: dir.join("probe.log.2"),
                    to: dir.join("probe.log.3"),
                },
                Move::Rename {
                    from: dir.join("probe.log.1"),
                    to: dir.join("probe.log.2"),
                },
                Move::Rename {
                    from: dir.join("probe.log"),
                    to: dir.join("probe.log.1"),
                },
            ]
        );
    }

    #[test]
    fn keeping_nothing_deletes_rather_than_renames() {
        let rotation = Rotation {
            max_size: 1,
            keep: 0,
        };
        let dir = Path::new("/tmp");

        assert_eq!(
            rotation.plan(dir, "probe"),
            [Move::Remove(dir.join("probe.log"))]
        );
    }
}
