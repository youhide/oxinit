//! The control protocol, shared by `oxinit` and `oxctl`.
//!
//! One crate rather than two copies of the same shapes, so the two sides
//! cannot drift: the wire format is a type, not a convention written down
//! twice.
//!
//! JSON, for now. It is inspectable with `socat` while the message set is
//! still changing, which matters more than bytes on the wire for a socket
//! carrying a few messages a minute. `postcard` is the likely replacement once
//! it stops changing, and this crate is where that change happens.
//!
//! The socket is `SOCK_SEQPACKET`, so a message is exactly one `send` and one
//! `recv`. There is no length prefix and no partial-message state, because
//! there is no stream to frame.
//!
//! This runs inside PID 1, so a panic here is a panic in PID 1.

#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use serde::{Deserialize, Serialize};

/// Where oxinit listens. Mode `0600`, owned by root, in a root-owned
/// directory: access to this socket is full control of the machine, and the
/// mode is the whole authorization story.
pub const CONTROL_PATH: &str = "/run/oxinit/control.sock";

/// The largest message either side will send or accept.
///
/// A bound rather than a guess: `SOCK_SEQPACKET` truncates a datagram that
/// does not fit, and silently acting on half a request is worse than refusing
/// it. Both sides use the same number so neither can send something the other
/// will cut in half.
pub const MAX_MESSAGE: usize = 64 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Request {
    /// Every unit oxinit knows about.
    List,
    /// One unit, in more detail.
    Status {
        unit: String,
    },
    Start {
        unit: String,
    },
    Stop {
        unit: String,
    },
    Restart {
        unit: String,
    },
    /// Re-read the unit directories. Does not restart anything.
    Reload,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Response {
    Units(Vec<UnitStatus>),
    /// The request was accepted. Not that it finished — a stop is not over
    /// until the unit's cgroup empties, and oxinit does not hold the
    /// connection open waiting for that.
    Accepted {
        message: String,
    },
    Error {
        message: String,
    },
}

/// What oxinit knows about one unit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct UnitStatus {
    pub name: String,
    /// `service`, `target`, or `socket`.
    pub kind: String,
    pub description: String,
    pub state: String,
    /// The process oxinit forked, when there is one. A `forking` service that
    /// is up has none: it is tracked by its cgroup.
    pub pid: Option<u32>,
    pub restarts: u32,
    /// The last `STATUS=` the service sent.
    pub status: Option<String>,
    /// `memory.current`, when the unit has a cgroup and the controller is on.
    pub memory: Option<u64>,
    /// `pids.current`, likewise.
    pub tasks: Option<u64>,
}

/// Encode a message, refusing one too large to survive the wire.
///
/// Checked on the sending side so an oversized message is an error where it
/// can be reported, rather than a truncation the receiver has to guess at.
pub fn encode<T: Serialize>(value: &T) -> Result<Vec<u8>, Error> {
    let bytes = serde_json::to_vec(value).map_err(|e| Error::Encode(e.to_string()))?;

    if bytes.len() > MAX_MESSAGE {
        return Err(Error::TooLarge {
            size: bytes.len(),
            limit: MAX_MESSAGE,
        });
    }
    Ok(bytes)
}

pub fn decode<T: for<'de> Deserialize<'de>>(bytes: &[u8]) -> Result<T, Error> {
    serde_json::from_slice(bytes).map_err(|e| Error::Decode(e.to_string()))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    Encode(String),
    Decode(String),
    TooLarge { size: usize, limit: usize },
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Encode(message) => write!(f, "encode: {message}"),
            Error::Decode(message) => write!(f, "decode: {message}"),
            Error::TooLarge { size, limit } => {
                write!(f, "message is {size} bytes, over the {limit} byte limit")
            }
        }
    }
}

impl std::error::Error for Error {}

#[cfg(test)]
mod tests {
    use super::*;

    fn status(name: &str) -> UnitStatus {
        UnitStatus {
            name: name.to_owned(),
            kind: "service".to_owned(),
            description: "a service".to_owned(),
            state: "active".to_owned(),
            pid: Some(42),
            restarts: 0,
            status: Some("serving".to_owned()),
            memory: Some(1024),
            tasks: Some(3),
        }
    }

    #[test]
    fn requests_round_trip() {
        for request in [
            Request::List,
            Request::Reload,
            Request::Status {
                unit: "sshd".to_owned(),
            },
            Request::Start {
                unit: "sshd".to_owned(),
            },
            Request::Stop {
                unit: "sshd".to_owned(),
            },
            Request::Restart {
                unit: "sshd".to_owned(),
            },
        ] {
            let bytes = encode(&request).unwrap();
            assert_eq!(decode::<Request>(&bytes).unwrap(), request);
        }
    }

    #[test]
    fn responses_round_trip() {
        for response in [
            Response::Units(vec![status("a"), status("b")]),
            Response::Accepted {
                message: "starting sshd".to_owned(),
            },
            Response::Error {
                message: "no such unit".to_owned(),
            },
        ] {
            let bytes = encode(&response).unwrap();
            assert_eq!(decode::<Response>(&bytes).unwrap(), response);
        }
    }

    #[test]
    fn the_wire_form_is_the_one_you_would_type() {
        // Inspectable with socat is the reason JSON is here at all, so the
        // shape is part of the contract rather than an accident of derive.
        let bytes = encode(&Request::Start {
            unit: "sshd".to_owned(),
        })
        .unwrap();

        assert_eq!(
            String::from_utf8(bytes).unwrap(),
            r#"{"start":{"unit":"sshd"}}"#
        );

        let bytes = encode(&Request::List).unwrap();
        assert_eq!(String::from_utf8(bytes).unwrap(), r#""list""#);
    }

    #[test]
    fn an_oversized_message_is_refused_by_the_sender() {
        let huge = Response::Error {
            message: "x".repeat(MAX_MESSAGE),
        };

        // Better here, where it can be reported, than as a truncation the
        // other side has to guess at.
        assert!(matches!(encode(&huge), Err(Error::TooLarge { .. })));
    }

    #[test]
    fn garbage_does_not_decode() {
        assert!(decode::<Request>(b"not json").is_err());
        assert!(decode::<Request>(b"{}").is_err());
        assert!(decode::<Request>(b"").is_err());
        // A known-shaped message with an unknown variant is still an error.
        assert!(decode::<Request>(br#"{"explode":{"unit":"x"}}"#).is_err());
    }
}
