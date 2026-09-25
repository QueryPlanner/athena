//! Where everything lives on the VM ("VM layout" in `plans/contracts.md`),
//! under a root that is `/` in production and a temporary directory in
//! tests.

use super::Env;
use std::path::{Path, PathBuf};

pub struct Layout {
    root: PathBuf,
}

impl Layout {
    pub fn new(root: impl Into<PathBuf>) -> Layout {
        Layout { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn at(&self, relative: &str) -> PathBuf {
        self.root.join(relative)
    }

    /// `/etc/athena/gate.env`: `ATHENA_REPO`, `ATHENA_KEEP_RELEASES`, `ORAS`.
    pub fn gate_env(&self) -> PathBuf {
        self.at("etc/athena/gate.env")
    }

    /// `/etc/athena/<env>.env`: the services' settings and secrets.
    pub fn env_file(&self, env: Env) -> PathBuf {
        self.at(&format!("etc/athena/{env}.env"))
    }

    pub fn releases(&self) -> PathBuf {
        self.at("opt/athena/releases")
    }

    pub fn release(&self, hex: &str) -> PathBuf {
        self.releases().join(hex)
    }

    /// `/opt/athena/<env>/current`, the symlink the units run from.
    pub fn current(&self, env: Env) -> PathBuf {
        self.at(&format!("opt/athena/{env}/current"))
    }

    pub fn installed_gate(&self) -> PathBuf {
        self.at("opt/athena/bin/deploy-gate")
    }

    pub fn athena_data(&self) -> PathBuf {
        self.at("var/lib/athena")
    }

    pub fn lock(&self) -> PathBuf {
        self.athena_data().join(".gate.lock")
    }

    pub fn data(&self, env: Env) -> PathBuf {
        self.athena_data().join(env.as_str())
    }

    pub fn db(&self, env: Env) -> PathBuf {
        self.data(env).join("agent.db")
    }

    pub fn backups(&self, env: Env) -> PathBuf {
        self.data(env).join("backups")
    }

    pub fn state(&self, env: Env) -> PathBuf {
        self.data(env).join("state.json")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn production_paths_match_the_contract() {
        let l = Layout::new("/");
        let paths = [
            l.gate_env(),
            l.env_file(Env::Prod),
            l.release("ab"),
            l.current(Env::Staging),
            l.installed_gate(),
            l.lock(),
            l.db(Env::Staging),
            l.backups(Env::Prod),
            l.state(Env::Staging),
        ];
        let expected = [
            "/etc/athena/gate.env",
            "/etc/athena/prod.env",
            "/opt/athena/releases/ab",
            "/opt/athena/staging/current",
            "/opt/athena/bin/deploy-gate",
            "/var/lib/athena/.gate.lock",
            "/var/lib/athena/staging/agent.db",
            "/var/lib/athena/prod/backups",
            "/var/lib/athena/staging/state.json",
        ];
        for (path, want) in paths.iter().zip(expected) {
            assert_eq!(path, Path::new(want));
        }
        assert_eq!(l.root(), Path::new("/"));
    }
}
