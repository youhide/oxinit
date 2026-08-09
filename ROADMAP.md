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

**Done.**

- [x] Socket units: create, bind, and listen before the service starts.
- [x] Pass descriptors starting at fd 3, clearing `FD_CLOEXEC`.
- [x] Set `LISTEN_FDS`, `LISTEN_PID`, and `LISTEN_FDNAMES` in the child
      environment.
- [x] Start the service on the first incoming connection.
- [x] Hold the listening socket across a service restart so connections are
      queued rather than refused.

`[socket]` is specified in [docs/UNIT_FORMAT.md](docs/UNIT_FORMAT.md): `listen`,
`service`, `type`, and `backlog`. A socket unit cannot share a name with its
service, because unit names are unique across kinds and that is what lets a
bare name identify one unit; so `service` is required rather than inferred. A
socket is implicitly ordered before its service and deliberately does not pull
it in — a service reachable from `default` starts at boot whether anything
connects or not, which is the opposite of activation.

`LISTEN_PID` is the interesting part. Its value is the pid of the process the
descriptors went to, and that pid does not exist until after the `fork`, where
nothing may allocate. So oxinit lays out the child's argv and environment in
the parent with a fixed-width gap for the digits, the child writes them in, and
the child execs that image itself from the `pre_exec` hook. `Command`'s own
environment is no longer used at all, which also means a service sees exactly
what oxinit set and never a stale `LISTEN_FDS` inherited second-hand.

A listening descriptor stays readable until someone accepts, and oxinit never
accepts anything, so one left registered after the service starts would spin
the loop. It is registered exactly while its service is `Inactive` or `Failed`
— reconciled once per loop iteration rather than armed and disarmed in each of
the dozen places a service changes state.

Verified under QEMU:

- Two socket units bind before anything starts, one on `/run/echo.sock` and one
  on `127.0.0.1:7000`, and the service they name does not start at boot.
- Connecting to the unix socket starts it. It reports `LISTEN_FDS=2`,
  `LISTEN_FDNAMES=echo-socket:echo-tcp`, and a `LISTEN_PID` equal to its own
  pid and to the pid oxinit logged. It finds its descriptor by name rather than
  by assuming fd 3, answers, and exits.
- A second connection made while the service was still running queues in the
  backlog, and is served by a fresh activation with a fresh `LISTEN_PID` once
  the first exits — the socket is watched again, and was never closed.

108 host tests across the five library crates.

Deferred out of M4: a `mode` key for unix sockets. They are chmodded to `0666`
after binding, because PID 1's umask would otherwise produce a socket only root
can reach, and restricting that is a choice an operator should make explicitly.
Nothing needs it yet.

## M5 — Shutdown and the client

**Done.**

- [x] Ordered shutdown: reverse topological order, `SIGTERM`, timeout,
      `cgroup.kill`.
- [x] Signal handling: `SIGTERM`, `SIGINT`, `SIGUSR1`, `SIGUSR2`, `SIGPWR`
      mapped to explicit actions. PID 1 has no default dispositions, so an
      unhandled signal does nothing.
- [x] Final `sync`, remount read-only, `reboot(2)` with the right command.
- [x] Control socket: `SOCK_SEQPACKET` at `/run/oxinit/control.sock`, mode
      `0600`.
- [x] `oxinit-ipc` request and response types, JSON on the wire.
- [x] `oxctl`: `start`, `stop`, `restart`, `status`, `list`, `reload`.
- [x] `cargo xtask test-boot` — boot with a timeout, assert on serial output.
- [x] `init.scope`: move PID 1 into a leaf cgroup before delegating
      controllers.
- [x] Read the hostname back when `sethostname` is refused, rather than
      assuming `localhost`.

The last two came out of the container investigation below, and neither is
container-specific.

`init.scope` is the more interesting one. Delegating controllers today works
only because the root cgroup is exempt from the "no internal processes" rule
and PID 1 lives there. Moving PID 1 into a leaf first removes that dependency,
stops accounting PID 1's own memory at the root, and is what makes the cgroup
path work anywhere the root is not really the root.

`oxinit-ipc` holds the request and response types, shared by both sides, so the
wire format is a type rather than a convention written down twice. One of its
tests pins the JSON encoding rather than letting derive decide it — being
readable with `socat` is the reason it is JSON at all. `serde_json` is the one
new dependency.

Nothing on the control socket blocks. A client connection is registered with
epoll like every other descriptor, and every answer is immediate: `Accepted`
says what was asked for, not what has finished, because a stop ends when the
unit's cgroup empties and the one socket that has to stay responsive must not
wait on the slowest thing on the machine.

`cargo xtask test-boot` asserts eighteen lines on the serial log, one per thing
this roadmap claims works, and fails if the machine does not power itself off
within ninety seconds. It was checked against a deliberately broken image:
removing `units/dropped.toml` fails exactly the two privilege-drop checks and
nothing else.

A bug found by running it: rustix's `recv` returns the *message* length
alongside the number of bytes that landed in the buffer, not the number
discarded. Read as "dropped", it rejected every well-formed request.

Verified under QEMU:

- `SIGUSR2` prints every unit's state, pid, restart count and last `STATUS=`.
- `SIGTERM` stops units in reverse start order — targets and sockets at once,
  services as their cgroups empty — then syncs and powers off. QEMU exits on
  its own. `SIGINT` reboots instead, and `Run /init as init process` appears
  twice in one log.
- `/proc/1/cgroup` is `0::/init.scope`, and the root's `cgroup.subtree_control`
  still reads `memory pids`.
- `oxctl list` prints all units; `oxctl status probe` prints its pid, `STATUS=`,
  `memory.current` and `pids.current`; `oxctl stop limited` is followed by
  oxinit reporting it inactive; `oxctl status nosuch` fails with a message and
  a non-zero exit.

Verified in a container: with a writable cgroupfs, `cgroup.subtree_control`
reads `memory pids` instead of failing with `EBUSY`, `oxinit.slice` and its
per-service cgroups exist, and `[resources]` limits land. `--hostname demo-box`
makes `%H` expand to `demo-box`.

115 host tests across seven library crates.

Deferred out of M5: the controlling terminal. M0 noted that the console shell
reports "can't access tty; job control turned off" and that `setsid` plus
`TIOCSCTTY` belonged with the signal work here. It belongs with the console
work in M6 instead — deciding whether to take over `/dev/console` at all comes
first, and doing terminal setup for a console oxinit should not have claimed
would be the wrong order.

## M6 — Containers

**Done.**

oxinit is a plausible PID 1 for a container — the job there is reaping
orphans, forwarding signals, and supervising more than one process, which is
most of what M0 through M4 already do. What it needed was to stop assuming it
booted a machine.

- [x] Detect that the machine is a container, and say so once.
- [x] Do not take over `/dev/console` when the runtime already gave PID 1 a
      usable stdio.
- [x] Tolerate the pseudo-filesystems the runtime has already mounted, rather
      than reporting seven failures nothing can act on.
- [x] `exit()` instead of `reboot(2)` where rebooting is not oxinit's to do —
      and only there, since exiting is a kernel panic anywhere else.
- [x] `cargo xtask container` — build an image, run it, and assert on its
      output, the way `test-boot` does for QEMU.
- [x] README and docs: running oxinit as PID 1 under Docker and Kubernetes,
      including what `[resources]` means when the runtime already caps the
      container.
- [x] Give the console shell a controlling terminal — `setsid` plus
      `TIOCSCTTY` — once it is settled which console it should own.

Measured in a container before any of this was written, against Docker with
cgroup v2. What already worked: unit loading and resolution, `sd_notify` and
the watchdog, the privilege drop, socket activation over both unix and TCP,
and orphan reaping — no zombies after a double fork, which is the usual reason
to reach for `tini`.

What did not, and where each was fixed:

| Symptom                                            | Cause                                              |
|----------------------------------------------------|----------------------------------------------------|
| `docker stop` always ends in `SIGKILL`, exit 137    | `SIGTERM` and `SIGINT` did nothing. M5, and PID 1 not exiting afterwards. M6. |
| No output at all from a privileged container        | `/dev/console` exists there, so oxinit redirected stdio onto it and the runtime's pipes saw nothing. |
| `oxinit.slice` is never created                     | `cgroup.subtree_control` returns `EBUSY`: inside a cgroup namespace the mount root is the container's own cgroup, not the real root, so the internal-process rule applies and PID 1 is in it. M5's `init.scope`. |
| `%H` expands to `localhost`                         | `sethostname` is refused and the fallback assumed rather than read. M5. |
| Seven `mount: EPERM` lines before anything else     | The runtime mounted them already.                   |

**Exactly one thing keys off the detection**: on a machine a shutdown ends in
`reboot(2)`, and in a container it ends by exiting. Everything else decides on
its own local evidence — whether stdio is already open, whether something is
already mounted at a path — because those questions have exact answers and "am
I in a container" does not. A privileged container has a console and can mount;
an initramfs may arrive with filesystems already there. Deriving the specific
from the general would have been wrong in both directions.

That is also why the console fix is not conditional. Each of fds 0, 1 and 2 is
filled from `/dev/console` only if it is not already open, which gives the same
answer on a machine, where the kernel hands init the console itself. It turned
up a latent bug on the way: where the kernel had left the descriptors closed —
the one case the code existed for — `open` returned fd 0, and dropping the
`OwnedFd` closed the standard input it had just installed.

The controlling terminal, open since M0, needed a decision rather than two
syscalls. A terminal belongs to one session, so a second claimant takes it from
the first, and on a machine that boots to a console *every* service has a
terminal on stdin — inferring it would have had the units take it from each
other one start at a time. So it is `tty = true` in `[service]`, new in the
unit format, declared by the one unit that is a console. `test-boot` now fails
on "can't access tty" rather than tolerating it.

`cargo xtask container` builds the same root filesystem `boot` packs into a
cpio, as a `FROM scratch` image, runs it, waits for the boot, asks the runtime
to stop it the way an operator would, and asserts on both the log and the exit
code. Nine checks unprivileged, eleven with `--privileged`, which adds a
writable cgroupfs and the two assertions that depend on one. A container test
that ran a different image from the QEMU test would not be testing the same
thing, which is why there is one staging tree and two ways of packing it.

Verified against Docker with cgroup v2:

- The detection names the runtime and its evidence — `docker, by /.dockerenv`,
  or `demo-runtime, by $container` when the runtime says so itself, which is
  checked first and wins.
- One line replaces the seven: `already mounted, left alone: /proc, /sys,
  /dev, /dev/pts, /dev/shm, /sys/fs/cgroup`, and one more for the `/run` the
  runtime does not mount and oxinit is not permitted to.
- The log exists at all, which is the whole of the console fix, and holds
  `READY=1` readiness, `uid=65534(nobody)`, and a socket-activated service
  answering over a descriptor it found by `LISTEN_FDNAMES`.
- `docker stop` finishes in 1.1 seconds with exit code 0, against a ten-second
  grace period that used to expire into a `SIGKILL` and exit 137.
- Unprivileged, `/sys/fs/cgroup` is read-only: oxinit says so once and carries
  on, and the unit declaring `[resources]` fails rather than running uncapped.
  With `--privileged`, `/proc/1/cgroup` is `0::/init.scope` and the limits
  land.

116 host tests across seven library crates.

Deferred out of M6: `tty` places the service in its own session and claims
*stdin*, whatever stdin happens to be. A `tty-path` key naming a device — so
that a unit can own a serial console oxinit itself is not on — is the obvious
next question and nothing needs it yet.

## M7 — Logs

**Done.**

A service's output goes wherever PID 1's happened to go. Every unit on the
machine writes to one console, nothing says which unit a line came from, and
nothing keeps any of it. It is the largest thing a service manager is expected
to do that oxinit does not.

- [x] `oxinit-log`: record format, line splitting, rotation policy, paths.
      Host-tested, no syscalls.
- [x] `oxlogd`: a separate process that reads service output and writes it.
- [x] One pipe per service start. The write end goes to the child; the read
      end is passed to `oxlogd` over a unix socket with `SCM_RIGHTS`.
- [x] `output` in `[service]`: `console`, `log`, `null`.
- [x] Size-based rotation, bounded on disk.
- [x] `oxctl logs <unit>`.
- [x] `test-boot` and `container` assert a service's output landed in its own
      file, and that the console no longer carries it.

**PID 1 reads no service output.** It creates the pipe, gives the write end to
the child, and passes the read end on. That is the whole reason there is a
second process: a service that writes a megabyte a second must not be able to
make PID 1 do work, and a bug in a log writer must kill a log writer. It is the
same argument that put `oxctl` out of process, applied to the one remaining
thing that would otherwise have to live inside the event loop.

**PID 1 keeps the read end rather than giving it away.** `oxlogd` is a
supervised service and can restart. If PID 1 handed over its only copy, the
last close would break every running service's stdout, and a service killed by
`SIGPIPE` because the log daemon bounced is worse than a service with no logs.
Holding it means a reconnecting `oxlogd` is handed every descriptor again.

The cost is stated rather than hidden: with no `oxlogd` connected, output backs
up in the pipe, and a service that fills it blocks on write. That is bounded by
the pipe buffer, it is recoverable, and it is why `output` defaults to
`console` rather than to `log`.

**PID 1 is the server.** `oxlogd` connects to `/run/oxinit/log.sock`. PID 1
already owns a listening socket and an event loop; the other direction would
have PID 1 connecting to a service it supervises, at every service start, and
deciding what to do when that fails.

**`stdout` and `stderr` share one pipe.** The order in which a service wrote to
its two streams is information, and two pipes would destroy it — they would be
read independently and interleaved by whichever the reader got to first.

**Logs are files, and `oxctl logs` reads them.** Not proxied through the
control socket: that is the one socket that has to stay responsive, and bulk
data is exactly what would stop it being.

A bug this milestone found in M5's: `oxctl restart` never restarted a unit
that was running. It scheduled an alarm that only acted on a unit in
`Restarting`, and a requested stop ends `Inactive` — precisely because
requesting a stop is what stops the restart policy applying. An explicit
restart is now remembered rather than timed, and started by the same event
that ends the stop. Nothing had tested it: M5 verified `oxctl stop`, and the
restart path was reached for the first time by M7 restarting `oxlogd` on
purpose.

Verified under QEMU and in a container:

- A service declaring `output = "log"` has its stdout, its stderr and its last
  line-without-a-newline in `/var/log/oxinit/chatty.log`, timestamped, in the
  order it wrote them — and `chatty-out` appears nowhere on the console, which
  the boot suite asserts as an absence.
- `oxctl logs` reads that file back. The assertion strips the record's
  timestamp with a pattern, so a format that lost it fails here too.
- **A restart of `oxlogd` is invisible to what it was shipping.** A service
  writing a line a second is at 1 line when `oxctl restart oxlogd` runs and at
  6 five seconds later: oxinit still held the read end and handed it to the
  replacement.

128 host tests across eight library crates.

Deferred out of M7: `oxctl logs -f`, and reading the rotated generations.
Following a file that a second process is appending to is a different program
from printing one, and `<unit>.log.1` and up are in the same directory in the
same format — reading them is `cat`, which is most of the reason the format is
plain text.

## M8 — Timers

**Done.**

The last thing a service manager is expected to do that oxinit does not.
Periodic work has to go somewhere, and without this the answer is `cron` —
a second scheduler, with its own idea of what a job is, no dependency
ordering, no cgroup, no restart policy and no logs.

- [x] `oxinit-timer`: schedules, calendar expressions, and the civil-date
      arithmetic that turns one into a moment. Host-tested, no syscalls.
- [x] `[timer]` units: `service`, `on-boot`, `interval`, `on-calendar`.
- [x] Arming, firing, and re-arming on the timerfd and deadline heap M2
      already has.
- [x] `oxctl` reports when a timer next elapses.
- [x] `test-boot` asserts a timer fired, fired again on its interval, and
      that the service it named ran each time.

**Three keys, not systemd's five.** `on-boot` is the first firing, measured
from when the timer unit started; `interval` is every firing after that,
measured from the previous one; `on-calendar` is wall-clock. At least one is
required. `interval` on its own implies an `on-boot` of the same length, and
`on-boot` on its own is a one-shot delayed task.

Measuring the interval from the previous *firing* rather than from the
service's last activation is the deliberate difference. It is what the key name
says, it does not depend on a state transition the service may never make — a
`oneshot` never becomes `Active` at all — and it is the behaviour anyone
writing "every hour" expects.

**The calendar vocabulary is closed**, like the specifier set: `hourly`,
`daily`, `weekly`, and a literal `HH:MM` or `HH:MM:SS`. It is not a cron
expression and will not grow into one. A schedule nobody can misread is worth
more than one that can say anything.

**Wall-clock times are UTC.** oxinit has no timezone database and is not going
to carry one. That is a real limitation and is written down rather than
implied.

**A timer names its service**, the way a socket unit does, and for the same
reason: unit names are unique across kinds, which is what lets `requires`,
`after` and `oxctl` take a bare name. It does not pull it in, either — a
service reachable from `default` starts at boot whether anything scheduled it
or not, which is the opposite of what a schedule is for.

**No new machinery.** M2 built one timerfd and a heap of deadlines because
restart backoff, start timeouts and watchdog intervals are all "wake me in N".
A schedule is the same statement, so this is one more `Alarm` variant and not
a second clock.

Two behaviours worth writing down because neither is obvious and both are
tested: a firing while the service is still running is **skipped, not
queued**, since a job that outlasts its own interval would otherwise
accumulate copies of itself; and a failed run does **not** stop the schedule,
because a timer says *when*, and what to do about a failure is the service's
`restart`.

Verified under QEMU: `tick-timer` arms with its `on-boot` of 2s, elapses,
starts `stamp`, which prints and completes — and the timer re-arms with its
3s `interval` and does it again, repeatedly, over the test image's uptime.
`stamp` is reachable from nothing, so the only thing that ever started it was
the schedule.

146 host tests across nine library crates, 14 of them on the calendar
arithmetic alone — including that firing `daily` at exactly midnight schedules
the next one a day later rather than the same second, forever.

Deferred out of M8: `persistent`, which would catch up a firing missed while
the machine was off. It needs state on disk that survives a boot, and where
that state lives is a bigger question than the feature.

## M9 — aarch64

**Done.**

README and ARCHITECTURE both listed `x86_64-unknown-linux-musl` and
`aarch64-unknown-linux-musl` as the supported targets. One of them had never
been compiled, let alone booted. "Supported" is a claim about something that
has been run.

- [x] `xtask` knows about architectures rather than one hard-coded triple:
      the Rust target, the QEMU binary, the machine, and what the kernel calls
      the serial port.
- [x] `--arch aarch64`, and `--arch all` for `test-boot`.
- [x] Per-architecture artifacts, defaults and environment variables, so two
      targets can live in one tree without one silently getting the other's
      kernel or the other's busybox.
- [x] The whole boot suite, run on both.

**It compiled and booted unchanged.** Every one of the twenty-six checks
passes on ARM64 — the mounts, the signalfd, cgroup placement between fork and
exec, the privilege drop, socket activation and `LISTEN_PID`, `cgroup.kill`,
log shipping over `SCM_RIGHTS`, timers, and the ordered shutdown. That is the
result, and it is worth stating plainly rather than dressing up: the
portability came from going through `rustix` and keeping the `unsafe` in one
file, and this is the first evidence that it actually worked.

Three things had to change, and none of them were in oxinit:

- `-M virt -cpu cortex-a57`. x86_64 has a default machine that boots a kernel
  and aarch64 has no such thing; `virt` is the board QEMU invented for exactly
  this, and the default CPU is one the kernel will not run on.
- `console=ttyAMA0`, not `ttyS0`. The wrong one produces a boot with no output
  at all and nothing to say why.
- A 300-second budget instead of 90. Emulating a foreign architecture has no
  hardware acceleration to fall back on and the whole boot runs through QEMU's
  JIT.

A bug found on the way, and not an aarch64 one: an image built without a
shell failed while installing `oxctl`, because `bin/` was only created in the
branch that had a shell to put in it. Every previous run had passed `--shell`.

Verified: `cargo xtask test-boot --arch all`, 26 checks on each, against an
Alpine 6.12 LTS kernel for each architecture. The aarch64 guest reports
`Machine model: linux,dummy-virt` and a kernel built on `build-3-22-aarch64`,
so it is genuinely a second architecture and not the first one relabelled.

## M10 — A real userspace

**Done.**

Every image before this one was hand-assembled: one statically linked busybox,
a `/etc/passwd` with two lines in it, and this project's own binaries. So
every service oxinit had ever supervised was static, and lived where `xtask`
had put it.

- [x] `cargo xtask test-distro`: an initramfs built from a real distribution's
      root filesystem, with oxinit as `/init` on top.
- [x] The same twenty-six checks, against a userspace this project did not
      assemble, plus two only a real one can pass.
- [x] The image is assembled inside the distribution's own container, so the
      root filesystem keeps its ownership and its modes.

**Every service in it is dynamically linked.** Alpine's busybox needs
`/lib/ld-musl-x86_64.so.1`, and finding it goes through `sys::raw::Image` —
the code that lays out the child's argv and environment by hand and execs it
directly, so that `LISTEN_PID` can be written after the fork. That path had
never been exercised with anything but a static binary. It works: `ldd /bin/sh`
run as a unit reports the interpreter, and the twenty-six existing checks pass
unchanged.

**It is an initramfs, not a disk, and that is a finding rather than a
shortcut.** Booting a distribution's root off a block device needs a kernel
with both a disk driver and a filesystem built in. Alpine builds all
thirty-four of its filesystems as modules — `CONFIG_EXT4_FS=m`,
`CONFIG_VIRTIO_BLK=m`, even in the `virt` flavour meant for VMs — which is
exactly what its initramfs exists to load before it `switch_root`s. Reaching a
disk root would mean oxinit growing a `switch_root`, and pivoting the root is
a job [README.md](README.md) says this project does not do. So oxinit sits
where it always did in the boot chain: after someone else's initramfs, or as
the initramfs. Confirmed by booting one: `Kernel panic — VFS: Unable to mount
root fs on "/dev/vda"`.

Two things the real userspace corrected, both in this repository rather than
in oxinit:

- `/bin/id` does not exist on Alpine; `id` is in `/usr/bin`. A unit names an
  absolute path, so the layout of the image is part of what the unit says. The
  hand-assembled image now follows the distribution rather than the other way
  round, and one unit set runs on both.
- The test group was gid 100, which a real distribution has already given to
  `users`. It is 900 now.

Verified: `cargo xtask test-distro`, 28 checks against Alpine 3.22.5 — the
release file read by a unit, `ld-musl-x86_64.so.1` reported by another, and
`uid=65534(nobody) gid=65534(nobody) groups=900(oxinit),65534(nobody)` from
the distribution's own user database rather than from a file xtask wrote.

## M11 — Dependencies that mean what the document says

**Done.**

[docs/UNIT_FORMAT.md](docs/UNIT_FORMAT.md) and
[ARCHITECTURE.md](ARCHITECTURE.md) describe four behaviours the supervisor
does not have. `conflicts` appears nowhere in the `oxinit` crate at all;
`requires` and `after` appear only in the graph, which uses them to compute a
start *order* and then hands that order to a loop that starts everything as
fast as it can.

- [x] `after` gates starting, not just ordering: a unit does not start until
      every unit it is `after` has finished activating.
- [x] `requires`: a unit whose requirement has failed fails instead of
      starting.
- [x] `conflicts`: starting a unit stops the ones it excludes, symmetrically,
      whichever side declared it.
- [x] `start-sec`: a bound on `Activating`, so a service that never becomes
      ready fails instead of waiting forever.
- [x] An ordering assertion in `test-boot`, because "A appears before B" is
      not something substring matching can say.

**The order was real; the waiting was not.** Kahn's algorithm has been correct
since M1 and produces a genuine topological order. What consumed it was a
`for` loop calling `start` on each name in turn — and `start` returns as soon
as the child is forked. A `notify` service is `Activating` until `READY=1`
arrives, which is an event, and by then the loop had started everything else.
So `after` meant "issued in this order", where the document says "not started
until every named unit has finished activating".

Making that real turns boot from a loop into a queue drained by the same event
loop as everything else — which is the shape shutdown already has, and for the
same reason.

**Which is why `start-sec` is in the same milestone.** Gating on activation
without bounding it means one service that never signals readiness stalls the
whole boot with nothing to break the deadlock. `Activating` has been unbounded
since M2, and ARCHITECTURE has claimed a start timeout since M2 as well —
transition (3) lists "the start timeout expired" as a way into `Failed`.
Nothing implemented it.

A second bug the new units found, and an older one: `begin_shutdown` walked
the resolved order and nothing else, so a unit that was running but not
reachable from `default` — started by a connection, or by `oxctl` — was never
asked to stop. `settled` waits for every unit, so the machine sat there until
the ninety-second whole-shutdown deadline and then went down anyway with a
service still holding whatever it held. Nothing had caught it because the only
such unit until now was socket-activated and exited on its own before anyone
asked. Shutdown walks the resolved order reversed, then everything else.

`test-boot` grew an ordering assertion, because "A appears before B" is not
something substring matching can say — and both lines are present whether the
waiting happened or not. The container suite deliberately does not run it:
`docker logs` hands back stdout and stderr as separate streams, so
concatenating them destroys the interleaving, and asserting an order the
harness cannot observe would be asserting nothing.

Verified under QEMU, both architectures, and against a real Alpine userspace:

- `slow` never sends `READY=1`, fails its two-second `start-sec`, and only
  then does `patient` — declared `after = ["slow"]` — start. Asserted as a
  sequence.
- `needy` requires `broken`, which exits non-zero. oxinit refuses to start it
  and says which requirement failed; the boot log is checked for the absence
  of anything `needy` would have printed.
- `oxctl start beta` stops `alpha` first, though `alpha` names `beta` nowhere.

29 checks on each architecture, 31 against the distribution image, 146 host
tests.

## M12 — Continuous verification

**Done.**

Every milestone above ends with a paragraph beginning "Verified". Until this
one, each of those was a claim about something someone had run once, on their
own machine, and nothing re-ran it afterwards.

- [x] `cargo xtask fetch`: download the kernel and the static busybox an
      architecture needs, so the setup is a command rather than a paragraph
      about where Alpine keeps things.
- [x] A workflow that runs `fmt`, `clippy` against both musl targets, the host
      tests, both boot suites, the container suite, and the distribution
      suite.
- [x] Serial logs kept when a boot fails.

Split by cost. The lint and test job is seconds and gates the rest; the four
boot suites run in parallel with one another. aarch64 has no hardware
acceleration on an x86_64 runner, so its boot goes through QEMU's JIT and is
the slowest thing in the set by a wide margin — which is an argument for
running it, not against: it is the configuration least likely to be exercised
by hand.

A boot suite that fails tells you which line was missing from a log you do not
have. So the log is uploaded, and a failure is readable without reproducing
it.

Verified by the thing itself, which is the only way this milestone can be:

| Job                     | Result  | Time  |
|-------------------------|---------|-------|
| fmt, clippy, host tests | success | 0m44s |
| boot (x86_64)           | success | 1m33s |
| boot (aarch64)          | success | 1m42s |
| container               | success | 1m18s |
| a real userspace        | success | 1m31s |

The whole set is under two minutes wall-clock after the gate, which was the
surprise: an emulated ARM64 boot on an x86_64 runner costs nine seconds more
than a native one. The 300-second budget picked in M9 turns out to be an order
of magnitude of headroom rather than a tight fit.

## Beyond M12

Not planned, not designed, listed only so the questions have an answer:

- A `CLOCK_REALTIME` timerfd with `TFD_TIMER_CANCEL_ON_SET`, so a calendar
  schedule survives the clock being stepped under it. Until then a wall-clock
  firing is computed as a monotonic delay and an NTP correction moves it by
  however much the clock moved.
- Local time, which means a timezone database.
- Replacing JSON with `postcard` on the control socket.
- A test suite that boots a real distribution userspace rather than a shell.
