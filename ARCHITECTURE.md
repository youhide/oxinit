# Architecture

This document specifies how oxinit is built. It is written for the person
implementing it, not for someone evaluating it. Where a decision has a
non-obvious reason, the reason is stated.

Nothing here is implemented yet. See [ROADMAP.md](ROADMAP.md) for status.

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

   A mount point that is already mounted is not an error — skip it.
3. **Wire the console.** Open `/dev/console` and `dup2` it onto fds 0, 1, 2.
   Before this, output goes nowhere and debugging early boot is guesswork.
4. **Block all signals** with `sigprocmask(SIG_SETMASK, <full set>)`.
5. **Create the signalfd** for the full set.
6. **Set the hostname** from `/etc/hostname`, defaulting to `localhost`.
7. **Build the event loop** — create the epoll fd, register the signalfd,
   create the control socket, notify socket, and timerfd.
8. **Load units** from the unit directories, build the graph, reject cycles.
9. **Start the default target.**

Steps 1–3 are the whole of milestone 0 alongside a reap loop and a shell.

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

Children get the mask reset between `fork` and `exec`. An inherited full block
mask breaks nearly every daemon.

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
dependency on one pulls in a set of units. The unit named `default` is what boot
starts. Socket units arrive in M4.

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

## cgroup v2

Every service gets a cgroup. This is how oxinit tracks processes, and it is
reliable in ways pid tracking is not: a double-forking daemon cannot escape it.

Hierarchy:

```
/sys/fs/cgroup/oxinit.slice/<name>.service/
```

**Placement.** The pid is written to `cgroup.procs` between `fork` and `exec`,
by the child, before it becomes the service binary. Writing it after `exec`
races with the service forking its own children.

**Limits.** `[resources]` maps directly onto cgroup files:

| Unit key     | cgroup file  |
|--------------|--------------|
| `memory-max` | `memory.max` |
| `tasks-max`  | `pids.max`   |

Absent keys are left at the kernel default (`max`).

**Accounting.** `memory.current` and `pids.current` are read on demand for
`oxctl status`. They are not polled.

**Emptiness.** `cgroup.events` contains a `populated` key that is `1` while any
process remains in the cgroup and `0` when the last one exits. The file
generates an `EPOLLPRI` event when the value changes, so oxinit registers it in
epoll and learns that a service is fully gone without polling.

This is what makes `type = "forking"` tractable. The daemon forks, the parent
exits, and the child is reparented to PID 1 with no relationship oxinit could
have tracked by pid. The cgroup still contains it, and `populated` still reads
`1`. When `populated` flips to `0`, the service is actually gone.

**Killing.** On stop, after `SIGTERM` and the stop timeout, oxinit writes to
`cgroup.kill` (Linux 5.14+). The kernel kills every process in the cgroup
atomically. The alternative — reading `cgroup.procs` and signalling each pid —
races against a process that forks while the list is being walked.

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

## Workspace layout

Not yet created. This is the intended shape:

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
│  ├─ oxinit-ipc/        # control protocol types
│  └─ xtask/             # build and boot automation
├─ docs/
└─ tests/
```

`oxinit-unit`, `oxinit-graph`, and `oxinit-service` have no Linux dependencies
and are testable on any host. That is deliberate: the parser, the graph, and the
restart policy are where the logic errors are, and they should not require a VM
to exercise. The `oxinit` crate refuses to compile off Linux, so anything placed
in it is unreachable from a host test suite.

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

- **Unit tests** in `oxinit-unit` and `oxinit-graph`, run on the host. Parser
  round-trips, invalid-input rejection, cycle detection, ordering correctness.
- **Integration tests** boot QEMU and assert against serial output.
  `cargo xtask test-boot` builds the image, boots it with a timeout, captures
  the serial log, and matches expected lines. A test that hangs fails on the
  timeout rather than blocking forever.
