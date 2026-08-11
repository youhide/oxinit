//! Thin layer over the syscalls oxinit needs.
//!
//! Everything here goes through rustix except what lives in [`raw`], which is
//! the only module in the crate allowed to contain `unsafe`.

// The one relaxation of the crate-level `deny(unsafe_code)`, and the whole of
// why the claim above is checkable rather than merely asserted.
#[allow(unsafe_code)]
pub mod raw;
