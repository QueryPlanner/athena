//! The command-line transport: argument parsing, output and the REPL, over
//! [`Service`]. Nothing here touches the database directly.
//!
//! Input and output are parameters rather than stdin/stdout so tests drive
//! the CLI in-process. The agent is built lazily through `make_agent`, so
//! commands that never run a turn work without an API key.

use crate::runner::Run;
use crate::service::{Service, User};
use anyhow::{Result, bail};
use std::io::{BufRead, Write};

pub const USAGE: &str = "usage: athena [--user TRANSPORT:ID] COMMAND

commands:
  [SESSION [PROMPT]]   chat in SESSION (default `default`), created if new;
                       with PROMPT, run one turn and print the reply
  sessions             list your sessions: name, message count
  sessions new NAME    create an empty session; prints name and id
  usage                token totals per session
  serve [--addr ADDR]  run the HTTP API (default 127.0.0.1:8080, or
                       ATHENA_ADDR); unauthenticated, see README
  telegram             run the Telegram bot (TELEGRAM_BOT_TOKEN; see README)
  backup DEST          copy the database to the new file DEST, online
  --version            print the version

serve and telegram need ATHENA_DB set to an absolute path.

--user acts as another user, e.g. telegram:42. The default is cli:local.";

/// The user the CLI acts as unless `--user` says otherwise.
pub const DEFAULT_USER: (&str, &str) = ("cli", "local");

#[derive(Debug, PartialEq, Eq)]
enum Command<'a> {
    Sessions,
    NewSession(&'a str),
    Usage,
    Chat {
        session: &'a str,
        prompt: Option<&'a str>,
    },
}

/// Split `args` into the user to act as and the command to run.
fn parse(args: &[String]) -> Result<((&str, &str), Command<'_>)> {
    let (user, rest) = match args {
        [flag, spec, rest @ ..] if flag == "--user" => (parse_user(spec)?, rest),
        [flag] if flag == "--user" => bail!("--user needs TRANSPORT:ID\n\n{USAGE}"),
        rest => (DEFAULT_USER, rest),
    };
    let command = match rest {
        [cmd] if cmd == "sessions" => Command::Sessions,
        [cmd, sub, name] if cmd == "sessions" && sub == "new" => Command::NewSession(name),
        [cmd, ..] if cmd == "sessions" => bail!("unknown `sessions` command\n\n{USAGE}"),
        [cmd, ..] if cmd == "usage" => Command::Usage,
        [] => Command::Chat {
            session: "default",
            prompt: None,
        },
        [session, rest @ ..] => Command::Chat {
            session,
            prompt: rest.first().map(String::as_str),
        },
    };
    Ok((user, command))
}

/// `TRANSPORT:ID`. The id may itself contain `:`; the transport may not.
fn parse_user(spec: &str) -> Result<(&str, &str)> {
    match spec.split_once(':') {
        Some((transport, id)) if !transport.is_empty() && !id.is_empty() => Ok((transport, id)),
        _ => bail!("--user takes TRANSPORT:ID, such as telegram:42; got `{spec}`"),
    }
}

/// Run one CLI invocation. See [`USAGE`] for the commands.
pub async fn run<R: Run>(
    args: &[String],
    service: &Service,
    make_agent: impl FnOnce() -> Result<R>,
    input: impl BufRead,
    out: &mut impl Write,
) -> Result<()> {
    let ((transport, external_id), command) = parse(args)?;

    let session = match command {
        Command::Sessions => {
            let user = service.user(transport, external_id).await?;
            for s in service.sessions(&user).await? {
                writeln!(out, "{}\t{} messages", s.session.name, s.messages)?;
            }
            return Ok(());
        }
        Command::NewSession(name) => {
            let user = service.user(transport, external_id).await?;
            let session = service.create_session(&user, name).await?;
            writeln!(out, "{}\t{}", session.name, session.id)?;
            return Ok(());
        }
        Command::Usage => {
            let user = service.user(transport, external_id).await?;
            writeln!(out, "session\truns\tcalls\tin\tout\tcached")?;
            for u in service.usage(&user).await? {
                writeln!(
                    out,
                    "{}\t{}\t{}\t{}\t{}\t{}",
                    u.name,
                    u.runs,
                    u.model_calls,
                    u.input_tokens,
                    u.output_tokens,
                    u.cached_input_tokens
                )?;
            }
            return Ok(());
        }
        Command::Chat { session, prompt } => (session, prompt),
    };

    // Before any write: a missing API key must leave the database untouched.
    let agent = make_agent()?;
    let user = service.user(transport, external_id).await?;
    let (name, prompt) = session;
    let session = service.open_session(&user, name).await?;
    eprintln!(
        "session `{name}` — {} messages restored",
        service.history(&user, &session.id).await?.len()
    );

    let chat = Chat {
        service,
        agent: &agent,
        user: &user,
        session_id: &session.id,
    };
    match prompt {
        Some(prompt) => writeln!(out, "{}", chat.send(prompt).await?)?,
        None => chat.repl(input, out).await?,
    }
    Ok(())
}

struct Chat<'a, R> {
    service: &'a Service,
    agent: &'a R,
    user: &'a User,
    session_id: &'a str,
}

impl<R: Run> Chat<'_, R> {
    async fn send(&self, prompt: &str) -> Result<String> {
        let turn = self
            .service
            .send(self.agent, self.user, self.session_id, prompt)
            .await?;
        Ok(turn.reply)
    }

    /// Read prompts until `exit` or end of input. Blank lines are skipped.
    async fn repl(&self, mut input: impl BufRead, out: &mut impl Write) -> Result<()> {
        loop {
            write!(out, "> ")?;
            out.flush()?;
            let mut line = String::new();
            if input.read_line(&mut line)? == 0 {
                break;
            }
            let prompt = line.trim();
            if prompt.is_empty() {
                continue;
            }
            if prompt == "exit" {
                break;
            }
            writeln!(out, "{}\n", self.send(prompt).await?)?;
        }
        Ok(())
    }
}

/// How the CLI reports a problem that did not fail the command.
pub fn warn(message: &str) {
    report(&mut std::io::stderr(), message);
}

fn report(to: &mut impl Write, message: &str) {
    // Nowhere left to report a failure to write a warning to stderr.
    let _ = writeln!(to, "warning: {message}");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    fn parsed(list: &[&str]) -> ((String, String), String) {
        let a = args(list);
        let ((t, id), command) = parse(&a).unwrap();
        ((t.into(), id.into()), format!("{command:?}"))
    }

    #[test]
    fn commands_parse_with_and_without_a_user() {
        let local = ("cli".to_string(), "local".to_string());
        let tg = ("telegram".to_string(), "42".to_string());
        for (list, user, command) in [
            (
                &[][..],
                &local,
                r#"Chat { session: "default", prompt: None }"#,
            ),
            (&["s"][..], &local, r#"Chat { session: "s", prompt: None }"#),
            (
                &["s", "hi", "ignored"][..],
                &local,
                r#"Chat { session: "s", prompt: Some("hi") }"#,
            ),
            (&["sessions"][..], &local, "Sessions"),
            (&["sessions", "new", "n"][..], &local, r#"NewSession("n")"#),
            (&["usage"][..], &local, "Usage"),
            (&["--user", "telegram:42", "usage"][..], &tg, "Usage"),
            (
                &["--user", "telegram:42"][..],
                &tg,
                r#"Chat { session: "default", prompt: None }"#,
            ),
        ] {
            let expected = (user.clone(), command.to_string());
            assert_eq!(parsed(list), expected, "{list:?}");
        }
    }

    #[test]
    fn a_user_id_may_contain_colons_but_neither_half_may_be_empty() {
        assert_eq!(parse_user("http:a:b").unwrap(), ("http", "a:b"));
        for bad in ["telegram", ":42", "telegram:", ""] {
            let err = parse_user(bad).unwrap_err().to_string();
            assert!(err.contains("TRANSPORT:ID"), "{bad}: {err}");
        }
    }

    #[test]
    fn malformed_commands_are_refused_with_the_usage() {
        for list in [
            &["--user"][..],
            &["sessions", "new"][..],
            &["sessions", "delete", "x"][..],
            &["--user", "nope", "sessions"][..],
        ] {
            let err = parse(&args(list)).unwrap_err().to_string();
            let explained = err.contains("TRANSPORT:ID") || err.contains("usage:");
            assert!(explained, "{list:?}: {err}");
        }
    }

    #[test]
    fn warnings_are_labelled() {
        let mut out = Vec::new();
        report(&mut out, "run telemetry not saved");
        assert_eq!(out, b"warning: run telemetry not saved\n");
        warn("this one goes to stderr");
    }
}
