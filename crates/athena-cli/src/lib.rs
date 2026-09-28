//! `athena-cli`: make a new Athena agent, and put it on the VM next to the
//! others. It runs on your laptop. The VM only ever pulls what an agent's CI
//! built; setting the VM up runs the same `setup-host.sh` Athena uses, with
//! `--agent`.

pub mod bundle;
pub mod scaffold;

use scaffold::Core;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

pub const USAGE: &str = "usage:
  athena-cli new NAME [--dir DIR] [--core-git URL] [--core-branch BRANCH | --core-rev SHA | --core-path DIR] [--no-git]
      write a new agent's repository (default ./NAME)
  athena-cli vm add NAME [setup-host.sh options, e.g. --vm USER@HOST --dry-run]
      set the agent up on the VM: user, directories, env files, units, CI keys
  athena-cli vm remove NAME [--purge] [setup-host.sh options]
      stop the agent and remove its units and keys (--purge: its data too)
  athena-cli vm doctor NAME [doctor.sh options, e.g. --vm USER@HOST]
      check the agent's setup end to end (read-only)
  athena-cli vm list --vm USER@HOST
      every agent on the VM, with its ports and what is deployed
  athena-cli github init [init-github.sh options, e.g. --vm-host HOST]
      give the current repository's CI its environments and deploy keys
  athena-cli --version";

/// Runs programs. The real one inherits the terminal, so scripts can ask
/// for a sudo password or a typed \"yes\".
pub trait Shell {
    /// The program's exit code.
    fn run(&mut self, program: &str, args: &[String], cwd: Option<&Path>) -> io::Result<i32>;
}

pub struct RealShell;

impl Shell for RealShell {
    fn run(&mut self, program: &str, args: &[String], cwd: Option<&Path>) -> io::Result<i32> {
        let mut command = Command::new(program);
        command.args(args);
        if let Some(dir) = cwd {
            command.current_dir(dir);
        }
        // Killed by a signal: no code, so report failure.
        Ok(command.status()?.code().unwrap_or(1))
    }
}

/// Run `athena-cli` with these arguments. Returns the exit code: the
/// script's for the VM commands, 2 for a usage error.
pub fn main(args: &[String], shell: &mut dyn Shell, out: &mut dyn Write) -> i32 {
    match run(args, shell, out) {
        Ok(code) => code,
        Err(message) => {
            let _ = writeln!(out, "athena-cli: {message}");
            2
        }
    }
}

fn run(args: &[String], shell: &mut dyn Shell, out: &mut dyn Write) -> Result<i32, String> {
    let words: Vec<&str> = args.iter().map(String::as_str).collect();
    match words.as_slice() {
        [] | ["help" | "-h" | "--help"] => {
            say(out, USAGE);
            Ok(0)
        }
        ["--version"] => {
            say(out, &format!("athena-cli {}", env!("CARGO_PKG_VERSION")));
            Ok(0)
        }
        ["new", name, rest @ ..] => new(name, rest, shell, out),
        ["vm", "add", name, rest @ ..] => script(shell, "setup-host.sh", name, &[], rest),
        ["vm", "remove", name, rest @ ..] => {
            script(shell, "setup-host.sh", name, &["--uninstall"], rest)
        }
        ["vm", "doctor", name, rest @ ..] => script(shell, "doctor.sh", name, &[], rest),
        ["vm", "list", "--vm", host] => {
            let args = strings(&["-t", host, "sudo /opt/athena/bin/deploy-gate list"]);
            shell
                .run("ssh", &args, None)
                .map_err(|e| format!("running ssh: {e}"))
        }
        ["github", "init", rest @ ..] => {
            let bundle = bundle::Bundle::write_in(&std::env::temp_dir())
                .map_err(|e| format!("writing the scripts: {e}"))?;
            let mut args = vec![path_string(&bundle.script("init-github.sh"))];
            args.extend(strings(rest));
            shell
                .run("bash", &args, None)
                .map_err(|e| format!("running bash: {e}"))
        }
        _ => Err(USAGE.into()),
    }
}

/// Run one of the bundled VM scripts for agent `name`.
fn script(
    shell: &mut dyn Shell,
    file: &str,
    name: &str,
    extra: &[&str],
    rest: &[&str],
) -> Result<i32, String> {
    check_agent(name)?;
    let bundle = bundle::Bundle::write_in(&std::env::temp_dir())
        .map_err(|e| format!("writing the scripts: {e}"))?;
    let mut args = vec![
        path_string(&bundle.script(file)),
        "--agent".into(),
        name.into(),
    ];
    args.extend(strings(extra));
    args.extend(strings(rest));
    shell
        .run("bash", &args, None)
        .map_err(|e| format!("running bash: {e}"))
}

/// Any agent name the VM accepts, Athena's included.
fn check_agent(name: &str) -> Result<(), String> {
    match scaffold::check_name(name) {
        Err(_) if name == "athena" => Ok(()),
        other => other,
    }
}

struct NewOptions {
    dir: PathBuf,
    core: Core,
    git: bool,
}

/// Which `athena-core` the new agent depends on, before the git URL is
/// known (`--core-git` may come after it).
enum Pin {
    Branch(String),
    Rev(String),
    Path(String),
}

fn parse_new(name: &str, rest: &[&str]) -> Result<NewOptions, String> {
    let mut dir = PathBuf::from(name);
    let mut git_url = scaffold::DEFAULT_GIT.to_string();
    let mut pin = None;
    let mut git = true;
    let mut words = rest.iter();
    while let Some(word) = words.next() {
        let mut value = || {
            words
                .next()
                .map(|v| v.to_string())
                .ok_or_else(|| format!("{word} needs a value"))
        };
        let chosen = match *word {
            "--dir" => {
                dir = PathBuf::from(value()?);
                None
            }
            "--core-git" => {
                git_url = value()?;
                None
            }
            "--core-branch" => Some(Pin::Branch(value()?)),
            "--core-rev" => Some(Pin::Rev(value()?)),
            "--core-path" => Some(Pin::Path(value()?)),
            "--no-git" => {
                git = false;
                None
            }
            other => return Err(format!("unknown option {other}\n{USAGE}")),
        };
        if let Some(chosen) = chosen {
            if pin.is_some() {
                return Err("give one of --core-branch, --core-rev, --core-path".into());
            }
            pin = Some(chosen);
        }
    }
    let core = match pin.unwrap_or_else(|| Pin::Branch("main".into())) {
        Pin::Branch(branch) => Core::Branch {
            git: git_url,
            branch,
        },
        Pin::Rev(rev) => Core::Rev { git: git_url, rev },
        Pin::Path(path) => Core::Path(path),
    };
    Ok(NewOptions { dir, core, git })
}

fn new(
    name: &str,
    rest: &[&str],
    shell: &mut dyn Shell,
    out: &mut dyn Write,
) -> Result<i32, String> {
    scaffold::check_name(name)?;
    let options = parse_new(name, rest)?;
    let dir = &options.dir;
    let written = scaffold::write(dir, name, &options.core).map_err(|e| format!("{e}"))?;
    say(
        out,
        &format!("wrote {} files to {}", written.len(), dir.display()),
    );
    if options.git {
        let args = strings(&["init", "-q", "-b", "main"]);
        match shell.run("git", &args, Some(dir)) {
            Ok(0) => say(out, "git repository initialised (branch main)"),
            Ok(code) => say(
                out,
                &format!("warning: git init exited {code}; run it yourself"),
            ),
            Err(e) => say(
                out,
                &format!("warning: could not run git ({e}); run git init yourself"),
            ),
        }
    }
    // CI builds with --locked, so the repository needs a Cargo.lock.
    match shell.run("cargo", &strings(&["generate-lockfile"]), Some(dir)) {
        Ok(0) => say(out, "Cargo.lock written"),
        Ok(_) | Err(_) => say(
            out,
            "warning: `cargo generate-lockfile` did not finish; run it in the new repository before the first push (CI builds with --locked)",
        ),
    }
    say(out, &next_steps(name, dir));
    Ok(0)
}

fn next_steps(name: &str, dir: &Path) -> String {
    format!(
        "\nNext:
  cd {dir}
  edit prompts/system.md and src/agent.rs, then: cargo test
  push it to GitHub (e.g. gh repo create --private --source . --push)
  athena-cli vm add {name} --vm USER@HOST --dry-run     # read it, then run without --dry-run
  athena-cli github init --vm-host HOST                  # from the new repository
  merge to main: CI deploys staging; tag v0.1.0 to release prod",
        dir = dir.display()
    )
}

fn say(out: &mut dyn Write, line: &str) {
    let _ = writeln!(out, "{line}");
}

fn strings(words: &[&str]) -> Vec<String> {
    words.iter().map(|w| w.to_string()).collect()
}

fn path_string(path: &Path) -> String {
    path.display().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// Records what would run, answers with `code` (or fails to start).
    struct FakeShell {
        calls: Vec<(String, Vec<String>, Option<PathBuf>)>,
        code: io::Result<i32>,
        /// What the bundled script said, read while it still exists.
        script_text: Option<String>,
    }

    impl FakeShell {
        fn answering(code: i32) -> FakeShell {
            FakeShell {
                calls: vec![],
                code: Ok(code),
                script_text: None,
            }
        }

        fn failing() -> FakeShell {
            FakeShell {
                calls: vec![],
                code: Err(io::Error::new(io::ErrorKind::NotFound, "no such program")),
                script_text: None,
            }
        }
    }

    impl Shell for FakeShell {
        fn run(&mut self, program: &str, args: &[String], cwd: Option<&Path>) -> io::Result<i32> {
            if program == "bash" {
                self.script_text = fs::read_to_string(&args[0]).ok();
            }
            self.calls
                .push((program.into(), args.to_vec(), cwd.map(Path::to_path_buf)));
            match &self.code {
                Ok(code) => Ok(*code),
                Err(e) => Err(io::Error::new(e.kind(), e.to_string())),
            }
        }
    }

    fn cli(words: &[&str], shell: &mut FakeShell) -> (i32, String) {
        let mut out = Vec::new();
        let code = main(&strings(words), shell, &mut out);
        (code, String::from_utf8(out).unwrap())
    }

    fn temp(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("athena-cli-lib-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn help_version_and_usage_errors() {
        let mut shell = FakeShell::answering(0);
        for words in [&[][..], &["help"], &["-h"], &["--help"]] {
            let (code, out) = cli(words, &mut shell);
            assert_eq!(code, 0);
            assert!(out.starts_with("usage:"), "{out}");
        }
        let (code, out) = cli(&["--version"], &mut shell);
        assert_eq!(
            (code, out.trim()),
            (
                0,
                format!("athena-cli {}", env!("CARGO_PKG_VERSION")).as_str()
            )
        );
        let (code, out) = cli(&["frobnicate"], &mut shell);
        assert_eq!(code, 2);
        assert!(out.starts_with("athena-cli: usage:"), "{out}");
        assert!(shell.calls.is_empty());
    }

    #[test]
    fn new_writes_the_repository_then_git_and_the_lockfile() {
        let dir = temp("new");
        let mut shell = FakeShell::answering(0);
        let (code, out) = cli(
            &["new", "notes", "--dir", dir.to_str().unwrap()],
            &mut shell,
        );
        assert_eq!(code, 0, "{out}");
        let programs: Vec<&str> = shell.calls.iter().map(|c| c.0.as_str()).collect();
        assert_eq!(programs, ["git", "cargo"]);
        assert_eq!(shell.calls[0].1, ["init", "-q", "-b", "main"]);
        assert_eq!(shell.calls[1].1, ["generate-lockfile"]);
        assert!(
            shell
                .calls
                .iter()
                .all(|c| c.2.as_deref() == Some(dir.as_path()))
        );
        let cargo = fs::read_to_string(dir.join("Cargo.toml")).unwrap();
        assert!(cargo.contains(r#"athena-core = { git = "https://github.com/QueryPlanner/athena", branch = "main" }"#), "{cargo}");
        for line in [
            "wrote 14 files",
            "git repository initialised",
            "Cargo.lock written",
            "athena-cli vm add notes",
        ] {
            assert!(out.contains(line), "{out}");
        }
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn new_pins_the_core_as_asked() {
        let cases: [(&[&str], &str); 4] = [
            (
                &["--core-rev", "abc123"],
                r#"git = "https://github.com/QueryPlanner/athena", rev = "abc123""#,
            ),
            (
                &["--core-branch", "dev", "--core-git", "https://x/y"],
                r#"git = "https://x/y", branch = "dev""#,
            ),
            (&["--core-path", "/src/core"], r#"path = "/src/core""#),
            (
                &["--core-git", "https://x/y"],
                r#"git = "https://x/y", branch = "main""#,
            ),
        ];
        for (i, (flags, dependency)) in cases.into_iter().enumerate() {
            let dir = temp(&format!("pin{i}"));
            let mut words = vec!["new", "notes", "--no-git", "--dir", dir.to_str().unwrap()];
            words.extend_from_slice(flags);
            let mut shell = FakeShell::answering(0);
            let (code, out) = cli(&words, &mut shell);
            assert_eq!(code, 0, "{out}");
            let cargo = fs::read_to_string(dir.join("Cargo.toml")).unwrap();
            assert!(cargo.contains(dependency), "{flags:?}: {cargo}");
            // --no-git: only the lockfile.
            assert_eq!(shell.calls.len(), 1);
            fs::remove_dir_all(&dir).unwrap();
        }
    }

    #[test]
    fn new_refuses_bad_names_options_and_directories() {
        let mut shell = FakeShell::answering(0);
        let cases: [(&[&str], &str); 6] = [
            (&["new", "My-Agent"], "is not an agent name"),
            (&["new", "athena"], "first agent"),
            (&["new", "notes", "--dir"], "--dir needs a value"),
            (&["new", "notes", "--bogus"], "unknown option --bogus"),
            (
                &["new", "notes", "--core-rev", "a", "--core-path", "b"],
                "give one of",
            ),
            (&["new", "notes", "--dir", "/"], "is not empty"),
        ];
        for (words, message) in cases {
            let (code, out) = cli(words, &mut shell);
            assert_eq!(code, 2, "{words:?}");
            assert!(out.contains(message), "{words:?}: {out}");
        }
        assert!(shell.calls.is_empty());
    }

    #[test]
    fn new_warns_when_git_or_cargo_do_not_finish() {
        for (mut shell, git_line) in [
            (FakeShell::answering(1), "git init exited 1"),
            (FakeShell::failing(), "could not run git"),
        ] {
            let dir = temp("warn");
            let (code, out) = cli(
                &["new", "notes", "--dir", dir.to_str().unwrap()],
                &mut shell,
            );
            assert_eq!(code, 0, "{out}");
            assert!(out.contains(git_line), "{out}");
            assert!(out.contains("run it in the new repository"), "{out}");
            fs::remove_dir_all(&dir).unwrap();
        }
    }

    #[test]
    fn vm_commands_run_the_bundled_scripts_for_the_agent() {
        let cases: [(&[&str], &str, &[&str]); 4] = [
            (
                &["vm", "add", "notes", "--vm", "me@vm", "--dry-run"],
                "setup-host.sh",
                &["--agent", "notes", "--vm", "me@vm", "--dry-run"],
            ),
            (
                &["vm", "remove", "notes", "--purge"],
                "setup-host.sh",
                &["--agent", "notes", "--uninstall", "--purge"],
            ),
            (
                &["vm", "doctor", "athena", "--vm", "me@vm"],
                "doctor.sh",
                &["--agent", "athena", "--vm", "me@vm"],
            ),
            (
                &["github", "init", "--vm-host", "vm"],
                "init-github.sh",
                &["--vm-host", "vm"],
            ),
        ];
        for (words, script, args) in cases {
            let mut shell = FakeShell::answering(7);
            let (code, _) = cli(words, &mut shell);
            // The script's exit code is the CLI's.
            assert_eq!(code, 7, "{words:?}");
            let (program, called, _) = &shell.calls[0];
            assert_eq!(program, "bash");
            let ran = called[0].ends_with(&format!("scripts/{script}"));
            assert!(ran, "{called:?}");
            assert_eq!(&called[1..], args);
            // The script was there to run, and is gone afterwards.
            assert!(
                shell
                    .script_text
                    .as_deref()
                    .unwrap()
                    .starts_with("#!/usr/bin/env bash")
            );
            assert!(!Path::new(&called[0]).exists());
        }
    }

    #[test]
    fn vm_list_asks_the_gate_over_ssh() {
        let mut shell = FakeShell::answering(0);
        let (code, _) = cli(&["vm", "list", "--vm", "me@vm"], &mut shell);
        assert_eq!(code, 0);
        assert_eq!(shell.calls[0].0, "ssh");
        assert_eq!(
            shell.calls[0].1,
            ["-t", "me@vm", "sudo /opt/athena/bin/deploy-gate list"]
        );
    }

    #[test]
    fn vm_commands_check_the_name_and_report_a_shell_that_cannot_start() {
        let mut shell = FakeShell::answering(0);
        let (code, out) = cli(&["vm", "add", "../etc"], &mut shell);
        assert_eq!(code, 2);
        assert!(out.contains("is not an agent name"), "{out}");
        for words in [
            &["vm", "add", "notes"][..],
            &["vm", "list", "--vm", "me@vm"],
            &["github", "init"],
        ] {
            let mut shell = FakeShell::failing();
            let (code, out) = cli(words, &mut shell);
            assert_eq!(code, 2, "{words:?}");
            assert!(out.contains("no such program"), "{out}");
        }
    }

    #[test]
    fn the_real_shell_reports_exit_codes_signals_and_missing_programs() {
        let mut shell = RealShell;
        assert_eq!(
            shell.run("sh", &strings(&["-c", "exit 3"]), None).unwrap(),
            3
        );
        let dir = std::env::temp_dir();
        let pwd = shell.run(
            "sh",
            &strings(&["-c", "test \"$PWD\" = \"$(pwd -P)\""]),
            Some(&dir),
        );
        assert_eq!(pwd.unwrap(), 0);
        assert_eq!(
            shell
                .run("sh", &strings(&["-c", "kill -9 $$"]), None)
                .unwrap(),
            1
        );
        assert!(shell.run("/no/such/program", &[], None).is_err());
    }
}
