# Roadmap

Milestones are sequential. Each one is expected to boot and do something
observable in QEMU before the next one starts. No milestone is a refactor of the
previous one.

There are no dates. Pre-alpha, nothing is stable, and the API and unit format
may change between any two milestones.

## M0 — Minimal PID 1

**In progress.**

The smallest program that can be PID 1 without the kernel panicking.

- [ ] Verify `getpid() == 1`; exit with an error otherwise.
- [ ] Mount `/proc`, `/sys`, `/dev`, `/dev/pts`, `/dev/shm`, `/run`, and
      `/sys/fs/cgroup`. Already-mounted is not an error.
- [ ] Open `/dev/console` and `dup2` it onto fds 0, 1, 2.
- [ ] Block all signals with `sigprocmask`; create a `signalfd`.
- [ ] Reap loop: on `SIGCHLD`, `waitpid(-1, WNOHANG)` until it returns 0 or
      `ECHILD`.
- [ ] Spawn `/bin/sh` on the console and respawn it when it exits.
- [ ] `cargo xtask boot` — musl build, cpio initramfs, QEMU.
- [ ] Never exit. Never `panic = "abort"`.

Roughly 150 lines. It gets a shell prompt in QEMU and does not leak zombies.

## M1 — Units and dependency resolution

**Not started.**

- [ ] TOML unit parsing in `oxinit-unit`, per
      [docs/UNIT_FORMAT.md](docs/UNIT_FORMAT.md).
- [ ] Unit kinds: `[service]` and `[target]`, exactly one per unit.
- [ ] Duration and size string parsers.
- [ ] Directory precedence: `/etc/oxinit/units/` over
      `/usr/lib/oxinit/units/`.
- [ ] Dependency graph in `oxinit-graph`: `requires`, `wants`, `after`,
      `before`, `conflicts`.
- [ ] Kahn topological sort over the ordering edges.
- [ ] Cycle detection that reports the cycle path and refuses to load.
- [ ] Sequential start of the resolved order.
- [ ] Unit tests for the parser and the graph, running on the host.

At the end of M1, oxinit starts a set of services in a correct order and
refuses to start with a broken configuration.

## M2 — Supervision

**Not started.**

- [ ] Service state machine: `Inactive`, `Activating`, `Active`,
      `Deactivating`, `Failed`, `Restarting`.
- [ ] Restart policies: `no`, `always`, `on-failure`, `on-abnormal`.
- [ ] Exponential backoff from `restart-sec`, capped, with reset after a
      sustained `Active` period.
- [ ] One timerfd plus a `BinaryHeap` of deadlines for backoff and timeouts.
- [ ] Readiness by `type`: `simple`, `oneshot`, `notify`.
- [ ] `sd_notify` on a `SOCK_DGRAM` socket at `/run/oxinit/notify`:
      `READY=1`, `STATUS=`, `WATCHDOG=1`.
- [ ] `SO_PASSCRED` on the notify socket; sender identity from the
      `SCM_CREDENTIALS` ancillary message, not from the payload.
- [ ] Watchdog deadlines and the miss path into `Failed`.

`type = "forking"` is deferred to M3, since it depends on cgroup emptiness
notification.

## M3 — cgroup v2

**Not started.**

- [ ] Hierarchy under `/sys/fs/cgroup/oxinit.slice/<name>.service/`.
- [ ] Write the pid to `cgroup.procs` between `fork` and `exec`.
- [ ] `memory-max` → `memory.max`, `tasks-max` → `pids.max`.
- [ ] Read `memory.current` and `pids.current` for status output.
- [ ] `cgroup.events` registered with `EPOLLPRI`; track the `populated` key.
- [ ] `type = "forking"` readiness, using `populated`.
- [ ] `cgroup.kill` on stop, after `SIGTERM` and the stop timeout.

## M4 — Socket activation

**Not started.**

- [ ] Socket units: create, bind, and listen before the service starts.
- [ ] Pass descriptors starting at fd 3, clearing `FD_CLOEXEC`.
- [ ] Set `LISTEN_FDS`, `LISTEN_PID`, and `LISTEN_FDNAMES` in the child
      environment.
- [ ] Start the service on the first incoming connection.
- [ ] Hold the listening socket across a service restart so connections are
      queued rather than refused.

## M5 — Shutdown and the client

**Not started.**

- [ ] Ordered shutdown: reverse topological order, `SIGTERM`, timeout,
      `cgroup.kill`.
- [ ] Signal handling: `SIGTERM`, `SIGINT`, `SIGUSR1`, `SIGUSR2`, `SIGPWR`
      mapped to explicit actions. PID 1 has no default dispositions, so an
      unhandled signal does nothing.
- [ ] Final `sync`, remount read-only, `reboot(2)` with the right command.
- [ ] Control socket: `SOCK_SEQPACKET` at `/run/oxinit/control.sock`, mode
      `0600`.
- [ ] `oxinit-ipc` request and response types, JSON on the wire.
- [ ] `oxctl`: `start`, `stop`, `restart`, `status`, `list`, `reload`.
- [ ] `cargo xtask test-boot` — boot with a timeout, assert on serial output.

## Beyond M5

Not planned, not designed, listed only so the questions have an answer:

- `aarch64-unknown-linux-musl` as a tested target rather than an assumed one.
- Replacing JSON with `postcard` on the control socket.
- Log shipping as a separate process reading service output.
- Timer units.
- A test suite that boots a real distribution userspace rather than a shell.
