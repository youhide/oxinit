# Architecture

This document specifies how oxinit is built. It is written for the person
implementing it, not for someone evaluating it. Where a decision has a
non-obvious reason, the reason is stated.

See [ROADMAP.md](ROADMAP.md) for what is implemented.

## Process model

PID 1 does supervision and nothing else. It tracks unit state, forks and reaps
children, owns the cgroup hierarchy, holds socket-activation file descriptors,
and sequences shutdown.

Everything else is a separate process talking to PID 1 over a unix socket:

- `oxctl` — the CLI client.
- Log shipping — reads service output, writes it somewhere.
- Device management — uevent handling, if it ever exists.

The reason is blast radius. A bug in a log shipper kills a log shipper. A bug in
PID 1 kills the machine. Code that does not need to run in PID 1 does not run in
PID 1.

## Failure policy

PID 1 cannot exit. If it does, the kernel panics. Everything below follows from
that.

**No panic.** The `oxinit` crate denies the lints that produce them:

```rust
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
```

Errors are values. Fallible operations return `Result`. Error types are defined
with `thiserror` per crate.

**Unwind, never abort.** The release profile sets `panic = "unwind"`. An abort
in PID 1 is a kernel panic — the process dies and the kernel has nothing to fall
back to. Unwinding gives one more chance to recover.

**Catch what escapes.** Each event-loop handler is wrapped in
`std::panic::catch_unwind`. A panic in one handler unwinds to the loop boundary,
gets logged, and the loop continues with the next event. A dispatch bug in
service start must not take down the reaper.

`catch_unwind` is a backstop, not a safety net, and it has one hole worth
knowing: a panic raised while another panic is already unwinding — that is, from
a `Drop` implementation — aborts the process, and no `catch_unwind` can
intercept it. In PID 1 that abort is a kernel panic. So `Drop` implementations
in the `oxinit` crate must not do anything fallible. Close descriptors, free
memory, and nothing else. The lint policy above is what keeps this true, since
the panicking constructs it denies are the ones that would otherwise appear in a
destructor.

**Last resort.** If the event loop itself cannot continue — the epoll fd is
gone, the signalfd is unreadable, the state machine is inconsistent — PID 1
spawns `/bin/sh` on `/dev/console` and keeps reaping. This leaves a machine that
an operator can log into and inspect. It never calls `exit()`.

**The one exception, and it is not an error path.** In a container, the end of
an ordered shutdown *is* exiting — see [Machine or container](#machine-or-container).
Nothing else in the process calls `exit()`, and no failure of any kind reaches
it: a container whose event loop has broken falls back to a shell like every
other machine, because a bug in the supervisor is not a reason to take the
container down.

## Unsafe policy

Syscalls go through [`rustix`](https://docs.rs/rustix), which wraps the raw
interface in safe Rust. Prefer it over `libc` everywhere.

Some things `rustix` does not cover — `fork` semantics between fork and exec,
`signalfd_siginfo` decoding, `clone` flags. That code lives in one module,
`oxinit::sys::raw`, and nowhere else. Every `unsafe` block in it carries a
`// SAFETY:` comment stating the invariant that makes it sound and why the
invariant holds at that call site.

A reviewer should be able to audit all unsafe in the project by reading one
file.

## Concurrency

Synchronous. One thread. One `epoll` loop. No async runtime in PID 1.

An async runtime is a work-stealing scheduler, a reactor, and a timer wheel
running underneath the process that must never fail. It is a large dependency
surface, it complicates `fork` (the child inherits a runtime it must not use),
and it buys nothing: PID 1 is not throughput-bound. It waits on a handful of
file descriptors and reacts.

`oxctl` and other out-of-process components may use whatever they like.

## Boot sequence

This is the milestone 0 path, in order.

1. **Verify `getpid() == 1`.** If not, print an error and exit. Running the
   supervisor as a normal process is a mistake worth catching immediately.
2. **Mount the pseudo-filesystems.** Nothing else works before this:

   | Mount point      | Type       |
   |------------------|------------|
   | `/proc`          | `proc`     |
   | `/sys`           | `sysfs`    |
   | `/dev`           | `devtmpfs` |
   | `/dev/pts`       | `devpts`   |
   | `/dev/shm`       | `tmpfs`    |
   | `/run`           | `tmpfs`    |
   | `/sys/fs/cgroup` | `cgroup2`  |

   A mount point that is already mounted is skipped, and that is asked before
   mounting rather than inferred from the error: mounting over an existing
   mount is `EBUSY` where oxinit has the privilege and `EPERM` where it does
   not, and `EPERM` is also what a genuinely missing filesystem returns. The
   test is two `stat` calls — a directory and its parent share a device unless
   something is mounted between them — which needs no `/proc`, and `/proc` is
   the first entry in the table.

   `EPERM` on the rest is collapsed into a single line. Under a container
   runtime every entry is either already mounted or forbidden, and seven copies
   of "not permitted" describe work that is already done and offer nothing to
   do about it.
3. **Wire the console.** Fill in whichever of fds 0, 1 and 2 is not already
   open, from `/dev/console`. Before this, output goes nowhere and debugging
   early boot is guesswork.

   **What was inherited comes first.** A container runtime execs PID 1 with
   stdio already connected to the pipes it collects logs from, and those images
   have a `/dev/console` too, so redirecting onto it unconditionally sent every
   line oxinit wrote to a device nobody was reading. The test is not "am I in a
   container" but whether the descriptor is open, which is the exact question
   and gives the same answer on a machine: the kernel hands init `/dev/console`
   when the image has one, and this keeps it. It opens the console itself in
   the case the code was written for — an image where the kernel found no
   console, and left the descriptors closed.
4. **Detect the environment.** Machine or container, said once. See below.
5. **Block all signals** with `sigprocmask(SIG_SETMASK, <full set>)`.
6. **Create the signalfd** for the full set.
7. **Set the hostname** from `/etc/hostname`, defaulting to `localhost`.
8. **Build the event loop** — create the epoll fd, register the signalfd,
   create the control socket, notify socket, and timerfd.
9. **Load units** from the unit directories, build the graph, reject cycles.
10. **Start the default target.**

Steps 1–3 are the whole of milestone 0 alongside a reap loop and a shell.

## Machine or container

oxinit is a plausible PID 1 for a container: the job there is reaping orphans,
forwarding signals, and supervising more than one process, which is most of
what it already does. What it needs is to stop assuming it booted a machine.

**Exactly one thing keys off the answer.** On a machine, a shutdown ends in
`reboot(2)`. In a container it ends by exiting, because the machine is not
oxinit's to reboot and the container lives exactly as long as its PID 1 does.
A supervisor that stopped every unit and then stayed running is a container
that will not stop, and `docker stop` ends that with `SIGKILL` and exit 137
ten seconds later.

Everything else decides on its own local evidence. Whether to open
`/dev/console` is answered by whether stdio is already open; whether to mount
`/proc` is answered by whether something is mounted there. Those questions have
exact answers, and "am I in a container" does not: a privileged container has a
console and can mount, and an initramfs may arrive with filesystems already
mounted. Deriving the specific from the general would be wrong in both
directions.

Detection is four checks, first match wins, ordered by how much each proves:

| Evidence                | Says                                            |
|-------------------------|-------------------------------------------------|
| `$container`            | The runtime's own name for itself. systemd's convention, followed by podman, LXC and nspawn. |
| `/run/.containerenv`    | podman.                                          |
| `/.dockerenv`           | Docker.                                          |
| No `CAP_SYS_ADMIN`      | Some runtime, unidentified.                      |

The last is the backstop for a runtime that leaves no marker. PID 1 of a
machine has every capability — the kernel starts it with a full set and nothing
has run yet to take any away — so one missing `CAP_SYS_ADMIN` was started by
something that dropped it. It proves there is a runtime and cannot say which,
which is why it is last, and it is incomplete in the other direction:
`--privileged` keeps the full set, and such a container is indistinguishable
from a machine here. That is what the three checks above it are for.

The exit status is the reply to the runtime: `0` for power off and halt, and
`133` — systemd's convention, honoured by podman — for a reboot oxinit cannot
perform itself. There is no `sync` and no read-only remount on the way out. The
filesystems are the runtime's, oxinit is not permitted to seal them, and
flushing the host's disks from inside a container is not its call.

## Signals

All signals are blocked with `sigprocmask` at startup and read through a
`signalfd` registered in epoll.

This is not a style preference. A signal handler runs at an arbitrary point in
the program and may only call async-signal-safe functions — no allocation, no
locks, no logging, no arbitrary Rust. The usual workaround is a
self-pipe plus a flag, which is a worse `signalfd`. Blocking everything and
reading signals as event-loop input deletes the entire async-signal-safety
problem class. There are no signal handlers in oxinit.

**PID 1 has no default signal dispositions.** The kernel does not apply default
actions to signals sent to PID 1. `SIGTERM` and `SIGINT` do not terminate it;
they do nothing at all unless the process explicitly handles them. This is a
kernel property, not something oxinit implements. It also means a signal oxinit
forgets to handle is silently ignored — the failure mode is an unresponsive
init, not a dead one.

That is why the table is written down rather than left implicit:

| Signal    | Action                                                          |
|-----------|-----------------------------------------------------------------|
| `SIGCHLD` | Reap, and apply each unit's restart policy.                      |
| `SIGTERM` | Ordered shutdown, then power off.                                |
| `SIGINT`  | Ordered shutdown, then reboot.                                   |
| `SIGPWR`  | Ordered shutdown, then power off.                                |
| `SIGUSR1` | Ordered shutdown, then halt without cutting power.               |
| `SIGUSR2` | Report every unit's state. Changes nothing.                      |

`SIGTERM` means power off because that is what a container runtime means by
it, and it is the whole of the contract `docker stop` and a Kubernetes pod
deletion rely on. On a machine it is not a signal anyone sends to PID 1 by
accident. `SIGINT` is what the kernel delivers for Ctrl-Alt-Del.

## Shutdown

Shutdown is a **state the supervisor is in**, not a function that runs to
completion.

Stopping a unit means signalling it and waiting — for its cgroup to empty, or
for its stop timeout — and both of those arrive as events on the same epoll
loop as everything else. A blocking shutdown routine would have to
re-implement the reaper, the timers, and the cgroup notifications it is
waiting on, during the one part of the boot where getting it wrong strands the
machine. So instead every unit is asked to stop, the ordinary loop keeps
running, and each event is also a chance to notice that the last unit has
gone.

Units are stopped in the reverse of the order they started, so a service goes
down before whatever it was ordered after.

A target or a socket unit runs no process, so there is nothing to signal and
nothing to wait for: it stops the moment it is asked to. Sending it through
the ordinary path would leave it `Deactivating` forever, waiting on an event
only a process can produce — and a shutdown waiting on that never finishes.

A unit waiting out a restart backoff is likewise not running and not coming
back, so its pending restart is abandoned rather than waited on.

There is a whole-shutdown deadline on top of each unit's `stop-sec`. It is not
the normal mechanism — it is the backstop for a unit whose stop path is itself
broken. A machine that will not power off is worse than one that loses a
service's last write.

Then `sync`, remount `/` read-only, and `reboot(2)` with the matching command.
The order is the whole of it: an unflushed filesystem is what makes a reboot
lose data, and remounting read-only afterwards stops anything still holding a
write from making more. The remount is best-effort — an initramfs root cannot
be remounted read-only, and failing there would strand a machine that was
about to go down cleanly.

In a container none of that last paragraph happens: the shutdown ends by
exiting, with a status that tells the runtime which way. See
[Machine or container](#machine-or-container).

If the kernel refuses to reboot a machine, oxinit stays up with everything
stopped and says so. It does not exit: PID 1 exiting is a kernel panic.

Children get the mask reset between `fork` and `exec`. An inherited full block
mask breaks nearly every daemon, and it breaks the service manager too: a
service that starts with `SIGTERM` blocked cannot be stopped, only killed.

oxinit does this reset itself, in its own `pre_exec` hook, rather than leaving
it to the standard library. `Command` makes no documented promise about the
child's signal mask; the implementation that happens to clear it does so
before `pre_exec` runs, where nothing oxinit writes can observe whether it
happened. A requirement this load-bearing does not rest on an implementation
detail.

## Reaping

PID 1 inherits every orphaned process on the machine and must reap them, or the
process table fills with zombies.

`SIGCHLD` does not queue. If four children exit while the signal is blocked, the
signalfd reports **one** `SIGCHLD`. Reaping one child per signal leaks the rest.

On each `SIGCHLD`:

```
loop waitpid(-1, WNOHANG)
    > 0     -> a child exited; dispatch to its unit, continue
    == 0    -> children remain but none have exited; stop
    ECHILD  -> no children at all; stop
```

The pid may belong to no unit — an orphan that was reparented here. That is
normal. Reap it and move on.

## Event loop

One epoll fd. The registered descriptors:

**signalfd** — all signals. Readable means one or more signals are pending.

**Control socket** — `SOCK_SEQPACKET` unix socket at
`/run/oxinit/control.sock`, mode `0600`. `SOCK_SEQPACKET` preserves message
boundaries and is connection-oriented, so a request is exactly one `recv` and
there is no framing layer, no length prefix, and no partial-message state to get
wrong. `SOCK_STREAM` would require all three; `SOCK_DGRAM` would drop the
connection semantics needed to reply.

**Timerfd** — exactly one, paired with a `BinaryHeap<Deadline>`. Restart
backoff, start timeouts, and watchdog intervals all become entries in the heap.
The timerfd is armed for the earliest deadline. When it fires, every expired
entry is popped and dispatched, then it is rearmed. One fd per timer would mean
hundreds of descriptors and an epoll registration on every restart.

**Notify socket** — `SOCK_DGRAM` unix socket at `/run/oxinit/notify`. Services
with `type = "notify"` send readiness and status here. See below.

**cgroup.events** — one per running service cgroup, registered with `EPOLLPRI`.
Kernel notification that the `populated` key changed. See cgroup v2 below.

**Listening sockets** — one per address a socket unit binds, registered with
`EPOLLIN` while its service is down. Readable means a connection is waiting.
oxinit does not accept it; it starts the service, which does. See socket
activation above.

epoll hands back one `u64` per registration and nothing else, so the token has
to carry both what kind of descriptor woke the loop and which one: the high
half is the kind, the low half is the id. A flat numbering with reserved ranges
would work too, right up until one range outgrew its reservation and silently
aliased another.

## sd_notify and socket activation

oxinit implements two protocols that originated in systemd:

**`sd_notify`** — the service inherits `NOTIFY_SOCKET` in its environment,
pointing at `/run/oxinit/notify`, and sends newline-separated `KEY=value`
datagrams. Supported keys:

| Key          | Meaning                                                   |
|--------------|-----------------------------------------------------------|
| `READY=1`    | Startup complete. Transitions `Activating` → `Active`.    |
| `STATUS=`    | Free-text status string, shown by `oxctl status`.         |
| `WATCHDOG=1` | Keepalive ping. Resets the watchdog deadline.             |

Unknown keys are ignored.

The sender is validated with `SCM_CREDENTIALS`. The notify socket has
`SO_PASSCRED` set, and datagrams are read with `recvmsg`; the kernel attaches an
ancillary `SCM_CREDENTIALS` message containing the sender's pid, uid, and gid,
which the sender cannot forge. A datagram with no credentials attached, or whose
pid is not in a known service cgroup, is dropped.

`SO_PEERCRED` does **not** work here. It reports the credentials of the peer
that called `connect()` or `socketpair()`, and services send with `sendto()` on
an unconnected `SOCK_DGRAM` socket, so there is no peer to report. Reading
`SO_PEERCRED` on the notify socket returns the credentials of oxinit itself.

**Socket activation** — oxinit creates and binds the listening sockets, then
passes them to the service as inherited descriptors starting at fd 3, with
`LISTEN_FDS` set to the count, `LISTEN_PID` set to the service's pid, and
`LISTEN_FDNAMES` set to a colon-separated list of names. A service must check
that `LISTEN_PID` matches its own pid before trusting `LISTEN_FDS`.

The descriptors belong to oxinit and are never closed. That is what the
feature buys:

- A unit ordered after a socket can rely on the address being **bound**, which
  is a fact. "The service started" is a guess.
- A restart does not refuse connections. The listening socket outlives the
  process that was serving them, so they queue in the kernel.
- The service need not run until something wants it.

**`LISTEN_PID` is why oxinit builds the child's exec image itself.** Its value
is the pid of the process the descriptors were passed to, and that pid does not
exist until after the `fork` — at which point nothing may allocate. So the
argv and environment are laid out in the parent with a fixed-width gap where
the digits go, the child writes them in, and the child execs that image
directly from the `pre_exec` hook. `Command`'s own environment is not used at
all; oxinit decides exactly what a service sees. See `sys::raw::Image`.

`LISTEN_FDS`, `LISTEN_PID`, `LISTEN_FDNAMES`, `NOTIFY_SOCKET` and
`WATCHDOG_USEC` are never inherited from oxinit's own environment. A service
sees the values oxinit set for it, or nothing — a stale `LISTEN_FDS` would
point it at a descriptor table it does not own.

**Watching a socket is conditional.** A registered listening descriptor stays
readable until someone accepts the connection, and oxinit never accepts
anything, so a descriptor left registered after the service starts would spin
the loop at full speed. It is therefore registered exactly while its service is
`Inactive` or `Failed`, and deregistered — not closed — otherwise.

That is done as one reconciliation pass at the end of every loop iteration
rather than as an arm/disarm call in each of the dozen places a service can
change state. It is idempotent, it cannot be forgotten by a path added later,
and the cost of getting it wrong is either a service that can never be
activated again or an event loop at 100%.

**Each `listen` entry means the address it names.** IPv6 sockets get
`IPV6_V6ONLY`, so `[::]` does not also claim every IPv4 address — without it a
unit listing both `0.0.0.0:22` and `[::]:22` fails its second bind with
`EADDRINUSE` for reasons nothing in the configuration explains. TCP sockets get
`SO_REUSEADDR`, or a restart is refused for as long as the kernel holds the old
socket in `TIME_WAIT`, which is exactly the window a service manager restarts
things in.

**A socket unit cannot share a name with its service.** Unit names are unique
across kinds, which is what lets `requires`, `after`, and `oxctl` take a bare
name; so the socket names its service with a required `service` key rather than
by convention. A socket does not pull its service in, either — a service
reachable from `default` starts at boot whether anything connects or not, which
is the opposite of what activation is for.

These are implemented deliberately, and the distinction from unit-file
compatibility matters:

- `sd_notify` and `LISTEN_FDS` are **runtime protocols**. They are a stable,
  small, documented wire contract that thousands of daemons already speak.
  Implementing them costs a few hundred lines and makes existing software work
  unmodified.
- systemd `.service` files are a **configuration format** defined by an
  implementation, not a specification. Supporting a subset means inheriting
  semantics that are only pinned down by systemd's source, and the subset is
  never the one a given distribution actually uses. oxinit does not do this.
  See the non-goals in [README.md](README.md).

## Units and dependencies

Units are TOML files. The full key specification is in
[docs/UNIT_FORMAT.md](docs/UNIT_FORMAT.md).

A unit is a service, a target, or a socket, determined by which section it
carries. Targets run no process; they exist to be depended on, so that a
dependency on one pulls in a set of units. Socket units run no process either;
they own listening descriptors and name the service those belong to. The unit
named `default` is what boot starts.

**Requirement and ordering are orthogonal.** This is the one design idea worth
taking from systemd unchanged. "A needs B to be running" and "A must start after
B" are different statements, and conflating them makes both impossible to
express precisely.

| Key         | Meaning                                                                  |
|-------------|--------------------------------------------------------------------------|
| `requires`  | Hard dependency. If the dep fails to start, this unit does not start. Implies **no** ordering. |
| `wants`     | Weak dependency. The dep is started; this unit proceeds regardless of the result. |
| `after`     | Ordering only. Start after the named unit has finished activating. Creates no dependency. |
| `before`    | Ordering only. The inverse of `after`.                                   |
| `conflicts` | Mutual exclusion. Starting this unit stops the named unit, and vice versa. |

A unit that needs both — the common case — declares both:

```toml
requires = ["network-online"]
after    = ["network-online"]
```

`requires` without `after` starts both units with no ordering guarantee. That
is a legitimate configuration, and it is what you get if you only write
`requires`. The parser does not add ordering implicitly.

### Graph

Ordering edges form a DAG. Resolution is Kahn's algorithm:

1. Compute in-degree for every node from the `after`/`before` edges.
2. Push all zero-in-degree nodes into the ready set.
3. Pop, emit, decrement the in-degree of each successor, push any that reach
   zero.
4. If the emitted count is less than the node count, a cycle exists.

A cycle is a **load-time refusal**. oxinit does not start with a broken graph
and does not silently break the cycle by dropping an edge. It reports the cycle
path — `a → b → c → a` — and refuses to load. Recovering the path means walking
the remaining non-zero-in-degree subgraph with a DFS after Kahn terminates.

`requires` and `conflicts` edges do not participate in the topological sort.
They are evaluated at start time against the state machine.

## Service state machine

```
  Inactive ──(1)──> Activating ──(2)──> Active ──(4)──> Deactivating ──(5)──> Inactive
                        │                  │                  │
                       (3)                (6)                (7)
                        v                  v                  v
                     Failed             Failed             Failed

  Activating ──(8)──> Inactive                    oneshot completed, status 0

  Failed ──(9)──> Restarting ──(10)──> Activating
  Inactive ──(9)──> Restarting
```

Every edge:

| #    | From         | To           | Trigger                                                        |
|------|--------------|--------------|----------------------------------------------------------------|
| (1)  | Inactive     | Activating   | Start requested, and all `requires` and `after` are satisfied.  |
| (2)  | Activating   | Active       | The readiness condition for `type` is met.                      |
| (3)  | Activating   | Failed       | `fork`/`exec` failed, a `requires` dep failed, the process exited before readiness, or the start timeout expired. |
| (4)  | Active       | Deactivating | Stop requested, by `oxctl`, by a `conflicts`, or by shutdown.    |
| (5)  | Deactivating | Inactive     | The cgroup emptied within the stop timeout.                     |
| (6)  | Active       | Failed       | The main process exited nonzero or on a signal, or the watchdog deadline was missed. |
| (7)  | Deactivating | Failed       | The stop timeout expired and `cgroup.kill` was used.            |
| (8)  | Activating   | Inactive     | A `oneshot` unit's process exited with status 0. It does **not** become `Active`. |
| (9)  | Failed, Inactive | Restarting | The `restart` policy applies to how the unit stopped.         |
| (10) | Restarting   | Activating   | The backoff delay elapsed.                                      |

The states:

- **Inactive** — not running. The initial state.
- **Activating** — forked; waiting for the readiness condition determined by
  `type`. `simple` is ready immediately after a successful `exec`; `forking`
  when the parent exits and the cgroup is still populated; `oneshot` when the
  process exits successfully; `notify` when `READY=1` arrives.
- **Active** — running and ready.
- **Deactivating** — stop requested; `SIGTERM` sent, waiting for exit or for
  the stop timeout to expire, after which `cgroup.kill` is used.
- **Inactive** (again) — clean stop.
- **Failed** — start failed, the main process exited nonzero, the start timeout
  expired, or the watchdog was missed.
- **Restarting** — a transient state entered from `Failed` or `Inactive` when
  the `restart` policy applies. The unit sits here for `restart-sec`, scaled by
  exponential backoff, then transitions to `Activating`.

Backoff is per-unit and resets after the unit has been `Active` for longer than
the current backoff interval. Without the reset, a service that crashes once a
day eventually takes hours to come back.

**Why a unit is stopping is part of its state.** Transition (5) and transition
(9) are both reached by the cgroup emptying, and which one applies depends on
what asked for the stop:

| Cause     | On the cgroup emptying                                        |
|-----------|---------------------------------------------------------------|
| Requested | `Inactive`. An explicit stop overrides the `restart` policy.   |
| Watchdog  | The `restart` policy applies, exactly as it would to a crash.  |

That asymmetry is the point of a watchdog. It exists to turn a hang — which
nothing else detects — into a failure the restart policy can act on, so it
must not take the same path as `oxctl stop`.

Either way, a unit that had to be killed with `cgroup.kill` ends `Failed`
rather than `Inactive`, even when the stop was requested. It did not stop when
it was asked to, and that is worth reporting.

A `forking` service reaches `Active` with no process oxinit forked left to
watch, so from that point its cgroup is the only thing that knows whether it
is running. When that cgroup empties, the unit is gone — reported as an
abnormal end, because that is all that can honestly be said: the process that
exited was never a child of oxinit's, and there is no wait status anywhere to
read.

## cgroup v2

Every service gets a cgroup. This is how oxinit tracks processes, and it is
reliable in ways pid tracking is not: a double-forking daemon cannot escape it.

Hierarchy:

```
/sys/fs/cgroup/oxinit.slice/<name>.service/
```

**Delegation.** A controller is only usable in a cgroup if the parent enabled
it in `cgroup.subtree_control`, so `+memory +pids` is written twice on the way
down: once at the root, once on `oxinit.slice`. Only controllers the kernel
actually reports in `cgroup.controllers` are enabled — a kernel built without
the memory controller still boots, and the units that asked for `memory-max`
are the only thing that fails.

The root write is legal because the root cgroup is the one cgroup exempt from
the "no internal processes" rule, and PID 1 lives there. The slice write is
legal because the slice holds only child cgroups and never a process.

**Placement.** The pid is written to `cgroup.procs` between `fork` and `exec`,
by the child, before it becomes the service binary. Writing it after `exec`
races with the service forking its own children.

The value written is `0`, which the kernel reads as "the process doing the
writing". It is the only value that cannot already be stale by the time the
kernel parses it.

**What the child does between fork and exec.** Four things, in this order, and
the order is the design:

1. Reset the signal mask. Unconditional; see [Signals](#signals).
2. `setsid` and `TIOCSCTTY`, for a unit that declared `tty`.
3. Write `cgroup.procs`.
4. `setgroups`, `setgid`, `setuid`.

Step 3 has to come before step 4, because writing `cgroup.procs` needs exactly
the privilege step 4 drops. Within step 4, `setuid` last for the same reason —
reversing it drops the privilege the group calls need, and does so silently.

Step 2 is opt-in per unit and best-effort. A controlling terminal belongs to
one session, so inferring it from stdin happening to be a terminal would have
every service on a machine that boots to a console take it from the last one.
Its failure does not fail the unit: a shell without job control is a nuisance,
and a console that would not start is a machine with no way in. The fallback
console shell in `shell.rs` is not a unit and asks for it whenever stdin is a
terminal, because there is nothing else it could be competing with.

This is also why `Command::uid`/`Command::gid` are not used: they run before
`pre_exec`, which is the wrong side of step 2, and they pass only the primary
group to `setgroups`.

Everything the hook needs is resolved in the parent, before the fork. The
username is looked up against `/etc/passwd` and `/etc/group` there, and the
`cgroup.procs` descriptor is opened there, because the hook runs in a freshly
forked process where only async-signal-safe calls are legal — no allocation,
no opening files, no name lookup. That also means a unit naming a user who
does not exist fails with an error against that unit, rather than failing
somewhere unreportable after the fork.

The lookup parses `/etc/passwd` directly rather than calling `getpwnam`, which
resolves through NSS: `dlopen`, arbitrary third-party code, and a lookup that
can block on a network, none of which belongs in PID 1.

**Limits.** `[resources]` maps directly onto cgroup files:

| Unit key     | cgroup file  |
|--------------|--------------|
| `memory-max` | `memory.max` |
| `tasks-max`  | `pids.max`   |

An absent key is left alone rather than written as `max`, so a limit an
operator set out of band is not silently reset on every restart of a unit that
declares no limit of its own.

A declared limit that cannot be applied fails the unit. It is not a degraded
mode: the unit asked to be capped, and running it uncapped is not a smaller
version of that. This is the same stance the parser takes when it refuses a
typo in `[resources]` rather than reading it as "no limit".

**Accounting.** `memory.current` and `pids.current` are read on demand for
`oxctl status`. They are not polled.

**Emptiness.** `cgroup.events` contains a `populated` key that is `1` while any
process remains in the cgroup and `0` when the last one exits. The file
generates an `EPOLLPRI` event when the value changes, so oxinit registers it in
epoll and learns that a service is fully gone without polling.

`EPOLLPRI` and not `EPOLLIN`: the file is always readable, so `EPOLLIN` on it
would spin the loop at full speed. And the read that follows the notification
has to go through the *same descriptor* that was registered — kernfs clears
the condition per open file, so reading a freshly opened one would leave
`EPOLLPRI` asserted on the registered one forever. Every service cgroup is
therefore created before anything starts, and its `cgroup.events` descriptor
is opened once, registered once, and held for the life of the process. A
restart reuses it rather than churning the hierarchy and the registration.

This is what makes `type = "forking"` tractable. The daemon forks, the parent
exits, and the child is reparented to PID 1 with no relationship oxinit could
have tracked by pid. The cgroup still contains it, and `populated` still reads
`1`. When `populated` flips to `0`, the service is actually gone.

**Killing.** A stop is `SIGTERM` to every process in the cgroup, then
`cgroup.kill` once `stop-sec` elapses.

The polite half does read `cgroup.procs` and signal each pid, and that read is
racy — a process may fork between the read and the signal. That is tolerable
for `SIGTERM`, where the point is to ask. It is not tolerable for the kill,
which is exactly why the escalation is `cgroup.kill` (Linux 5.14+) rather than
more of the same: the kernel kills every process in the cgroup atomically,
with nothing to race.

A unit is `Deactivating` until its cgroup empties — not until its main process
exits, because a service that forked children is not stopped while they are
still running.

## IPC

`oxctl` connects to `/run/oxinit/control.sock` and exchanges request/response
messages. Types live in the `oxinit-ipc` crate, shared by both sides.

Serialization is JSON initially. It is inspectable with `socat` during
development, which matters more than bytes on the wire for a socket that carries
a few messages per minute. `postcard` is the likely replacement once the message
set stops changing; the crate boundary is placed so this is a one-file change.

Authorization is the socket's mode. `0600`, owned by root, in a root-owned
directory. There is no in-protocol authentication and no per-command
permissions. Access to the socket is full control of the machine.

**Nothing blocks.** A client connection is registered with epoll like every
other descriptor, so a client that connects and then says nothing costs a slot
and no time at all. One request per connection: the reply goes back and the
connection closes, which is why nothing has to remember anything between
messages.

There is a cap on concurrent clients. A client that never sends holds its slot
until it goes away, and that is bounded and only reachable by someone who can
already open this socket — which is to say someone who already has full
control. A timer to evict them would be machinery guarding nothing.

**Every answer is immediate, and says what was asked for rather than what has
finished.** A stop is not over when the reply goes out; it ends when the unit's
cgroup empties. Holding the connection open until then would make the one
socket that has to stay responsive wait on the slowest thing on the machine.

**`reload` applies to units that are not running.** A unit that is up keeps the
definition it was started with until it stops — rewriting it underneath a
running process would describe something that is not what is running. A socket
unit is not re-bound either: its descriptors were bound and registered with
epoll once, at boot.

## Workspace layout

```
oxinit/
├─ Cargo.toml            # workspace
├─ crates/
│  ├─ oxinit/            # PID 1 binary
│  ├─ oxctl/             # CLI client
│  ├─ oxinit-unit/       # unit parsing + validation
│  ├─ oxinit-graph/      # dependency resolution
│  ├─ oxinit-service/    # service state machine, restart policy
│  ├─ oxinit-cgroup/     # cgroup v2
│  ├─ oxinit-user/       # /etc/passwd and /etc/group
│  ├─ oxinit-ipc/        # control protocol types
│  ├─ notify-probe/      # test fixture: a service that speaks sd_notify
│  ├─ listen-probe/      # test fixture: a socket-activated service
│  └─ xtask/             # build and boot automation
├─ docs/
└─ tests/
```

`oxctl` and `oxinit-ipc` arrive in M5. Everything else exists.

Every library crate is testable on any host, and that is the point of the
split. The `oxinit` crate refuses to compile off Linux, so anything placed in
it is unreachable from a host test suite — which means anything that can be
tested without a kernel should not be in it.

- `oxinit-unit`, `oxinit-graph`, `oxinit-service` have no Linux dependencies at
  all. The parser, the graph, and the restart policy are where the logic errors
  are.
- `oxinit-cgroup` is `std::fs` and nothing more, because a cgroup is a
  directory of small text files. The layout, every value written to a limit
  file, and every value parsed back out are exercised against a temporary
  directory. What genuinely needs a kernel — the `EPOLLPRI` registration, the
  `pre_exec` write — is what stayed behind.
- `oxinit-user` is text parsing. Getting a uid wrong is a privilege bug, so the
  parsing is separated from the `setuid` that consumes it and tested on its
  own.

## Development loop

`cargo xtask boot`:

1. Builds `oxinit` for `x86_64-unknown-linux-musl`, statically linked.
2. Packs a cpio initramfs — the binary at `/init`, and nothing else unless
   `--shell PATH` adds a statically linked shell at `/bin/sh`. Without one the
   image boots and exercises the mount, console, signalfd, and reap paths, but
   oxinit has no shell to supervise and logs a spawn failure.
3. Boots it:

```bash
qemu-system-x86_64 -kernel bzImage -initrd oxinit.cpio.gz -nographic -append "console=ttyS0"
```

Edit to boot is a few seconds. `-nographic` puts the guest's serial console on
the terminal, which is why the boot sequence wires stdio to `/dev/console`.
`Ctrl-A X` exits QEMU.

## Platform support

- **MSRV** — stable minus two releases. No nightly features.
- **Targets** — `x86_64-unknown-linux-musl` and `aarch64-unknown-linux-musl`.
- **Kernel** — 5.14 or later, for `cgroup.kill`. cgroup v2 must be available;
  the legacy v1 hierarchy is not supported.

## Testing

- **Unit tests** in every library crate, run on the host. Parser round-trips,
  invalid-input rejection, cycle detection, ordering correctness, restart and
  stop policy, cgroup layout and file formats, user resolution.
- **Integration tests** boot QEMU and assert against serial output.
  `cargo xtask test-boot` builds the image, boots it with a timeout, captures
  the serial log, and matches expected lines. A test that hangs fails on the
  timeout rather than blocking forever.
