# Contributing

oxinit is pre-alpha. The design is written down in
[ARCHITECTURE.md](ARCHITECTURE.md) and the unit format in
[docs/UNIT_FORMAT.md](docs/UNIT_FORMAT.md). Read both before proposing changes
to either.

## Setup

The host tests in `oxinit-unit` and `oxinit-graph` run anywhere Rust runs. The
QEMU boot loop wants a Linux host — see [Other hosts](#other-hosts) if you are
on macOS.

**Toolchain.** Install Rust via [rustup](https://rustup.rs) and add the musl
target:

```bash
rustup target add x86_64-unknown-linux-musl
rustup target add aarch64-unknown-linux-musl
```

MSRV is stable minus two releases. No nightly features are used.

**Tools.** `qemu-system-x86_64`, `qemu-system-aarch64` and `cpio`. Docker or
podman as well, for `cargo xtask container`, `test-distro` and `test-demo`.

```bash
# Debian/Ubuntu
sudo apt install qemu-system-x86 qemu-system-arm cpio

# Fedora
sudo dnf install qemu-system-x86 qemu-system-aarch64 cpio

# Arch
sudo pacman -S qemu-system-x86 qemu-system-aarch64 cpio
```

**Boot artifacts.** A kernel and a statically linked shell per architecture.
One command each:

```bash
cargo xtask fetch --arch x86_64
cargo xtask fetch --arch aarch64
```

That downloads them from Alpine into `target/`, where `xtask` looks for them by
name — `vmlinuz-<arch>` and `busybox-<arch>`. To use a kernel you already have
instead, `--kernel` and `--shell` override, and so do `$OXINIT_KERNEL_<ARCH>`
and `$OXINIT_SHELL_<ARCH>`. On a Linux host the installed kernel usually works:

```bash
cargo xtask boot --kernel /boot/vmlinuz-$(uname -r)
```

Any kernel needs `CONFIG_DEVTMPFS`, `CONFIG_CGROUPS` with the v2 hierarchy, and
`CONFIG_BLK_DEV_INITRD`. Version 5.14 or later, for `cgroup.kill`.

## Development loop

```bash
cargo xtask boot
```

This builds a static `oxinit` for `x86_64-unknown-linux-musl`, packs it as
`/init` into a cpio initramfs, and boots:

```bash
qemu-system-x86_64 -kernel target/vmlinuz-x86_64 \
  -initrd target/oxinit-x86_64.cpio.gz -nographic -append "console=ttyS0"
```

Edit to boot takes a few seconds. `-nographic` puts the guest serial console on
your terminal. `Ctrl-A X` exits QEMU. `Ctrl-C` goes to the guest, not to QEMU.

The image holds only `/init` unless there is a shell to put in it, which is
what `cargo xtask fetch` provides. It must be statically linked, since the
image has no libraries. Without one the boot still exercises the mounts, the
console, the signalfd and the reap loop, but oxinit has nothing to supervise
and logs a spawn failure for `/bin/sh`.

Use this loop rather than reasoning about whether the boot path works. Early
boot has too many ways to fail silently.

## Other hosts

The parser and graph work needs none of this. `cargo test -p oxinit-unit -p
oxinit-graph` runs on any host with a Rust toolchain, and that is where most of
[M1](ROADMAP.md) lives.

Booting from macOS needs two extra pieces:

- **A cross linker.** `rustup target add` installs the standard library but not
  something that can link it. `.cargo/config.toml` in this repository points
  both musl targets at `rust-lld`, which ships with every rustup toolchain, so
  this is already handled — but that is why the file exists.
- **Patience for the foreign architecture.** Emulating the one your host is not
  has no hardware acceleration behind it, and the whole boot runs through
  QEMU's JIT. Both are tested — `cargo xtask test-boot --arch all` — and the
  slower of the two takes about ninety seconds rather than about thirty.

A Linux VM with a distribution kernel is the shorter route if you are doing
kernel-facing work.

## Tests

The `oxinit` crate is Linux-only, so a bare `cargo build` or `cargo test` at the
root fails on macOS. That is expected; the crate says so at compile time. Pass a
target:

```bash
cargo build --target x86_64-unknown-linux-musl -p oxinit
cargo clippy --target x86_64-unknown-linux-musl -p oxinit -- -D warnings
```

Arriving with the milestones that introduce them:

```bash
cargo test -p oxinit-unit -p oxinit-graph   # M1: host tests, no VM
cargo test -p oxinit-log                    # M7: records, splitting, rotation
cargo test -p oxinit-timer                  # M8: schedules and the calendar
cargo xtask test-boot                       # M5: boots QEMU, asserts on serial
cargo xtask test-boot --arch all            # M9: x86_64 and aarch64, in turn
cargo xtask container                       # M6: runs it in Docker, same
cargo xtask test-demo                       # M16: every command the README gives
cargo xtask test-distro                     # M10: a real Alpine userspace
```

The library crates have no Linux dependencies and run anywhere. Parser, graph,
restart policy, log format and calendar arithmetic belong there, with tests,
rather than in `oxinit` — which refuses to compile off Linux and so cannot be
tested on a host at all.

`test-boot` boots with a timeout and matches expected lines in the serial log. A
test that hangs fails on the timeout instead of blocking the run.

`container` needs Docker or podman — `--engine podman` — and nothing else. It
packs the same staged root filesystem as a `FROM scratch` image, runs it, stops
it the way an operator would, and asserts on the log and the exit code.
`--privileged` adds a writable cgroupfs and the assertions that need one.

`test-demo` runs every command the README's opening section tells a newcomer to
type, against the image built from `demo/`. That README is the first thing
anybody reads, and this is what stops it quietly ceasing to be true.

## Code policy

Full rules are in [CLAUDE.md](CLAUDE.md). The ones that get PRs rejected:

**No panicking constructs in `crates/oxinit`.** The crate denies
`clippy::unwrap_used`, `clippy::expect_used`, `clippy::panic`, and
`clippy::indexing_slicing`. Do not add `#[allow]` for them. Use `get()`,
`Result`, and `match`. `panic = "abort"` is never set in any profile — an abort
in PID 1 is a kernel panic.

**No async in `crates/oxinit`.** One thread, one `epoll` loop. Other crates are
unconstrained.

**Unsafe is quarantined, and the build enforces it.** Syscalls go through
`rustix`. Anything `rustix` does not cover lives in `oxinit::sys::raw`, with a
`// SAFETY:` comment per block stating the invariant and why it holds at that
call site. The `oxinit` crate denies `unsafe_code` with that one module
relaxing it, and every crate that should contain none carries `forbid`, so a PR
adding `unsafe` elsewhere does not compile rather than being asked to move it.
A PR adding one to `sys::raw` without a `// SAFETY:` comment will be asked to
write one.

**Dependencies need a reason.** The dependency tree of PID 1 is part of its
attack surface. State the reason in the commit message.

## Commits

[Conventional Commits](https://www.conventionalcommits.org), scoped by
component:

```
feat(cgroup): write pid to cgroup.procs before exec
fix(unit): reject bare integers in duration fields
docs(architecture): explain why signalfd instead of handlers
```

Types in use: `feat`, `fix`, `docs`, `refactor`, `test`, `chore`.

## Pull requests

- One logical change per PR. A refactor and a behaviour change go in separate
  PRs.
- `cargo fmt --all` and `cargo clippy --all-targets -- -D warnings` must pass.
- New parser or graph behaviour needs a test in `oxinit-unit` or
  `oxinit-graph`.
- Changing the unit format means changing `docs/UNIT_FORMAT.md` in the same PR.
  The document is the specification, not a description of the parser.
- Changing a design decision means changing `ARCHITECTURE.md` in the same PR,
  including the reason.
- Describe what you tested. "Boots in QEMU, `oxctl status` shows the service
  active" is enough; "should work" is not.

For anything that changes the architecture, open an issue first. A rejected
design is cheaper as an issue than as a PR.

## Where to start

Issues tagged `good first issue` are self-contained and do not require
understanding the whole boot path. The parser and graph work in
[M1](ROADMAP.md) is the largest set of tasks that can be done on a host with no
VM and no kernel image.

Design review is also useful right now. If something in
[ARCHITECTURE.md](ARCHITECTURE.md) is wrong, say so. Before it is implemented is
the cheapest time to find out.
