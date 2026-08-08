# Security

## Status

oxinit is pre-alpha. There is no release, no user base, and no formal security
process. There is no advisory pipeline, no CVE workflow, no embargo policy, and
no committed response time.

This document exists so that the threat model is written down before the code
is, and so there is an address to send a report to.

Do not run oxinit on a machine you care about.

## Threat model

PID 1 has a position no other process on the machine has:

- It runs as root, and it is the only process that cannot be restarted by
  something above it. If it exits, the kernel panics.
- It is the parent of every process on the machine, directly or by
  reparenting.
- It owns the cgroup hierarchy under `/sys/fs/cgroup/oxinit.slice/`, which is
  what enforces every service's resource limits and what makes `cgroup.kill`
  work.
- It holds the listening file descriptors for socket activation before any
  service exists to receive them.
- It decides the order in which the system shuts down, including whether
  filesystems are unmounted cleanly.

Code execution in PID 1 is code execution as root with no supervisor. A crash in
PID 1 is a kernel panic — availability loss for the whole machine, not one
service. Both outcomes are total.

The design decisions that follow from this — no panicking constructs, unwinding
rather than aborting, `catch_unwind` at handler boundaries, quarantined
`unsafe`, no async runtime, minimal dependency tree — are described in
[ARCHITECTURE.md](ARCHITECTURE.md). They are security properties, not style.

## Attack surface

Three interfaces process input that oxinit did not produce.

### Unit file parser

`oxinit-unit` parses TOML from `/etc/oxinit/units/` and
`/usr/lib/oxinit/units/`. Under a correct installation both directories are
root-owned and mode `0755`, so their contents are trusted input.

They are not always correct. If either directory, or any path component leading
to it, is writable by a non-root user, unit files become attacker-controlled
input to a parser running as PID 1. Unit files also specify `user`, `exec`, and
resource limits, so write access to them is equivalent to root regardless of any
parser bug.

The parser is written to reject rather than guess: unknown keys and sections are
errors, values of the wrong type are errors, and a load error means oxinit does
not start rather than starting with a partially applied configuration. It is
still the largest piece of untrusted-input handling in the project.

Relevant to a report: parser crashes, unbounded allocation from a crafted file,
integer overflow in the duration or size parsers, path traversal via a unit
name.

### Control socket

`SOCK_SEQPACKET` at `/run/oxinit/control.sock`, mode `0600`, root-owned, in a
root-owned directory. Authorization is entirely the socket's permissions. There
is no in-protocol authentication and no per-command permission model.

Access to this socket is full control of the machine: it can start and stop
arbitrary units. This is intended — the socket is not meant to be reachable by
anything but root — but it means a permissions mistake on `/run/oxinit/` is a
complete compromise, with no second layer behind it.

Relevant to a report: any path by which a non-root process can reach the socket,
and any message that causes PID 1 to crash, hang, or consume unbounded memory.

### Notify protocol

`SOCK_DGRAM` at `/run/oxinit/notify`. Services with `type = "notify"` send
`READY=1`, `STATUS=`, and `WATCHDOG=1` datagrams here.

Unlike the control socket, this one is reachable by every service, including
services running as unprivileged users — that is what it is for. The datagram
payload is unauthenticated by construction, so oxinit sets `SO_PASSCRED` on the
socket, reads with `recvmsg`, and takes the sender's identity from the
kernel-attached `SCM_CREDENTIALS` ancillary message rather than from anything in
the payload. Datagrams with no credentials attached, or whose pid is not in a
known service cgroup, are dropped.

That check is the whole boundary. If it can be bypassed, a service can mark
another service ready, keep a hung service's watchdog alive, or write arbitrary
text into another unit's status field.

`SCM_CREDENTIALS` gives a pid the sender cannot forge, but a pid is not a
durable identity: the sending process can exit and its pid be reused before
oxinit resolves it to a cgroup. That window is the known weak point.

Relevant to a report: any way to attribute a datagram to a cgroup that did not
send it, including pid reuse races; crashes from a malformed datagram;
unbounded `STATUS=` strings.

### Out of scope

- Anything requiring root access to begin with. Root can replace the binary.
- Denial of service by a service exhausting its own configured limits.
- Configuration mistakes: a `0666` control socket directory, a world-writable
  `/etc/oxinit/units/`, a unit that runs an untrusted binary as root.

## Reporting

Email **youri@youhide.com.br** with a description, affected component, and
reproduction steps if you have them.

Do not open a public issue for a security report.

Expect a slow response. This is a pre-alpha project with no security team. You
will get an acknowledgement and an honest answer about whether and when it will
be fixed. There is no bounty.

Since there is no release and no user base, the current default is to fix in
public with a normal commit and an explanatory message. If that is wrong for
something you have found, say so in your report.
