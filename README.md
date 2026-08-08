# oxinit

A service manager and PID 1 for Linux, written in Rust.

[![License](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue)](#license)
[![Status](https://img.shields.io/badge/status-pre--alpha-orange)](ROADMAP.md)

**Pre-alpha. Nothing is stable. Nothing is implemented yet.** See
[ROADMAP.md](ROADMAP.md).

## Why

Every init system in production use is written in C: systemd, OpenRC, runit,
s6, dinit. The Rust attempts either stalled — rustysd is unmaintained — or
explicitly declined to be PID 1, as initd did.

PID 1 is the process that cannot fail. If it exits, the kernel panics. It is the
parent of every service on the machine, it owns the cgroup hierarchy, it holds
the file descriptors for socket activation, and it determines the order in which
everything shuts down. There is no supervisor above it to restart it.

That is a large blast radius for a language where a bounds error is a memory
error. The correctness properties Rust enforces at compile time are worth more
in PID 1 than anywhere else in the system.

## Design principles

1. **PID 1 stays small.** Supervision only. The CLI, log shipping, and device
   management are separate processes talking over a unix socket.
2. **No panic in PID 1.** `panic = "unwind"`, never `abort` — an abort in PID 1
   is a kernel panic. Every event-loop handler is wrapped in `catch_unwind`. The
   `oxinit` crate denies `unwrap`, `expect`, `panic`, and slice indexing. If the
   loop cannot continue, it spawns `/bin/sh` on the console. It never exits.
3. **Unsafe is quarantined.** Syscalls go through `rustix`. Whatever `unsafe`
   remains lives in one module, with a `// SAFETY:` comment per block stating
   the invariant.
4. **Synchronous.** One epoll loop, one thread, no async runtime in PID 1.
5. **Declarative units.** TOML. No shell, no scripting, no runtime
   interpolation beyond a small documented set of specifiers.

## Units

```toml
# /etc/oxinit/units/sshd.toml

[unit]
description = "OpenSSH daemon"
after       = ["network-online"]
requires    = ["network-online"]

[service]
type        = "notify"          # simple | forking | oneshot | notify
exec        = "/usr/sbin/sshd -D"
restart     = "on-failure"      # no | always | on-failure | on-abnormal
restart-sec = "5s"
user        = "root"

[resources]
memory-max = "256M"
tasks-max  = 512
```

Every key is specified in [docs/UNIT_FORMAT.md](docs/UNIT_FORMAT.md).

## Status

```
$ oxctl status sshd
sshd.service - OpenSSH daemon
     State: active (running) since 2026-08-08 09:14:22
      Main: 812 (sshd)
    Status: "Server listening on 0.0.0.0 port 22"
    Memory: 4.1M (max 256M)
     Tasks: 3 (max 512)
  Restarts: 0
    CGroup: /sys/fs/cgroup/oxinit.slice/sshd.service
```

Memory and task counts are read from `memory.current` and `pids.current` in the
service's cgroup.

## Current state

Milestone 0 is done: a minimal PID 1 that mounts the pseudo-filesystems, wires
the console, blocks signals and reads them from a signalfd, supervises a shell,
and reaps orphans. It boots under QEMU and gets a prompt.

There are no units, no dependency graph, and no cgroups yet — milestone 1 is
next. [ROADMAP.md](ROADMAP.md) has the breakdown.

## Running it

Requires a Rust toolchain with the musl target, `qemu-system-x86_64`, `cpio`,
and a kernel image.

```bash
cargo xtask boot
```

This builds a static binary for `x86_64-unknown-linux-musl`, packs it into a
single-file cpio initramfs, and boots it:

```bash
qemu-system-x86_64 -kernel bzImage -initrd oxinit.cpio.gz -nographic -append "console=ttyS0"
```

Edit to boot takes a few seconds. `Ctrl-A X` exits QEMU. Setup details are in
[CONTRIBUTING.md](CONTRIBUTING.md).

## Non-goals

oxinit does not, and will not:

- Resolve DNS.
- Implement an NTP client.
- Manage network configuration.
- Run containers.
- Boot the machine — it is not a bootloader.
- Manage logins or sessions.

It also will **not** aim for compatibility with systemd unit files. Supporting a
subset of `.service` means adopting a specification that is defined by another
project's implementation rather than by a document, and the subset you implement
is never the one your distribution actually uses. Units are TOML, specified
here, and that is the whole surface.

### What is implemented, and why that is different

oxinit does implement two protocols that came from systemd:

- **`sd_notify`** — `NOTIFY_SOCKET`, `READY=1`, `STATUS=`, `WATCHDOG=1`.
- **Socket activation** — `LISTEN_FDS`, `LISTEN_PID`, `LISTEN_FDNAMES`.

These are runtime protocols, not configuration formats. They are small, stable,
documented wire contracts that a large amount of existing software already
speaks. Implementing them lets unmodified daemons report readiness and receive
pre-opened sockets. That is a different thing from adopting another project's
config language, and the distinction is deliberate. See
[ARCHITECTURE.md](ARCHITECTURE.md).

## Name

`ox` — oxidation, meaning Rust — plus `init`. It states the implementation
language and the category of software, and it follows the naming convention the
category already uses: sysvinit, runit, dinit.

## Contributing

Pre-alpha, so the useful contributions right now are design review and the
milestone 0 work. See [CONTRIBUTING.md](CONTRIBUTING.md) for setup and the
development loop, and [ARCHITECTURE.md](ARCHITECTURE.md) before proposing
changes to the design.

## License

Dual-licensed under either of:

- MIT License
- Apache License, Version 2.0

at your option. Contributions submitted for inclusion are dual-licensed on the
same terms, with no additional conditions.
