//! A service that speaks `sd_notify`, for exercising oxinit's notify socket.
//!
//! Not part of the running system. `cargo xtask boot` installs it in the test
//! image so readiness and the watchdog can be verified from inside a guest,
//! which shell tools cannot do — nothing in busybox sends a unix datagram.
//!
//! Usage: `notify-probe [--hang-after SECONDS]`
//!
//! Sends `READY=1` and a `STATUS=`, then pings `WATCHDOG=1` every second. With
//! `--hang-after`, it stops pinging after that many seconds and sleeps
//! forever, which is what a hung service looks like from oxinit's side.

use std::io;
use std::os::unix::net::UnixDatagram;
use std::time::Duration;

fn main() -> io::Result<()> {
    let socket_path =
        std::env::var("NOTIFY_SOCKET").map_err(|_| io::Error::other("NOTIFY_SOCKET is not set"))?;

    let socket = UnixDatagram::unbound()?;
    let send = |message: &str| socket.send_to(message.as_bytes(), &socket_path);

    let hang_after = std::env::args()
        .skip_while(|arg| arg != "--hang-after")
        .nth(1)
        .and_then(|value| value.parse::<u64>().ok());

    send("READY=1\nSTATUS=probe up\n")?;
    println!("notify-probe: sent READY=1");

    let mut elapsed = 0u64;
    loop {
        std::thread::sleep(Duration::from_secs(1));
        elapsed += 1;

        if hang_after.is_some_and(|limit| elapsed > limit) {
            println!("notify-probe: pretending to hang");
            loop {
                std::thread::sleep(Duration::from_secs(3600));
            }
        }

        send("WATCHDOG=1\n")?;
    }
}
