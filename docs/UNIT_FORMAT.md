# Unit file format

Units are TOML files. This document specifies every key: its type, whether it is
required, its default, and what happens when it is absent.

Nothing here is implemented yet. This is the specification the parser in
`oxinit-unit` is written against.

## File naming and location

A unit's name is its filename without the `.toml` extension. A file named
`sshd.toml` defines the unit `sshd`, referenced as `sshd` in `requires`,
`after`, and on the `oxctl` command line.

Names must match `[A-Za-z0-9_.-]+`. A name containing `/` or path components is
rejected at load time.

### Unit kinds

A unit is one of three kinds, determined by which section it carries. Exactly
one of these must be present:

| Section     | Kind    | Status                                              |
|-------------|---------|-----------------------------------------------------|
| `[service]` | service | Specified below.                                    |
| `[target]`  | target  | A named group of dependencies. Runs no process.     |
| `[socket]`  | socket  | Specified below. Listens, and starts a service.     |
| `[timer]`   | timer   | Specified below. Waits, and starts a service.       |

Names are unique across kinds: there cannot be both a `sshd` service and a
`sshd` target. That is what lets `requires`, `after`, and `oxctl` take a bare
name with no suffix and no ambiguity.

This is why a socket unit names its service explicitly rather than sharing a
name with it. `sshd-socket` activating `sshd` is two units with two names; the
alternative would mean `requires = ["sshd"]` no longer identifies one unit.

A unit's **fully-qualified name** is `<name>.<kind>` — `sshd.service`,
`multi-user.target`. It appears in status output, in cgroup paths, and in the
`%N` specifier. It is never required as input.

`[target]` takes no keys. A target exists only to be depended on: it groups
units so that `requires = ["multi-user"]` pulls in a set rather than a list.
Targets have no process, no cgroup, and no `[resources]`.

A target is **not** implicitly ordered after the units it pulls in. The rule
that `requires` creates no ordering has no exception for targets, so a target
that lists ten services is reached at whatever point the ordering constraints
allow — possibly before any of them have started. If you want a target to mean
"all of this is up", order it explicitly:

```toml
[target]

[unit]
requires = ["network", "sshd"]
after    = ["network", "sshd"]
```

Without the `after`, the target is a label on a set, not a milestone.

The unit named `default` is started at the end of boot. It is normally a target.
Every service that should run on this machine is reachable from it, directly or
transitively. A machine with no `default` unit boots to an idle PID 1.

### Directory precedence

Units are loaded from two directories, in this order:

| Directory                | Purpose                          |
|--------------------------|----------------------------------|
| `/usr/lib/oxinit/units/` | Units shipped by packages.       |
| `/etc/oxinit/units/`     | Units installed by the operator. |

A file in `/etc` **replaces** the file of the same name in `/usr/lib`
completely. There is no per-key merging and no drop-in directory. To change one
key of a packaged unit, copy the whole file to `/etc` and edit it.

This is a deliberate restriction. Merged configuration means the effective unit
is not any file you can read; it is the result of an algorithm applied to
several. Full replacement means `cat` shows you what is running.

An empty file in `/etc` does not mask the packaged unit — it is a unit with no
kind section, which is a load error. To disable a unit, remove it from the
dependencies of the target that pulls it in.

## Example

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
stop-sec    = "20s"
user        = "root"

[resources]
memory-max = "256M"
tasks-max  = 512
```

Note that this unit declares both `requires` and `after` on `network-online`.
They mean different things and neither implies the other. See
[Dependencies](#dependencies).

## Sections

| Section       | Required                    | Contents                       |
|---------------|-----------------------------|--------------------------------|
| `[unit]`      | No                          | Identity and dependencies.     |
| `[service]`   | Exactly one kind section    | How to run the process.        |
| `[target]`    | Exactly one kind section    | Nothing. Marks the unit a target. |
| `[socket]`    | Exactly one kind section    | What to listen on, and what to start. |
| `[timer]`     | Exactly one kind section    | When to fire, and what to start. |
| `[resources]` | No; service units only      | cgroup limits.                 |

An unknown section is a load error, not a warning. So is an unknown key. A
typo in a resource limit must not silently mean "no limit".

`[resources]` on a target unit is a load error rather than a no-op, for the same
reason.

## `[unit]`

### `description`

- **Type:** string
- **Required:** no
- **Default:** the unit name

Human-readable label shown by `oxctl status` and in log messages. Has no effect
on behaviour.

### `requires`

- **Type:** array of strings
- **Required:** no
- **Default:** `[]`

Hard dependencies. Each named unit is started when this unit starts. If any of
them fails to reach `Active`, this unit does not start and enters `Failed`.

**`requires` does not imply ordering.** Without a matching `after`, both units
start concurrently and this unit may reach `Active` before its dependency does.
If you need the dependency running first, declare `after` as well.

A `requires` on a unit that does not exist is a load error.

### `wants`

- **Type:** array of strings
- **Required:** no
- **Default:** `[]`

Weak dependencies. Each named unit is started when this unit starts, but the
result is ignored: if the wanted unit fails, this unit starts anyway.

Like `requires`, this implies no ordering.

A `wants` on a unit that does not exist is a warning, not an error. This is the
one asymmetry with `requires`, and it exists so that optional units can be
listed without requiring them to be installed.

### `after`

- **Type:** array of strings
- **Required:** no
- **Default:** `[]`

Ordering only. This unit is not started until every named unit has finished
activating — reached `Active`, or `Failed`, or completed in the case of
`oneshot`.

`after` creates **no dependency**. If the named unit is not being started at
all, `after` has no effect and imposes no delay. It orders units that are
already going to start; it does not cause them to start.

### `before`

- **Type:** array of strings
- **Required:** no
- **Default:** `[]`

The inverse of `after`. `a` declaring `before = ["b"]` is equivalent to `b`
declaring `after = ["a"]`. Provided so a unit can order itself relative to units
it should not have to modify.

### `conflicts`

- **Type:** array of strings
- **Required:** no
- **Default:** `[]`

Mutual exclusion. Starting this unit stops every named unit first, and starting
a named unit stops this one. The relationship is symmetric even if only one side
declares it.

Declaring `conflicts` and `requires` on the same unit is a load error.

## Dependencies

Requirement and ordering are separate axes. The four combinations:

| Declaration                                | Effect                                                              |
|--------------------------------------------|---------------------------------------------------------------------|
| `requires = ["b"]`                         | `b` is started. If it fails, this unit fails. No ordering guarantee. |
| `after = ["b"]`                            | If `b` is starting anyway, wait for it. `b` is not started by this.  |
| `requires = ["b"]` and `after = ["b"]`     | `b` is started, must succeed, and must finish first. The common case. |
| Neither                                    | No relationship.                                                     |

The dependency graph is checked at load time. A cycle in the `after`/`before`
edges is a load error reporting the full cycle path; oxinit refuses to start
rather than breaking the cycle by dropping an edge.

## `[service]`

### `type`

- **Type:** string, one of `simple`, `forking`, `oneshot`, `notify`
- **Required:** no
- **Default:** `simple`

Determines when the unit is considered ready — that is, when it moves from
`Activating` to `Active`.

| Value     | Ready when                                                                     |
|-----------|--------------------------------------------------------------------------------|
| `simple`  | `exec` succeeds. The process is assumed ready immediately.                      |
| `forking` | The initial process exits and the service cgroup is still populated.            |
| `oneshot` | The process exits with status 0. The unit then goes to `Inactive`, not `Active`. |
| `notify`  | The process sends `READY=1` to `NOTIFY_SOCKET`.                                 |

`simple` is the least accurate: a unit ordered `after` a `simple` service may
start before that service can accept connections. Use `notify` where the daemon
supports it.

`forking` requires cgroup tracking, since the process that oxinit forked is not
the process that ends up running. See the cgroup notes in
[ARCHITECTURE.md](../ARCHITECTURE.md).

`oneshot` is for units that do work and exit — mounting a filesystem, loading a
module. A `oneshot` unit that exits nonzero enters `Failed`. Other units may be
ordered `after` it and will wait for it to complete.

### `exec`

- **Type:** string
- **Required:** **yes**
- **Default:** none

The command to run. Absent, the unit fails to load.

The string is split on whitespace into an argv array. It is **not** passed to a
shell. There is no globbing, no variable expansion, no pipes, no redirection,
no `&&`. Quoting with `"` or `'` groups an argument containing spaces; a
backslash escapes the next character.

The first element must be an absolute path. A command that needs shell features
should be a script on disk, invoked as `/usr/bin/sh /path/to/script`.

Specifiers are expanded before splitting. See [Specifiers](#specifiers).

### `restart`

- **Type:** string, one of `no`, `always`, `on-failure`, `on-abnormal`
- **Required:** no
- **Default:** `no`

When to restart the service after it stops.

| Value         | Restarts on                                                             |
|---------------|-------------------------------------------------------------------------|
| `no`          | Never.                                                                  |
| `always`      | Any exit, clean or not, including a clean exit with status 0.            |
| `on-failure`  | Nonzero exit status, termination by signal, timeout, or watchdog miss.   |
| `on-abnormal` | Termination by signal, timeout, or watchdog miss. Not a nonzero exit.    |

`on-abnormal` is for services where a nonzero exit is a deliberate, meaningful
failure that should not be retried, but a crash should be.

A restart requested by `oxctl stop` never happens. An explicit stop stops the
unit regardless of policy.

### `restart-sec`

- **Type:** duration string
- **Required:** no
- **Default:** `"100ms"`

Base delay before restarting. Ignored when `restart = "no"`.

This is the base for exponential backoff, not a fixed interval. Each consecutive
restart doubles the delay, capped at 5 minutes. The backoff resets once the
service has been `Active` for longer than the current delay — without that, a
service that fails once a week eventually takes hours to come back.

### `stop-sec`

- **Type:** duration string
- **Required:** no
- **Default:** `"30s"`

How long the service has to stop after being sent `SIGTERM`, before oxinit
writes `cgroup.kill` and the kernel kills everything in its cgroup at once.

The unit is `Deactivating` for this window. It leaves it when the cgroup
empties — not when the main process exits, because a service that forked
children is not stopped until they are gone too.

A unit that had to be killed this way ends in `Failed` rather than `Inactive`,
even when the stop was requested. It did not stop when it was asked to, and
that is worth reporting.

### `watchdog-sec`

- **Type:** duration string
- **Required:** no
- **Default:** none — the watchdog is disabled

How long the service may go without sending `WATCHDOG=1` before oxinit
considers it hung. The service is told the interval through `WATCHDOG_USEC` in
its environment, and is expected to ping at roughly half that.

Missing the deadline puts the unit in `Failed`, which means the `restart`
policy applies exactly as it would to a crash — the point of a watchdog is to
turn a hang, which nothing else detects, into a failure that does.

Only meaningful with `type = "notify"`: nothing else has a channel to ping on.
Setting it on another type is a load error.

### `user`

- **Type:** string
- **Required:** no
- **Default:** `"root"`

Username to run the service as. Resolved to a uid and gid against `/etc/passwd`,
and to a supplementary group set against `/etc/group`, at start time rather than
at load time — so a unit may reference a user that an earlier service in the
boot creates.

`setgroups` and `setgid` are called before `setuid`. Reversing that order would
drop the privilege needed to change groups, and would do it silently: the
process keeps the groups it was supposed to shed.

The supplementary set includes the primary group, as `initgroups` does. Dropping
it would quietly give the service less than a login shell for the same account
has.

An unresolvable username at start time fails the unit.

### `tty`

- **Type:** boolean
- **Required:** no
- **Default:** `false`

Start the service in its own session, with its standard input as that
session's controlling terminal. `setsid` then `TIOCSCTTY`, in the child,
between `fork` and `exec`.

For a login shell or a getty, and for nothing else. Without it a shell on the
console reports "can't access tty; job control turned off": Ctrl-C reaches
nothing, `fg` and `bg` do not work, and every program started from that shell
inherits the same.

**Declare it on exactly one unit per terminal.** A controlling terminal belongs
to one session, so a second unit claiming the same one takes it from the first.
That is why this is a key rather than something oxinit infers from stdin
happening to be a terminal — on a machine that boots to a console, stdin is a
terminal for *every* service.

Failing to get the terminal does not fail the unit. A service that cannot be
given job control is a nuisance; a console that will not start is a machine
with no way in.

### `output`

- **Type:** string — `"console"`, `"log"`, or `"null"`
- **Required:** no
- **Default:** `"console"`

Where the service's standard output and standard error go. They share one
destination, and for `"log"` one pipe: the order in which a service wrote to
its two streams is information, and two pipes would destroy it — they would be
read independently and interleaved by whichever the reader reached first.

| Value       | Where                                                      |
|-------------|------------------------------------------------------------|
| `"console"` | The descriptors oxinit itself has: the console on a machine, the runtime's pipes in a container. |
| `"log"`     | `oxlogd`, which writes `/var/log/oxinit/<unit>.log`.        |
| `"null"`    | `/dev/null`.                                                |

`"console"` is the default, deliberately. oxinit does not move a service's
output somewhere the operator did not ask for, and on a machine with no
`oxlogd` a `"log"` default would be a pipe with no reader — output would back
up until the service blocked on a write.

**`"log"` needs `oxlogd`,** and a unit that wants it should say so:

```toml
after    = ["oxlogd"]
requires = ["oxlogd"]
```

`oxlogd` is `type = "notify"`, so `after` on it means "once it can actually
receive descriptors", not merely "once it has been exec'd".

A unit whose output cannot be routed fails to start rather than falling back
to the console. `output` is a statement about where a service's output goes,
and quietly sending it somewhere else is a different statement.

Standard *input* is untouched by this key.

## `[resources]`

Limits applied to the service's cgroup. An absent key means the kernel default,
which is unlimited (`max`). Setting a limit does not reserve anything; it caps.

### `memory-max`

- **Type:** size string
- **Required:** no
- **Default:** unlimited

Written to `memory.max`. When the cgroup exceeds it and reclaim fails, the
kernel OOM killer kills a process inside the cgroup. That will usually appear as
the service exiting on `SIGKILL`, which counts as abnormal termination for
`restart` purposes.

### `tasks-max`

- **Type:** integer
- **Required:** no
- **Default:** unlimited

Written to `pids.max`. Caps the number of processes and threads in the cgroup.
Once reached, `fork` and `clone` in the cgroup fail with `EAGAIN`.

Note this key takes a bare integer, not a size string. `tasks-max = 512` is 512
tasks; `tasks-max = "512"` is a type error.

## `[socket]`

A socket unit is a set of listening descriptors and the name of the service
they belong to. oxinit creates, binds, and listens on them at boot; the service
does not start until the first connection arrives.

```toml
# /etc/oxinit/units/sshd-socket.toml

[unit]
description = "OpenSSH socket"

[socket]
listen  = ["0.0.0.0:22", "[::]:22"]
service = "sshd"
type    = "stream"          # stream | datagram | seqpacket
backlog = 128
```

The descriptors are held by oxinit, not by the service, which is what makes
this worth doing:

- Units ordered after the socket can rely on the port being **bound**, which is
  a fact, where "the service started" is a guess.
- A restart does not refuse connections. The listening socket never closes, so
  they queue in the kernel and the new process picks them up.
- The service can be started late, or not at all until something wants it.

A socket unit runs no process and has no cgroup, so `[resources]` on one is a
load error, exactly as on a target.

### `listen`

- **Type:** array of strings
- **Required:** **yes**
- **Default:** none

Addresses to bind. One descriptor is created per entry, in the order written,
and that is the order the service receives them in.

| Form              | Meaning                                        |
|-------------------|------------------------------------------------|
| `1.2.3.4:80`      | IPv4 address and port.                         |
| `[::1]:80`        | IPv6 address and port, address in brackets.    |
| `/run/name.sock`  | Unix socket. Must be an absolute path.         |

Addresses are literal. There is no name resolution — PID 1 does not do DNS,
and a boot that depends on a resolver is a boot that hangs when the resolver
does. `0.0.0.0` and `[::]` are how you say "every interface".

A bare port is a parse error. `listen = ["22"]` is rejected rather than guessed
at: write `0.0.0.0:22` or `[::]:22` and say which.

An empty `listen` is a load error. A socket unit that listens on nothing is a
unit that does nothing.

Unix sockets are unlinked before binding, so a stale file from a previous boot
is not an error, and are set to mode `0666` afterwards — a socket nobody can
connect to is not a socket. There is no key to change that yet; when there is,
it will be `mode`.

### `service`

- **Type:** string
- **Required:** **yes**
- **Default:** none

The service unit to start when a connection arrives. It must exist and must be
a service; anything else is a load error.

Required rather than inferred. A socket cannot share a name with its service
(see [Unit kinds](#unit-kinds)), and inferring one name from the other would
put the wiring in a filename convention instead of in the file.

The socket is ordered **before** its service implicitly. If both are started at
boot, the descriptors are bound before the service runs, or passing them to it
would mean nothing.

The socket does **not** pull its service in. That is the entire point: a
service reachable from `default` starts at boot whether anything connects or
not, and a socket-activated service should normally be reachable only through
its socket.

### `type`

- **Type:** string, one of `stream`, `datagram`, `seqpacket`
- **Required:** no
- **Default:** `stream`

The socket type, applied to every address in `listen`.

| Value       | TCP/Unix equivalent                                  |
|-------------|------------------------------------------------------|
| `stream`    | `SOCK_STREAM` — TCP, or a connection-oriented unix socket. |
| `datagram`  | `SOCK_DGRAM` — UDP, or a unix datagram socket.       |
| `seqpacket` | `SOCK_SEQPACKET` — unix only.                        |

`datagram` sockets are not listened on; a datagram arriving is what starts the
service, and the datagram is still queued when it does.

### `backlog`

- **Type:** integer
- **Required:** no
- **Default:** `128`

The `listen(2)` backlog. Connections beyond it are refused by the kernel.

Meaningful only for `stream` and `seqpacket`. Setting it alongside
`type = "datagram"` is a load error rather than a silent no-op, for the same
reason a typo in `[resources]` is.

## `[timer]`

Starts a service on a schedule. A timer unit runs no process and owns no
cgroup; being armed is the whole of what it does.

```toml
# /etc/oxinit/units/backup-timer.toml

[unit]
description = "Nightly backup"

[timer]
service     = "backup"
on-calendar = "03:30"
```

At least one of `on-boot`, `interval` and `on-calendar` is required. A timer
with none of them would sit armed forever and look, from outside, like a
service that never ran, so it is a load error.

When more than one is declared, the next firing is **the soonest of them**.
That is the only reading of "and" that does not silently ignore one.

### `service`

- **Type:** string
- **Required:** yes

The unit to start when the timer elapses.

Required rather than inferred from the timer's own name, for the reason a
socket unit's is: unit names are unique across kinds, which is what lets
`requires`, `after` and `oxctl` take a bare name — so a timer and its service
cannot share one.

Like a socket unit, a timer does **not** pull its service in. A service
reachable from `default` starts at boot whether or not anything scheduled it,
which is the opposite of what a schedule is for.

**A firing while the service is still running is skipped, not queued.** A job
that takes longer than its own interval would otherwise accumulate copies of
itself until the machine fell over, and the firing after it is one interval
away either way.

**A failed run does not stop the schedule.** The timer is re-armed before the
service is started. A timer is a statement about *when*, not about whether the
last run worked; what to do about a failure is `restart` on the service.

### `on-boot`

- **Type:** duration string
- **Required:** no
- **Default:** the value of `interval`, if there is one

The first firing, measured from when the timer unit started.

On its own — no `interval`, no `on-calendar` — it is a delayed one-shot: the
timer fires once and is not re-armed.

### `interval`

- **Type:** duration string
- **Required:** no
- **Default:** none

Every firing after the first, measured from the **previous firing**.

Not from the service's last activation, which is the deliberate difference
from systemd's `OnUnitActiveSec`. It is what the key name says, it does not
depend on a state transition the service may never make — a `oneshot` service
never becomes `Active` at all — and it is what anyone writing "every hour"
means.

With no `on-boot`, the first firing is also one interval away.

### `on-calendar`

- **Type:** string, from a closed vocabulary
- **Required:** no
- **Default:** none

A wall-clock schedule.

| Expression  | Fires                                   |
|-------------|-----------------------------------------|
| `hourly`    | Every hour, on the hour.                |
| `daily`     | Every day at 00:00:00.                  |
| `weekly`    | Every Monday at 00:00:00.               |
| `HH:MM`     | Every day at that minute.               |
| `HH:MM:SS`  | Every day at that second.               |

**The vocabulary is closed**, like the specifier set. This is not a cron
expression and will not grow into one: a schedule nobody can misread is worth
more than one that can say anything. Anything the table cannot express is an
argument for a real scheduler, not for a bigger grammar here.

Whitespace is not trimmed. `" 9:00"` is a typo, and a parser that accepts it
teaches an operator a syntax this document does not describe.

**Times are UTC.** oxinit has no timezone database and is not going to carry
one.

**A clock step moves a pending firing.** A calendar deadline is computed once,
as a delay on the monotonic clock, so an NTP correction between arming and
firing moves that firing by however much the clock moved. Every firing after
it is recomputed against the corrected clock. Fixing this properly needs a
`CLOCK_REALTIME` timerfd with `TFD_TIMER_CANCEL_ON_SET`; see
[ROADMAP.md](../ROADMAP.md).

## File descriptor passing

A socket-activated service inherits its descriptors and is told about them
through the environment, using the same protocol systemd defined:

| Variable          | Value                                                     |
|-------------------|-----------------------------------------------------------|
| `LISTEN_FDS`      | How many descriptors, starting at fd 3.                   |
| `LISTEN_PID`      | The pid the descriptors were passed to.                   |
| `LISTEN_FDNAMES`  | Colon-separated names, one per descriptor.                |

Descriptors arrive at fd 3, 4, 5 … in `listen` order, with `FD_CLOEXEC`
cleared. `LISTEN_FDNAMES` names each one after the socket unit that created it,
so a service given descriptors by two socket units can tell them apart.

**A service must check `LISTEN_PID` against its own pid before trusting
`LISTEN_FDS`.** The variables are inherited by every descendant, and a child
that read `LISTEN_FDS` without that check would believe it owns descriptors
that belong to its parent.

This is a runtime protocol, not a configuration format. See the argument in
[ARCHITECTURE.md](../ARCHITECTURE.md).

## Value formats

### Duration strings

A duration is a string of one or more `<number><unit>` pairs, concatenated with
no separator. Whitespace between pairs is allowed.

| Suffix                | Unit         |
|-----------------------|--------------|
| `us`, `usec`          | microseconds |
| `ms`, `msec`          | milliseconds |
| `s`, `sec`, `seconds` | seconds      |
| `min`, `minutes`      | minutes      |
| `h`, `hours`          | hours        |
| `d`, `days`           | days         |

The suffix is mandatory. A bare number is a parse error — `restart-sec = "5"` is
rejected rather than guessed at.

Valid: `"5s"`, `"2min"`, `"1h"`, `"100ms"`, `"1min 30s"`, `"1h30min"`.

Invalid: `"5"` (no suffix), `"5 s"` (space before suffix), `"-5s"` (negative),
`"1.5s"` (fractional — write `"1500ms"`).

Multiple pairs are summed. Order is not enforced, and a repeated unit is
summed rather than rejected: `"30s 30s"` is one minute.

The maximum representable duration is `u64` microseconds. Anything larger is a
parse error.

### Size strings

A size is a number with an optional single-letter suffix.

| Suffix | Multiplier          |
|--------|---------------------|
| (none) | 1 (bytes)           |
| `K`    | 1024                |
| `M`    | 1024²               |
| `G`    | 1024³               |
| `T`    | 1024⁴               |

Multiples are **binary**, not decimal. `"1K"` is 1024 bytes. The suffix is
case-sensitive: `"256M"` is valid, `"256m"` is a parse error. Fractional values
are rejected; write `"1536M"` rather than `"1.5G"`.

The literal string `"max"` means unlimited and is written to the cgroup file
verbatim.

Valid: `"256M"`, `"1G"`, `"65536"`, `"max"`.

Invalid: `"256MB"`, `"256 M"`, `"1.5G"`, `"256m"`.

### Booleans

TOML `true` and `false`. The strings `"yes"`, `"on"`, and `"1"` are not
accepted.

## Specifiers

A small set of `%`-prefixed specifiers is expanded in `exec` before the string
is split into argv. This is the only runtime interpolation in the format.

| Specifier | Expands to                                    |
|-----------|-----------------------------------------------|
| `%n`      | The unit name, e.g. `sshd`.                   |
| `%N`      | The fully-qualified name, e.g. `sshd.service`. |
| `%H`      | The hostname at the time of expansion.        |
| `%u`      | The value of `user`.                          |
| `%%`      | A literal `%`.                                |

An unrecognised specifier is a load error. The set is closed and will grow only
with a documented reason; it is not an escape hatch for a template language.

Specifiers are not expanded in any other key. Environment variables are never
expanded — `$HOME` in `exec` is a literal `$HOME`.

## Validation

The parser rejects, at load time:

- An unknown section or key.
- Zero or more than one kind section — every unit must have exactly one of
  `[service]`, `[target]`, `[socket]`.
- A missing `exec` in a service unit.
- A missing or empty `listen`, or a missing `service`, in a socket unit.
- A `listen` entry that is neither `address:port` nor an absolute path.
- A `backlog` on a `datagram` socket.
- A socket whose `service` does not exist, or is not a service.
- A `[resources]` section on a unit that is not a service.
- Two units with the same name but different kinds.
- A value of the wrong type, including a bare number where a duration or size
  string is expected.
- A `type`, `restart`, or specifier outside its enumerated set.
- A `requires` naming a unit that does not exist.
- A unit named in both `requires` and `conflicts`.
- A cycle in the `after`/`before` edges, reported with the cycle path.
- A `exec` whose first element is not an absolute path.

A load error means oxinit does not start with that unit set. It does not skip
the bad unit and continue, because a partially loaded configuration produces a
machine whose running state does not match any file on disk.
