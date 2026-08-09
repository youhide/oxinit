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
podman as well, if you are running `cargo xtask container`.

**Boot artifacts.** A kernel and a static busybox per architecture, under
`target/`, where `xtask` looks for them by name — `vmlinuz-x86_64`,
`vmlinuz-aarch64`, `busybox-x86_64`, `busybox-aarch64`. Alpine publishes both:
a kernel under `releases/<arch>/netboot/vmlinuz-lts` and a `busybox-static`
package under `main/<arch>/`. `--kernel` and `--shell` override, and so do
`$OXINIT_KERNEL_<ARCH>` and `$OXINIT_SHELL_<ARCH>`.

```bash
# Debian/Ubuntu
sudo apt install qemu-system-x86 cpio

# Fedora
sudo dnf install qemu-system-x86 cpio

# Arch
sudo pacman -S qemu-system-x86 cpio
```

**Kernel.** You need a bootable `bzImage`. Any distribution kernel with cgroup
v2 and devtmpfs works; on most Linux systems the installed one is in `/boot`.

```bash
cargo xtask boot --kernel /boot/vmlinuz-$(uname -r)
```

Or set it once:

```bash
export OXINIT_KERNEL=/path/to/bzImage
```

`xtask` looks for `--kernel`, then `OXINIT_KERNEL`, then a `bzImage` in the
repository root.

The kernel must have `CONFIG_DEVTMPFS`, `CONFIG_CGROUPS` with the v2 hierarchy,
and `CONFIG_BLK_DEV_INITRD`. Kernel 5.14 or later, for `cgroup.kill`.

## Development loop

```bash
cargo xtask boot
```

This builds a static `oxinit` for `x86_64-unknown-linux-musl`, packs it as
`/init` into a cpio initramfs, and boots:

```bash
qemu-system-x86_64 -kernel bzImage -initrd oxinit.cpio.gz -nographic -append "console=ttyS0"
```

Edit to boot takes a few seconds. `-nographic` puts the guest serial console on
your terminal. `Ctrl-A X` exits QEMU. `Ctrl-C` goes to the guest, not to QEMU.

The image holds only `/init` unless you give it a shell:

```bash
cargo xtask boot --shell /path/to/busybox
```

It must be statically linked, since the image has no libraries. Without it the
boot still exercises the mounts, console, signalfd, and reap loop, but oxinit
has nothing to supervise and logs a spawn failure for `/bin/sh`.

Use this loop rather than reasoning about whether the boot path works. Early
boot has too many ways to fail silently.

## Other hosts

The parser and graph work needs none of this. `cargo test -p oxinit-unit -p
oxinit-graph` runs on any host with a Rust toolchain, and that is where most of
[M1](ROADMAP.md) lives.

Booting from macOS needs two extra pieces:

- **A cross linker.** `rustup target add x86_64-unknown-linux-musl
rustup target add aarch64-unknown-linux-musl` installs the
  standard library but not something that can link it. Use
  [`cargo-zigbuild`](https://github.com/rust-cross/cargo-zigbuild), which needs
  only `zig`, or a `musl-cross` toolchain from Homebrew with the linker
  configured in `.cargo/config.toml`.
- **Patience on Apple Silicon.** `qemu-system-x86_64` there is full emulation,
  not virtualization, so the boot is a few seconds rather than under one. It
  works. Building for `aarch64-unknown-linux-musl` and booting
  `qemu-system-aarch64` is faster but is not the tested path.

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
cargo xtask test-distro                     # M10: a real Alpine userspace
```

`oxinit-unit` and `oxinit-graph` will have no Linux dependencies and will run
anywhere. Parser and graph logic belongs there, with tests, rather than in
`oxinit`.

`test-boot` boots with a timeout and matches expected lines in the serial log. A
test that hangs fails on the timeout instead of blocking the run.

`container` needs Docker or podman — `--engine podman` — and nothing else. It
packs the same staged root filesystem as a `FROM scratch` image, runs it, stops
it the way an operator would, and asserts on the log and the exit code.
`--privileged` adds a writable cgroupfs and the assertions that need one.

## Code policy

Full rules are in [CLAUDE.md](CLAUDE.md). The ones that get PRs rejected:

**No panicking constructs in `crates/oxinit`.** The crate denies
`clippy::unwrap_used`, `clippy::expect_used`, `clippy::panic`, and
`clippy::indexing_slicing`. Do not add `#[allow]` for them. Use `get()`,
`Result`, and `match`. `panic = "abort"` is never set in any profile — an abort
in PID 1 is a kernel panic.

**No async in `crates/oxinit`.** One thread, one `epoll` loop. Other crates are
unconstrained.

**Unsafe is quarantined.** Syscalls go through `rustix`. Anything `rustix` does
not cover lives in `oxinit::sys::raw`, with a `// SAFETY:` comment per block
stating the invariant and why it holds at that call site. A PR adding `unsafe`
elsewhere will be asked to move it. A PR adding `unsafe` without a `// SAFETY:`
comment will be asked to write one.

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
