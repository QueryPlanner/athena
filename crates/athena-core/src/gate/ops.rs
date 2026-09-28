//! The commands besides deploy and promote: `smoke`, `bench`, `eval`,
//! `status`, and the admin-only `restore` and `install-gate`.

use super::{
    ATHENA_USER, Context, Digest, Env, Gate, Result, failed, remove_file_if_exists, sibling, time,
};
use serde_json::{Value, json};
use std::fs::{self, OpenOptions};
use std::io;
use std::net::SocketAddr;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt, fchown};
use std::path::Path;
use std::time::Duration;

/// The user smoke tests run as, so their sessions are easy to find.
pub const SMOKE_USER: &str = "smoke";
pub const SMOKE_PROMPT: &str = "Reply with the single word: pong";
/// Creating a session is quick; a turn calls the model.
const SESSION_TIMEOUT: Duration = Duration::from_secs(10);
const TURN_TIMEOUT: Duration = Duration::from_secs(180);

impl Gate<'_> {
    /// An HTTP call as the smoke user. Returns the status and the JSON body
    /// (`null` when the body is not JSON).
    fn call(
        &self,
        addr: SocketAddr,
        path: &str,
        body: Value,
        timeout: Duration,
    ) -> Result<(u16, Value)> {
        self.say(format!("POST {path}"));
        let request = super::Request {
            method: "POST",
            addr,
            path: path.into(),
            headers: vec![("X-Athena-User", SMOKE_USER.into())],
            body: Some(body.to_string()),
            timeout,
        };
        let response = self.sys.http(&request).context(format!("POST {path}"))?;
        let json = serde_json::from_str(&response.body).unwrap_or(Value::Null);
        Ok((response.status, json))
    }

    /// Health, version, then one real turn through the HTTP API.
    pub(super) fn smoke(&self, env: Env) -> Result<Value> {
        let settings = self.settings(env)?;
        let state = self.read_state(env).ok();
        let expected = state.as_ref().and_then(|s| s["version"].as_str());
        let version = self.probe(settings.addr, expected).map_err(failed)?;

        let name = format!("smoke-{}", time::compact(self.sys.now()));
        let (status, session) = self.call(
            settings.addr,
            "/sessions",
            json!({"name": name}),
            SESSION_TIMEOUT,
        )?;
        let id = session["id"]
            .as_str()
            .filter(|id| status == 201 && is_session_id(id))
            .ok_or_else(|| failed(format!("POST /sessions returned {status} {session}")))?
            .to_string();

        let path = format!("/sessions/{id}/messages");
        let (status, turn) = self.call(
            settings.addr,
            &path,
            json!({"text": SMOKE_PROMPT}),
            TURN_TIMEOUT,
        )?;
        let reply = turn["reply"]
            .as_str()
            .filter(|r| status == 200 && !r.trim().is_empty())
            .ok_or_else(|| failed(format!("POST {path} returned {status} {turn}")))?;
        self.say(format!("reply: {reply}"));
        Ok(json!({
            "env": env.as_str(),
            "version": version,
            "session": id,
            "reply_chars": reply.chars().count(),
        }))
    }

    /// `athena bench` from the current staging release against staging.
    pub(super) fn bench(&self) -> Result<Value> {
        let env = Env::Staging;
        let settings = self.settings(env)?;
        let athena = self.current_binary(env)?;
        let url = format!("http://{}", settings.addr);
        let cmd = self.athena_cmd(env, &settings, &athena, &["bench", "--url", &url]);
        let out = self.exec(&cmd)?;
        self.relay(&out.stderr);
        let summary = serde_json::from_str(out.stdout.trim())
            .unwrap_or_else(|_| Value::String(out.stdout.trim().to_string()));
        if !out.success {
            return Err(failed(format!("bench failed: {summary}")));
        }
        Ok(json!({"env": env.as_str(), "summary": summary}))
    }

    /// `athena eval run` from the current staging release, when it ships
    /// eval cases. Advisory: a finished run succeeds whatever it scored.
    pub(super) fn eval(&self) -> Result<Value> {
        let env = Env::Staging;
        let settings = self.settings(env)?;
        let athena = self.current_binary(env)?;
        let cases = self.layout.current(env).join("evals");
        if !cases.is_dir() {
            self.say(format!(
                "{} does not exist: no evals to run",
                cases.display()
            ));
            return Ok(json!({"env": env.as_str(), "skipped": true}));
        }
        let url = format!("http://{}", settings.addr);
        let cases_arg = cases.to_string_lossy();
        let args = ["eval", "run", "--target", &url, "--cases", &cases_arg];
        let out = self.exec(&self.athena_cmd(env, &settings, &athena, &args))?;
        self.relay(&out.stdout);
        self.relay(&out.stderr);
        if !out.success {
            return Err(failed("eval run errored; see the output above"));
        }
        Ok(json!({"env": env.as_str(), "skipped": false}))
    }

    pub(super) fn status(&self, env: Env) -> Result<Value> {
        let state = self.read_state(env)?;
        let current = self.current_release(env).and_then(|p| {
            p.file_name()
                .map(|name| name.to_string_lossy().into_owned())
        });
        Ok(json!({"env": env.as_str(), "state": state, "current": current}))
    }

    /// `<current>/athena`, which must exist.
    fn current_binary(&self, env: Env) -> Result<std::path::PathBuf> {
        let athena = self.layout.current(env).join("athena");
        if athena.is_file() {
            Ok(athena)
        } else {
            Err(failed(format!(
                "{} does not exist; deploy first",
                athena.display()
            )))
        }
    }

    /// Replace the env's database with one of its backups. The current
    /// database is backed up first, so a restore can itself be undone.
    pub(super) fn restore(&self, env: Env, file: &str) -> Result<Value> {
        let _lock = self.lock()?;
        let settings = self.settings(env)?;
        let source = self.layout.backups(env).join(file);
        // The backups directory belongs to the service user: a symlink
        // planted there must not make root read another file.
        let not_link =
            |p: &Path| fs::symlink_metadata(p).is_ok_and(|m| !m.file_type().is_symlink());
        let regular = not_link(&self.layout.backups(env))
            && fs::symlink_metadata(&source).is_ok_and(|m| m.file_type().is_file());
        if !regular {
            return Err(failed(format!("{} is not a backup file", source.display())));
        }
        let release = self
            .current_release(env)
            .ok_or_else(|| failed(format!("{env} has no current release to start")))?;
        let db = self.layout.db(env);
        // Copy first, while the source cannot yet be pruned by the safety
        // backup below.
        let staged = sibling(&db, ".restore");
        let what = format!("restoring {} to {}", source.display(), db.display());
        stage_copy(&source, &staged).context(&what)?;

        self.stop_units(env)?;
        let result = self
            .backup(env, &settings, Some(release.as_path()))
            .and_then(|safety| {
                for suffix in ["-wal", "-shm"] {
                    remove_file_if_exists(&sibling(&db, suffix)).context(&what)?;
                }
                fs::rename(&staged, &db).context(&what)?;
                self.say(&what);
                Ok(safety)
            });
        let safety = result.map_err(|e| {
            // Best effort: a leftover copy is overwritten by the next restore.
            let _ = fs::remove_file(&staged);
            self.restart_after(env, e)
        })?;
        self.start_units(env)?;
        let version = super::envfile::get(&settings.vars, "ATHENA_VERSION");
        self.wait_healthy(settings.addr, version)?;
        Ok(json!({
            "env": env.as_str(),
            "restored": file,
            "safety_backup": safety.map(|p| p.display().to_string()),
        }))
    }

    /// Replace `/opt/athena/bin/deploy-gate` with the one in a release.
    /// Only an admin can: a bad app release must never replace the tool
    /// that rolls it back.
    pub(super) fn install_gate(&self, digest: &Digest) -> Result<Value> {
        let _lock = self.lock()?;
        let config = self.gate_config()?;
        self.check_disk()?;
        let release = self.ensure_release(&config, digest)?;
        if !release.join("deploy-gate").is_file() {
            return Err(failed(format!("{digest} has no `deploy-gate` file")));
        }
        let version = self.check_binary(&release, "deploy-gate")?;
        let target = self.layout.installed_gate();
        let staged = sibling(&target, ".tmp");
        let what = format!("installing {}", target.display());
        fs::create_dir_all(target.parent().unwrap_or(self.layout.root())).context(&what)?;
        remove_file_if_exists(&staged).context(&what)?;
        fs::copy(release.join("deploy-gate"), &staged).context(&what)?;
        fs::set_permissions(&staged, fs::Permissions::from_mode(0o755)).context(&what)?;
        fs::rename(&staged, &target).context(&what)?;
        self.say(format!("{what}: {version}"));
        Ok(json!({
            "digest": digest.as_str(),
            "installed": target.display().to_string(),
            "version": version,
            "user": ATHENA_USER,
        }))
    }
}

/// Copy `source` to a new file `staged`, both in a directory the service
/// user can write. Nothing planted there is followed: `source` is opened
/// with `O_NOFOLLOW`, a stale `staged` is removed and the copy is created
/// exclusively, and the owner is set through the open file, not the path.
fn stage_copy(source: &Path, staged: &Path) -> io::Result<()> {
    let mut from = OpenOptions::new()
        .read(true)
        // Non-blocking, so a FIFO swapped in cannot hang the open.
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(source)?;
    let meta = from.metadata()?;
    if !meta.is_file() {
        return Err(io::Error::other("not a regular file"));
    }
    remove_file_if_exists(staged)?;
    let mut to = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(staged)?;
    io::copy(&mut from, &mut to)?;
    fchown(&to, Some(meta.uid()), Some(meta.gid()))?;
    to.set_permissions(meta.permissions())?;
    to.sync_all()
}

/// A session id safe to put in a URL path.
fn is_session_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
}

#[cfg(test)]
mod tests {
    use super::super::testing::*;
    use super::super::{Failure, Response};
    use super::*;

    fn digest(hex: &str) -> Digest {
        Digest::parse(&format!("sha256:{hex}")).unwrap()
    }

    fn deployed() -> Vm {
        let vm = Vm::new();
        vm.gate().deploy(Env::Staging, &digest(HEX_A)).unwrap();
        vm.fake.clear();
        vm
    }

    #[test]
    fn smoke_checks_health_version_and_a_real_turn_as_the_smoke_user() {
        let vm = deployed();
        let out = vm.gate().smoke(Env::Staging).unwrap();
        assert_eq!(
            out,
            json!({"env": "staging", "version": REVISION, "session": "s-1", "reply_chars": 4})
        );
        assert_eq!(
            vm.fake.effects(),
            [
                "http GET /health",
                "http GET /version",
                "http POST /sessions",
                "http POST /sessions/s-1/messages"
            ]
        );
        let requests = vm.fake.requests.borrow();
        let create = &requests[2];
        assert_eq!(create.headers, [("X-Athena-User", "smoke".to_string())]);
        assert_eq!(
            create.body.as_deref(),
            Some("{\"name\":\"smoke-20260925T120000Z\"}")
        );
        assert_eq!(create.addr.to_string(), "127.0.0.1:18081");
        assert_eq!(
            requests[3].body.as_deref(),
            Some("{\"text\":\"Reply with the single word: pong\"}")
        );
        assert_eq!(requests[3].timeout, TURN_TIMEOUT);
        assert!(requests[0].headers.is_empty());
    }

    #[test]
    fn smoke_fails_when_the_running_version_is_not_the_recorded_one() {
        let vm = deployed();
        vm.write_state(Env::Staging, &format!("sha256:{HEX_B}"), "other");
        let err = vm.gate().smoke(Env::Staging).unwrap_err().to_string();
        assert!(err.contains("expected \"other\""), "{err}");
    }

    #[test]
    fn smoke_without_state_accepts_any_version() {
        let vm = Vm::new();
        vm.set_env_var(Env::Prod, "ATHENA_VERSION", "manual");
        let out = vm.gate().smoke(Env::Prod).unwrap();
        assert_eq!(out["version"], "manual");
    }

    #[test]
    fn smoke_reports_each_failed_step() {
        let cases: [(&str, Option<Response>, &str); 7] = [
            (
                "GET /health",
                Some(response(503, "")),
                "GET /health returned 503",
            ),
            ("GET /health", None, "GET /health: "),
            (
                "GET /version",
                Some(response(200, "nope")),
                "GET /version returned 200 nope",
            ),
            (
                "GET /version",
                Some(response(404, "{\"version\":\"x\"}")),
                "GET /version returned 404",
            ),
            (
                "POST /sessions",
                Some(response(409, "{\"id\":\"s-1\"}")),
                "POST /sessions returned 409",
            ),
            (
                "POST /sessions",
                Some(response(201, "{\"id\":\"../x\"}")),
                "POST /sessions returned 201",
            ),
            (
                "/messages",
                Some(response(200, "{\"reply\":\"  \"}")),
                "returned 200",
            ),
        ];
        for (needle, reply, message) in cases {
            let vm = deployed();
            vm.fake.http_rule(needle, reply, usize::MAX);
            let err = vm.gate().smoke(Env::Staging).unwrap_err().to_string();
            assert!(err.contains(message), "{needle}: {err}");
        }
        let vm = deployed();
        vm.fake.http_rule("POST /sessions", None, usize::MAX);
        let err = vm.gate().smoke(Env::Staging).unwrap_err().to_string();
        assert!(err.starts_with("POST /sessions: "), "{err}");
        let vm = deployed();
        vm.fake.http_rule("/messages", None, usize::MAX);
        let err = vm.gate().smoke(Env::Staging).unwrap_err().to_string();
        assert!(err.starts_with("POST /sessions/s-1/messages: "), "{err}");
        let vm = deployed();
        vm.fake.http_rule(
            "/messages",
            Some(response(502, "{\"error\":{}}")),
            usize::MAX,
        );
        let err = vm.gate().smoke(Env::Staging).unwrap_err().to_string();
        assert!(err.contains("returned 502"), "{err}");
    }

    #[test]
    fn bench_runs_the_current_release_as_athena_against_staging() {
        let vm = deployed();
        vm.fake
            .run_stdout(" bench ", "{\"p95_ms\":1200,\"error_rate\":0.0}\n");
        let out = vm.gate().bench().unwrap();
        assert_eq!(
            out,
            json!({"env": "staging", "summary": {"p95_ms": 1200, "error_rate": 0.0}})
        );
        let cmd = vm.fake.find_run(" bench ").unwrap();
        let current = vm.root().join("opt/athena/staging/current/athena");
        assert_eq!(cmd.program, current.to_string_lossy());
        assert_eq!(cmd.args, ["bench", "--url", "http://127.0.0.1:18081"]);
        assert_eq!(cmd.user.as_deref(), Some("athena"));
    }

    #[test]
    fn a_bench_over_its_thresholds_fails_with_its_summary() {
        let vm = deployed();
        vm.fake.fail_run(" bench ", "p95 over 30000ms");
        let err = vm.gate().bench().unwrap_err().to_string();
        assert_eq!(err, "bench failed: \"\"");
        assert!(vm.fake.said("  | p95 over 30000ms"));
    }

    #[test]
    fn bench_and_eval_need_a_deployed_release() {
        let vm = Vm::new();
        for err in [
            vm.gate().bench().unwrap_err(),
            vm.gate().eval().unwrap_err(),
        ] {
            assert!(err.to_string().contains("deploy first"), "{err}");
        }
    }

    #[test]
    fn eval_is_skipped_when_the_release_has_no_cases() {
        let vm = deployed();
        assert_eq!(
            vm.gate().eval().unwrap(),
            json!({"env": "staging", "skipped": true})
        );
        assert_eq!(vm.fake.effects(), Vec::<String>::new());
    }

    #[test]
    fn eval_runs_the_releases_cases_and_is_advisory() {
        let vm = deployed();
        fs::create_dir_all(
            vm.root()
                .join("opt/athena/releases")
                .join(HEX_A)
                .join("evals"),
        )
        .unwrap();
        vm.fake.run_stdout(" eval ", "case-1 pass\ncase-2 fail\n");
        assert_eq!(
            vm.gate().eval().unwrap(),
            json!({"env": "staging", "skipped": false})
        );
        let cmd = vm.fake.find_run(" eval ").unwrap();
        let cases = vm.root().join("opt/athena/staging/current/evals");
        assert_eq!(
            cmd.args,
            [
                "eval",
                "run",
                "--target",
                "http://127.0.0.1:18081",
                "--cases",
                &cases.to_string_lossy()
            ]
        );
        assert!(vm.fake.said("  | case-2 fail"));

        let vm = deployed();
        fs::create_dir_all(
            vm.root()
                .join("opt/athena/releases")
                .join(HEX_A)
                .join("evals"),
        )
        .unwrap();
        vm.fake.fail_run(" eval ", "cannot read cases");
        let err = vm.gate().eval().unwrap_err().to_string();
        assert!(err.contains("eval run errored"), "{err}");
    }

    #[test]
    fn status_prints_the_state_and_the_current_release() {
        let vm = deployed();
        let out = vm.gate().status(Env::Staging).unwrap();
        assert_eq!(out["state"], vm.state(Env::Staging));
        assert_eq!(out["current"], HEX_A);
        fs::write(
            vm.root().join("var/lib/athena/gate/staging.state.json"),
            "{",
        )
        .unwrap();
        let err = vm.gate().status(Env::Staging).unwrap_err().to_string();
        assert!(err.contains("state.json: "), "{err}");
    }

    fn with_backup(vm: &Vm) -> std::path::PathBuf {
        vm.create_db(Env::Staging);
        let dir = vm.root().join("var/lib/athena/staging");
        fs::create_dir_all(dir.join("backups")).unwrap();
        fs::write(dir.join("backups/20260901T000000Z.db"), "restored contents").unwrap();
        fs::write(dir.join("agent.db-wal"), "wal").unwrap();
        fs::write(dir.join("agent.db-shm"), "shm").unwrap();
        dir
    }

    #[test]
    fn restore_backs_up_then_replaces_the_database_and_restarts() {
        let vm = deployed();
        let dir = with_backup(&vm);
        vm.fake.now.set(NOW + 5);
        vm.fake.telegram_enabled.set(true);

        let out = vm
            .gate()
            .restore(Env::Staging, "20260901T000000Z.db")
            .unwrap();

        assert_eq!(
            fs::read_to_string(dir.join("agent.db")).unwrap(),
            "restored contents"
        );
        assert!(!dir.join("agent.db-wal").exists() && !dir.join("agent.db-shm").exists());
        assert!(!dir.join("agent.db.restore").exists());
        let safety = dir.join("backups/20260925T120005Z.db");
        assert_eq!(out["safety_backup"], safety.to_string_lossy().as_ref());
        assert_eq!(out["restored"], "20260901T000000Z.db");
        let stop = vm.fake.position("systemctl stop").unwrap();
        let backup = vm.fake.position(" backup ").unwrap();
        let start = vm.fake.position("systemctl start").unwrap();
        assert!(stop < backup && backup < start);
        assert_eq!(vm.fake.count("GET /version"), 1);
        assert_eq!(vm.fake.count("systemctl start athena-telegram@staging"), 1);
    }

    #[test]
    fn restore_never_follows_a_symlink_the_service_user_planted() {
        let vm = deployed();
        let dir = with_backup(&vm);
        let secret = vm.root().join("secret");
        fs::write(&secret, "root only").unwrap();
        std::os::unix::fs::symlink(&secret, dir.join("backups/20260902T000000Z.db")).unwrap();
        let err = vm
            .gate()
            .restore(Env::Staging, "20260902T000000Z.db")
            .unwrap_err();
        assert_has(&err.to_string(), &["is not a backup file"]);

        let target = vm.root().join("etc-passwd");
        fs::write(&target, "untouched").unwrap();
        std::os::unix::fs::symlink(&target, dir.join("agent.db.restore")).unwrap();
        vm.gate()
            .restore(Env::Staging, "20260901T000000Z.db")
            .unwrap();
        assert_eq!(fs::read_to_string(&target).unwrap(), "untouched");
        assert_eq!(
            fs::read_to_string(dir.join("agent.db")).unwrap(),
            "restored contents"
        );
    }

    #[test]
    fn a_backup_opened_through_a_symlink_is_refused() {
        let tmp = TempRoot::new();
        let real = tmp.path().join("real.db");
        fs::write(&real, "x").unwrap();
        let link = tmp.path().join("link.db");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        assert!(stage_copy(&link, &tmp.path().join("staged")).is_err());
        assert!(!tmp.path().join("staged").exists());
        let err = stage_copy(tmp.path(), &tmp.path().join("staged")).unwrap_err();
        assert_eq!(err.to_string(), "not a regular file");
    }

    #[test]
    fn restore_refuses_a_backups_directory_that_is_a_symlink() {
        let vm = deployed();
        let elsewhere = vm.root().join("elsewhere");
        fs::create_dir_all(&elsewhere).unwrap();
        fs::write(elsewhere.join("20260901T000000Z.db"), "root only").unwrap();
        let backups = vm.root().join("var/lib/athena/staging/backups");
        std::os::unix::fs::symlink(&elsewhere, &backups).unwrap();
        let err = vm
            .gate()
            .restore(Env::Staging, "20260901T000000Z.db")
            .unwrap_err();
        assert_has(&err.to_string(), &["is not a backup file"]);
    }

    #[test]
    fn restore_refuses_a_missing_backup_or_release() {
        let vm = deployed();
        let err = vm
            .gate()
            .restore(Env::Staging, "20260901T000000Z.db")
            .unwrap_err();
        assert_has(
            &err.to_string(),
            &["20260901T000000Z.db is not a backup file"],
        );

        let vm = Vm::new();
        with_backup(&vm);
        let err = vm
            .gate()
            .restore(Env::Staging, "20260901T000000Z.db")
            .unwrap_err();
        assert!(err.to_string().contains("no current release"), "{err}");
        assert_eq!(vm.fake.effects(), Vec::<String>::new());
    }

    #[test]
    fn a_failed_safety_backup_keeps_the_database_and_restarts() {
        let vm = deployed();
        let dir = with_backup(&vm);
        vm.fake.fail_run(" backup ", "locked");
        let err = vm
            .gate()
            .restore(Env::Staging, "20260901T000000Z.db")
            .unwrap_err();
        assert!(matches!(err, Failure::Failed(_)), "{err:?}");
        assert_has(&err.to_string(), &["locked"]);
        assert_eq!(fs::read_to_string(dir.join("agent.db")).unwrap(), "db");
        assert!(!dir.join("agent.db.restore").exists());
        assert_eq!(vm.fake.count("systemctl start athena-serve@staging"), 1);
    }

    #[test]
    fn install_gate_replaces_the_installed_gate_after_it_runs() {
        let vm = Vm::new();
        let out = vm.gate().install_gate(&digest(HEX_A)).unwrap();
        let installed = vm.root().join("opt/athena/bin/deploy-gate");
        assert_eq!(
            fs::read_to_string(&installed).unwrap(),
            "deploy-gate binary"
        );
        assert_eq!(
            fs::metadata(&installed).unwrap().permissions().mode() & 0o777,
            0o755
        );
        assert_eq!(out["version"], "deploy-gate 0.1.0");
        assert_eq!(out["installed"], installed.to_string_lossy().as_ref());
        let check = vm.fake.find_run("deploy-gate --version").unwrap();
        assert_eq!(check.user.as_deref(), Some("athena"));
        assert_eq!(vm.fake.count("systemctl"), 0);
    }

    #[test]
    fn install_gate_refuses_an_artifact_whose_gate_is_missing_or_broken() {
        let vm = Vm::new();
        vm.fake.pull_files.replace(vec!["athena"]);
        let err = vm
            .gate()
            .install_gate(&digest(HEX_A))
            .unwrap_err()
            .to_string();
        assert!(err.contains("has no `deploy-gate` file"), "{err}");

        let vm = Vm::new();
        vm.fake.fail_run("deploy-gate --version", "segfault");
        let err = vm
            .gate()
            .install_gate(&digest(HEX_A))
            .unwrap_err()
            .to_string();
        assert!(err.contains("segfault"), "{err}");
        assert!(!vm.root().join("opt/athena/bin/deploy-gate").exists());
    }

    #[test]
    fn session_ids_are_url_safe() {
        assert!(is_session_id("0b6f-4c_1"));
        for bad in ["", "a/b", "a?b", "a b", &"a".repeat(129)] {
            assert!(!is_session_id(bad), "{bad:?}");
        }
    }
}
