//! A fake VM for the gate's tests: a temporary root directory and a
//! [`System`] that behaves like systemd, ORAS, the `athena` binary and
//! `athena serve`, and records everything asked of it.

use super::sys::{Cmd, Output, Request, Response, SYSTEMCTL, System};
use super::{Env, Gate, MIN_FREE_BYTES, envfile};
use serde_json::{Value, json};
use std::cell::{Cell, RefCell};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

pub const HEX_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
pub const HEX_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
pub const HEX_C: &str = "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc";
pub const HEX_D: &str = "dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd";
/// The fake manifest's revision annotation is `rev-` and the digest's
/// first seven hex digits.
pub const REVISION: &str = "rev-aaaaaaa";
pub const REVISION_B: &str = "rev-bbbbbbb";
/// 2026-09-25T12:00:00Z.
pub const NOW: u64 = 1_790_337_600;
pub const MIN_FREE: u64 = MIN_FREE_BYTES;

/// A directory under the system temp dir, removed on drop.
pub struct TempRoot(PathBuf);

impl TempRoot {
    pub fn new() -> TempRoot {
        let path = std::env::temp_dir().join(format!("athena-gate-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&path).unwrap();
        TempRoot(path)
    }

    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempRoot {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// `text` contains every one of `parts`.
pub fn assert_has(text: &str, parts: &[&str]) {
    for part in parts {
        assert!(text.contains(part), "{text:?} lacks {part:?}");
    }
}

pub fn response(status: u16, body: &str) -> Response {
    Response {
        status,
        body: body.into(),
    }
}

struct Rule<T> {
    needle: String,
    /// Matching calls to let through before this rule applies.
    skip: usize,
    /// Matching calls this rule still answers.
    times: usize,
    reply: T,
}

/// Find the rule for `line` and use it up by one call.
fn apply<T: Clone>(rules: &RefCell<Vec<Rule<T>>>, line: &str) -> Option<T> {
    let mut rules = rules.borrow_mut();
    let rule = rules
        .iter_mut()
        .find(|r| r.times > 0 && line.contains(&r.needle))?;
    if rule.skip > 0 {
        rule.skip -= 1;
        return None;
    }
    rule.times -= 1;
    Some(rule.reply.clone())
}

pub struct Fake {
    root: PathBuf,
    /// `run <cmd>`, `http <method> <path>`, `say <line>`, in order.
    events: RefCell<Vec<String>>,
    cmds: RefCell<Vec<Cmd>>,
    pub requests: RefCell<Vec<Request>>,
    pub now: Cell<u64>,
    pub free: Cell<u64>,
    pub free_fails: Cell<bool>,
    pub telegram_enabled: Cell<bool>,
    /// The files `oras pull` writes.
    pub pull_files: RefCell<Vec<&'static str>>,
    /// What `oras manifest fetch` prints, instead of a manifest with a
    /// revision annotation.
    pub manifest: RefCell<Option<String>>,
    pub version_output: RefCell<String>,
    /// `athena serve` running as one of these versions is never healthy.
    pub unhealthy_versions: RefCell<Vec<String>>,
    backup_saw: RefCell<Option<PathBuf>>,
    run_rules: RefCell<Vec<Rule<Option<Output>>>>,
    http_rules: RefCell<Vec<Rule<Option<Response>>>>,
}

impl Fake {
    pub fn new(root: &Path) -> Fake {
        Fake {
            root: root.to_path_buf(),
            events: RefCell::default(),
            cmds: RefCell::default(),
            requests: RefCell::default(),
            now: Cell::new(NOW),
            free: Cell::new(10 * MIN_FREE),
            free_fails: Cell::new(false),
            telegram_enabled: Cell::new(false),
            pull_files: RefCell::new(vec!["athena", "deploy-gate"]),
            manifest: RefCell::default(),
            version_output: RefCell::new("athena 0.1.0-dev".into()),
            unhealthy_versions: RefCell::default(),
            backup_saw: RefCell::default(),
            run_rules: RefCell::default(),
            http_rules: RefCell::default(),
        }
    }

    /// Commands matching `needle` fail with `stderr`, after `skip` of
    /// them succeed, for `times` calls.
    pub fn fail_run_n(&self, needle: &str, skip: usize, times: usize, stderr: &str) {
        let reply = Some(Output {
            success: false,
            stdout: String::new(),
            stderr: stderr.into(),
        });
        self.run_rules.borrow_mut().push(Rule {
            needle: needle.into(),
            skip,
            times,
            reply,
        });
    }

    pub fn fail_run(&self, needle: &str, stderr: &str) {
        self.fail_run_n(needle, 0, usize::MAX, stderr);
    }

    /// Commands matching `needle` fail to start at all.
    pub fn spawn_fails(&self, needle: &str) {
        self.run_rules.borrow_mut().push(Rule {
            needle: needle.into(),
            skip: 0,
            times: usize::MAX,
            reply: None,
        });
    }

    pub fn run_stdout(&self, needle: &str, stdout: &str) {
        let reply = Some(Output {
            success: true,
            stdout: stdout.into(),
            stderr: String::new(),
        });
        self.run_rules.borrow_mut().push(Rule {
            needle: needle.into(),
            skip: 0,
            times: usize::MAX,
            reply,
        });
    }

    /// Requests matching `needle` get `reply` (`None`: a connection error)
    /// for `times` calls.
    pub fn http_rule(&self, needle: &str, reply: Option<Response>, times: usize) {
        self.http_rules.borrow_mut().push(Rule {
            needle: needle.into(),
            skip: 0,
            times,
            reply,
        });
    }

    pub fn fail_http(&self, needle: &str, times: usize) {
        self.http_rule(needle, Some(response(503, "")), times);
    }

    pub fn clear(&self) {
        self.events.borrow_mut().clear();
        self.cmds.borrow_mut().clear();
        self.requests.borrow_mut().clear();
    }

    /// Processes run and HTTP requests made, in order.
    pub fn effects(&self) -> Vec<String> {
        self.events
            .borrow()
            .iter()
            .filter(|e| e.starts_with("run ") || e.starts_with("http "))
            .cloned()
            .collect()
    }

    pub fn assert_effects_start_with(&self, prefixes: &[&str]) {
        let effects = self.effects();
        for (i, prefix) in prefixes.iter().enumerate() {
            let effect = effects.get(i).map(String::as_str).unwrap_or("<none>");
            assert!(effect.starts_with(prefix), "{i}: {effect:?}");
        }
    }

    pub fn position(&self, needle: &str) -> Option<usize> {
        self.effects().iter().position(|e| e.contains(needle))
    }

    pub fn count(&self, needle: &str) -> usize {
        self.effects().iter().filter(|e| e.contains(needle)).count()
    }

    pub fn said(&self, needle: &str) -> bool {
        self.events
            .borrow()
            .iter()
            .any(|e| e.starts_with("say ") && e.contains(needle))
    }

    pub fn find_run(&self, needle: &str) -> Option<Cmd> {
        self.cmds
            .borrow()
            .iter()
            .find(|c| c.line().contains(needle))
            .cloned()
    }

    /// Where the env's `current` pointed while the last backup ran.
    pub fn current_at_backup(&self) -> Option<PathBuf> {
        self.backup_saw.borrow().clone()
    }

    fn served_version(&self, request: &Request) -> String {
        let env = if request.addr.port() == 18080 {
            Env::Prod
        } else {
            Env::Staging
        };
        let file = self.root.join(format!("etc/athena/{env}.env"));
        let vars = envfile::read(&file).unwrap();
        envfile::get(&vars, "ATHENA_VERSION")
            .unwrap_or("unset")
            .to_string()
    }

    fn default_run(&self, cmd: &Cmd) -> Output {
        let ok = |stdout: String| Output {
            success: true,
            stdout,
            stderr: String::new(),
        };
        let args: Vec<&str> = cmd.args.iter().map(String::as_str).collect();
        match args.as_slice() {
            ["pull", _, "-o", dir] => {
                fs::create_dir_all(dir).unwrap();
                for name in self.pull_files.borrow().iter() {
                    fs::write(Path::new(dir).join(name), format!("{name} binary")).unwrap();
                }
                ok(String::new())
            }
            ["manifest", "fetch", reference] => {
                let hex = &reference[reference.find("sha256:").unwrap() + 7..];
                let annotations =
                    json!({"org.opencontainers.image.revision": format!("rev-{}", &hex[..7])});
                let manifest = json!({"schemaVersion": 2, "annotations": annotations}).to_string();
                ok(self.manifest.borrow().clone().unwrap_or(manifest))
            }
            ["--version"] if cmd.program.ends_with("deploy-gate") => {
                ok("deploy-gate 0.1.0\n".into())
            }
            ["--version"] => ok(format!("{}\n", self.version_output.borrow())),
            ["backup", dest] => {
                let env = cmd
                    .cwd
                    .as_ref()
                    .unwrap()
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .into_owned();
                let link = self.root.join(format!("opt/athena/{env}/current"));
                self.backup_saw.replace(fs::read_link(link).ok());
                fs::create_dir_all(Path::new(dest).parent().unwrap()).unwrap();
                fs::write(dest, "backup").unwrap();
                ok(String::new())
            }
            ["is-enabled", ..] if cmd.program == SYSTEMCTL => Output {
                success: self.telegram_enabled.get(),
                ..Output::default()
            },
            _ => ok(String::new()),
        }
    }
}

impl System for Fake {
    fn run(&self, cmd: &Cmd) -> io::Result<Output> {
        let line = cmd.line();
        assert!(cmd.program.starts_with('/'), "relative program: {line}");
        self.events.borrow_mut().push(format!("run {line}"));
        self.cmds.borrow_mut().push(cmd.clone());
        match apply(&self.run_rules, &line) {
            Some(Some(out)) => Ok(out),
            Some(None) => Err(io::Error::new(io::ErrorKind::NotFound, "no such program")),
            None => Ok(self.default_run(cmd)),
        }
    }

    fn http(&self, request: &Request) -> io::Result<Response> {
        let line = format!("{} {}", request.method, request.path);
        self.events.borrow_mut().push(format!("http {line}"));
        self.requests.borrow_mut().push(request.clone());
        if let Some(reply) = apply(&self.http_rules, &line) {
            return reply
                .ok_or_else(|| io::Error::new(io::ErrorKind::ConnectionRefused, "refused"));
        }
        let version = self.served_version(request);
        let healthy = !self.unhealthy_versions.borrow().contains(&version);
        Ok(match line.as_str() {
            "GET /health" if healthy => response(200, "{\"status\":\"ok\"}"),
            "GET /health" => response(503, ""),
            "GET /version" => response(200, &json!({"version": version}).to_string()),
            "POST /sessions" => response(201, "{\"id\":\"s-1\",\"name\":\"smoke\"}"),
            // POST /sessions/s-1/messages
            _ => response(200, "{\"reply\":\"pong\",\"run\":{}}"),
        })
    }

    fn now(&self) -> u64 {
        self.now.get()
    }

    fn sleep(&self, duration: Duration) {
        self.now.set(self.now.get() + duration.as_secs());
    }

    fn free_bytes(&self, path: &Path) -> io::Result<u64> {
        assert!(path.is_dir(), "{} is not a directory", path.display());
        if self.free_fails.get() {
            return Err(io::Error::other("statvfs failed"));
        }
        Ok(self.free.get())
    }

    fn say(&self, line: &str) {
        self.events.borrow_mut().push(format!("say {line}"));
    }
}

/// A VM with both env files written and nothing deployed.
pub struct Vm {
    tmp: TempRoot,
    pub fake: Fake,
}

impl Vm {
    pub fn new() -> Vm {
        let tmp = TempRoot::new();
        let fake = Fake::new(tmp.path());
        let vm = Vm { tmp, fake };
        vm.write(
            "etc/athena/staging.env",
            "ATHENA_ADDR=127.0.0.1:18081\nSECRET=s3cret\nATHENA_ENV=staging\n",
        );
        vm.write(
            "etc/athena/prod.env",
            "ATHENA_ADDR=127.0.0.1:18080\nATHENA_ENV=prod\n",
        );
        // systemd's `StateDirectory=` creates these, owned by athena.
        for env in Env::ALL {
            fs::create_dir_all(vm.root().join(format!("var/lib/athena/{env}"))).unwrap();
        }
        vm
    }

    pub fn root(&self) -> &Path {
        self.tmp.path()
    }

    pub fn gate(&self) -> Gate<'_> {
        Gate::new(self.root(), &self.fake)
    }

    pub fn write(&self, relative: &str, text: &str) {
        let path = self.root().join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    pub fn write_state(&self, env: Env, digest: &str, version: &str) {
        let state = json!({"digest": digest, "version": version, "deployed_at": "x"});
        self.write(
            &format!("var/lib/athena/gate/{env}.state.json"),
            &state.to_string(),
        );
    }

    pub fn state(&self, env: Env) -> Value {
        let path = self
            .root()
            .join(format!("var/lib/athena/gate/{env}.state.json"));
        serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap()
    }

    pub fn env_var(&self, env: Env, key: &str) -> Option<String> {
        let vars = envfile::read(&self.root().join(format!("etc/athena/{env}.env"))).unwrap();
        envfile::get(&vars, key).map(str::to_string)
    }

    pub fn set_env_var(&self, env: Env, key: &str, value: &str) {
        let path = self.root().join(format!("etc/athena/{env}.env"));
        envfile::set_var(&path, key, value).unwrap();
    }

    pub fn create_db(&self, env: Env) {
        self.write(&format!("var/lib/athena/{env}/agent.db"), "db");
    }

    /// The release name `current` points at.
    pub fn current(&self, env: Env) -> Option<String> {
        let link = fs::read_link(self.root().join(format!("opt/athena/{env}/current"))).ok()?;
        Some(link.file_name()?.to_string_lossy().into_owned())
    }
}
