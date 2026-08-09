# CLAUDE.md

Conventions for this repository. Read [ARCHITECTURE.md](ARCHITECTURE.md) before
writing code that touches PID 1.

## What this is

`oxinit` is a service manager and PID 1 for Linux. `oxinit` is the PID 1 binary;
`oxctl` is the CLI client. Pre-alpha — see [ROADMAP.md](ROADMAP.md) for what
exists.

## Workspace

```
crates/oxinit/         PID 1 binary
crates/oxctl/          CLI client
crates/oxinit-unit/    unit parsing + validation
crates/oxinit-graph/   dependency resolution
crates/oxinit-service/ service state machine and restart policy
crates/oxinit-cgroup/  cgroup v2
crates/oxinit-ipc/     control protocol types
crates/xtask/          build and boot automation
docs/                  specifications
tests/                 integration tests
```

`oxinit-unit`, `oxinit-graph`, and `oxinit-service` must stay free of
Linux-specific dependencies. They are tested on the host without a VM. Do not
add a syscall to any of them.

That split is why the state machine lives outside the `oxinit` crate: the
binary is Linux-only and denies compilation elsewhere, so anything inside it is
untestable on a macOS or BSD host. Policy logic goes in a library crate.

## Rules for the `oxinit` crate

These apply to `crates/oxinit` specifically. PID 1 cannot exit; if it does, the
kernel panics.

**No panicking constructs.** The crate carries:

```rust
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
```

Do not add `#[allow]` for any of these. Do not use `unwrap`, `expect`, `panic!`,
`todo!`, `unimplemented!`, `assert!` outside `#[cfg(test)]`, or `slice[i]`. Use
`get()`, `Result`, and `match`.

**`panic = "unwind"`.** Never set `panic = "abort"` in any profile. An abort in
PID 1 is a kernel panic.

**Wrap event-loop handlers in `catch_unwind`.** A panic that escapes a handler
must unwind to the loop boundary, get logged, and let the loop continue.

**Never call `exit()`,** with one exception that is not an error path: the end
of an ordered shutdown in a container, in `shutdown::exit`, where the container
only exists for as long as its PID 1 does. Nowhere else, and never on a
failure. If the loop cannot continue — in a container too — spawn `/bin/sh` on
`/dev/console` and keep reaping.

**No async.** No `tokio`, no `async-std`, no `futures`, no `async fn`. One
thread, one `epoll` loop. This is not negotiable for `crates/oxinit`; other
crates may do what they like.

**Syscalls go through `rustix`,** not `libc`. Any `unsafe` that `rustix` cannot
cover lives in `oxinit::sys::raw` and nowhere else, with a `// SAFETY:` comment
per block stating the invariant and why it holds at that call site.

**Errors are `thiserror` enums,** one per crate, no `anyhow` in `oxinit`.
`anyhow` is fine in `oxctl` and `xtask`.

## Rules everywhere

- MSRV is stable minus two releases. No nightly features, no `feature(...)`.
- `cargo fmt` with the default profile. No `rustfmt.toml` overrides.
- `cargo clippy -- -D warnings` must pass.
- Commits follow Conventional Commits: `feat(cgroup): ...`, `fix(unit): ...`.
- Do not add a dependency without saying why in the commit message. The
  dependency tree of PID 1 is part of its attack surface.

## Commands

```bash
cargo fmt --all
cargo clippy --all-targets -- -D warnings
cargo test -p oxinit-unit -p oxinit-graph   # host tests, fast
cargo xtask boot                            # build + boot in QEMU
cargo xtask test-boot                       # boot and assert on serial output
```

`cargo xtask boot` builds `oxinit` for `x86_64-unknown-linux-musl`, packs a
single-file cpio initramfs, and boots:

```bash
qemu-system-x86_64 -kernel bzImage -initrd oxinit.cpio.gz -nographic -append "console=ttyS0"
```

`Ctrl-A X` exits QEMU. Edit to boot is a few seconds — use it, do not reason
about whether the boot path works.

## Working on this repo

- Changing the unit format means changing [docs/UNIT_FORMAT.md](docs/UNIT_FORMAT.md)
  in the same commit. The document is the specification, not a description of
  the parser.
- Changing a design decision means changing [ARCHITECTURE.md](ARCHITECTURE.md)
  in the same commit, including the reason.
- Do not write code for a milestone that is not the current one. See
  [ROADMAP.md](ROADMAP.md).
- Do not add systemd unit file compatibility. It is an explicit non-goal. The
  `sd_notify` and `LISTEN_FDS` protocols are in scope; the `.service` config
  format is not.
