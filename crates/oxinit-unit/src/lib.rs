//! Unit file parsing and validation.
//!
//! Units are TOML. The format is specified in `docs/UNIT_FORMAT.md`, and this
//! crate is the implementation of that document — where the two disagree, the
//! document is right and this is a bug.
//!
//! This crate has no Linux dependencies. It runs and tests anywhere.
//!
//! The same failure policy as the `oxinit` binary applies: this code runs
//! inside PID 1, so a panic here is a panic in PID 1.

#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

pub mod error;
pub mod exec;
pub mod unit;
pub mod value;

pub use error::{ExecError, UnitError, ValueError};
pub use unit::{parse, Deps, Kind, Resources, Restart, Service, ServiceType, Unit};
pub use value::{parse_duration, parse_size, DurationValue, SizeValue};
