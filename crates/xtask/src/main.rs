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
        "image" => image(&rest).map(|path| println!("{}", path.display())),
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
  build    build oxinit for x86_64-unknown-linux-musl
  image    build and pack an initramfs, printing its path; does not boot
  boot     build, pack an initramfs, and boot it under QEMU

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
    run(Command::new(cargo())
        .args(["build", "--release", "--target", TARGET, "-p", "oxinit"])
        .current_dir(root()))?;

    let binary = root().join("target").join(TARGET).join("release/oxinit");
    if !binary.exists() {
        return Err(format!("expected a binary at {}", binary.display()));
    }
    Ok(binary)
}

/// Build and pack, without booting. Used by `boot`, and on its own by anything
/// that wants to drive QEMU itself.
fn image(args: &[String]) -> Result<PathBuf, String> {
    let shell = find_shell(args)?;
    let binary = build()?;
    pack_initramfs(&binary, shell.as_deref())
}

fn boot(args: &[String]) -> Result<(), String> {
    let kernel = find_kernel(args)?;
    let image = image(args)?;

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
fn pack_initramfs(binary: &Path, shell: Option<&Path>) -> Result<PathBuf, String> {
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
    "dmesg", "kill", "mkdir", "echo",
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
