//! Build and boot automation.
//!
//! `cargo xtask boot` builds a static `oxinit`, packs it into a single-file
//! cpio initramfs, and boots it under QEMU. Edit to boot is a few seconds.
//!
//! This runs on the developer's machine. The rules that apply to the `oxinit`
//! crate — no panic, no unsafe, no async — do not apply here.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// A target oxinit is built and booted for.
///
/// Both are claimed as supported. Until M9 only one had ever been compiled,
/// which is the difference between a supported target and an assumed one.
#[derive(Clone, Copy)]
struct Arch {
    /// What `--arch` takes, and what `$OXINIT_KERNEL_<ARCH>` is named after.
    name: &'static str,
    target: &'static str,
    qemu: &'static str,
    /// Extra QEMU arguments. x86_64 has a default machine that boots a kernel;
    /// aarch64 has no such thing — `virt` is the board QEMU invented for
    /// exactly this, and without `-cpu` it defaults to one the kernel will not
    /// run on.
    machine: &'static [&'static str],
    /// What the kernel calls the serial port it is told to use. Getting this
    /// wrong produces a boot with no output at all and nothing to say why.
    console: &'static str,
    /// How long `test-boot` waits. Emulating a foreign architecture has no
    /// hardware acceleration to fall back on, and the whole boot runs through
    /// QEMU's JIT.
    timeout: u32,
}

const ARCHES: &[Arch] = &[
    Arch {
        name: "x86_64",
        target: "x86_64-unknown-linux-musl",
        qemu: "qemu-system-x86_64",
        machine: &[],
        console: "ttyS0",
        timeout: 90,
    },
    Arch {
        name: "aarch64",
        target: "aarch64-unknown-linux-musl",
        qemu: "qemu-system-aarch64",
        machine: &["-M", "virt", "-cpu", "cortex-a57"],
        console: "ttyAMA0",
        timeout: 300,
    },
];

/// `--arch NAME`, then `$OXINIT_ARCH`, then the host's own if it is one of
/// them, then x86_64.
fn find_arch(args: &[String]) -> Result<Arch, String> {
    let wanted = match flag(args, "--arch")? {
        Some(name) => name.to_owned(),
        None => env::var("OXINIT_ARCH").unwrap_or_else(|_| default_arch().to_owned()),
    };

    ARCHES
        .iter()
        .find(|arch| arch.name == wanted)
        .copied()
        .ok_or_else(|| {
            let known: Vec<&str> = ARCHES.iter().map(|arch| arch.name).collect();
            format!("unknown --arch `{wanted}`; known: {}", known.join(", "))
        })
}

/// Building for the host's own architecture is what a developer means by
/// nothing, and it is the one QEMU can accelerate.
fn default_arch() -> &'static str {
    if cfg!(target_arch = "aarch64") {
        "aarch64"
    } else {
        "x86_64"
    }
}

fn main() {
    let mut args = env::args().skip(1);
    let cmd = args.next().unwrap_or_default();
    let rest: Vec<String> = args.collect();

    let result = match cmd.as_str() {
        "boot" => boot(&rest),
        "build" => find_arch(&rest)
            .and_then(build)
            .map(|path| println!("{}", path.display())),
        "image" => find_arch(&rest)
            .and_then(|arch| image(arch, &rest, false))
            .map(|path| println!("{}", path.display())),
        "test-boot" => test_boot_all(&rest),
        "container" => container(&rest),
        "" | "help" | "--help" | "-h" => {
            usage();
            Ok(())
        }
        other => Err(format!("unknown command `{other}`\n\n{USAGE}")),
    };

    if let Err(message) = result {
        eprintln!("xtask: {message}");
        std::process::exit(1);
    }
}

const USAGE: &str = "\
usage: cargo xtask <command>

commands:
  build      build oxinit for x86_64-unknown-linux-musl
  image      build and pack an initramfs, printing its path; does not boot
  boot       build, pack an initramfs, and boot it under QEMU
  test-boot  boot with a timeout and assert on the serial log
  container  build a container image, run it, and assert on its output

options:
  --arch NAME     x86_64 (default), aarch64, or `all` for test-boot, which
                  boots every supported target in turn. Both are supported
                  targets; the host's own is the one QEMU can accelerate.

boot options:
  --kernel PATH   kernel image; falls back to $OXINIT_KERNEL_<ARCH>, then
                  $OXINIT_KERNEL, then target/vmlinuz-<arch>
  --shell PATH    statically linked shell to place at /bin/sh, normally
                  busybox; falls back to $OXINIT_SHELL_<ARCH>, then
                  $OXINIT_SHELL. It has to match --arch. Without one, the
                  image holds only /init and oxinit has no shell to spawn.

container options:
  --engine NAME   docker (default) or podman
  --privileged    run with full capabilities and a writable cgroupfs, and
                  additionally assert that cgroups work

Ctrl-A X exits QEMU.";

fn usage() {
    println!("{USAGE}");
}

/// Build a static `oxinit` and return the path to the binary.
fn build(arch: Arch) -> Result<PathBuf, String> {
    build_package(arch, "oxinit")
}

fn build_package(arch: Arch, package: &str) -> Result<PathBuf, String> {
    run(Command::new(cargo())
        .args(["build", "--release", "--target", arch.target, "-p", package])
        .current_dir(root()))?;

    let binary = root()
        .join("target")
        .join(arch.target)
        .join("release")
        .join(package);
    if !binary.exists() {
        return Err(format!("expected a binary at {}", binary.display()));
    }
    Ok(binary)
}

/// Build and pack, without booting. Used by `boot`, and on its own by anything
/// that wants to drive QEMU itself.
fn image(arch: Arch, args: &[String], test: bool) -> Result<PathBuf, String> {
    let shell = find_shell(arch, args)?;
    let binary = build(arch)?;
    pack_initramfs(arch, &binary, shell.as_deref(), test)
}

fn boot(args: &[String]) -> Result<(), String> {
    let arch = find_arch(args)?;
    let kernel = find_kernel(arch, args)?;
    let image = image(arch, args, false)?;

    println!(
        "xtask: booting {} on {} with {}",
        image.display(),
        arch.name,
        kernel.display()
    );
    println!("xtask: Ctrl-A X to exit QEMU");

    let mut command = Command::new(arch.qemu);
    command.args(arch.machine).args([
        "-kernel".as_ref(),
        kernel.as_os_str(),
        "-initrd".as_ref(),
        image.as_os_str(),
        "-nographic".as_ref(),
        "-m".as_ref(),
        "512".as_ref(),
        "-append".as_ref(),
        format!("console={}", arch.console).as_ref(),
    ]);

    run(&mut command)
}

/// Lines the serial log must contain, and what each one proves.
///
/// One per thing that has been claimed to work, so a regression fails here
/// rather than in a boot someone happens to be watching. They are matched as
/// substrings, not equality: pids and byte counts change every boot, and
/// asserting on them would make the test fail for being alive.
const EXPECTED: &[(&str, &str)] = &[
    ("Run /init as init process", "the kernel reached oxinit"),
    (
        "oxinit-m1: banner on",
        "M1: units load and specifiers expand",
    ),
    ("oxinit: reached target default", "M1: the graph resolved"),
    ("is ready", "M2: sd_notify readiness"),
    ("missed its watchdog deadline", "M2: the watchdog fires"),
    (
        "uid=65534(nobody)",
        "M3: privilege drop, with supplementary groups",
    ),
    ("groups=100(oxinit)", "M3: setgroups, not just setgid"),
    (
        "forked and is ready",
        "M3: forking readiness from cgroup.populated",
    ),
    ("limited: memory", "M3: memory.current and pids.current"),
    ("0::/init.scope", "M5: PID 1 parked before delegation"),
    (
        "did not stop in time; killing its cgroup",
        "M3: escalation to cgroup.kill",
    ),
    (
        "listening for echo-socket on /run/echo.sock",
        "M4: sockets bind before anything starts",
    ),
    ("LISTEN_PID=", "M4: descriptor passing"),
    (
        "is mine, serving `echo-socket` on fd 3",
        "M4: LISTEN_FDNAMES",
    ),
    ("got \"echo: hello\"", "M4: an activated service answered"),
    (
        "oxinit: logs: shipper connected",
        "M7: oxlogd took the log socket",
    ),
    (
        "chatty.log: seen-out",
        "M7: a service's stdout landed in its own file, timestamped",
    ),
    (
        "chatty.log: seen-err",
        "M7: and its stderr, in the order it was written",
    ),
    (
        "chatty.log: seen-last",
        "M7: including the last line before it exited",
    ),
    (
        "oxinit: tick-timer will start stamp in 2s",
        "M8: a timer arms with on-boot",
    ),
    (
        "oxinit: tick-timer elapsed; starting stamp",
        "M8: and fires",
    ),
    ("oxinit-m8: stamp fired", "M8: the service it named ran"),
    (
        "oxinit: tick-timer will start stamp in 3s",
        "M8: and re-arms with interval, so it fires again",
    ),
    (
        "oxinit: shutting down to power off",
        "M5: SIGTERM is handled",
    ),
    (
        "oxinit: syncing",
        "M5: the machine flushes before going down",
    ),
    ("oxinit: power off", "M5: reboot(2) with the right command"),
];

/// Lines that must not appear. A boot that logs one of these has failed even
/// if everything else is present.
///
/// "can't access tty" is busybox reporting that it has no controlling
/// terminal, which is what `tty = true` on the console unit exists to prevent.
/// It is asserted as an absence because there is no line to assert on when it
/// works — a shell with job control simply does not mention it.
/// "chatty-out" is the other half of the log assertions. The `logged` unit
/// rewrites it to "seen-out" as it reads it back out of the file, so the
/// original token reaching the serial log at all means a service that declared
/// `output = "log"` wrote to the console instead.
const FORBIDDEN: &[&str] = &["panicked", "Kernel panic", "can't access tty", "chatty-out"];

/// `--arch all` boots every supported target, one after the other.
///
/// The point of the milestone that added it: "supported" is a claim about
/// something that has been run, and one command that runs all of them is what
/// keeps that true. Sequential rather than concurrent — two emulated machines
/// on one host contend for the same cores and neither result means anything
/// about the other.
fn test_boot_all(args: &[String]) -> Result<(), String> {
    if flag(args, "--arch")? != Some("all") {
        return test_boot(find_arch(args)?, args);
    }

    // A kernel and a shell are each for one architecture, so a single value
    // cannot mean anything across all of them. Refused rather than applied to
    // the first and silently wrong for the rest.
    for name in ["--kernel", "--shell"] {
        if flag(args, name)?.is_some() {
            return Err(format!(
                "{name} names one architecture's file; with `--arch all` use \
                 $OXINIT_KERNEL_<ARCH> and $OXINIT_SHELL_<ARCH>, or the \
                 defaults under target/"
            ));
        }
    }

    let mut failures = Vec::new();
    for arch in ARCHES {
        println!("\nxtask: === {} ===", arch.name);
        if let Err(e) = test_boot(*arch, args) {
            failures.push(format!("{}: {e}", arch.name));
        }
    }

    if failures.is_empty() {
        println!("\nxtask: every architecture booted");
        return Ok(());
    }

    Err(failures.join("\n"))
}

/// Boot under QEMU, with a timeout, and assert on what came out of the serial
/// port.
fn test_boot(arch: Arch, args: &[String]) -> Result<(), String> {
    let kernel = find_kernel(arch, args)?;
    let image = image(arch, args, true)?;

    let log = root().join(format!("target/test-boot-{}.log", arch.name));
    let _ = fs::remove_file(&log);

    println!(
        "xtask: booting {} with a {}s limit, logging to {}",
        arch.name,
        arch.timeout,
        log.display()
    );

    // `-serial file:` rather than -nographic, so the log is a file this can
    // read rather than this process's own stdout. `-display none` keeps QEMU
    // from opening a window on a developer's machine.
    let mut child = Command::new(arch.qemu)
        .args(arch.machine)
        .args([
            "-kernel".as_ref(),
            kernel.as_os_str(),
            "-initrd".as_ref(),
            image.as_os_str(),
            "-display".as_ref(),
            "none".as_ref(),
            "-serial".as_ref(),
            format!("file:{}", log.display()).as_ref(),
            "-append".as_ref(),
            format!("console={}", arch.console).as_ref(),
            "-m".as_ref(),
            "512".as_ref(),
        ])
        .spawn()
        .map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => {
                format!("{} not found; is it installed?", arch.qemu)
            }
            _ => format!("run {}: {e}", arch.qemu),
        })?;

    let clean = wait_for(&mut child, arch.timeout)?;
    let text = fs::read_to_string(&log).map_err(|e| format!("read {}: {e}", log.display()))?;

    let failures = if clean {
        Vec::new()
    } else {
        vec![format!(
            "qemu had to be killed after {}s: the machine never powered off",
            arch.timeout
        )]
    };

    check(&text, EXPECTED, FORBIDDEN, failures)
}

/// Wait for QEMU, killing it if it overruns. `true` if it exited on its own,
/// which for this image means oxinit powered the machine off.
fn wait_for(child: &mut std::process::Child, seconds: u32) -> Result<bool, String> {
    for _ in 0..seconds * 10 {
        match child.try_wait() {
            Ok(Some(_)) => return Ok(true),
            Ok(None) => std::thread::sleep(std::time::Duration::from_millis(100)),
            Err(e) => return Err(format!("wait for qemu: {e}")),
        }
    }

    let _ = child.kill();
    let _ = child.wait();
    Ok(false)
}

/// Compare a log against what it has to contain and what it must not.
///
/// `failures` is what the caller already knows went wrong — a machine that
/// never powered off, a container the runtime had to kill — carried in so that
/// one report covers everything rather than the caller printing half of it.
fn check(
    text: &str,
    expected: &[(&str, &str)],
    forbidden: &[&str],
    mut failures: Vec<String>,
) -> Result<(), String> {
    for (needle, proves) in expected {
        if text.contains(needle) {
            println!("  ok    {proves}");
        } else {
            failures.push(format!("missing `{needle}` — {proves}"));
        }
    }

    for needle in forbidden {
        if text.contains(needle) {
            failures.push(format!("log contains `{needle}`"));
        }
    }

    if failures.is_empty() {
        println!("xtask: {} checks passed", expected.len());
        return Ok(());
    }

    Err(format!(
        "{} check(s) failed:\n  {}",
        failures.len(),
        failures.join("\n  ")
    ))
}

/// What a container log must contain, and what each line proves.
///
/// The first three are M6 and nothing else tests them. The middle four are the
/// same features `test-boot` asserts on under QEMU, checked again here because
/// "it works when the kernel booted it" and "it works when a runtime execed
/// it" are different claims.
const EXPECTED_CONTAINER: &[(&str, &str)] = &[
    (
        "oxinit: pid 1 of a container",
        "M6: the runtime was detected",
    ),
    (
        "oxinit-m1: banner on",
        "M6: the runtime's stdio was kept — this log exists at all",
    ),
    (
        "already mounted, left alone: /proc",
        "M6: the runtime's mounts are tolerated, not reported as failures",
    ),
    ("oxinit: reached target default", "the graph resolved"),
    ("is ready", "sd_notify readiness"),
    ("uid=65534(nobody)", "the privilege drop"),
    ("got \"echo: hello\"", "socket activation"),
    (
        "oxinit: shutting down to power off",
        "M6: the runtime's stop signal is an ordered shutdown",
    ),
    (
        "power off: exiting with status 0",
        "M6: exiting, rather than a reboot(2) that is not oxinit's to do",
    ),
    (
        "oxinit: logs: shipper connected",
        "M7: oxlogd took the log socket",
    ),
];

/// Additionally, when the container has the capabilities and a writable
/// cgroupfs. Unprivileged is the default because it is what `docker run` gives
/// everyone, and oxinit is supposed to degrade rather than refuse.
const EXPECTED_PRIVILEGED: &[(&str, &str)] = &[
    (
        "oxinit: pid 1 is in",
        "M5: init.scope, in a container's own cgroup namespace",
    ),
    ("limited: memory", "M3: the [resources] limits landed"),
];

/// "can't access tty" is absent from [`FORBIDDEN`]'s container counterpart on
/// purpose: a detached container has no terminal for `tty = true` to claim,
/// and busybox saying so is correct there.
const FORBIDDEN_CONTAINER: &[&str] = &["panicked"];

const CONTAINER_IMAGE: &str = "oxinit:test";
const CONTAINER_NAME: &str = "oxinit-container-test";

/// The line that means the boot finished and it is fair to ask for a stop.
const BOOTED: &str = "oxinit: reached target default";

/// How long to wait for that line.
const BOOT_TIMEOUT: u32 = 30;

/// How long the runtime waits after `SIGTERM` before `SIGKILL`.
///
/// Deliberately longer than the default ten seconds. A test that failed
/// because the grace period was tight would be testing the grace period; the
/// assertion that matters is the exit code, and a `SIGKILL` shows up there as
/// 137 no matter how long the wait was.
const STOP_GRACE: u32 = 30;

/// `test-boot` for a runtime that is not QEMU.
///
/// Builds the same root filesystem the initramfs is packed from, as a
/// container image; runs it; waits for the boot to finish; asks the runtime to
/// stop it the way an operator would; and asserts on the log and on the exit
/// code. The exit code is the point: before M6 every `docker stop` ended in
/// `SIGKILL` and 137, because `SIGTERM` did nothing and PID 1 never exited.
fn container(args: &[String]) -> Result<(), String> {
    let engine = flag(args, "--engine")?.unwrap_or("docker").to_owned();
    let privileged = args.iter().any(|arg| arg == "--privileged");

    let arch = find_arch(args)?;
    let shell = find_shell(arch, args)?;
    let binary = build(arch)?;

    // `false`, unlike `test-boot`: no unit that ends the test by signalling
    // PID 1, because what ends this one is the runtime's stop — which is the
    // thing being tested.
    let staging = stage(arch, &binary, shell.as_deref(), false)?;
    let dockerfile = write_dockerfile()?;

    println!("xtask: building {CONTAINER_IMAGE} with {engine}");
    run(Command::new(&engine).args([
        "build".as_ref(),
        "-q".as_ref(),
        "-t".as_ref(),
        CONTAINER_IMAGE.as_ref(),
        "-f".as_ref(),
        dockerfile.as_os_str(),
        staging.as_os_str(),
    ]))?;

    // Left over from a run that was interrupted before it could clean up.
    remove(&engine);

    let mut start = vec!["run", "-d", "--name", CONTAINER_NAME];
    if privileged {
        start.push("--privileged");
    }
    start.push(CONTAINER_IMAGE);

    run(Command::new(&engine).args(&start).stdout(Stdio::null()))?;

    let result = drive(&engine, privileged);
    remove(&engine);
    result
}

/// Wait for the boot, stop it, and check what came out.
///
/// Split from [`container`] so that the container is removed on every path out
/// of it, including a failed assertion.
fn drive(engine: &str, privileged: bool) -> Result<(), String> {
    println!("xtask: waiting up to {BOOT_TIMEOUT}s for `{BOOTED}`");
    let booted = wait_for_line(engine, BOOTED, BOOT_TIMEOUT)?;

    let mut failures = Vec::new();
    if !booted {
        failures.push(format!("the container never logged `{BOOTED}`"));
    }

    // The claim M7 rests on: oxinit holds the read end of every pipe, so a
    // restarted oxlogd is handed them all again and a service writing into one
    // never notices. Killing the shipper and watching a live file keep growing
    // is the only way to tell that from a design that merely looks right.
    failures.extend(survives_a_shipper_restart(engine)?);

    println!("xtask: asking {engine} to stop it");
    let started = std::time::Instant::now();
    run(Command::new(engine)
        .args(["stop", "-t", &STOP_GRACE.to_string(), CONTAINER_NAME])
        .stdout(Stdio::null()))?;
    let took = started.elapsed();

    let status = inspect(engine, "{{.State.ExitCode}}")?;
    println!(
        "xtask: stopped in {:.1}s, exit code {status}",
        took.as_secs_f32()
    );

    if status != "0" {
        failures.push(format!(
            "exit code {status}, not 0. 137 is SIGKILL — the runtime gave up \
             waiting, which is what M6 exists to fix"
        ));
    }

    let mut expected = EXPECTED_CONTAINER.to_vec();
    if privileged {
        expected.extend_from_slice(EXPECTED_PRIVILEGED);
    }

    check(&logs(engine)?, &expected, FORBIDDEN_CONTAINER, failures)
}

/// Restart `oxlogd` under a running `ticker` and check the log keeps growing.
///
/// Returns what went wrong, if anything, rather than failing early: a
/// container that is still up has more to say, and the caller reports it all
/// at once.
fn survives_a_shipper_restart(engine: &str) -> Result<Vec<String>, String> {
    const TICKER: &str = "/var/log/oxinit/ticker.log";

    let count = |engine: &str| -> Result<usize, String> {
        let out = exec(
            engine,
            &["/bin/sh", "-c", &format!("/bin/wc -l < {TICKER}")],
        )?;
        out.trim()
            .parse()
            .map_err(|_| format!("`wc -l < {TICKER}` said `{}`", out.trim()))
    };

    let before = count(engine)?;
    if before == 0 {
        return Ok(vec![format!(
            "{TICKER} was empty before the restart, so the rest proves nothing"
        )]);
    }

    println!("xtask: restarting oxlogd under a running ticker ({before} lines so far)");
    exec(engine, &["/bin/oxctl", "restart", "oxlogd"])?;

    // Long enough for the stop, the restart backoff, the reconnect, and a few
    // more ticks. Nothing here is instantaneous and none of it is worth a
    // polling loop.
    std::thread::sleep(std::time::Duration::from_secs(5));

    let after = count(engine)?;
    println!("xtask: {after} lines after it came back");

    if after <= before {
        return Ok(vec![format!(
            "{TICKER} stopped at {before} lines across an oxlogd restart; \
             the descriptor was not handed to the replacement"
        )]);
    }

    Ok(Vec::new())
}

/// Run something inside the container and return its standard output.
fn exec(engine: &str, argv: &[&str]) -> Result<String, String> {
    let mut command = Command::new(engine);
    command.args(["exec", CONTAINER_NAME]).args(argv);

    let out = command
        .output()
        .map_err(|e| format!("run {engine} exec: {e}"))?;

    if !out.status.success() {
        return Err(format!(
            "{engine} exec {}: {}",
            argv.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }

    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Poll the log until `needle` shows up. `false` on timeout.
fn wait_for_line(engine: &str, needle: &str, seconds: u32) -> Result<bool, String> {
    for _ in 0..seconds * 5 {
        if logs(engine)?.contains(needle) {
            return Ok(true);
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    Ok(false)
}

/// Everything the container has written, both streams.
///
/// oxinit reports failures on stderr and everything else on stdout, and the
/// assertions do not care which — so they are concatenated rather than kept
/// apart.
fn logs(engine: &str) -> Result<String, String> {
    let out = Command::new(engine)
        .args(["logs", CONTAINER_NAME])
        .output()
        .map_err(|e| format!("run {engine} logs: {e}"))?;

    Ok(String::from_utf8_lossy(&out.stdout).into_owned() + &String::from_utf8_lossy(&out.stderr))
}

fn inspect(engine: &str, format: &str) -> Result<String, String> {
    let out = Command::new(engine)
        .args(["inspect", "-f", format, CONTAINER_NAME])
        .output()
        .map_err(|e| format!("run {engine} inspect: {e}"))?;

    Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

/// Best-effort, and called on every path out of a run.
fn remove(engine: &str) {
    let _ = Command::new(engine)
        .args(["rm", "-f", CONTAINER_NAME])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

/// The image is the staged tree and an entrypoint, and nothing else.
///
/// `FROM scratch` because there is nothing to layer on: oxinit is statically
/// linked, and everything it needs — the shell, the client, the units, the
/// user database — is already in the tree. A base image would only add things
/// that could hide a missing one.
///
/// Written outside the build context so it does not end up inside the image it
/// describes.
fn write_dockerfile() -> Result<PathBuf, String> {
    const DOCKERFILE: &str = "\
# Written by `cargo xtask container`. The build context is the same staged
# tree `cargo xtask boot` packs into a cpio.
FROM scratch
COPY . /
ENTRYPOINT [\"/init\"]
";

    let path = root().join("target/oxinit.dockerfile");
    fs::write(&path, DOCKERFILE).map_err(|e| format!("write {}: {e}", path.display()))?;
    Ok(path)
}

/// Pack the staged tree into a cpio initramfs.
fn pack_initramfs(
    arch: Arch,
    binary: &Path,
    shell: Option<&Path>,
    test: bool,
) -> Result<PathBuf, String> {
    let staging = stage(arch, binary, shell, test)?;
    let image = root().join(format!("target/oxinit-{}.cpio.gz", arch.name));

    // bsdcpio and GNU cpio both accept `-o -H newc`, which is the format the
    // kernel's initramfs unpacker reads.
    let script = format!(
        "cd {} && find . | cpio --quiet -o -H newc | gzip -9 > {}",
        shell_quote(&staging),
        shell_quote(&image),
    );
    run(Command::new("sh").arg("-c").arg(script))?;

    Ok(image)
}

/// Assemble the root filesystem: the binary as `/init`, optionally with a
/// shell at `/bin/sh`, plus the client, the test fixtures, a user database,
/// and the units.
///
/// One tree, two destinations. The kernel unpacks an initramfs and runs
/// `/init`, and a container runtime execs the image's entrypoint — which is
/// the same binary in the same place — so `boot` and `container` are the same
/// filesystem packed two ways. That is deliberate: a container test that ran a
/// different image from the QEMU test would not be testing the same thing.
///
/// The kernel needs nothing but `/init`, but M0 supervises a shell, and an
/// image containing only `/init` has no shell to spawn — you get a booting
/// init with no prompt and a spawn error in the log. So `--shell PATH` (or
/// `$OXINIT_SHELL`) adds a statically linked shell, normally busybox, at
/// `/bin/sh`. Without it the boot still proves the mount, console, signalfd,
/// and reap paths, which is most of M0.
fn stage(arch: Arch, binary: &Path, shell: Option<&Path>, test: bool) -> Result<PathBuf, String> {
    let staging = root().join(format!("target/initramfs-{}", arch.name));
    let _ = fs::remove_dir_all(&staging);
    fs::create_dir_all(&staging).map_err(|e| format!("create {}: {e}", staging.display()))?;

    install(binary, &staging.join("init"))?;

    // Unconditionally, because the client and the fixtures below go here too.
    // Created only alongside a shell, an image built without one failed on the
    // first thing that tried to land in it.
    let bin = staging.join("bin");
    fs::create_dir_all(&bin).map_err(|e| format!("create {}: {e}", bin.display()))?;

    match shell {
        Some(path) => {
            install(path, &bin.join("sh"))?;

            if is_busybox(path) {
                install_applets(path, &bin)?;
            }
        }
        None => println!(
            "xtask: no shell in the image; oxinit will log a spawn failure for /bin/sh. \
             Pass --shell PATH (busybox, statically linked) for a prompt."
        ),
    }

    // The client and the log writer. Part of the system, unlike the fixtures
    // below.
    for program in ["oxctl", "oxlogd"] {
        let binary = build_package(arch, program)?;
        install(&binary, &staging.join("bin").join(program))?;
    }

    // Test fixtures. Only useful in a test image, and only there because
    // nothing in busybox sends a unix datagram or speaks LISTEN_FDS.
    for fixture in ["notify-probe", "listen-probe"] {
        if let Ok(binary) = build_package(arch, fixture) {
            install(&binary, &staging.join("bin").join(fixture))?;
        }
    }

    install_passwd(&staging)?;

    // Units, if any. `/etc` rather than `/usr/lib`, since a hand-assembled
    // test image is the operator's, not a package's.
    let units_src = root().join("units");
    if units_src.is_dir() {
        let units_dst = staging.join("etc/oxinit/units");
        fs::create_dir_all(&units_dst)
            .map_err(|e| format!("create {}: {e}", units_dst.display()))?;

        let mut count = 0;
        for entry in
            fs::read_dir(&units_src).map_err(|e| format!("read {}: {e}", units_src.display()))?
        {
            let path = entry.map_err(|e| format!("read entry: {e}"))?.path();
            if path.extension().is_some_and(|ext| ext == "toml") {
                if let Some(name) = path.file_name() {
                    fs::copy(&path, units_dst.join(name))
                        .map_err(|e| format!("copy {}: {e}", path.display()))?;
                    count += 1;
                }
            }
        }
        println!("xtask: installed {count} units from units/");

        if test {
            install_test_shutdown(&units_dst)?;
        }
    }

    Ok(staging)
}

/// Applets worth having in a debug shell. busybox is a multi-call binary: it
/// picks its behaviour from argv[0], so without these names `ls` and `cat` do
/// not exist and the shell can only run its own builtins.
const APPLETS: &[&str] = &[
    "cat", "ls", "ps", "sleep", "mount", "umount", "hostname", "grep", "poweroff", "reboot",
    "dmesg", "kill", "mkdir", "echo", "true", "false", "date", "id", "sed", "wc",
];

/// Whether to treat this binary as busybox, by filename.
///
/// A heuristic, and deliberately a shallow one: the alternative is executing
/// the binary to ask, which does not work when the host is not Linux — which
/// is the case this whole cross-compiling setup exists for.
fn is_busybox(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.contains("busybox"))
}

/// How long the test image runs before asking oxinit to shut down.
///
/// Long enough for the slowest thing being asserted on — the watchdog miss at
/// three seconds, its stop timeout, and the escalation to `cgroup.kill` after
/// that.
const TEST_UPTIME: u32 = 16;

/// The unit that makes `test-boot` end.
///
/// Only ever written into a test image. `cargo xtask boot` gives you a machine
/// that stays up, which is the whole point of it.
fn install_test_shutdown(units: &Path) -> Result<(), String> {
    let unit = format!(
        "# Written by `cargo xtask test-boot`. Not in units/.\n\
         [unit]\n\
         description = \"End the test\"\n\n\
         [service]\n\
         exec = '/bin/sh -c \"/bin/sleep {TEST_UPTIME}; /bin/kill -TERM 1\"'\n"
    );

    fs::write(units.join("test-shutdown.toml"), unit)
        .map_err(|e| format!("write test-shutdown.toml: {e}"))?;

    // Unreachable units do not start, so it has to be pulled into `default`.
    // Edited rather than duplicated: a second copy of the unit set would drift
    // from the real one and the test would stop testing it.
    let default = units.join("default.toml");
    let text =
        fs::read_to_string(&default).map_err(|e| format!("read {}: {e}", default.display()))?;

    const MARKER: &str = "wants       = [";
    if !text.contains(MARKER) {
        return Err(format!(
            "{} no longer contains `{MARKER}`, so test-shutdown cannot be pulled in",
            default.display()
        ));
    }

    let patched = text.replacen(MARKER, &format!("{MARKER}\n    \"test-shutdown\","), 1);
    fs::write(&default, patched).map_err(|e| format!("write {}: {e}", default.display()))?;

    Ok(())
}

/// A user database, so a unit can declare `user` and mean something.
///
/// A cpio initramfs has no `/etc` unless something puts one there, and
/// `nobody` is the account the test units drop to. `nobody` is a member of
/// `oxinit` as well as its own primary group, which is what makes the
/// supplementary-group half of the privilege drop visible in `id` output.
fn install_passwd(staging: &Path) -> Result<(), String> {
    const PASSWD: &str = "\
root:x:0:0:root:/root:/bin/sh
nobody:x:65534:65534:nobody:/:/bin/false
";

    const GROUP: &str = "\
root:x:0:
oxinit:x:100:nobody
nogroup:x:65534:
";

    let etc = staging.join("etc");
    fs::create_dir_all(&etc).map_err(|e| format!("create {}: {e}", etc.display()))?;

    fs::write(etc.join("passwd"), PASSWD).map_err(|e| format!("write passwd: {e}"))?;
    fs::write(etc.join("group"), GROUP).map_err(|e| format!("write group: {e}"))?;

    Ok(())
}

/// Install busybox under its own name plus a symlink per applet.
fn install_applets(shell: &Path, bin: &Path) -> Result<(), String> {
    let busybox = bin.join("busybox");
    install(shell, &busybox)?;

    for applet in APPLETS {
        let link = bin.join(applet);
        let _ = fs::remove_file(&link);

        #[cfg(unix)]
        std::os::unix::fs::symlink("busybox", &link)
            .map_err(|e| format!("symlink {}: {e}", link.display()))?;
    }

    println!("xtask: installed busybox with {} applets", APPLETS.len());
    Ok(())
}

/// Copy a file into the staging tree and make it executable.
fn install(from: &Path, to: &Path) -> Result<(), String> {
    fs::copy(from, to).map_err(|e| format!("copy {} to {}: {e}", from.display(), to.display()))?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(to, fs::Permissions::from_mode(0o755))
            .map_err(|e| format!("chmod {}: {e}", to.display()))?;
    }

    Ok(())
}

/// The shell to place at `/bin/sh`, if any. Optional by design — see
/// [`pack_initramfs`].
fn find_shell(arch: Arch, args: &[String]) -> Result<Option<PathBuf>, String> {
    // Arch-suffixed first. A shell is a binary for one architecture, and
    // silently packing an x86_64 busybox into an aarch64 image produces a boot
    // where every unit fails to exec and nothing says why.
    let path = match flag(args, "--shell")? {
        Some(path) => PathBuf::from(path),
        None => match per_arch(arch, "OXINIT_SHELL") {
            Some(path) => path,
            // Same fallback as the kernel, and for the same reason: two
            // architectures in one tree means two files, named after which.
            None => match in_tree(format!("target/busybox-{}", arch.name)) {
                Some(path) => path,
                None => return Ok(None),
            },
        },
    };

    if !path.exists() {
        return Err(format!("no shell at {}", path.display()));
    }
    Ok(Some(path))
}

/// A path under the workspace root, if something is actually there.
fn in_tree(relative: String) -> Option<PathBuf> {
    let path = root().join(relative);
    path.exists().then_some(path)
}

/// `$OXINIT_<NAME>_<ARCH>`, then `$OXINIT_<NAME>`.
///
/// The suffixed form is what makes two architectures usable from one shell
/// session; the bare one is what a single-architecture setup already has.
fn per_arch(arch: Arch, name: &str) -> Option<PathBuf> {
    let suffixed = format!("{name}_{}", arch.name.to_uppercase());

    env::var(&suffixed)
        .or_else(|_| env::var(name))
        .ok()
        .map(PathBuf::from)
}

fn find_kernel(arch: Arch, args: &[String]) -> Result<PathBuf, String> {
    if let Some(path) = flag(args, "--kernel")? {
        return Ok(PathBuf::from(path));
    }

    if let Some(path) = per_arch(arch, "OXINIT_KERNEL") {
        return Ok(path);
    }

    // A kernel is for one architecture too, so the fallback in the tree is
    // named after it rather than being one file two arches fight over.
    if let Some(path) = in_tree(format!("target/vmlinuz-{}", arch.name)) {
        return Ok(path);
    }
    let default = root().join(format!("target/vmlinuz-{}", arch.name));

    Err(format!(
        "no {} kernel image. Pass --kernel PATH, set $OXINIT_KERNEL_{} or \
         $OXINIT_KERNEL, or put one at {}.\nOn Linux the running kernel is \
         usually at /boot/vmlinuz-$(uname -r); Alpine publishes both \
         architectures under releases/<arch>/netboot/.",
        arch.name,
        arch.name.to_uppercase(),
        default.display()
    ))
}

/// Value of `--name VALUE`, if present.
fn flag<'a>(args: &'a [String], name: &str) -> Result<Option<&'a str>, String> {
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        if arg == name {
            return match it.next() {
                Some(value) => Ok(Some(value.as_str())),
                None => Err(format!("{name} needs a value")),
            };
        }
    }
    Ok(None)
}

fn run(command: &mut Command) -> Result<(), String> {
    let name = command.get_program().to_string_lossy().into_owned();

    let status = command
        .stdin(Stdio::inherit())
        .status()
        .map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => format!("{name} not found; is it installed?"),
            _ => format!("run {name}: {e}"),
        })?;

    if !status.success() {
        return Err(format!("{name} failed with {status}"));
    }
    Ok(())
}

fn cargo() -> String {
    env::var("CARGO").unwrap_or_else(|_| "cargo".to_owned())
}

/// The workspace root, derived from this crate's manifest rather than the
/// current directory, so `cargo xtask` works from any subdirectory.
fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .unwrap_or(Path::new("."))
        .to_path_buf()
}

fn shell_quote(path: &Path) -> String {
    format!("'{}'", path.display().to_string().replace('\'', r"'\''"))
}
