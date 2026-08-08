//! The `sd_notify` wire format.
//!
//! A runtime contract, not a config format — the distinction argued in README
//! and ARCHITECTURE. Daemons already speak this, so implementing it makes
//! unmodified software report readiness correctly.
//!
//! The socket lives in the `oxinit` binary; the parsing lives here, because
//! this is the part worth testing without a VM and the binary does not compile
//! off Linux.

/// One message from a service.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    /// The sending process, from `SCM_CREDENTIALS`. Unforgeable by the sender.
    pub pid: u32,
    pub ready: bool,
    pub watchdog: bool,
    pub status: Option<String>,
}

/// Parse a datagram body into a message.
///
/// Unknown keys are ignored: the protocol is meant to be extended by senders
/// that do not know what the receiver supports.
pub fn parse(pid: u32, body: &[u8]) -> Option<Message> {
    let text = std::str::from_utf8(body).ok()?;

    let mut message = Message {
        pid,
        ready: false,
        watchdog: false,
        status: None,
    };

    for line in text.lines() {
        match line.split_once('=') {
            Some(("READY", "1")) => message.ready = true,
            Some(("WATCHDOG", "1")) => message.watchdog = true,
            Some(("STATUS", value)) => {
                // Bounded: this string is held for the lifetime of the service
                // and comes from the service itself.
                message.status = Some(value.chars().take(256).collect());
            }
            _ => {}
        }
    }

    Some(message)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parsed(body: &str) -> Message {
        parse(42, body.as_bytes()).unwrap()
    }

    #[test]
    fn reads_the_documented_keys() {
        let message = parsed("READY=1\nSTATUS=serving\nWATCHDOG=1\n");
        assert!(message.ready);
        assert!(message.watchdog);
        assert_eq!(message.status.as_deref(), Some("serving"));
        assert_eq!(message.pid, 42);
    }

    #[test]
    fn ignores_unknown_keys() {
        let message = parsed("READY=1\nMAINPID=99\nSOMETHING=else\n");
        assert!(message.ready);
    }

    #[test]
    fn ready_must_be_exactly_one() {
        assert!(!parsed("READY=0\n").ready);
        assert!(!parsed("READY=yes\n").ready);
    }

    #[test]
    fn status_may_contain_equals_signs() {
        assert_eq!(
            parsed("STATUS=a=b=c").status.as_deref(),
            Some("a=b=c"),
            "only the first `=` separates key from value"
        );
    }

    #[test]
    fn status_is_bounded() {
        let long = format!("STATUS={}", "x".repeat(10_000));
        assert_eq!(parsed(&long).status.map(|s| s.chars().count()), Some(256));
    }

    #[test]
    fn rejects_non_utf8() {
        assert!(parse(1, &[0xff, 0xfe]).is_none());
    }
}
