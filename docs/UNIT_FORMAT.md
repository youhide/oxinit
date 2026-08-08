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
| `[socket]`  | socket  | Reserved for [M4](../ROADMAP.md). Not yet specified. |

Names are unique across kinds: there cannot be both a `sshd` service and a
`sshd` target. That is what lets `requires`, `after`, and `oxctl` take a bare
name with no suffix and no ambiguity.

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
| `[socket]`    | Exactly one kind section    | Reserved for M4.               |
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

### `user`

- **Type:** string
- **Required:** no
- **Default:** `"root"`

Username to run the service as. Resolved to a uid and gid against `/etc/passwd`
at start time, not at load time, so a unit may reference a user created later in
the boot.

`setgid` and `initgroups` are called before `setuid`. Reversing that order would
drop the privilege needed to change groups.

An unresolvable username at start time fails the unit.

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
