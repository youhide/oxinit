use thiserror::Error;

/// A unit file that does not parse or does not validate.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum UnitError {
    #[error("unit `{unit}`: {message}")]
    Toml { unit: String, message: String },

    #[error("unit `{unit}`: exec: {source}")]
    Exec {
        unit: String,
        #[source]
        source: ExecError,
    },

    #[error(
        "unit `{unit}` has no [service], [target], or [socket] section; \
         every unit needs exactly one kind"
    )]
    NoKind { unit: String },

    #[error(
        "unit `{unit}` has more than one kind section; \
         use exactly one of [service], [target], [socket]"
    )]
    MultipleKinds { unit: String },

    #[error("unit `{unit}`: listen is empty; a socket that listens on nothing does nothing")]
    EmptyListen { unit: String },

    #[error(
        "unit `{unit}`: cannot listen on `{address}`: expected `address:port` \
         with a literal IP, or an absolute path for a unix socket"
    )]
    ListenAddress { unit: String, address: String },

    #[error("unit `{unit}`: backlog has no meaning for type = \"datagram\"")]
    BacklogOnDatagram { unit: String },

    #[error("unit `{unit}`: watchdog-sec needs type = \"notify\"; nothing else can ping")]
    WatchdogWithoutNotify { unit: String },

    #[error("unit `{unit}`: [resources] applies to services only")]
    ResourcesOnNonService { unit: String },

    #[error("unit `{unit}` both requires and conflicts with `{other}`")]
    RequiresAndConflicts { unit: String, other: String },

    #[error("invalid unit name `{unit}`: expected only letters, digits, `_`, `.`, and `-`")]
    Name { unit: String },

    #[error("read {path}: {message}")]
    Read { path: String, message: String },

    #[error("read directory {path}: {message}")]
    Directory { path: String, message: String },
}

/// A malformed `exec` string.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ExecError {
    #[error("empty")]
    Empty,

    #[error("`{program}` is not an absolute path; there is no PATH lookup and no shell")]
    NotAbsolute { program: String },

    #[error("unknown specifier `%{specifier}`: expected %n, %N, %H, %u, or %%")]
    UnknownSpecifier { specifier: char },

    #[error("trailing `%`")]
    TrailingPercent,

    #[error("unterminated quote")]
    UnterminatedQuote,

    #[error("trailing backslash")]
    TrailingBackslash,
}

/// A malformed duration or size string.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ValueError {
    #[error("empty duration")]
    EmptyDuration,

    #[error("malformed duration `{input}`: expected digits followed by a unit, as in `5s`")]
    DurationSyntax { input: String },

    #[error("duration `{input}` has no unit: write `5s` or `100ms`, not a bare number")]
    DurationSuffix { input: String },

    #[error("unknown duration unit `{unit}`: expected us, ms, s, min, h, or d")]
    DurationUnit { unit: String },

    #[error("duration is too large to represent")]
    DurationOverflow,

    #[error("empty size")]
    EmptySize,

    #[error("malformed size `{input}`: expected digits with an optional K, M, G, or T")]
    SizeSyntax { input: String },

    #[error("unknown size unit `{unit}`: expected K, M, G, or T, uppercase")]
    SizeUnit { unit: String },

    #[error("size is too large to represent")]
    SizeOverflow,
}
