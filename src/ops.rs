//! Running athena as a deployed service: its version, online database
//! backups, and the rule that long-running processes name their database.
//!
//! `athena --version` and `athena backup` live here rather than in the CLI
//! because they need neither a [`crate::service::Service`] nor an API key,
//! and `backup` must never open the database through [`crate::store::Store`],
//! which migrates it.

use anyhow::{Context, Result, bail};
use rusqlite::backup::{Backup, StepResult};
use rusqlite::{Connection, OpenFlags};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// How long a backup waits for a writer that holds the database locked.
pub const BACKUP_TIMEOUT: Duration = Duration::from_secs(30);

/// This build's version: `ATHENA_VERSION`, else the crate version marked
/// `-dev`. Deploys set `ATHENA_VERSION` to the release they installed.
pub fn version() -> String {
    version_or_dev(std::env::var("ATHENA_VERSION").ok())
}

fn version_or_dev(configured: Option<String>) -> String {
    configured
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| format!("{}-dev", env!("CARGO_PKG_VERSION")))
}

/// Whether `args` asks for one of the commands [`run`] handles.
pub fn requested(args: &[String]) -> bool {
    args.first()
        .is_some_and(|a| a == "--version" || a == "backup")
}

/// `athena --version` or `athena backup DEST`, backing up
/// [`crate::store::path`].
pub fn run(args: &[String], out: &mut impl Write) -> Result<()> {
    match args {
        [flag] if flag == "--version" => {
            writeln!(out, "athena {}", version())?;
            Ok(())
        }
        [cmd, dest] if cmd == "backup" => {
            let source = crate::store::path();
            backup(&source, Path::new(dest))?;
            writeln!(out, "backed up {source} to {dest}")?;
            Ok(())
        }
        [cmd, ..] if cmd == "backup" => bail!("usage: athena backup DEST"),
        _ => bail!("usage: athena --version"),
    }
}

/// Copy the database at `source` to a new file at `dest`, creating its
/// parent directories. Safe while other processes use the database: SQLite's
/// online backup copies every page in one step, under one read transaction,
/// so the copy is a consistent snapshot that includes what is still in the
/// WAL. While a writer holds the database locked it waits, for up to
/// [`BACKUP_TIMEOUT`].
///
/// `source` is opened read-only and as it is, never migrated, so an older
/// binary can back up the database a newer one is about to migrate.
/// An existing `dest` is refused rather than overwritten. The copy is
/// written beside `dest` and renamed into place, so `dest` only ever holds
/// a whole backup, and a failed one leaves nothing behind.
pub fn backup(source: &str, dest: &Path) -> Result<()> {
    backup_within(source, dest, BACKUP_TIMEOUT)
}

fn backup_within(source: &str, dest: &Path, timeout: Duration) -> Result<()> {
    if dest.exists() {
        bail!("{} already exists; back up to a new file", dest.display());
    }
    let db = Connection::open_with_flags(source, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .with_context(|| format!("opening the database {source} read-only"))?;
    // Only `/` has no parent, and `create_dir_all("")` does nothing.
    let parent = dest.parent().unwrap_or(Path::new(""));
    let created = std::fs::create_dir_all(parent);
    created.with_context(|| format!("creating {}", parent.display()))?;
    let partial = partial_path(dest);
    let copied = copy(&db, &partial, timeout).and_then(|()| Ok(std::fs::rename(&partial, dest)?));
    if copied.is_err() {
        // The error below is the one worth reporting.
        let _ = std::fs::remove_file(&partial);
    }
    copied.with_context(|| format!("backing up {source} to {}", dest.display()))
}

/// Where a backup to `dest` is written until it is complete.
fn partial_path(dest: &Path) -> PathBuf {
    let mut partial = dest.as_os_str().to_owned();
    partial.push(".partial");
    partial.into()
}

fn copy(db: &Connection, dest: &Path, timeout: Duration) -> Result<()> {
    // The step below waits in SQLite's busy handler while a writer holds
    // the lock, and reports `Busy` once `timeout` has passed.
    db.busy_timeout(timeout)?;
    let mut to = Connection::open(dest)?;
    let backup = Backup::new(db, &mut to)?;
    // -1 copies every page in one step: one read transaction, one snapshot.
    match backup.step(-1)? {
        StepResult::Done => Ok(()),
        _ => bail!("the database stayed locked for {timeout:?}"),
    }
}

/// `serve` and `telegram` refuse to start without an absolute `ATHENA_DB`,
/// so a service never creates a fresh database wherever it happens to run.
pub fn require_absolute_db() -> Result<()> {
    absolute_db(std::env::var("ATHENA_DB").ok())
}

fn absolute_db(configured: Option<String>) -> Result<()> {
    match configured {
        Some(path) if Path::new(&path).is_absolute() => Ok(()),
        Some(path) => {
            bail!("ATHENA_DB must be an absolute path for `serve` and `telegram`; got `{path}`")
        }
        None => bail!(
            "ATHENA_DB is not set; `serve` and `telegram` need an absolute database \
             path, such as /var/lib/athena/prod/agent.db"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn the_version_is_the_configured_one_else_the_crate_version_marked_dev() {
        let dev = format!("{}-dev", env!("CARGO_PKG_VERSION"));
        assert_eq!(version_or_dev(None), dev);
        assert_eq!(version_or_dev(Some(String::new())), dev);
        assert_eq!(version_or_dev(Some("v1.2.3+abc".into())), "v1.2.3+abc");
    }

    #[test]
    fn only_version_and_backup_are_handled_here() {
        assert!(requested(&args(&["--version"])));
        assert!(requested(&args(&["backup", "x"])));
        for other in [&[][..], &["serve"][..], &["backups"][..]] {
            assert!(!requested(&args(other)), "{other:?}");
        }
    }

    #[test]
    fn malformed_commands_are_refused_with_their_usage() {
        for (list, usage) in [
            (&["backup"][..], "usage: athena backup DEST"),
            (&["backup", "a", "b"][..], "usage: athena backup DEST"),
            (&["--version", "x"][..], "usage: athena --version"),
        ] {
            let err = run(&args(list), &mut Vec::new()).unwrap_err().to_string();
            assert_eq!(err, usage, "{list:?}");
        }
    }

    /// A database path in the temp dir, removed with its companions on drop.
    struct Temp(PathBuf);

    impl Temp {
        fn new() -> Self {
            let name = format!("athena-ops-{}.db", uuid::Uuid::new_v4());
            Self(std::env::temp_dir().join(name))
        }

        fn path(&self) -> &str {
            self.0.to_str().unwrap()
        }
    }

    impl Drop for Temp {
        fn drop(&mut self) {
            for suffix in ["", "-journal", ".partial", ".partial-journal"] {
                let _ = std::fs::remove_file(format!("{}{suffix}", self.path()));
            }
        }
    }

    /// A database with one row in `t`, and a connection holding it
    /// exclusively locked part way through inserting a second.
    fn locked(source: &Temp) -> Connection {
        let writer = Connection::open(source.path()).unwrap();
        writer
            .execute_batch(
                "CREATE TABLE t (x); INSERT INTO t VALUES (1);
                 BEGIN EXCLUSIVE; INSERT INTO t VALUES (2);",
            )
            .unwrap();
        writer
    }

    fn rows(path: &str) -> i64 {
        let db = Connection::open(path).unwrap();
        db.query_row("SELECT COUNT(*) FROM t", [], |r| r.get(0))
            .unwrap()
    }

    #[test]
    fn a_backup_waits_for_a_writer_and_copies_what_it_committed() {
        let (source, dest) = (Temp::new(), Temp::new());
        let writer = locked(&source);
        let committer = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(150));
            writer.execute_batch("COMMIT").unwrap();
        });

        backup_within(source.path(), &dest.0, Duration::from_secs(10)).unwrap();

        committer.join().unwrap();
        assert_eq!(rows(dest.path()), 2);
        assert!(!partial_path(&dest.0).exists());
    }

    #[test]
    fn a_backup_gives_up_on_a_lock_held_too_long_and_leaves_no_file() {
        let (source, dest) = (Temp::new(), Temp::new());
        let _writer = locked(&source);

        let err = backup_within(source.path(), &dest.0, Duration::from_millis(50)).unwrap_err();

        assert!(format!("{err:#}").contains("stayed locked"), "{err:#}");
        assert!(!dest.0.exists());
        assert!(!partial_path(&dest.0).exists());
    }

    #[test]
    fn services_need_an_absolute_database_path() {
        assert!(absolute_db(Some("/var/lib/athena/prod/agent.db".into())).is_ok());
        let relative = absolute_db(Some("agent.db".into()))
            .unwrap_err()
            .to_string();
        assert!(relative.contains("absolute path"), "{relative}");
        assert!(relative.contains("`agent.db`"), "{relative}");
        let unset = absolute_db(None).unwrap_err().to_string();
        assert!(unset.contains("ATHENA_DB is not set"), "{unset}");
    }
}
