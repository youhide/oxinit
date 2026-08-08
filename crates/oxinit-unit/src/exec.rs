//! Turning the `exec` string into an argv array.
//!
//! There is no shell involved. No globbing, no environment expansion, no
//! pipes, no redirection, no `&&`. The only substitution is the closed
//! specifier set below, applied before splitting.

use crate::error::ExecError;

/// What the specifiers expand to. Supplied by the caller because two of them
/// are properties of the running system, and this crate does not touch the
/// system.
#[derive(Debug, Clone)]
pub struct Specifiers {
    /// `%n` — the unit name, e.g. `sshd`.
    pub name: String,
    /// `%N` — the fully-qualified name, e.g. `sshd.service`.
    pub full_name: String,
    /// `%H` — the hostname at the time of expansion.
    pub hostname: String,
    /// `%u` — the value of `user`.
    pub user: String,
}

/// Expand specifiers, then split into argv.
pub fn parse(exec: &str, specifiers: &Specifiers) -> Result<Vec<String>, ExecError> {
    let expanded = expand(exec, specifiers)?;
    let argv = split(&expanded)?;

    let program = argv.first().ok_or(ExecError::Empty)?;
    if !program.starts_with('/') {
        return Err(ExecError::NotAbsolute {
            program: program.clone(),
        });
    }

    Ok(argv)
}

/// Substitute `%`-prefixed specifiers.
///
/// An unrecognised specifier is an error. The set is closed: it is not an
/// escape hatch for a template language.
fn expand(input: &str, specifiers: &Specifiers) -> Result<String, ExecError> {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars();

    while let Some(c) = chars.next() {
        if c != '%' {
            out.push(c);
            continue;
        }

        match chars.next() {
            Some('n') => out.push_str(&specifiers.name),
            Some('N') => out.push_str(&specifiers.full_name),
            Some('H') => out.push_str(&specifiers.hostname),
            Some('u') => out.push_str(&specifiers.user),
            Some('%') => out.push('%'),
            Some(other) => return Err(ExecError::UnknownSpecifier { specifier: other }),
            None => return Err(ExecError::TrailingPercent),
        }
    }

    Ok(out)
}

/// Split on whitespace, honouring quotes and backslash escapes.
fn split(input: &str) -> Result<Vec<String>, ExecError> {
    let mut argv = Vec::new();
    let mut current = String::new();
    let mut has_current = false;
    let mut quote: Option<char> = None;
    let mut chars = input.chars();

    while let Some(c) = chars.next() {
        match c {
            '\\' => {
                // Escapes the next character anywhere, including inside
                // quotes, so a literal quote is reachable without switching
                // quoting styles.
                let escaped = chars.next().ok_or(ExecError::TrailingBackslash)?;
                current.push(escaped);
                has_current = true;
            }

            '"' | '\'' if quote.is_none() => {
                quote = Some(c);
                // An empty quoted string is still an argument.
                has_current = true;
            }

            c if quote == Some(c) => quote = None,

            c if c.is_whitespace() && quote.is_none() => {
                if has_current {
                    argv.push(std::mem::take(&mut current));
                    has_current = false;
                }
            }

            c => {
                current.push(c);
                has_current = true;
            }
        }
    }

    if quote.is_some() {
        return Err(ExecError::UnterminatedQuote);
    }
    if has_current {
        argv.push(current);
    }

    Ok(argv)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn specifiers() -> Specifiers {
        Specifiers {
            name: "sshd".to_owned(),
            full_name: "sshd.service".to_owned(),
            hostname: "box".to_owned(),
            user: "root".to_owned(),
        }
    }

    fn argv(exec: &str) -> Vec<String> {
        parse(exec, &specifiers()).unwrap()
    }

    #[test]
    fn splits_on_whitespace() {
        assert_eq!(argv("/usr/sbin/sshd -D"), ["/usr/sbin/sshd", "-D"]);
        assert_eq!(argv("  /bin/true   -a    -b  "), ["/bin/true", "-a", "-b"]);
    }

    #[test]
    fn quotes_group_arguments() {
        assert_eq!(
            argv(r#"/bin/echo "hello world" 'and more'"#),
            ["/bin/echo", "hello world", "and more"]
        );
        // The other quote character is literal inside a quoted run.
        assert_eq!(argv(r#"/bin/echo "it's""#), ["/bin/echo", "it's"]);
        // An empty quoted string is still an argument.
        assert_eq!(argv(r#"/bin/echo "" x"#), ["/bin/echo", "", "x"]);
    }

    #[test]
    fn backslash_escapes_next_character() {
        assert_eq!(argv(r"/bin/echo a\ b"), ["/bin/echo", "a b"]);
        assert_eq!(argv(r#"/bin/echo \"quoted\""#), ["/bin/echo", "\"quoted\""]);
    }

    #[test]
    fn expands_specifiers() {
        assert_eq!(
            argv("/bin/x %n %N %H %u"),
            ["/bin/x", "sshd", "sshd.service", "box", "root"]
        );
        assert_eq!(argv("/bin/x 100%%"), ["/bin/x", "100%"]);
    }

    #[test]
    fn does_not_expand_environment_variables() {
        // $HOME is literal. There is no shell and no environment expansion.
        assert_eq!(argv("/bin/echo $HOME"), ["/bin/echo", "$HOME"]);
    }

    #[test]
    fn rejects_unknown_specifier() {
        assert!(parse("/bin/x %z", &specifiers()).is_err());
        assert!(parse("/bin/x %", &specifiers()).is_err());
    }

    #[test]
    fn rejects_malformed_quoting() {
        assert!(parse(r#"/bin/x "unterminated"#, &specifiers()).is_err());
        assert!(parse(r"/bin/x trailing\", &specifiers()).is_err());
    }

    #[test]
    fn requires_an_absolute_program() {
        assert!(parse("sshd -D", &specifiers()).is_err());
        assert!(parse("./sshd", &specifiers()).is_err());
        assert!(parse("", &specifiers()).is_err());
    }
}
