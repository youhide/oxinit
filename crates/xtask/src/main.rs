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

const TARGET: &str = "x86_64-unknown-linux-musl";

fn main() {
    let mut args = env::args().skip(1);
    let cmd = args.next().unwrap_or_default();
    let rest: Vec<String> = args.collect();

    let result = match cmd.as_str() {
        "boot" => boot(&rest),
        "build" => build().map(|path| println!("{}", path.display())),
        "image" => image(&rest, false).map(|path| println!("{}", path.display())),
        "test-boot" => test_boot(&rest),
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

boot options:
  --kernel PATH   kernel image; falls back to $OXINIT_KERNEL, then ./bzImage
  --shell PATH    statically linked shell to place at /bin/sh, normally
                  busybox; falls back to $OXINIT_SHELL. Without one, the image
                  holds only /init and oxinit has no shell to spawn.

Ctrl-A X exits QEMU.";

fn usage() {
    println!("{USAGE}");
}

/// Build a static `oxinit` and return the path to the binary.
fn build() -> Result<PathBuf, String> {
    build_package("oxinit")
}

fn build_package(package: &str) -> Result<PathBuf, String> {
    run(Command::new(cargo())
        .args(["build", "--release", "--target", TARGET, "-p", package])
        .current_dir(root()))?;

    let binary = root()
        .join("target")
        .join(TARGET)
        .join("release")
        .join(package);
    if !binary.exists() {
        return Err(format!("expected a binary at {}", binary.display()));
    }
    Ok(binary)
}

/// Build and pack, without booting. Used by `boot`, and on its own by anything
/// that wants to drive QEMU itself.
fn image(args: &[String], test: bool) -> Result<PathBuf, String> {
    let shell = find_shell(args)?;
    let binary = build()?;
    pack_initramfs(&binary, shell.as_deref(), test)
}

fn boot(args: &[String]) -> Result<(), String> {
    let kernel = find_kernel(args)?;
    let image = image(args, false)?;

    println!(
        "xtask: booting {} with {}",
        image.display(),
        kernel.display()
    );
    println!("xtask: Ctrl-A X to exit QEMU");

    run(Command::new("qemu-system-x86_64").args([
        "-kernel".as_ref(),
        kernel.as_os_str(),
        "-initrd".as_ref(),
        image.as_os_str(),
        "-nographic".as_ref(),
        "-append".as_ref(),
        "console=ttyS0".as_ref(),
    ]))
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
const FORBIDDEN: &[&str] = &["panicked", "Kernel panic", "can't access tty"];

/// Hard limit on the whole boot. A test that hangs has to fail, not block.
const TEST_TIMEOUT: u32 = 90;

/// Boot under QEMU, with a timeout, and assert on what came out of the serial
/// port.
fn test_boot(args: &[String]) -> Result<(), String> {
    let kernel = find_kernel(args)?;
    let image = image(args, true)?;

    let log = root().join("target/test-boot.log");
    let _ = fs::remove_file(&log);

    println!(
        "xtask: booting with a {TEST_TIMEOUT}s limit, logging to {}",
        log.display()
    );

    // `-serial file:` rather than -nographic, so the log is a file this can
    // read rather than this process's own stdout. `-display none` keeps QEMU
    // from opening a window on a developer's machine.
    let mut child = Command::new("qemu-system-x86_64")
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
            "console=ttyS0".as_ref(),
            "-m".as_ref(),
            "512".as_ref(),
        ])
        .spawn()
        .map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => "qemu-system-x86_64 not found; is it installed?".into(),
            _ => format!("run qemu-system-x86_64: {e}"),
        })?;

    let clean = wait_for(&mut child, TEST_TIMEOUT)?;
    let text = fs::read_to_string(&log).map_err(|e| format!("read {}: {e}", log.display()))?;

    check(&text, clean)
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

/// Compare the log against [`EXPECTED`] and [`FORBIDDEN`].
fn check(text: &str, clean: bool) -> Result<(), String> {
    let mut failures = Vec::new();

    if !clean {
        failures.push(format!(
            "qemu had to be killed after {TEST_TIMEOUT}s: the machine never powered off"
        ));
    }

    for (needle, proves) in EXPECTED {
        if text.contains(needle) {
            println!("  ok    {proves}");
        } else {
            failures.push(format!("missing `{needle}` — {proves}"));
        }
    }

    for needle in FORBIDDEN {
        if text.contains(needle) {
            failures.push(format!("log contains `{needle}`"));
        }
    }

    if failures.is_empty() {
        println!("xtask: {} checks passed", EXPECTED.len());
        return Ok(());
    }

    Err(format!(
        "{} check(s) failed:\n  {}",
        failures.len(),
        failures.join("\n  ")
    ))
}

/// Pack the binary as `/init`, optionally with a shell at `/bin/sh`.
///
/// The kernel unpacks an initramfs and runs `/init`, so oxinit alone is a
/// valid image. But M0 supervises a shell, and an image containing only
/// `/init` has no shell to spawn — you get a booting init with no prompt and a
/// spawn error in the log.
///
/// So `--shell PATH` (or `$OXINIT_SHELL`) adds a statically linked shell,
/// normally busybox, at `/bin/sh`. Without it the boot still proves the mount,
/// console, signalfd, and reap paths, which is most of M0.
fn pack_initramfs(binary: &Path, shell: Option<&Path>, test: bool) -> Result<PathBuf, String> {
    let staging = root().join("target/initramfs");
    let _ = fs::remove_dir_all(&staging);
    fs::create_dir_all(&staging).map_err(|e| format!("create {}: {e}", staging.display()))?;

    install(binary, &staging.join("init"))?;

    match shell {
        Some(path) => {
            let bin = staging.join("bin");
            fs::create_dir_all(&bin).map_err(|e| format!("create {}: {e}", bin.display()))?;
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

    // The client. Part of the system, unlike the fixtures below.
    if let Ok(binary) = build_package("oxctl") {
        install(&binary, &staging.join("bin/oxctl"))?;
    }

    // Test fixtures. Only useful in a test image, and only there because
    // nothing in busybox sends a unix datagram or speaks LISTEN_FDS.
    for fixture in ["notify-probe", "listen-probe"] {
        if let Ok(binary) = build_package(fixture) {
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

    let image = root().join("target/oxinit.cpio.gz");

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

/// Applets worth having in a debug shell. busybox is a multi-call binary: it
/// picks its behaviour from argv[0], so without these names `ls` and `cat` do
/// not exist and the shell can only run its own builtins.
const APPLETS: &[&str] = &[
    "cat", "ls", "ps", "sleep", "mount", "umount", "hostname", "grep", "poweroff", "reboot",
    "dmesg", "kill", "mkdir", "echo", "true", "false", "date", "id",
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
fn find_shell(args: &[String]) -> Result<Option<PathBuf>, String> {
    let path = match flag(args, "--shell")? {
        Some(path) => PathBuf::from(path),
        None => match env::var("OXINIT_SHELL") {
            Ok(path) => PathBuf::from(path),
            Err(_) => return Ok(None),
        },
    };

    if !path.exists() {
        return Err(format!("no shell at {}", path.display()));
    }
    Ok(Some(path))
}

fn find_kernel(args: &[String]) -> Result<PathBuf, String> {
    if let Some(path) = flag(args, "--kernel")? {
        return Ok(PathBuf::from(path));
    }

    if let Ok(path) = env::var("OXINIT_KERNEL") {
        return Ok(PathBuf::from(path));
    }

    let default = root().join("bzImage");
    if default.exists() {
        return Ok(default);
    }

    Err(format!(
        "no kernel image. Pass --kernel PATH, set $OXINIT_KERNEL, or put a \
         bzImage at {}.\nOn Linux the running kernel is usually at \
         /boot/vmlinuz-$(uname -r).",
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
