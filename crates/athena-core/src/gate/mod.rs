//! `deploy-gate`: the only program CI can run on the VM.
//!
//! CI connects over SSH with a key whose `authorized_keys` entry forces
//! `sudo /opt/athena/bin/deploy-gate --key-env <env>`; the command CI asked
//! for arrives in `SSH_ORIGINAL_COMMAND`. [`command::parse`] checks it
//! against the key's whitelist before anything happens. An admin with root
//! runs `restore` and `install-gate` directly. The protocol is in
//! `plans/contracts.md` ("deploy-gate protocol") and `DEPLOY.md`.
//!
//! Output: progress lines on stderr, then one JSON line on stdout. Exit
//! codes: 0 done, 1 the operation failed, 2 the input was rejected.
//!
//! Every path is under [`Layout`]'s root (`/` in production) and every
//! other side effect goes through [`System`], so the tests run the real
//! logic against a temporary directory and a fake VM.

mod command;
mod deploy;
mod envfile;
mod http;
mod layout;
mod ops;
mod sys;
#[cfg(test)]
mod testing;
mod time;

pub use command::{AgentCommand, Command, Digest, Env, Parsed, parse};
pub use layout::{Agent, Layout};
pub use sys::{Cmd, Output, RealSystem, Request, Response, System};

use serde_json::{Value, json};
use std::fmt;
use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{self, Write};
use std::net::SocketAddr;
use std::os::unix::fs::{OpenOptionsExt, chown};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// `deploy staging` refuses to start with less free space than this.
pub const MIN_FREE_BYTES: u64 = 1 << 30;
pub const HEALTH_TIMEOUT: Duration = Duration::from_secs(60);
const POLL_INTERVAL: Duration = Duration::from_secs(1);
/// How long a held lock is retried before it counts as another gate.
const LOCK_WAIT: Duration = Duration::from_secs(1);
const LOCK_RETRY: Duration = Duration::from_millis(10);
/// How long one health or version request may take.
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, PartialEq, Eq)]
pub enum Failure {
    /// The input was not acceptable (exit 2). Nothing was changed.
    Rejected(String),
    /// The operation failed (exit 1). Its message says what state it left.
    Failed(String),
}

impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Failure::Rejected(m) | Failure::Failed(m) => f.write_str(m),
        }
    }
}

pub type Result<T> = std::result::Result<T, Failure>;

fn failed(message: impl Into<String>) -> Failure {
    Failure::Failed(message.into())
}

/// Turn an I/O error into a failure that says what was being done.
trait Context<T> {
    fn context(self, what: impl fmt::Display) -> Result<T>;
}

impl<T> Context<T> for io::Result<T> {
    fn context(self, what: impl fmt::Display) -> Result<T> {
        self.map_err(|e| failed(format!("{what}: {e}")))
    }
}

/// Run `deploy-gate` with these arguments. Returns the exit code.
pub fn main(args: &[String], ssh_command: Option<&str>, gate: &Gate, out: &mut dyn Write) -> i32 {
    let command = match parse(args, ssh_command) {
        Ok(Parsed::Version) => {
            let _ = writeln!(out, "deploy-gate {}", env!("CARGO_PKG_VERSION"));
            return 0;
        }
        Ok(Parsed::Run(request)) => request,
        Err(reason) => {
            return finish(
                gate,
                out,
                2,
                json!({"rejected": reason}),
                &format!("rejected: {reason}"),
            );
        }
    };
    let name = command.command.name();
    match gate.execute(&command) {
        Ok(mut result) => {
            result["ok"] = json!(true);
            result["command"] = json!(name);
            let _ = writeln!(out, "{result}");
            0
        }
        Err(Failure::Rejected(reason)) => {
            let body = json!({"command": name, "rejected": reason});
            finish(gate, out, 2, body, &format!("rejected: {reason}"))
        }
        Err(Failure::Failed(error)) => {
            let body = json!({"command": name, "error": error});
            finish(gate, out, 1, body, &format!("failed: {error}"))
        }
    }
}

fn finish(gate: &Gate, out: &mut dyn Write, code: i32, mut body: Value, line: &str) -> i32 {
    gate.say(line);
    body["ok"] = json!(false);
    let _ = writeln!(out, "{body}");
    code
}

/// `deploy-gate`'s logic, bound to a root directory and a system.
pub struct Gate<'a> {
    layout: Layout,
    sys: &'a dyn System,
}

/// What the gate needs from an env file.
struct Settings {
    vars: envfile::Vars,
    /// `ATHENA_ADDR`: where that env's `athena serve` listens.
    addr: SocketAddr,
}

impl<'a> Gate<'a> {
    pub fn new(root: impl Into<PathBuf>, sys: &'a dyn System) -> Gate<'a> {
        Gate {
            layout: Layout::new(root),
            sys,
        }
    }

    /// Run `request` for the agent it names. Every path, unit and user it
    /// touches is that agent's.
    pub fn execute(&self, request: &AgentCommand) -> Result<Value> {
        let gate = Gate {
            layout: Layout::for_agent(self.layout.root(), request.agent.clone()),
            sys: self.sys,
        };
        gate.require_known()?;
        gate.run(&request.command)
    }

    /// Athena always exists; any other agent only once setup-host has
    /// written its `agent.env`, which only root can.
    fn require_known(&self) -> Result<()> {
        let agent = self.layout.agent();
        if agent.is_legacy() || self.layout.agent_env().is_file() {
            return Ok(());
        }
        Err(Failure::Rejected(format!(
            "unknown agent {agent}: {} does not exist",
            self.layout.agent_env().display()
        )))
    }

    fn run(&self, command: &Command) -> Result<Value> {
        match command {
            Command::Deploy { digest } => self.deploy(Env::Staging, digest),
            Command::Promote { digest } => self.deploy(Env::Prod, digest),
            Command::Smoke(env) => self.smoke(*env),
            Command::Bench => self.bench(),
            Command::Eval => self.eval(),
            Command::Status(env) => self.status(*env),
            Command::Restore { env, file } => self.restore(*env, file),
            Command::InstallGate { digest } => self.install_gate(digest),
            Command::List => self.list(),
        }
    }

    fn say(&self, line: impl AsRef<str>) {
        self.sys.say(line.as_ref());
    }

    /// Print a child's output as indented progress lines.
    fn relay(&self, text: &str) {
        for line in text.lines() {
            self.say(format!("  | {line}"));
        }
    }

    fn exec(&self, cmd: &Cmd) -> Result<Output> {
        self.say(format!("$ {}", cmd.line()));
        self.sys
            .run(cmd)
            .context(format!("running {}", cmd.program))
    }

    /// Run `cmd` and require it to succeed.
    fn exec_ok(&self, cmd: &Cmd) -> Result<Output> {
        let out = self.exec(cmd)?;
        if out.success {
            Ok(out)
        } else {
            self.relay(&out.stderr);
            Err(failed(format!(
                "`{}` failed: {}",
                cmd.line(),
                out.stderr.trim()
            )))
        }
    }

    /// A release binary, run as the service user with the env file's
    /// variables, in the env's data directory.
    fn athena_cmd(&self, env: Env, settings: &Settings, program: &Path, args: &[&str]) -> Cmd {
        Cmd::new(program, args)
            .as_user(&self.layout.user())
            .with_env(settings.vars.clone())
            .in_dir(self.layout.data(env))
    }

    fn settings(&self, env: Env) -> Result<Settings> {
        let path = self.layout.env_file(env);
        let vars = envfile::read(&path)?;
        let addr = envfile::get(&vars, "ATHENA_ADDR")
            .and_then(|a| a.parse::<SocketAddr>().ok())
            .ok_or_else(|| failed(format!("{} needs ATHENA_ADDR=<ip>:<port>", path.display())))?;
        // The gate sends `Host: <ATHENA_ADDR>`; a server that refuses it
        // would fail every health check and roll every deploy back.
        if let Some(hosts) = envfile::get(&vars, "ATHENA_ALLOWED_HOSTS") {
            let (full, ip) = (addr.to_string(), addr.ip().to_string());
            if !hosts
                .split(',')
                .map(str::trim)
                .any(|h| h == full || h == ip)
            {
                let file = path.display();
                return Err(failed(format!(
                    "{file}: ATHENA_ALLOWED_HOSTS must include {full}"
                )));
            }
        }
        Ok(Settings { vars, addr })
    }

    /// Held for the whole operation; the lock is released when the file
    /// closes, even if the process dies.
    fn lock(&self) -> Result<File> {
        self.lock_at(&self.layout.lock())
    }

    fn lock_at(&self, path: &Path) -> Result<File> {
        let what = format!("locking {}", path.display());
        fs::create_dir_all(path.parent().unwrap_or(self.layout.root())).context(&what)?;
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(path)
            .context(&what)?;
        // A child process that any thread spawns shares this open file
        // until its exec closes it, so a lock just released elsewhere can
        // look held for a moment. Only contention that outlasts
        // LOCK_WAIT is another deploy-gate.
        let started = Instant::now();
        loop {
            match file.try_lock() {
                Ok(()) => return Ok(file),
                Err(TryLockError::WouldBlock) if started.elapsed() < LOCK_WAIT => {
                    std::thread::sleep(LOCK_RETRY);
                }
                Err(e) => {
                    return Err(failed(format!(
                        "{what}: {e} (another deploy-gate is running)"
                    )));
                }
            }
        }
    }

    fn check_disk(&self) -> Result<()> {
        for dir in [self.layout.releases(), self.layout.athena_data()] {
            fs::create_dir_all(&dir).context(format!("creating {}", dir.display()))?;
            let free = self
                .sys
                .free_bytes(&dir)
                .context(format!("checking free space on {}", dir.display()))?;
            if free < MIN_FREE_BYTES {
                let mib = free >> 20;
                let dir = dir.display();
                return Err(failed(format!("only {mib} MiB free on {dir}; need 1 GiB")));
            }
        }
        Ok(())
    }

    /// `systemctl <args>`, which must succeed.
    fn systemctl(&self, args: &[&str]) -> Result<()> {
        self.exec_ok(&Cmd::new(sys::SYSTEMCTL, args)).map(drop)
    }

    fn telegram_enabled(&self, env: Env) -> Result<bool> {
        let unit = self.layout.telegram_unit(env);
        let cmd = Cmd::new(sys::SYSTEMCTL, &["is-enabled", "--quiet", &unit]);
        Ok(self.exec(&cmd)?.success)
    }

    /// Stop the target and both units by name: stopping a target alone
    /// leaves units it only `Wants=` running.
    fn stop_units(&self, env: Env) -> Result<()> {
        let target = self.layout.target(env);
        let serve = self.layout.serve_unit(env);
        let telegram = self.layout.telegram_unit(env);
        self.systemctl(&["stop", &target, &serve, &telegram])
    }

    /// Start serve, then telegram if it is enabled for this env.
    fn start_units(&self, env: Env) -> Result<()> {
        self.systemctl(&["start", &self.layout.serve_unit(env)])?;
        if self.telegram_enabled(env)? {
            self.systemctl(&["start", &self.layout.telegram_unit(env)])?;
        }
        Ok(())
    }

    /// Start the units after a failure, and say so if that fails too.
    fn restart_after(&self, env: Env, cause: Failure) -> Failure {
        self.say(format!("{cause}; starting the units again"));
        match self.start_units(env) {
            Ok(()) => cause,
            Err(e) => failed(format!("{cause}; starting the units again failed too: {e}")),
        }
    }

    fn get(&self, addr: SocketAddr, path: &str) -> std::result::Result<Response, String> {
        let request = Request {
            method: "GET",
            addr,
            path: path.into(),
            headers: vec![],
            body: None,
            timeout: PROBE_TIMEOUT,
        };
        self.sys
            .http(&request)
            .map_err(|e| format!("GET {path}: {e}"))
    }

    /// `/health` is 200 and `/version` reports `expected` (any version
    /// when `None`). Returns the reported version.
    fn probe(
        &self,
        addr: SocketAddr,
        expected: Option<&str>,
    ) -> std::result::Result<String, String> {
        let health = self.get(addr, "/health")?;
        if health.status != 200 {
            return Err(format!("GET /health returned {}", health.status));
        }
        // Releases from before agents had names do not say; any that does
        // must be this agent, not another one configured onto its port.
        let agent = self.layout.agent().as_str();
        if let Some(answered) = serde_json::from_str::<Value>(&health.body)
            .ok()
            .and_then(|b| b["agent"].as_str().map(str::to_string))
            .filter(|answered| answered != agent)
        {
            return Err(format!("/health is agent {answered:?}, not {agent:?}"));
        }
        let version = self.get(addr, "/version")?;
        let reported = serde_json::from_str::<Value>(&version.body)
            .ok()
            .filter(|_| version.status == 200)
            .and_then(|v| v["version"].as_str().map(str::to_string))
            .ok_or(format!(
                "GET /version returned {} {}",
                version.status, version.body
            ))?;
        match expected {
            Some(want) if want != reported => {
                Err(format!("/version reports {reported:?}, expected {want:?}"))
            }
            _ => Ok(reported),
        }
    }

    /// Poll [`Gate::probe`] once a second for up to [`HEALTH_TIMEOUT`].
    fn wait_healthy(&self, addr: SocketAddr, expected: Option<&str>) -> Result<()> {
        self.say(format!("waiting for http://{addr}/health and /version"));
        let deadline = self.sys.now() + HEALTH_TIMEOUT.as_secs();
        loop {
            match self.probe(addr, expected) {
                Ok(version) => {
                    self.say(format!("healthy, version {version}"));
                    return Ok(());
                }
                Err(why) if self.sys.now() >= deadline => {
                    let secs = HEALTH_TIMEOUT.as_secs();
                    return Err(failed(format!("not healthy after {secs}s: {why}")));
                }
                Err(_) => self.sys.sleep(POLL_INTERVAL),
            }
        }
    }

    fn read_state(&self, env: Env) -> Result<Value> {
        let path = self.layout.state(env);
        let text = fs::read_to_string(&path).context(format!("reading {}", path.display()))?;
        serde_json::from_str(&text).map_err(|e| failed(format!("{}: {e}", path.display())))
    }

    /// The release directory `current` points at, if any.
    fn current_release(&self, env: Env) -> Option<PathBuf> {
        fs::read_link(self.layout.current(env)).ok()
    }
}

/// Replace `path` with `contents` in one rename, so a crash leaves either
/// the old file or the new one. The temporary file is created with `mode`
/// already set: env files hold secrets and must never be readable by
/// others, even briefly.
fn write_atomic(path: &Path, contents: &[u8], mode: u32, owner: Option<(u32, u32)>) -> Result<()> {
    let tmp = sibling(path, ".tmp");
    let what = format!("writing {}", path.display());
    remove_file_if_exists(&tmp).context(&what)?;
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .open(&tmp)
        .context(&what)?;
    file.write_all(contents).context(&what)?;
    file.sync_all().context(&what)?;
    if let Some((uid, gid)) = owner {
        chown(&tmp, Some(uid), Some(gid)).context(&what)?;
    }
    fs::rename(&tmp, path).context(&what)
}

/// `path` with `suffix` appended to its file name.
fn sibling(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(suffix);
    PathBuf::from(name)
}

fn remove_file_if_exists(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::testing::*;
    use super::*;

    fn run(vm: &Vm, args: &[&str], ssh: Option<&str>) -> (i32, Value) {
        let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        let mut out = Vec::new();
        let code = main(&args, ssh, &vm.gate(), &mut out);
        let text = String::from_utf8(out).unwrap();
        assert_eq!(text.lines().count(), 1, "{text}");
        let value = serde_json::from_str(&text).unwrap_or(Value::String(text));
        (code, value)
    }

    fn notes(vm: &Vm, env: &str, line: &str) -> (i32, Value) {
        run(vm, &["--agent", "notes", "--key-env", env], Some(line))
    }

    #[test]
    fn an_agent_deploys_into_its_own_tree_as_its_own_user() {
        let vm = Vm::new();
        vm.add_agent("notes", 18082, 18083);
        let (code, out) = notes(&vm, "staging", &format!("deploy staging sha256:{HEX_A}"));
        assert_eq!((code, &out["ok"]), (0, &json!(true)), "{out}");

        // Its own repository, release cache, current link, env file and state.
        assert!(
            vm.fake
                .find_run(&format!("pull ghcr.io/me/notes@sha256:{HEX_A}"))
                .is_some()
        );
        assert!(
            vm.agent_path("opt/athena", "notes", &format!("releases/{HEX_A}/athena"))
                .is_file()
        );
        let link = fs::read_link(vm.agent_path("opt/athena", "notes", "staging/current")).unwrap();
        assert!(link.ends_with(HEX_A), "{}", link.display());
        let vars = envfile::read(&vm.agent_path("etc/athena", "notes", "staging.env")).unwrap();
        assert_eq!(envfile::get(&vars, "ATHENA_VERSION"), Some(REVISION));
        let state = vm
            .root()
            .join("var/lib/athena/gate/notes/staging.state.json");
        assert!(state.is_file());
        // Its own units and user; nothing of Athena's.
        assert_eq!(
            vm.fake
                .count("systemctl start athena-notes-serve@staging.service"),
            1
        );
        assert_eq!(vm.fake.count("athena-serve@"), 0);
        let version = vm.fake.find_run("athena --version").unwrap();
        assert_eq!(version.user.as_deref(), Some("athena-notes"));
        assert!(
            vm.fake
                .find_run("oras pull ghcr.io/queryplanner/athena")
                .is_none()
        );
        let untouched = vm.current(Env::Staging).is_none();
        assert!(untouched, "Athena's staging was touched");
        assert!(!vm.root().join("var/lib/athena/.gate.lock").exists());
    }

    #[test]
    fn an_agent_promotes_only_what_its_own_staging_runs() {
        let vm = Vm::new();
        vm.add_agent("notes", 18082, 18083);
        // Athena's staging runs HEX_A; that is not notes' staging.
        vm.write_state(Env::Staging, &format!("sha256:{HEX_A}"), REVISION);
        let (code, out) = notes(&vm, "prod", &format!("promote prod sha256:{HEX_A}"));
        assert_eq!(code, 2, "{out}");
        assert!(
            out["rejected"]
                .as_str()
                .unwrap()
                .contains("deploy to staging first")
        );

        notes(&vm, "staging", &format!("deploy staging sha256:{HEX_A}"));
        let (code, out) = notes(&vm, "prod", &format!("promote prod sha256:{HEX_A}"));
        assert_eq!(code, 0, "{out}");
        assert_eq!(
            vm.fake
                .count("systemctl start athena-notes-serve@prod.service"),
            1
        );
    }

    #[test]
    fn an_unknown_agent_is_refused_before_anything_happens() {
        let vm = Vm::new();
        let (code, out) = notes(&vm, "staging", "status staging");
        assert_eq!(code, 2);
        assert_has(
            out["rejected"].as_str().unwrap(),
            &["unknown agent notes", "agent.env"],
        );
        assert_eq!(vm.fake.effects(), Vec::<String>::new());
    }

    #[test]
    fn an_agent_without_a_repository_cannot_deploy() {
        let vm = Vm::new();
        vm.add_agent("notes", 18082, 18083);
        vm.write("etc/athena/agents/notes/agent.env", "PORT_STAGING=18082\n");
        let (code, out) = notes(&vm, "staging", &format!("deploy staging sha256:{HEX_A}"));
        assert_eq!(code, 1);
        assert_has(out["error"].as_str().unwrap(), &["needs ATHENA_REPO"]);
    }

    #[test]
    fn a_port_answered_by_another_agent_is_not_healthy() {
        let vm = Vm::new();
        vm.add_agent("notes", 18082, 18083);
        vm.fake.http_rule(
            "GET /health",
            Some(response(200, r#"{"status":"ok","agent":"athena"}"#)),
            usize::MAX,
        );
        let (code, out) = notes(&vm, "staging", &format!("deploy staging sha256:{HEX_A}"));
        assert_eq!(code, 1);
        assert_has(
            out["error"].as_str().unwrap(),
            &["/health is agent \"athena\", not \"notes\""],
        );
    }

    #[test]
    fn a_port_answered_by_its_own_agent_is_healthy() {
        let vm = Vm::new();
        vm.add_agent("notes", 18082, 18083);
        vm.fake.http_rule(
            "GET /health",
            Some(response(200, r#"{"status":"ok","agent":"notes"}"#)),
            usize::MAX,
        );
        let (code, out) = notes(&vm, "staging", &format!("deploy staging sha256:{HEX_A}"));
        assert_eq!(code, 0, "{out}");
    }

    #[test]
    fn list_shows_every_agent_with_its_ports_and_state() {
        let vm = Vm::new();
        vm.add_agent("notes", 18082, 18083);
        vm.add_agent("chat", 18084, 18085);
        // Not an agent: no agent.env, a bad name, and Athena's own entry.
        fs::create_dir_all(vm.root().join("etc/athena/agents/half")).unwrap();
        fs::create_dir_all(vm.root().join("etc/athena/agents/Bad-Name")).unwrap();
        vm.write("etc/athena/agents/athena/agent.env", "");
        vm.write_state(Env::Prod, &format!("sha256:{HEX_A}"), "v1");
        let (code, out) = run(&vm, &["list"], None);
        assert_eq!(code, 0, "{out}");
        let names: Vec<&str> = out["agents"]
            .as_array()
            .unwrap()
            .iter()
            .map(|a| a["agent"].as_str().unwrap())
            .collect();
        assert_eq!(names, ["athena", "chat", "notes"]);
        let athena = &out["agents"][0];
        assert_eq!(athena["user"], "athena");
        assert_eq!(athena["envs"]["prod"]["addr"], "127.0.0.1:18080");
        assert_eq!(athena["envs"]["prod"]["state"]["version"], "v1");
        assert_eq!(athena["envs"]["staging"]["state"], Value::Null);
        assert_eq!(
            out["agents"][2]["envs"]["staging"]["addr"],
            "127.0.0.1:18082"
        );
        assert_eq!(out["agents"][2]["user"], "athena-notes");
    }

    #[test]
    fn list_works_before_anything_is_set_up() {
        let vm = Vm::new();
        for env in Env::ALL {
            fs::remove_file(vm.root().join(format!("etc/athena/{env}.env"))).unwrap();
        }
        let (code, out) = run(&vm, &["list"], None);
        assert_eq!((code, &out["agents"]), (0, &json!([])));
    }

    #[test]
    fn list_fails_when_the_agents_directory_cannot_be_read() {
        let vm = Vm::new();
        vm.write("etc/athena/agents", "not a directory");
        let (code, out) = run(&vm, &["list"], None);
        assert_eq!(code, 1);
        assert!(out["error"].as_str().unwrap().contains("reading"), "{out}");
    }

    #[test]
    fn a_rejected_command_exits_2_before_any_side_effect() {
        let vm = Vm::new();
        let (code, out) = run(
            &vm,
            &["--key-env", "staging"],
            Some("deploy prod; rm -rf /"),
        );
        assert_eq!(code, 2);
        assert_eq!(out["ok"], false);
        assert!(out["rejected"].as_str().unwrap().contains("not allowed"));
        assert_eq!(vm.fake.effects(), Vec::<String>::new());
        assert!(
            vm.fake
                .said("rejected: \"deploy prod; rm -rf /\" is not allowed")
        );
        assert!(!vm.root().join("var/lib/athena/.gate.lock").exists());
    }

    #[test]
    fn a_successful_command_prints_ok_and_its_name() {
        let vm = Vm::new();
        vm.write_state(Env::Prod, &format!("sha256:{HEX_A}"), "v1");
        let (code, out) = run(&vm, &["--key-env", "prod"], Some("status prod"));
        assert_eq!(
            (code, &out["ok"], &out["command"]),
            (0, &json!(true), &json!("status"))
        );
        assert_eq!(out["state"]["version"], "v1");
    }

    #[test]
    fn a_failed_command_exits_1_with_the_error() {
        let vm = Vm::new();
        let (code, out) = run(&vm, &["--key-env", "prod"], Some("status prod"));
        assert_eq!(
            (code, &out["ok"], &out["command"]),
            (1, &json!(false), &json!("status"))
        );
        assert!(out["error"].as_str().unwrap().contains("state.json"));
        assert!(vm.fake.said("failed: reading"));
    }

    #[test]
    fn a_refused_operation_exits_2_with_its_name() {
        let vm = Vm::new();
        let line = format!("promote prod sha256:{HEX_A}");
        let (code, out) = run(&vm, &["--key-env", "prod"], Some(&line));
        assert_eq!((code, &out["command"]), (2, &json!("promote")));
        assert!(
            out["rejected"]
                .as_str()
                .unwrap()
                .contains("deploy to staging first")
        );
    }

    #[test]
    fn every_command_runs_through_main() {
        let vm = Vm::new();
        let d = format!("sha256:{HEX_A}");
        let staging = |line: &str| run(&vm, &["--key-env", "staging"], Some(line));
        let prod = |line: &str| run(&vm, &["--key-env", "prod"], Some(line));
        let cases = [
            ("deploy", staging(&format!("deploy staging {d}"))),
            ("promote", prod(&format!("promote prod {d}"))),
            ("smoke", prod("smoke prod")),
            ("bench", staging("bench staging")),
            ("eval", staging("eval staging")),
            ("status", staging("status staging")),
            ("install-gate", run(&vm, &["install-gate", &d], None)),
        ];
        for (name, (code, out)) in cases {
            assert_eq!(
                (code, &out["ok"], &out["command"]),
                (0, &json!(true), &json!(name))
            );
        }
        vm.create_db(Env::Prod);
        vm.write("var/lib/athena/prod/backups/20260901T000000Z.db", "old");
        let (code, out) = run(&vm, &["restore", "prod", "20260901T000000Z.db"], None);
        assert_eq!(
            (code, &out["command"], &out["restored"]),
            (0, &json!("restore"), &json!("20260901T000000Z.db"))
        );
    }

    /// Regression: a process spawned by another thread briefly shares the
    /// lock file's open description (until its exec closes it), and with it
    /// the flock. A released lock must not look held because of that.
    #[test]
    fn a_child_spawned_by_another_thread_does_not_make_the_lock_look_held() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let vm = Vm::new();
        let stop = AtomicBool::new(false);
        std::thread::scope(|s| {
            s.spawn(|| {
                while !stop.load(Ordering::Relaxed) {
                    std::process::Command::new("/usr/bin/true")
                        .status()
                        .unwrap();
                }
            });
            let gate = vm.gate();
            let failures = (0..2000).filter(|_| gate.lock().is_err()).count();
            stop.store(true, Ordering::Relaxed);
            assert_eq!(failures, 0);
        });
    }

    #[test]
    fn version_prints_a_plain_line() {
        let vm = Vm::new();
        let (code, out) = run(&vm, &["--version"], None);
        assert_eq!(code, 0);
        assert_eq!(
            out,
            json!(format!("deploy-gate {}\n", env!("CARGO_PKG_VERSION")))
        );
    }

    #[test]
    fn an_io_error_names_what_was_being_done() {
        let err: Result<()> = Err(io::Error::other("boom")).context("reading x");
        assert_eq!(err, Err(failed("reading x: boom")));
        assert_eq!(Failure::Rejected("no".into()).to_string(), "no");
    }

    #[test]
    fn an_atomic_write_replaces_a_stale_temp_file_and_sets_the_mode() {
        use std::os::unix::fs::MetadataExt;
        let tmp = TempRoot::new();
        let path = tmp.path().join("state.json");
        fs::write(sibling(&path, ".tmp"), "stale").unwrap();
        write_atomic(&path, b"new", 0o600, None).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "new");
        assert_eq!(fs::metadata(&path).unwrap().mode() & 0o777, 0o600);
        assert!(!sibling(&path, ".tmp").exists());
        let err = write_atomic(&tmp.path().join("no/dir/f"), b"x", 0o600, None).unwrap_err();
        assert!(err.to_string().starts_with("writing"), "{err}");
        let dir = tmp.path().join("a-dir");
        fs::create_dir(&dir).unwrap();
        fs::create_dir(sibling(&dir, ".tmp")).unwrap();
        assert!(write_atomic(&dir, b"x", 0o600, None).is_err());
    }
}
