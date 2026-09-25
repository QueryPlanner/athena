//! `deploy staging <digest>` and `promote prod <digest>`, and the release
//! cache they share with `install-gate`.

use super::command::is_hex64;
use super::{
    ATHENA_USER, Cmd, Context, Digest, Env, Failure, Gate, Result, Settings, envfile, failed,
    remove_file_if_exists, time, write_atomic,
};
use serde_json::{Value, json};
use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};

pub const DEFAULT_REPO: &str = "ghcr.io/queryplanner/athena";
pub const DEFAULT_KEEP_RELEASES: usize = 3;
pub const DEFAULT_ORAS: &str = "/usr/local/bin/oras";
/// Backups kept per env.
pub const KEEP_BACKUPS: usize = 10;
/// The manifest annotation CI sets to the git commit.
pub const REVISION_ANNOTATION: &str = "org.opencontainers.image.revision";
/// Written into each release directory: the `ATHENA_VERSION` it runs as.
pub const VERSION_FILE: &str = ".version";
/// Written into each release directory: when a deploy last used it.
const LAST_USED_FILE: &str = ".last-used";

/// `/etc/athena/gate.env`.
#[derive(Debug, PartialEq, Eq)]
pub struct GateConfig {
    pub repo: String,
    pub keep: usize,
    pub oras: String,
}

impl GateConfig {
    pub fn from_vars(vars: &envfile::Vars) -> Result<GateConfig> {
        let repo = envfile::get(vars, "ATHENA_REPO").unwrap_or(DEFAULT_REPO);
        let repo_ok = !repo.is_empty()
            && repo.bytes().all(|b| {
                b.is_ascii_alphanumeric() || matches!(b, b'.' | b'/' | b'-' | b'_' | b':')
            });
        if !repo_ok {
            return Err(failed(format!(
                "gate.env: ATHENA_REPO {repo:?} is not a repository"
            )));
        }
        let keep = match envfile::get(vars, "ATHENA_KEEP_RELEASES") {
            None => DEFAULT_KEEP_RELEASES,
            Some(n) => n.parse().ok().filter(|n| *n >= 1).ok_or_else(|| {
                failed(format!(
                    "gate.env: ATHENA_KEEP_RELEASES {n:?} is not a number >= 1"
                ))
            })?,
        };
        let oras = envfile::get(vars, "ORAS").unwrap_or(DEFAULT_ORAS);
        if !Path::new(oras).is_absolute() {
            return Err(failed(format!(
                "gate.env: ORAS {oras:?} must be an absolute path"
            )));
        }
        Ok(GateConfig {
            repo: repo.to_string(),
            keep,
            oras: oras.to_string(),
        })
    }
}

/// A version string safe to write into an env file and compare with
/// `/version`: no whitespace, quotes or newlines.
pub fn valid_version(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'+' | b'-' | b':'))
}

/// The git commit from an OCI manifest's annotations, if it is usable.
pub fn revision(manifest: &str) -> Option<String> {
    let manifest: Value = serde_json::from_str(manifest).ok()?;
    let revision = manifest["annotations"][REVISION_ANNOTATION].as_str()?;
    valid_version(revision).then(|| revision.to_string())
}

/// The version a release runs as when its manifest names no commit.
fn short_digest(hex: &str) -> String {
    format!("sha256:{}", &hex[..12])
}

impl Gate<'_> {
    pub(super) fn deploy(&self, env: Env, digest: &Digest) -> Result<Value> {
        let _lock = self.lock()?;
        if env == Env::Prod {
            self.require_staged(digest)?;
        }
        // Everything that can fail without changing anything goes first.
        let settings = self.settings(env)?;
        let config = self.gate_config()?;
        self.check_disk()?;
        let release = self.ensure_release(&config, digest)?;
        let version = self.release_version(&release);
        self.check_binary(&release, "athena")?;
        fs::write(release.join(LAST_USED_FILE), self.sys.now().to_string())
            .context(format!("marking {} as used", release.display()))?;

        let previous = self.current_release(env);
        self.stop_units(env)?;
        let backup = self
            .backup(env, &settings, previous.as_deref())
            .map_err(|e| self.restart_after(env, e))?;
        let switched = self.switch(env, &settings, &release, &version);
        if let Err(cause) = switched {
            return Err(self.roll_back(env, &settings, previous.as_deref(), cause));
        }

        let state = json!({
            "digest": digest.as_str(),
            "version": version,
            "deployed_at": time::rfc3339(self.sys.now()),
        });
        let path = self.layout.state(env);
        write_atomic(&path, format!("{state}\n").as_bytes(), 0o644, None)?;
        let pruned = self.prune_releases(config.keep)?;
        self.say(format!("{env} now runs {digest} as {version}"));
        Ok(json!({
            "env": env.as_str(),
            "digest": digest.as_str(),
            "version": version,
            "previous": previous.as_deref().and_then(release_name),
            "backup": backup.map(|p| p.display().to_string()),
            "pruned": pruned,
        }))
    }

    /// Only what staging runs may reach prod.
    fn require_staged(&self, digest: &Digest) -> Result<()> {
        let staged = self.read_state(Env::Staging).ok();
        match staged.as_ref().and_then(|s| s["digest"].as_str()) {
            Some(d) if d == digest.as_str() => Ok(()),
            Some(d) => Err(Failure::Rejected(format!(
                "{digest} is not what staging runs ({d}); deploy it to staging first"
            ))),
            None => Err(Failure::Rejected(
                "staging has no recorded deployment; deploy to staging first".into(),
            )),
        }
    }

    pub(super) fn gate_config(&self) -> Result<GateConfig> {
        let path = self.layout.gate_env();
        let vars = match fs::read_to_string(&path) {
            Ok(text) => envfile::parse(&text),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => vec![],
            Err(e) => return Err(failed(format!("reading {}: {e}", path.display()))),
        };
        GateConfig::from_vars(&vars)
    }

    /// The release directory for `digest`, pulled unless already cached.
    /// A pull lands in `<hex>.tmp` and is renamed only when complete, so a
    /// cached directory is always a whole release.
    pub(super) fn ensure_release(&self, config: &GateConfig, digest: &Digest) -> Result<PathBuf> {
        let release = self.layout.release(digest.hex());
        if release.join("athena").is_file() {
            self.say(format!("{digest} is cached in {}", release.display()));
            return Ok(release);
        }
        let tmp = self.layout.releases().join(format!("{}.tmp", digest.hex()));
        for stale in [&tmp, &release] {
            if stale.exists() {
                fs::remove_dir_all(stale).context(format!("removing {}", stale.display()))?;
            }
        }
        let reference = format!("{}@{digest}", config.repo);
        let tmp_arg = tmp.to_string_lossy();
        self.exec_ok(&Cmd::new(
            &config.oras,
            &["pull", &reference, "-o", &tmp_arg],
        ))?;
        if !tmp.join("athena").is_file() {
            return Err(failed(format!("{reference} has no `athena` file")));
        }
        for name in ["athena", "deploy-gate"] {
            let file = tmp.join(name);
            if file.is_file() {
                fs::set_permissions(&file, fs::Permissions::from_mode(0o755))
                    .context(format!("chmod {}", file.display()))?;
            }
        }
        let version = self.fetch_version(config, &reference, digest);
        fs::write(tmp.join(VERSION_FILE), &version).context(format!("writing {VERSION_FILE}"))?;
        fs::rename(&tmp, &release).context(format!("renaming {}", tmp.display()))?;
        Ok(release)
    }

    /// The commit CI recorded in the manifest, else the short digest.
    fn fetch_version(&self, config: &GateConfig, reference: &str, digest: &Digest) -> String {
        let cmd = Cmd::new(&config.oras, &["manifest", "fetch", reference]);
        let found = match self.exec_ok(&cmd) {
            Ok(out) => revision(&out.stdout),
            Err(e) => {
                self.say(e.to_string());
                None
            }
        };
        found.unwrap_or_else(|| {
            self.say(format!(
                "no usable {REVISION_ANNOTATION}; the version is the digest"
            ));
            short_digest(digest.hex())
        })
    }

    /// The `ATHENA_VERSION` a release directory runs as.
    fn release_version(&self, release: &Path) -> String {
        fs::read_to_string(release.join(VERSION_FILE))
            .ok()
            .map(|v| v.trim().to_string())
            .filter(|v| valid_version(v))
            .unwrap_or_else(|| short_digest(release_name(release).unwrap_or_default().as_str()))
    }

    /// `<release>/<binary> --version`, run as the service user, must work.
    pub(super) fn check_binary(&self, release: &Path, binary: &str) -> Result<String> {
        let cmd = Cmd::new(release.join(binary), &["--version"])
            .as_user(ATHENA_USER)
            .in_dir(self.layout.root());
        let out = self.exec_ok(&cmd)?;
        let line = out.stdout.trim().to_string();
        if line.starts_with(&format!("{binary} ")) {
            Ok(line)
        } else {
            Err(failed(format!("`{binary} --version` printed {line:?}")))
        }
    }

    /// Back up the database with the release that wrote it. Skipped on a
    /// first deploy or when there is no database yet.
    pub(super) fn backup(
        &self,
        env: Env,
        settings: &Settings,
        release: Option<&Path>,
    ) -> Result<Option<PathBuf>> {
        let Some(release) = release else {
            self.say("no previous release: no backup");
            return Ok(None);
        };
        if !self.layout.db(env).exists() {
            self.say("no database yet: no backup");
            return Ok(None);
        }
        let dest = self
            .layout
            .backups(env)
            .join(format!("{}.db", time::compact(self.sys.now())));
        let dest_arg = dest.to_string_lossy();
        let athena = release.join("athena");
        self.exec_ok(&self.athena_cmd(env, settings, &athena, &["backup", &dest_arg]))?;
        self.prune_backups(env)?;
        Ok(Some(dest))
    }

    /// Delete all but the newest [`KEEP_BACKUPS`] backups the gate named.
    /// Other files in the directory are left alone.
    fn prune_backups(&self, env: Env) -> Result<()> {
        let dir = self.layout.backups(env);
        let what = format!("pruning {}", dir.display());
        let mut names = Vec::new();
        for entry in fs::read_dir(&dir).context(&what)? {
            let name = entry
                .context(&what)?
                .file_name()
                .to_string_lossy()
                .into_owned();
            if is_backup_name(&name) {
                names.push(name);
            }
        }
        names.sort();
        let excess = names.len().saturating_sub(KEEP_BACKUPS);
        for name in &names[..excess] {
            fs::remove_file(dir.join(name)).context(&what)?;
            self.say(format!("removed old backup {name}"));
        }
        Ok(())
    }

    /// Point `current` at `release`, record the version, start the units
    /// and wait until they are healthy.
    fn switch(&self, env: Env, settings: &Settings, release: &Path, version: &str) -> Result<()> {
        self.point_current(env, release)?;
        envfile::set_var(&self.layout.env_file(env), "ATHENA_VERSION", version)?;
        self.systemctl(&["start", &format!("athena-serve@{env}.service")])?;
        self.wait_healthy(settings.addr, Some(version))?;
        if self.telegram_enabled(env)? {
            self.systemctl(&["start", &format!("athena-telegram@{env}.service")])?;
        }
        Ok(())
    }

    /// Swap the `current` symlink in one rename.
    fn point_current(&self, env: Env, release: &Path) -> Result<()> {
        let link = self.layout.current(env);
        let tmp = link.with_file_name("current.tmp");
        let what = format!("pointing {} at {}", link.display(), release.display());
        fs::create_dir_all(link.parent().unwrap_or(Path::new("/"))).context(&what)?;
        remove_file_if_exists(&tmp).context(&what)?;
        symlink(release, &tmp).context(&what)?;
        fs::rename(&tmp, &link).context(&what)?;
        self.say(what);
        Ok(())
    }

    /// Put the previous release back after a failed switch.
    fn roll_back(
        &self,
        env: Env,
        settings: &Settings,
        previous: Option<&Path>,
        cause: Failure,
    ) -> Failure {
        self.say(format!("deploy failed: {cause}"));
        let Some(previous) = previous else {
            let stopped = self
                .stop_units(env)
                .err()
                .map(|e| format!("; stopping: {e}"));
            return failed(format!(
                "{cause}; there is no previous release to roll back to, so {env} is stopped{}",
                stopped.unwrap_or_default()
            ));
        };
        let name = release_name(previous).unwrap_or_default();
        self.say(format!("rolling {env} back to {name}"));
        let version = self.release_version(previous);
        let result = self
            .stop_units(env)
            .and_then(|()| self.switch(env, settings, previous, &version));
        match result {
            Ok(()) => failed(format!(
                "{cause}; rolled back to {name}. If the new release migrated the \
                 database, the old one may refuse it: restore the pre-deploy backup"
            )),
            Err(e) => failed(format!("{cause}; rolling back to {name} failed too: {e}")),
        }
    }

    /// Keep the [`GateConfig::keep`] most recently used releases, and
    /// every release an env's `current` points at. Remove stale partial
    /// pulls. Returns the removed release names.
    fn prune_releases(&self, keep: usize) -> Result<Vec<String>> {
        let dir = self.layout.releases();
        let what = format!("pruning {}", dir.display());
        let in_use: Vec<String> = Env::ALL
            .into_iter()
            .filter_map(|env| self.current_release(env))
            .filter_map(|p| release_name(&p))
            .collect();
        let mut releases = Vec::new();
        for entry in fs::read_dir(&dir).context(&what)? {
            let path = entry.context(&what)?.path();
            let name = path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned();
            if name.strip_suffix(".tmp").is_some_and(is_hex64) {
                fs::remove_dir_all(&path).context(&what)?;
            } else if is_hex64(&name) {
                releases.push((last_used(&path), name));
            }
        }
        releases.sort_by(|a, b| b.cmp(a));
        let mut pruned = Vec::new();
        for (_, name) in releases.into_iter().skip(keep) {
            if !in_use.contains(&name) {
                fs::remove_dir_all(dir.join(&name)).context(&what)?;
                self.say(format!("pruned release {name}"));
                pruned.push(name);
            }
        }
        Ok(pruned)
    }
}

fn release_name(path: &Path) -> Option<String> {
    Some(path.file_name()?.to_string_lossy().into_owned())
}

fn last_used(release: &Path) -> u64 {
    fs::read_to_string(release.join(LAST_USED_FILE))
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0)
}

/// `20260925T120000Z.db`, as the gate names backups.
fn is_backup_name(name: &str) -> bool {
    let b = name.as_bytes();
    name.len() == 19
        && name.ends_with("Z.db")
        && b[8] == b'T'
        && b[..8].iter().chain(&b[9..15]).all(u8::is_ascii_digit)
}

#[cfg(test)]
mod tests {
    use super::super::testing::*;
    use super::*;

    fn pairs(list: &[(&str, &str)]) -> envfile::Vars {
        list.iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    fn digest(hex: &str) -> Digest {
        Digest::parse(&format!("sha256:{hex}")).unwrap()
    }

    #[test]
    fn gate_config_defaults_and_validation() {
        let defaults = GateConfig::from_vars(&vec![]).unwrap();
        assert_eq!(
            defaults,
            GateConfig {
                repo: DEFAULT_REPO.into(),
                keep: 3,
                oras: DEFAULT_ORAS.into()
            }
        );
        let set = GateConfig::from_vars(&pairs(&[
            ("ATHENA_REPO", "ghcr.io/x/y"),
            ("ATHENA_KEEP_RELEASES", "5"),
            ("ORAS", "/bin/oras"),
        ]))
        .unwrap();
        assert_eq!(
            (set.repo.as_str(), set.keep, set.oras.as_str()),
            ("ghcr.io/x/y", 5, "/bin/oras")
        );
        for (key, value, message) in [
            ("ATHENA_REPO", "", "ATHENA_REPO"),
            ("ATHENA_REPO", "x y", "ATHENA_REPO"),
            ("ATHENA_KEEP_RELEASES", "0", "ATHENA_KEEP_RELEASES"),
            ("ATHENA_KEEP_RELEASES", "three", "ATHENA_KEEP_RELEASES"),
            ("ORAS", "oras", "absolute"),
        ] {
            let err = GateConfig::from_vars(&pairs(&[(key, value)])).unwrap_err();
            assert!(err.to_string().contains(message), "{key}={value}: {err}");
        }
    }

    #[test]
    fn the_version_comes_from_a_safe_revision_annotation() {
        let manifest = |rev: &str| json!({"annotations": {REVISION_ANNOTATION: rev}}).to_string();
        assert_eq!(revision(&manifest("0123abc")), Some("0123abc".into()));
        assert_eq!(
            revision(&manifest("v1.2.3+0123abc")),
            Some("v1.2.3+0123abc".into())
        );
        for bad in [
            "",
            "a b",
            "a\nATHENA_DB=/etc/shadow",
            "\"quoted\"",
            &"a".repeat(65),
        ] {
            assert_eq!(revision(&manifest(bad)), None, "{bad:?}");
        }
        assert_eq!(revision("{}"), None);
        assert_eq!(revision("not json"), None);
    }

    #[test]
    fn backup_names_are_the_gates_own_timestamps() {
        assert!(is_backup_name("20260925T120000Z.db"));
        for name in [
            "20260925T120000Z.db.bak",
            "2026092xT120000Z.db",
            "20260925X120000Z.db",
            "20260925T12000aZ.db",
            "manual.db",
            "20260925T120000Y.db",
        ] {
            assert!(!is_backup_name(name), "{name}");
        }
    }

    #[test]
    fn a_first_deploy_pulls_switches_starts_and_records_state() {
        let vm = Vm::new();
        let out = vm.gate().deploy(Env::Staging, &digest(HEX_A)).unwrap();

        let release = vm.root().join("opt/athena/releases").join(HEX_A);
        assert_eq!(
            fs::read_link(vm.root().join("opt/athena/staging/current")).unwrap(),
            release
        );
        let mode = fs::metadata(release.join("athena"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o755);
        assert_eq!(
            vm.env_var(Env::Staging, "ATHENA_VERSION").as_deref(),
            Some(REVISION)
        );
        assert_eq!(
            vm.env_var(Env::Staging, "SECRET").as_deref(),
            Some("s3cret")
        );
        assert_eq!(
            vm.state(Env::Staging),
            json!({"digest": format!("sha256:{HEX_A}"), "version": REVISION, "deployed_at": "2026-09-25T12:00:00Z"})
        );
        assert_eq!(
            out,
            json!({
                "env": "staging", "digest": format!("sha256:{HEX_A}"), "version": REVISION,
                "previous": null, "backup": null, "pruned": [],
            })
        );
        let oras = format!("run /usr/local/bin/oras pull {DEFAULT_REPO}@sha256:{HEX_A} -o");
        let expected = [
            &oras[..],
            "run /usr/local/bin/oras manifest fetch",
            &format!("run [athena] {}/athena --version", release.display()),
            "run /usr/bin/systemctl stop athena@staging.target athena-serve@staging.service athena-telegram@staging.service",
            "run /usr/bin/systemctl start athena-serve@staging.service",
            "http GET /health",
            "http GET /version",
            "run /usr/bin/systemctl is-enabled --quiet athena-telegram@staging.service",
        ];
        vm.fake.assert_effects_start_with(&expected);
        assert_eq!(vm.fake.effects().len(), expected.len());
        assert!(vm.fake.said("no previous release: no backup"));
    }

    #[test]
    fn a_redeploy_backs_up_with_the_old_release_before_switching() {
        let vm = Vm::new();
        vm.gate().deploy(Env::Staging, &digest(HEX_A)).unwrap();
        vm.create_db(Env::Staging);
        vm.fake.clear();
        vm.fake.now.set(NOW + 60);

        let out = vm.gate().deploy(Env::Staging, &digest(HEX_B)).unwrap();

        let old = vm.root().join("opt/athena/releases").join(HEX_A);
        let dest = vm
            .root()
            .join("var/lib/athena/staging/backups/20260925T120100Z.db");
        let backup = vm.fake.find_run(" backup ").expect("a backup ran");
        assert_eq!(backup.program, old.join("athena").to_string_lossy());
        assert_eq!(backup.args, ["backup", &dest.to_string_lossy()]);
        assert_eq!(backup.user.as_deref(), Some("athena"));
        assert_eq!(
            backup.cwd.as_deref(),
            Some(vm.root().join("var/lib/athena/staging").as_path())
        );
        assert!(backup.env.contains(&("SECRET".into(), "s3cret".into())));
        // While the backup ran, `current` still pointed at the old release.
        assert_eq!(vm.fake.current_at_backup(), Some(old));
        let stop = vm.fake.position("systemctl stop").unwrap();
        let backed_up = vm.fake.position(" backup ").unwrap();
        let start = vm.fake.position("systemctl start").unwrap();
        assert!(stop < backed_up && backed_up < start);
        assert_eq!(out["previous"], HEX_A);
        assert_eq!(out["backup"], dest.to_string_lossy().as_ref());
    }

    #[test]
    fn only_the_newest_ten_backups_are_kept_and_other_files_are_left() {
        let vm = Vm::new();
        vm.gate().deploy(Env::Staging, &digest(HEX_A)).unwrap();
        vm.create_db(Env::Staging);
        let dir = vm.root().join("var/lib/athena/staging/backups");
        fs::create_dir_all(&dir).unwrap();
        for day in 10..22 {
            fs::write(dir.join(format!("202609{day}T000000Z.db")), "old").unwrap();
        }
        fs::write(dir.join("manual.db"), "keep").unwrap();

        vm.gate().deploy(Env::Staging, &digest(HEX_B)).unwrap();

        let mut names: Vec<String> = fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        let mut expected: Vec<String> = (13..22).map(|d| format!("202609{d}T000000Z.db")).collect();
        expected.push("20260925T120000Z.db".into());
        expected.push("manual.db".into());
        assert_eq!(names, expected);
    }

    #[test]
    fn no_database_means_no_backup() {
        let vm = Vm::new();
        vm.gate().deploy(Env::Staging, &digest(HEX_A)).unwrap();
        let out = vm.gate().deploy(Env::Staging, &digest(HEX_B)).unwrap();
        assert_eq!(out["backup"], Value::Null);
        assert!(vm.fake.find_run(" backup ").is_none());
        assert!(vm.fake.said("no database yet: no backup"));
    }

    #[test]
    fn a_failed_backup_restarts_the_old_release_and_changes_nothing() {
        let vm = Vm::new();
        vm.gate().deploy(Env::Staging, &digest(HEX_A)).unwrap();
        vm.create_db(Env::Staging);
        vm.fake.fail_run(" backup ", "disk I/O error");
        vm.fake.clear();

        let err = vm.gate().deploy(Env::Staging, &digest(HEX_B)).unwrap_err();

        assert!(err.to_string().contains("disk I/O error"), "{err}");
        assert_eq!(vm.current(Env::Staging).as_deref(), Some(HEX_A));
        assert_eq!(
            vm.env_var(Env::Staging, "ATHENA_VERSION").as_deref(),
            Some(REVISION)
        );
        assert!(
            vm.fake
                .position("systemctl start athena-serve@staging")
                .unwrap()
                > vm.fake.position(" backup ").unwrap()
        );
        assert_eq!(vm.state(Env::Staging)["digest"], format!("sha256:{HEX_A}"));
    }

    #[test]
    fn a_failed_restart_after_a_failed_backup_says_both() {
        let vm = Vm::new();
        vm.gate().deploy(Env::Staging, &digest(HEX_A)).unwrap();
        vm.create_db(Env::Staging);
        vm.fake.fail_run(" backup ", "disk I/O error");
        vm.fake.fail_run("systemctl start", "unit failed");
        let err = vm
            .gate()
            .deploy(Env::Staging, &digest(HEX_B))
            .unwrap_err()
            .to_string();
        assert_has(&err, &["disk I/O error", "failed too: ", "unit failed"]);
    }

    #[test]
    fn a_failed_health_check_rolls_back_to_the_previous_release() {
        let vm = Vm::new();
        vm.gate().deploy(Env::Staging, &digest(HEX_A)).unwrap();
        let before = vm.state(Env::Staging);
        // The new release never becomes healthy.
        vm.fake
            .unhealthy_versions
            .borrow_mut()
            .push(REVISION_B.into());
        vm.fake.clear();

        let err = vm
            .gate()
            .deploy(Env::Staging, &digest(HEX_B))
            .unwrap_err()
            .to_string();

        assert_has(&err, &["not healthy after 60s", "rolled back to"]);
        assert_eq!(vm.current(Env::Staging).as_deref(), Some(HEX_A));
        assert_eq!(
            vm.env_var(Env::Staging, "ATHENA_VERSION").as_deref(),
            Some(REVISION)
        );
        assert_eq!(vm.state(Env::Staging), before);
        // It polled for the whole minute, then restarted the old release.
        assert_eq!(vm.fake.now.get(), NOW + 60);
        let starts = vm.fake.count("systemctl start athena-serve@staging");
        assert_eq!(starts, 2);
        assert!(vm.root().join("opt/athena/releases").join(HEX_B).exists());
    }

    #[test]
    fn a_service_that_becomes_healthy_within_the_minute_is_accepted() {
        let vm = Vm::new();
        vm.fake.fail_http("GET /health", 3);
        vm.gate().deploy(Env::Staging, &digest(HEX_A)).unwrap();
        assert_eq!(vm.fake.now.get(), NOW + 3);
    }

    #[test]
    fn a_failed_first_deploy_has_nothing_to_roll_back_to() {
        let vm = Vm::new();
        vm.fake.fail_http("GET /health", 1000);
        let err = vm
            .gate()
            .deploy(Env::Staging, &digest(HEX_A))
            .unwrap_err()
            .to_string();
        assert_has(&err, &["no previous release"]);
        assert!(err.ends_with("staging is stopped"), "{err}");
        assert_eq!(vm.fake.count("systemctl stop"), 2);
        assert!(!vm.root().join("var/lib/athena/staging/state.json").exists());
    }

    #[test]
    fn a_failed_stop_during_a_failed_first_deploy_is_reported() {
        let vm = Vm::new();
        vm.fake.fail_http("GET /health", 1000);
        vm.fake
            .fail_run_n("systemctl stop", 1, usize::MAX, "stop timed out");
        let err = vm
            .gate()
            .deploy(Env::Staging, &digest(HEX_A))
            .unwrap_err()
            .to_string();
        assert_has(&err, &["staging is stopped; stopping: ", "stop timed out"]);
    }

    #[test]
    fn a_failed_rollback_is_reported_with_the_cause() {
        let vm = Vm::new();
        vm.gate().deploy(Env::Staging, &digest(HEX_A)).unwrap();
        vm.fake.fail_http("GET /health", 1000);
        let err = vm
            .gate()
            .deploy(Env::Staging, &digest(HEX_B))
            .unwrap_err()
            .to_string();
        assert_has(&err, &["not healthy", "rolling back to", "failed too"]);
    }

    #[test]
    fn telegram_starts_after_serve_is_healthy_when_enabled() {
        let vm = Vm::new();
        vm.fake.telegram_enabled.set(true);
        vm.gate().deploy(Env::Staging, &digest(HEX_A)).unwrap();
        let version = vm.fake.position("GET /version").unwrap();
        let telegram = vm
            .fake
            .position("systemctl start athena-telegram@staging")
            .unwrap();
        assert!(version < telegram);
    }

    #[test]
    fn a_telegram_unit_that_fails_to_start_rolls_back() {
        let vm = Vm::new();
        vm.gate().deploy(Env::Staging, &digest(HEX_A)).unwrap();
        vm.fake.telegram_enabled.set(true);
        vm.fake
            .fail_run_n("systemctl start athena-telegram", 0, 1, "bad token");
        let err = vm
            .gate()
            .deploy(Env::Staging, &digest(HEX_B))
            .unwrap_err()
            .to_string();
        assert_has(&err, &["bad token", "rolled back to"]);
        assert_eq!(vm.current(Env::Staging).as_deref(), Some(HEX_A));
    }

    #[test]
    fn low_disk_refuses_before_pulling_or_stopping() {
        let vm = Vm::new();
        vm.fake.free.set(MIN_FREE - 1);
        let err = vm
            .gate()
            .deploy(Env::Staging, &digest(HEX_A))
            .unwrap_err()
            .to_string();
        assert_has(&err, &["1023 MiB free", "need 1 GiB"]);
        assert_eq!(vm.fake.effects(), Vec::<String>::new());
        vm.fake.free.set(MIN_FREE);
        assert!(vm.gate().deploy(Env::Staging, &digest(HEX_A)).is_ok());
    }

    #[test]
    fn an_unreadable_disk_is_a_failure() {
        let vm = Vm::new();
        vm.fake.free_fails.set(true);
        let err = vm
            .gate()
            .deploy(Env::Staging, &digest(HEX_A))
            .unwrap_err()
            .to_string();
        assert!(err.starts_with("checking free space on"), "{err}");
    }

    #[test]
    fn a_held_lock_refuses_a_second_operation() {
        let vm = Vm::new();
        let held = vm.gate().lock().unwrap();
        let err = vm
            .gate()
            .deploy(Env::Staging, &digest(HEX_A))
            .unwrap_err()
            .to_string();
        assert!(err.contains("another deploy-gate is running"), "{err}");
        assert_eq!(vm.fake.effects(), Vec::<String>::new());
        drop(held);
        assert!(vm.gate().deploy(Env::Staging, &digest(HEX_A)).is_ok());
    }

    #[test]
    fn a_lock_that_cannot_be_created_is_a_failure() {
        let vm = Vm::new();
        fs::create_dir_all(vm.root().join("var/lib/athena/.gate.lock/x")).unwrap();
        let err = vm
            .gate()
            .deploy(Env::Staging, &digest(HEX_A))
            .unwrap_err()
            .to_string();
        assert!(err.starts_with("locking"), "{err}");
    }

    #[test]
    fn a_failed_pull_leaves_no_release_and_touches_no_service() {
        let vm = Vm::new();
        vm.fake.fail_run("oras pull", "manifest unknown");
        let err = vm
            .gate()
            .deploy(Env::Staging, &digest(HEX_A))
            .unwrap_err()
            .to_string();
        assert!(err.contains("manifest unknown"), "{err}");
        assert!(!vm.root().join("opt/athena/releases").join(HEX_A).exists());
        assert_eq!(vm.fake.count("systemctl"), 0);
    }

    #[test]
    fn a_missing_oras_is_a_failure_naming_it() {
        let vm = Vm::new();
        vm.fake.spawn_fails("oras pull");
        let err = vm
            .gate()
            .deploy(Env::Staging, &digest(HEX_A))
            .unwrap_err()
            .to_string();
        assert_eq!(err, "running /usr/local/bin/oras: no such program");
    }

    #[test]
    fn an_artifact_without_athena_is_refused() {
        let vm = Vm::new();
        vm.fake.pull_files.replace(vec!["deploy-gate"]);
        let err = vm
            .gate()
            .deploy(Env::Staging, &digest(HEX_A))
            .unwrap_err()
            .to_string();
        assert!(err.contains("has no `athena` file"), "{err}");
        assert_eq!(vm.fake.count("systemctl"), 0);
    }

    #[test]
    fn a_stale_partial_pull_is_replaced() {
        let vm = Vm::new();
        let releases = vm.root().join("opt/athena/releases");
        fs::create_dir_all(releases.join(format!("{HEX_A}.tmp"))).unwrap();
        fs::write(releases.join(format!("{HEX_A}.tmp/junk")), "").unwrap();
        fs::create_dir_all(releases.join(HEX_A)).unwrap(); // no athena inside
        vm.gate().deploy(Env::Staging, &digest(HEX_A)).unwrap();
        assert!(!releases.join(HEX_A).join("junk").exists());
        assert!(releases.join(HEX_A).join("athena").is_file());
        assert!(!releases.join(format!("{HEX_A}.tmp")).exists());
    }

    #[test]
    fn a_binary_that_does_not_run_stops_the_deploy_before_the_services() {
        let vm = Vm::new();
        vm.fake.fail_run("athena --version", "Exec format error");
        let err = vm
            .gate()
            .deploy(Env::Staging, &digest(HEX_A))
            .unwrap_err()
            .to_string();
        assert!(err.contains("Exec format error"), "{err}");
        assert_eq!(vm.fake.count("systemctl"), 0);

        let vm = Vm::new();
        vm.fake.version_output.replace("something else".into());
        let err = vm
            .gate()
            .deploy(Env::Staging, &digest(HEX_A))
            .unwrap_err()
            .to_string();
        assert!(err.contains("printed \"something else\""), "{err}");
    }

    #[test]
    fn without_a_usable_revision_the_version_is_the_short_digest() {
        let vm = Vm::new();
        vm.fake.fail_run("manifest fetch", "denied");
        let out = vm.gate().deploy(Env::Staging, &digest(HEX_A)).unwrap();
        assert_eq!(out["version"], format!("sha256:{}", &HEX_A[..12]));

        let vm = Vm::new();
        vm.fake.manifest.replace(Some("{}".into()));
        let out = vm.gate().deploy(Env::Staging, &digest(HEX_A)).unwrap();
        assert_eq!(out["version"], format!("sha256:{}", &HEX_A[..12]));
        assert!(vm.fake.said("no usable org.opencontainers.image.revision"));
    }

    #[test]
    fn a_cached_release_is_not_pulled_again() {
        let vm = Vm::new();
        vm.gate().deploy(Env::Staging, &digest(HEX_A)).unwrap();
        vm.fake.clear();
        vm.gate().deploy(Env::Staging, &digest(HEX_A)).unwrap();
        assert_eq!(vm.fake.count("oras"), 0);
        assert!(vm.fake.said("is cached in"));
    }

    #[test]
    fn a_missing_or_invalid_env_file_fails_before_any_change() {
        let vm = Vm::new();
        fs::remove_file(vm.root().join("etc/athena/staging.env")).unwrap();
        let err = vm
            .gate()
            .deploy(Env::Staging, &digest(HEX_A))
            .unwrap_err()
            .to_string();
        assert!(err.contains("staging.env"), "{err}");

        let vm = Vm::new();
        fs::write(
            vm.root().join("etc/athena/staging.env"),
            "ATHENA_ADDR=athena-vm:18081\n",
        )
        .unwrap();
        let err = vm
            .gate()
            .deploy(Env::Staging, &digest(HEX_A))
            .unwrap_err()
            .to_string();
        assert!(err.contains("needs ATHENA_ADDR=<ip>:<port>"), "{err}");
        assert_eq!(vm.fake.effects(), Vec::<String>::new());
    }

    #[test]
    fn gate_env_sets_the_repository_and_oras() {
        let vm = Vm::new();
        vm.write(
            "etc/athena/gate.env",
            "ATHENA_REPO=ghcr.io/me/app\nORAS=/opt/oras\n",
        );
        vm.gate().deploy(Env::Staging, &digest(HEX_A)).unwrap();
        assert_eq!(
            vm.fake
                .count(&format!("run /opt/oras pull ghcr.io/me/app@sha256:{HEX_A}")),
            1
        );

        let vm = Vm::new();
        fs::create_dir_all(vm.root().join("etc/athena/gate.env")).unwrap();
        let err = vm
            .gate()
            .deploy(Env::Staging, &digest(HEX_A))
            .unwrap_err()
            .to_string();
        assert!(err.contains("reading") && err.contains("gate.env"), "{err}");
    }

    #[test]
    fn a_failed_stop_aborts_before_the_switch() {
        let vm = Vm::new();
        vm.fake.fail_run("systemctl stop", "access denied");
        let err = vm
            .gate()
            .deploy(Env::Staging, &digest(HEX_A))
            .unwrap_err()
            .to_string();
        assert!(err.contains("access denied"), "{err}");
        assert_eq!(vm.current(Env::Staging), None);
    }

    #[test]
    fn promote_refuses_a_digest_staging_does_not_run() {
        let vm = Vm::new();
        let err = vm.gate().deploy(Env::Prod, &digest(HEX_A)).unwrap_err();
        assert!(matches!(err, Failure::Rejected(_)), "{err:?}");
        assert_has(&err.to_string(), &["no recorded deployment"]);

        vm.gate().deploy(Env::Staging, &digest(HEX_A)).unwrap();
        vm.fake.clear();
        let err = vm.gate().deploy(Env::Prod, &digest(HEX_B)).unwrap_err();
        assert!(matches!(err, Failure::Rejected(_)), "{err:?}");
        assert_has(&err.to_string(), &["is not what staging runs"]);
        assert_eq!(vm.fake.effects(), Vec::<String>::new());
        assert_eq!(vm.current(Env::Prod), None);
    }

    #[test]
    fn promote_reuses_the_staged_release_and_starts_telegram() {
        let vm = Vm::new();
        vm.gate().deploy(Env::Staging, &digest(HEX_A)).unwrap();
        vm.fake.telegram_enabled.set(true);
        vm.fake.clear();

        let out = vm.gate().deploy(Env::Prod, &digest(HEX_A)).unwrap();

        assert_eq!(out["env"], "prod");
        assert_eq!(out["version"], REVISION);
        assert_eq!(vm.current(Env::Prod).as_deref(), Some(HEX_A));
        assert_eq!(
            vm.env_var(Env::Prod, "ATHENA_VERSION").as_deref(),
            Some(REVISION)
        );
        assert_eq!(vm.state(Env::Prod)["digest"], format!("sha256:{HEX_A}"));
        assert_eq!(vm.fake.count("oras"), 0);
        assert_eq!(
            vm.fake
                .count("systemctl start athena-telegram@prod.service"),
            1
        );
        assert_eq!(vm.fake.count("@staging"), 0);
    }

    #[test]
    fn pruning_keeps_the_newest_and_never_removes_a_release_in_use() {
        let vm = Vm::new();
        vm.write("etc/athena/gate.env", "ATHENA_KEEP_RELEASES=2\n");
        // Prod runs A (the oldest); staging moves through B, C, D.
        vm.gate().deploy(Env::Staging, &digest(HEX_A)).unwrap();
        vm.gate().deploy(Env::Prod, &digest(HEX_A)).unwrap();
        for (i, hex) in [HEX_B, HEX_C, HEX_D].into_iter().enumerate() {
            vm.fake.now.set(NOW + 100 * (i as u64 + 1));
            vm.gate().deploy(Env::Staging, &digest(hex)).unwrap();
        }
        let releases = vm.root().join("opt/athena/releases");
        fs::create_dir_all(releases.join("not-a-release")).unwrap();
        fs::create_dir_all(releases.join(format!("{HEX_B}.tmp"))).unwrap();
        vm.fake.now.set(NOW + 1000);
        let out = vm.gate().deploy(Env::Staging, &digest(HEX_D)).unwrap();

        let mut left: Vec<String> = fs::read_dir(&releases)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        left.sort();
        // D is newest and in use, C is second newest, A is prod's.
        assert_eq!(left, [HEX_A, HEX_C, HEX_D, "not-a-release"]);
        assert_eq!(out["pruned"], json!([]));
        assert!(vm.fake.said(&format!("pruned release {HEX_B}")));
    }

    #[test]
    fn a_release_without_a_version_file_runs_as_its_short_digest() {
        let vm = Vm::new();
        vm.gate().deploy(Env::Staging, &digest(HEX_A)).unwrap();
        let release = vm.root().join("opt/athena/releases").join(HEX_A);
        fs::write(release.join(VERSION_FILE), "bad version\n").unwrap();
        assert_eq!(
            vm.gate().release_version(&release),
            format!("sha256:{}", &HEX_A[..12])
        );
        assert_eq!(last_used(&release), NOW);
        assert_eq!(last_used(&vm.root().join("missing")), 0);
    }
}
