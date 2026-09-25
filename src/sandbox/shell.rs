//! Building shell text that runs exactly the arguments given.
//!
//! execd's `/command` takes `argv` in newer releases, which is never
//! shell-expanded, but the execd behind OpenSandbox 0.2.3 rejects it. Every
//! command is therefore shell text, and every argument Athena puts into one
//! goes through [`quote`]. Inside single quotes a POSIX shell expands
//! nothing, so the only character that needs care is the single quote
//! itself, written as `'\''`: close the quote, an escaped quote, reopen.

use super::Error;

/// `arg` as one shell word that expands to exactly `arg`.
///
/// Fails on a NUL byte, which no argument can carry.
pub fn quote(arg: &str) -> Result<String, Error> {
    if arg.contains('\0') {
        return Err(Error::Invalid("arguments cannot contain a NUL byte".into()));
    }
    Ok(format!("'{}'", arg.replace('\'', r"'\''")))
}

/// Every argument quoted and joined into one command line.
pub fn command_line<S: AsRef<str>>(argv: &[S]) -> Result<String, Error> {
    let words: Result<Vec<String>, Error> = argv.iter().map(|a| quote(a.as_ref())).collect();
    Ok(words?.join(" "))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Strings chosen to break naive quoting: every shell metacharacter,
    /// quotes of both kinds, substitutions, newlines, non-ASCII, and the
    /// empty string.
    fn nasty() -> Vec<String> {
        let mut cases: Vec<String> = [
            "",
            "plain",
            "two words",
            "'",
            "''",
            "it's",
            "\"double\"",
            "$(touch /tmp/pwned)",
            "`id`",
            "${HOME}",
            "$HOME",
            "a\nb\n",
            "\\",
            "\\'",
            "'\\''",
            "; rm -rf / #",
            "&& echo hi || true | cat > /dev/null",
            "*?[a-z]",
            "~",
            "-n",
            "--flag=value",
            "tab\there",
            "é ✓ 日本",
            "!!",
            "\r\n",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        // Every pairing too, so a case that only fails next to another shows up.
        let singles = cases.clone();
        for a in &singles {
            for b in &singles {
                cases.push(format!("{a}{b}"));
            }
        }
        cases
    }

    /// Run `printf '%s\0' <args>` through a real `sh -c` and split the output.
    fn through_sh(line: &str) -> Vec<String> {
        let out = std::process::Command::new("sh")
            .arg("-c")
            .arg(format!("printf '%s\\0' {line}"))
            .output()
            .unwrap();
        assert!(out.status.success(), "{line}: {out:?}");
        let text = String::from_utf8(out.stdout).unwrap();
        let mut words: Vec<String> = text.split('\0').map(str::to_string).collect();
        words.pop(); // after the final NUL
        words
    }

    #[test]
    fn every_quoted_argument_reaches_the_program_unchanged() {
        for case in nasty() {
            assert_eq!(through_sh(&quote(&case).unwrap()), [case]);
        }
    }

    #[test]
    fn a_command_line_keeps_its_arguments_apart() {
        let argv = nasty();
        // In batches, to stay under the OS's argument length limit.
        for batch in argv.chunks(50) {
            assert_eq!(through_sh(&command_line(batch).unwrap()), batch);
        }
    }

    #[test]
    fn a_nul_byte_is_refused() {
        assert!(matches!(quote("a\0b"), Err(Error::Invalid(_))));
        assert!(command_line(&["ok", "a\0b"]).is_err());
    }
}
