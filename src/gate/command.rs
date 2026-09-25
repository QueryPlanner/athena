//! What `deploy-gate` is asked to do, parsed without touching anything.
//!
//! This is the security boundary. CI reaches the VM only through an SSH
//! forced command, so the text in `SSH_ORIGINAL_COMMAND` is untrusted. It is
//! split on whitespace (there is no shell) and must match one whitelisted
//! command for the key that sent it, word for word.

use std::fmt;

/// A deployment environment. Each SSH key is bound to one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Env {
    Staging,
    Prod,
}

impl Env {
    pub const ALL: [Env; 2] = [Env::Staging, Env::Prod];

    pub fn as_str(self) -> &'static str {
        match self {
            Env::Staging => "staging",
            Env::Prod => "prod",
        }
    }

    fn parse(word: &str) -> Option<Env> {
        Env::ALL.into_iter().find(|env| env.as_str() == word)
    }
}

impl fmt::Display for Env {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A release artifact digest, `sha256:` and 64 lowercase hex digits.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Digest(String);

impl Digest {
    pub fn parse(word: &str) -> Option<Digest> {
        let hex = word.strip_prefix("sha256:")?;
        is_hex64(hex).then(|| Digest(word.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The 64 hex digits, which name the release directory.
    pub fn hex(&self) -> &str {
        &self.0["sha256:".len()..]
    }
}

impl fmt::Display for Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Exactly 64 lowercase hex digits: a digest, or a release directory name.
pub fn is_hex64(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

#[derive(Debug, PartialEq, Eq)]
pub enum Command {
    Deploy { digest: Digest },
    Promote { digest: Digest },
    Smoke(Env),
    Bench,
    Eval,
    Status(Env),
    Restore { env: Env, file: String },
    InstallGate { digest: Digest },
}

impl Command {
    pub fn name(&self) -> &'static str {
        match self {
            Command::Deploy { .. } => "deploy",
            Command::Promote { .. } => "promote",
            Command::Smoke(_) => "smoke",
            Command::Bench => "bench",
            Command::Eval => "eval",
            Command::Status(_) => "status",
            Command::Restore { .. } => "restore",
            Command::InstallGate { .. } => "install-gate",
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum Parsed {
    Version,
    Run(Command),
}

/// Longer than any allowed command; anything past it is refused unread.
pub const MAX_COMMAND_LEN: usize = 256;

const USAGE: &str = "usage: deploy-gate --key-env <staging|prod> (command in SSH_ORIGINAL_COMMAND), \
                     deploy-gate restore <staging|prod> <backup-file>, \
                     deploy-gate install-gate <digest>, or deploy-gate --version";

/// Parse the process arguments and, for a key, `SSH_ORIGINAL_COMMAND`.
///
/// `Err` holds the reason to print after `rejected: `. Nothing has been
/// done when this returns.
pub fn parse(args: &[String], ssh_command: Option<&str>) -> Result<Parsed, String> {
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    match args.as_slice() {
        ["--version"] => Ok(Parsed::Version),
        ["--key-env", key] => {
            let key = Env::parse(key).ok_or_else(|| format!("unknown key env {key:?}"))?;
            let line =
                ssh_command.ok_or("no SSH_ORIGINAL_COMMAND; this key only runs a command")?;
            if line.len() > MAX_COMMAND_LEN {
                return Err(format!("command longer than {MAX_COMMAND_LEN} bytes"));
            }
            let words: Vec<&str> = line.split_whitespace().collect();
            keyed(key, &words).map(Parsed::Run)
        }
        ["--key-env", ..] => Err("--key-env takes exactly one value and nothing else".into()),
        words => admin(words).map(Parsed::Run),
    }
}

/// The commands a CI key may run. The key fixes the environment.
fn keyed(key: Env, words: &[&str]) -> Result<Command, String> {
    use Env::{Prod, Staging};
    let command = match (key, words) {
        (Staging, ["deploy", "staging", digest]) => Command::Deploy {
            digest: parse_digest(digest)?,
        },
        (Staging, ["smoke", "staging"]) => Command::Smoke(Staging),
        (Staging, ["bench", "staging"]) => Command::Bench,
        (Staging, ["eval", "staging"]) => Command::Eval,
        (Staging, ["status", "staging"]) => Command::Status(Staging),
        (Prod, ["promote", "prod", digest]) => Command::Promote {
            digest: parse_digest(digest)?,
        },
        (Prod, ["smoke", "prod"]) => Command::Smoke(Prod),
        (Prod, ["status", "prod"]) => Command::Status(Prod),
        _ => {
            let line = words.join(" ");
            return Err(format!("{line:?} is not allowed for the {key} key"));
        }
    };
    Ok(command)
}

/// The commands only a person with root on the VM may run.
fn admin(words: &[&str]) -> Result<Command, String> {
    match words {
        ["restore", env, file] => Ok(Command::Restore {
            env: Env::parse(env).ok_or_else(|| format!("unknown env {env:?}"))?,
            file: parse_backup_name(file)?,
        }),
        ["install-gate", digest] => Ok(Command::InstallGate {
            digest: parse_digest(digest)?,
        }),
        _ => Err(USAGE.into()),
    }
}

fn parse_digest(word: &str) -> Result<Digest, String> {
    Digest::parse(word).ok_or_else(|| format!("{word:?} is not a digest (sha256:<64 hex>)"))
}

/// A file name inside the env's backups directory. No paths: a name can
/// only select a backup, never point elsewhere.
fn parse_backup_name(word: &str) -> Result<String, String> {
    let ok = word.len() <= 128
        && word.ends_with(".db")
        && !word.starts_with('.')
        && word
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'));
    if ok {
        Ok(word.to_string())
    } else {
        Err(format!("{word:?} is not a backup file name"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HEX: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    fn digest() -> String {
        format!("sha256:{HEX}")
    }

    fn key(env: &str, line: &str) -> Result<Parsed, String> {
        parse(&["--key-env".into(), env.into()], Some(line))
    }

    fn admin_args(list: &[&str]) -> Result<Parsed, String> {
        let args: Vec<String> = list.iter().map(|s| s.to_string()).collect();
        parse(&args, None)
    }

    fn run(command: Command) -> Result<Parsed, String> {
        Ok(Parsed::Run(command))
    }

    #[test]
    fn each_key_runs_exactly_its_whitelist() {
        let d = Digest::parse(&digest()).unwrap();
        let staging = [
            (
                format!("deploy staging {}", digest()),
                Command::Deploy { digest: d.clone() },
            ),
            ("smoke staging".into(), Command::Smoke(Env::Staging)),
            ("bench staging".into(), Command::Bench),
            ("eval staging".into(), Command::Eval),
            ("status staging".into(), Command::Status(Env::Staging)),
        ];
        for (line, command) in staging {
            assert_eq!(key("staging", &line), run(command), "{line}");
        }
        let prod = [
            (
                format!("promote prod {}", digest()),
                Command::Promote { digest: d },
            ),
            ("smoke prod".into(), Command::Smoke(Env::Prod)),
            ("status prod".into(), Command::Status(Env::Prod)),
        ];
        for (line, command) in prod {
            assert_eq!(key("prod", &line), run(command), "{line}");
        }
    }

    #[test]
    fn extra_whitespace_between_words_is_only_a_separator() {
        assert_eq!(
            key("staging", "  status \t staging\n"),
            run(Command::Status(Env::Staging))
        );
    }

    #[test]
    fn a_key_cannot_reach_the_other_env() {
        let d = digest();
        for line in [
            format!("deploy prod {d}"),
            format!("promote prod {d}"),
            "smoke prod".into(),
            "status prod".into(),
        ] {
            let err = key("staging", &line).unwrap_err();
            assert!(err.contains("not allowed for the staging key"), "{err}");
        }
        for line in [
            format!("deploy staging {d}"),
            format!("promote staging {d}"),
            "bench staging".into(),
            "eval staging".into(),
            "bench prod".into(),
            "status staging".into(),
        ] {
            let err = key("prod", &line).unwrap_err();
            assert!(err.contains("not allowed for the prod key"), "{err}");
        }
    }

    #[test]
    fn shell_syntax_and_extra_words_are_refused() {
        let d = digest();
        for line in [
            "deploy prod; rm -rf /".to_string(),
            "status staging; rm -rf /".into(),
            "status staging && id".into(),
            "status staging | sh".into(),
            "status staging`id`".into(),
            "status $(id)".into(),
            format!("deploy staging {d};id"),
            format!("deploy staging {d} extra"),
            format!("deploy staging {d} --force"),
            "status".into(),
            "".into(),
            "   ".into(),
            "restore staging x.db".into(),
            format!("install-gate {d}"),
        ] {
            assert!(key("staging", &line).is_err(), "accepted {line:?}");
        }
    }

    #[test]
    fn a_digest_must_be_sha256_and_64_lowercase_hex_digits() {
        let bad = [
            format!("sha256:{}", HEX.to_uppercase()),
            format!("sha256:{}", &HEX[1..]),
            format!("sha256:{HEX}0"),
            format!("sha512:{HEX}"),
            format!("SHA256:{HEX}"),
            HEX.to_string(),
            format!("sha256:{}g", &HEX[1..]),
            "sha256:".into(),
            "latest".into(),
        ];
        for word in bad {
            let err = key("staging", &format!("deploy staging {word}")).unwrap_err();
            assert!(err.contains("is not a digest"), "{word}: {err}");
            assert!(Digest::parse(&word).is_none());
        }
        let d = Digest::parse(&digest()).unwrap();
        assert_eq!((d.as_str(), d.hex()), (digest().as_str(), HEX));
        assert_eq!(d.to_string(), digest());
    }

    #[test]
    fn the_key_argument_is_exact_and_needs_a_command() {
        assert!(
            key("dev", "status staging")
                .unwrap_err()
                .contains("unknown key env")
        );
        let missing = parse(&["--key-env".into(), "staging".into()], None).unwrap_err();
        assert!(missing.contains("no SSH_ORIGINAL_COMMAND"), "{missing}");
        for args in [
            vec!["--key-env"],
            vec!["--key-env", "staging", "status", "staging"],
            vec!["--key-env", "staging", "--key-env", "prod"],
        ] {
            let err = admin_args(&args).unwrap_err();
            assert!(err.contains("exactly one value"), "{args:?}: {err}");
        }
    }

    #[test]
    fn an_overlong_command_is_refused_unread() {
        let line = format!("status staging{}", " ".repeat(MAX_COMMAND_LEN));
        assert!(key("staging", &line).unwrap_err().contains("longer than"));
    }

    #[test]
    fn a_refusal_quotes_the_command_with_control_characters_escaped() {
        let err = key("staging", "status\u{1b}[2J staging").unwrap_err();
        assert!(err.contains("\\u{1b}"), "{err}");
    }

    #[test]
    fn admin_commands_are_restore_and_install_gate_only() {
        assert_eq!(
            admin_args(&["restore", "prod", "20260925T120000Z.db"]),
            run(Command::Restore {
                env: Env::Prod,
                file: "20260925T120000Z.db".into()
            })
        );
        assert_eq!(
            admin_args(&["install-gate", &digest()]),
            run(Command::InstallGate {
                digest: Digest::parse(&digest()).unwrap()
            })
        );
        assert_eq!(admin_args(&["--version"]), Ok(Parsed::Version));
        for args in [
            vec![],
            vec!["deploy", "staging", &digest()],
            vec!["status", "staging"],
            vec!["restore", "staging"],
            vec!["install-gate"],
            vec!["--version", "x"],
        ] {
            let err = admin_args(&args).unwrap_err();
            assert!(err.starts_with("usage:"), "{args:?}");
        }
        assert!(
            admin_args(&["install-gate", "latest"])
                .unwrap_err()
                .contains("not a digest")
        );
        assert!(
            admin_args(&["restore", "dev", "a.db"])
                .unwrap_err()
                .contains("unknown env")
        );
    }

    #[test]
    fn a_backup_name_cannot_leave_the_backups_directory() {
        for name in [
            "../agent.db",
            "/etc/shadow",
            "a/b.db",
            ".hidden.db",
            "..db",
            "backup.txt",
            "back up.db",
            &format!("{}.db", "a".repeat(126)),
        ] {
            let err = admin_args(&["restore", "staging", name]).unwrap_err();
            assert!(err.contains("is not a backup file name"), "{name}: {err}");
        }
        assert!(admin_args(&["restore", "staging", "pre_restore-1.db"]).is_ok());
    }

    #[test]
    fn commands_have_names_and_envs_display_as_words() {
        let d = Digest::parse(&digest()).unwrap();
        let names: Vec<&str> = [
            Command::Deploy { digest: d.clone() },
            Command::Promote { digest: d.clone() },
            Command::Smoke(Env::Prod),
            Command::Bench,
            Command::Eval,
            Command::Status(Env::Prod),
            Command::Restore {
                env: Env::Prod,
                file: "a.db".into(),
            },
            Command::InstallGate { digest: d },
        ]
        .iter()
        .map(Command::name)
        .collect();
        assert_eq!(
            names,
            [
                "deploy",
                "promote",
                "smoke",
                "bench",
                "eval",
                "status",
                "restore",
                "install-gate"
            ]
        );
        assert_eq!(format!("{} {}", Env::Staging, Env::Prod), "staging prod");
    }
}
