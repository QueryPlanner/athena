//! The VM scripts and the files they read, compiled into this binary. The
//! VM commands write them to a temporary directory laid out like the repo,
//! then run them from there, so `athena-cli` needs no checkout of Athena.

use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

/// `(path in the repo, contents)`. Scripts are made executable.
pub const FILES: &[(&str, &str)] = &[
    (
        "scripts/setup-host.sh",
        include_str!("../../../scripts/setup-host.sh"),
    ),
    (
        "scripts/doctor.sh",
        include_str!("../../../scripts/doctor.sh"),
    ),
    (
        "scripts/init-github.sh",
        include_str!("../../../scripts/init-github.sh"),
    ),
    (
        "deploy/athena.env.template",
        include_str!("../../../deploy/athena.env.template"),
    ),
    (
        "deploy/openobserve/openobserve.env.template",
        include_str!("../../../deploy/openobserve/openobserve.env.template"),
    ),
    (
        "deploy/systemd/athena-serve@.service.in",
        include_str!("../../../deploy/systemd/athena-serve@.service.in"),
    ),
    (
        "deploy/systemd/athena-telegram@.service.in",
        include_str!("../../../deploy/systemd/athena-telegram@.service.in"),
    ),
    (
        "deploy/systemd/athena@.target.in",
        include_str!("../../../deploy/systemd/athena@.target.in"),
    ),
    (
        "deploy/systemd/openobserve.service",
        include_str!("../../../deploy/systemd/openobserve.service"),
    ),
];

/// A directory holding [`FILES`], removed on drop.
pub struct Bundle {
    root: PathBuf,
}

impl Bundle {
    /// Write the files under `parent/athena-cli-<pid>-<n>`.
    pub fn write_in(parent: &Path) -> io::Result<Bundle> {
        let root = parent.join(format!("athena-cli-{}-{}", std::process::id(), next_id()));
        for (path, contents) in FILES {
            let target = root.join(path);
            fs::create_dir_all(target.parent().unwrap_or(&root))?;
            fs::write(&target, contents)?;
            if path.ends_with(".sh") {
                fs::set_permissions(&target, fs::Permissions::from_mode(0o755))?;
            }
        }
        Ok(Bundle { root })
    }

    pub fn script(&self, name: &str) -> PathBuf {
        self.root.join("scripts").join(name)
    }
}

impl Drop for Bundle {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn next_id() -> usize {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_file_the_scripts_read_is_in_the_bundle() {
        let setup = FILES[0].1;
        // `"$REPO_ROOT/deploy/..."` paths, plus the unit templates it loops over.
        let mut wanted: Vec<String> = setup
            .match_indices("$REPO_ROOT/deploy/")
            .map(|(i, _)| {
                setup[i + "$REPO_ROOT/".len()..]
                    .split(|c: char| c == '"' || c.is_whitespace())
                    .next()
                    .unwrap()
                    .to_string()
            })
            .filter(|p| p != "deploy" && !p.ends_with('/') && !p.contains('$'))
            .collect();
        for unit in [
            "athena-serve@.service",
            "athena-telegram@.service",
            "athena@.target",
        ] {
            assert!(setup.contains(unit), "{unit}");
            wanted.push(format!("deploy/systemd/{unit}.in"));
        }
        assert!(wanted.len() >= 5, "{wanted:?}");
        for path in wanted {
            let bundled = FILES.iter().any(|(p, _)| *p == path);
            assert!(bundled, "{path} is not bundled");
        }
    }

    #[test]
    fn a_bundle_is_a_repo_like_tree_that_goes_away() {
        let bundle = Bundle::write_in(&std::env::temp_dir()).unwrap();
        let setup = bundle.script("setup-host.sh");
        let mode = fs::metadata(&setup).unwrap().permissions().mode();
        assert_eq!(mode & 0o111, 0o111);
        let template = bundle.root.join("deploy/athena.env.template");
        assert!(fs::read_to_string(template).unwrap().contains("@AGENT@"));
        let root = bundle.root.clone();
        drop(bundle);
        assert!(!root.exists());
    }

    #[test]
    fn a_bundle_that_cannot_be_written_is_an_error() {
        let file = std::env::temp_dir().join(format!("athena-cli-file-{}", std::process::id()));
        fs::write(&file, "").unwrap();
        assert!(Bundle::write_in(&file).is_err());
        fs::remove_file(&file).unwrap();
    }
}
