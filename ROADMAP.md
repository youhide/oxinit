# Roadmap

Milestones are sequential. Each one is expected to boot and do something
observable in QEMU before the next one starts. No milestone is a refactor of the
previous one.

There are no dates. Pre-alpha, nothing is stable, and the API and unit format
may change between any two milestones.

## M0 — Minimal PID 1

**Done.**

The smallest program that can be PID 1 without the kernel panicking.

- [x] Verify `getpid() == 1`; exit with an error otherwise.
- [x] Mount `/proc`, `/sys`, `/dev`, `/dev/pts`, `/dev/shm`, `/run`, and
      `/sys/fs/cgroup`. Already-mounted is not an error.
- [x] Open `/dev/console` and `dup2` it onto fds 0, 1, 2.
- [x] Block all signals with `sigprocmask`; create a `signalfd`.
- [x] Reap loop: on `SIGCHLD`, `waitpid(-1, WNOHANG)` until it returns 0 or
      `ECHILD`.
- [x] Spawn `/bin/sh` on the console and respawn it when it exits.
- [x] `cargo xtask boot` — musl build, cpio initramfs, QEMU.
- [x] Never exit. Never `panic = "abort"`.

Verified under QEMU with an Alpine 6.18 kernel: the guest reaches
`Run /init as init process` and gets a shell prompt, `/proc/1/comm` is the
oxinit binary, `hostname` is `localhost`, `/sys/fs/cgroup/cgroup.controllers`
reads back the v2 controller set, an orphaned process leaves no zombie, and
oxinit logs nothing — every mount, the console, the hostname, the signalfd,
and the shell spawn all succeeded.

No epoll in M0. With one descriptor there is nothing to multiplex, so the loop
is a blocking read on the signalfd; epoll arrives in M1 with the second fd.

Known gap, deferred: the shell has no controlling terminal, so busybox reports
"can't access tty; job control turned off". Fixing it needs `setsid` plus
`TIOCSCTTY` on the child, which belongs with the rest of the console and
signal work in M5.

## M1 — Units and dependency resolution

**Done.**

- [x] TOML unit parsing in `oxinit-unit`, per
      [docs/UNIT_FORMAT.md](docs/UNIT_FORMAT.md).
- [x] Unit kinds: `[service]` and `[target]`, exactly one per unit.
- [x] Duration and size string parsers.
- [x] Directory precedence: `/etc/oxinit/units/` over
      `/usr/lib/oxinit/units/`.
- [x] Dependency graph in `oxinit-graph`: `requires`, `wants`, `after`,
      `before`, `conflicts`.
- [x] Kahn topological sort over the ordering edges.
- [x] Cycle detection that reports the cycle path and refuses to load.
- [x] Sequential start of the resolved order.
- [x] Unit tests for the parser and the graph, running on the host.

Verified under QEMU. A three-unit set starts in declared order, a `wants` on a
missing unit warns without blocking, `%H` and `%n` expand, a `oneshot` exit is
reported, and a deliberately introduced cycle is refused with its path —
`banner -> console -> banner` — after which PID 1 falls back to a console shell
rather than exiting.

52 host tests across the two library crates, no VM needed.

Deferred out of M1: `user` is parsed and validated but not applied. Dropping
privilege needs setgid, initgroups, and setuid against `/etc/passwd`. Until
that lands, a service declaring a non-root `user` is refused rather than
silently run as root.

## M2 — Supervision

**Done.**

- [x] Service state machine: `Inactive`, `Activating`, `Active`,
      `Deactivating`, `Failed`, `Restarting`.
- [x] Restart policies: `no`, `always`, `on-failure`, `on-abnormal`.
- [x] Exponential backoff from `restart-sec`, capped, with reset after a
      sustained `Active` period.
- [x] One timerfd plus a `BinaryHeap` of deadlines for backoff and timeouts.
- [x] epoll: signalfd and timerfd multiplexed, level-triggered.
- [x] Readiness by `type`: `simple`, `oneshot`, `notify`.
- [x] `sd_notify` on a `SOCK_DGRAM` socket at `/run/oxinit/notify`:
      `READY=1`, `STATUS=`, `WATCHDOG=1`.
- [x] `SO_PASSCRED` on the notify socket; sender identity from the
      `SCM_CREDENTIALS` ancillary message, not from the payload.
- [x] Watchdog deadlines and the miss path into `Failed`.
`type = "forking"` is deferred to M3, since it depends on cgroup emptiness
notification, and is treated as `simple` with a warning until then.

Applying `user` moved to M3 as well. Dropping privilege happens between fork
and exec, which is exactly where cgroup placement happens, and both want the
same `pre_exec` hook and the same `unsafe` block. Until then a non-root `user`
is refused rather than run as root.

Verified under QEMU:

- `restart = "on-failure"` with `restart-sec = "200ms"` comes back after 200ms,
  400ms, 800ms, 1.6s, 3.2s, 6.4s — doubling per consecutive failure.
- A `notify` service's `READY=1` moves it from `Activating` to `Active`, its
  `STATUS=` is captured, and both are attributed by `SCM_CREDENTIALS` rather
  than by anything in the payload.
- A service that goes ready and then stops pinging is killed on its watchdog
  deadline and lands in `Failed`, while a service that keeps pinging is
  untouched.

## M3 — cgroup v2

**Done.**

- [x] Hierarchy under `/sys/fs/cgroup/oxinit.slice/<name>.service/`.
- [x] Write the pid to `cgroup.procs` between `fork` and `exec`.
- [x] `memory-max` → `memory.max`, `tasks-max` → `pids.max`.
- [x] Read `memory.current` and `pids.current` for status output.
- [x] `cgroup.events` registered with `EPOLLPRI`; track the `populated` key.
- [x] `type = "forking"` readiness, using `populated`.
- [x] `cgroup.kill` on stop, after `SIGTERM` and the stop timeout.
- [x] Apply `user` in the same `pre_exec` hook as cgroup placement: setgid,
      initgroups, setuid, in that order.

Two crates, both host-tested without a VM. `oxinit-cgroup` is `std::fs` and
nothing more, because a cgroup is a directory of text files; `oxinit-user`
parses `/etc/passwd` and `/etc/group`, deliberately not `getpwnam`, which
resolves through NSS and would put `dlopen` and arbitrary third-party code
inside PID 1.

`stop-sec` is new in the unit format: the grace period between `SIGTERM` and
`cgroup.kill`. A unit is `Deactivating` until its cgroup empties, not until its
main process exits, and one that had to be killed ends `Failed` rather than
`Inactive`.

Fixed on the way: children inherited PID 1's full signal block mask. The
standard library's own reset runs before `pre_exec` and is not part of
`Command`'s contract, and a service that starts with `SIGTERM` blocked cannot
be stopped at all — every stop would have escalated to `cgroup.kill`. oxinit
now resets the mask itself, first thing in its own hook. ARCHITECTURE.md always
required this; nothing had tested it, because M2's watchdog used `SIGKILL`.

Verified under QEMU:

- `memory.max` reads back `16777216` and `pids.max` reads back `8` for a unit
  declaring `memory-max = "16M"` and `tasks-max = 8`, and the root's
  `cgroup.subtree_control` reads back `memory pids`.
- A service's `cgroup.procs` holds its pid, so placement happened before
  `exec`, not after it.
- A `forking` unit's `cgroup.procs` holds the pid of the process left behind,
  not the one oxinit forked, and the unit reaches `Active` on that basis. When
  that process later exits, the empty cgroup is what reports the unit gone, and
  the restart policy applies.
- A `oneshot` running `/bin/id` as `user = "nobody"` prints
  `uid=65534(nobody) gid=65534(nogroup) groups=100(oxinit),65534(nogroup)` —
  supplementary groups included, not just the primary one.
- `memory.current` and `pids.current` are reported when a unit comes up.
- A child's `/proc/self/status` shows `SigBlk: 0000000000000000`.
- A hung service is sent `SIGTERM` and is gone before its `stop-sec` expires.
  One that ignores `SIGTERM` outlives it, gets `cgroup.kill`, and lands in
  `Failed`.

99 host tests across the five library crates, no VM needed.

Deferred out of M3: nothing from the list above. The `Requested` stop cause
exists and is tested, but has no caller until `oxctl` in M5 — there is no way
to ask for a stop yet.

## M4 — Socket activation

**Next.**

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
