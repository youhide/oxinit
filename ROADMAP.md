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

## Beyond M7

Not planned, not designed, listed only so the questions have an answer:

- `aarch64-unknown-linux-musl` as a tested target rather than an assumed one.
- Replacing JSON with `postcard` on the control socket.
- Timer units.
- A test suite that boots a real distribution userspace rather than a shell.
