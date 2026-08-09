# oxinit

A service manager and PID 1 for Linux, written in Rust.

[![License](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue)](#license)
[![Status](https://img.shields.io/badge/status-pre--alpha-orange)](ROADMAP.md)

**Pre-alpha. Nothing is stable.** See [ROADMAP.md](ROADMAP.md) for what works.

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

Milestones 0 through 6 are done. oxinit boots under QEMU and runs as a
container's PID 1: it mounts the pseudo-filesystems, multiplexes signalfd,
timerfd, the notify socket, the control socket and every service's
`cgroup.events` on one epoll loop, parses TOML units, resolves their dependency
graph, starts them in order, restarts them with exponential backoff, places
each service in a cgroup v2 and applies its limits, drops privilege between
fork and exec, binds and passes listening sockets, answers `oxctl`, and shuts
the machine down in reverse order.

[ROADMAP.md](ROADMAP.md) has the breakdown, including what each milestone was
verified against.

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

## In a container

oxinit works as a container's PID 1. That job is reaping orphans, forwarding
signals, and supervising more than one process — which is most of what a
service manager already does, and the reason `tini` and `dumb-init` exist. The
difference is that oxinit supervises: restart policy, readiness, ordering, and
a stop that is ordered rather than a broadcast `SIGKILL`.

The image is the binary, the units, and whatever the services need:

```dockerfile
FROM scratch
COPY oxinit /init
COPY units/ /etc/oxinit/units/
COPY myservice /usr/bin/myservice
ENTRYPOINT ["/init"]
```

```bash
cargo xtask container --privileged
```

builds exactly that from this repository's own test units, runs it, and asserts
on the log and on the exit code.

**`docker stop` is an ordered shutdown.** `SIGTERM` stops every unit in the
reverse of its start order, and oxinit then exits: `0` for a clean stop, `133`
— systemd's convention, honoured by podman — where a machine would have
rebooted. Exiting is the whole contract in a container, since the container is
up for exactly as long as its PID 1 is. A supervisor that stopped everything
and stayed running is a container the runtime has to `SIGKILL` ten seconds
later, which is the exit code 137 you have seen.

**The runtime's environment is left alone.** Whatever it already mounted is
skipped rather than re-mounted or reported, and the stdio it exec'd oxinit with
is kept rather than redirected onto `/dev/console` — which is what a container
runtime reads its logs from.

**`[resources]` needs a writable cgroupfs.** By default a runtime mounts
`/sys/fs/cgroup` read-only, and oxinit says so once and carries on without
cgroups: no per-service limits, no `type = "forking"` readiness, and
`cgroup.kill` falls back to signalling the one pid it knows. A unit that
declared a limit oxinit cannot apply fails rather than running uncapped — the
unit asked to be capped, and running it uncapped is not a smaller version of
that. With `--privileged`, or under Kubernetes with the equivalent security
context, the hierarchy works and the limits land.

Note what those limits mean there: the runtime has already capped the whole
container, and `[resources]` divides that budget between the units inside it.
It cannot raise the container's own ceiling.

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

### Linux only

Not incidentally — the mechanisms oxinit is built on have no portable
equivalent:

| Job | Mechanism | Elsewhere |
|---|---|---|
| Track a service's processes | cgroup v2 | jails/rctl on FreeBSD; a different model |
| Signals as event loop input | `signalfd` | `kqueue` + `EVFILT_SIGNAL` |
| Multiplexing | `epoll` | `kqueue` |
| Deadlines | `timerfd` | `EVFILT_TIMER` |
| Sender identity on a socket | `SO_PASSCRED` / `SCM_CREDENTIALS` | `LOCAL_PEERCRED` / `SCM_CREDS` |
| Boot filesystems | devtmpfs, procfs, sysfs | devfs, no sysfs |

`cgroup.kill` and the `populated` key of `cgroup.events` are what make
`type = "forking"` tractable at all, and they have no direct analogue.

macOS is not a question of effort: `launchd` is PID 1, SIP prevents replacing
it, and there is no interface to substitute an init.

A FreeBSD port is possible in principle — `init_path` is settable — but it
would be a different program. It would, however, reuse `oxinit-unit`,
`oxinit-graph`, and `oxinit-service`, which have no OS dependency at all: the
unit format, the dependency semantics, the topological ordering, and the
restart policy are all portable and test on any host today. Only the system
layer is Linux-bound.

None of that is planned. Getting one platform right comes first.

Architecture support is a separate axis: x86_64 and aarch64, both musl.

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
current milestone's work. See [CONTRIBUTING.md](CONTRIBUTING.md) for setup and the
development loop, and [ARCHITECTURE.md](ARCHITECTURE.md) before proposing
changes to the design.

## License

Dual-licensed under either of:

- MIT License
- Apache License, Version 2.0

at your option. Contributions submitted for inclusion are dual-licensed on the
same terms, with no additional conditions.
