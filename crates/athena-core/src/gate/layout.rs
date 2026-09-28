//! Where everything lives on the VM ("VM layout" in `plans/contracts.md`),
//! under a root that is `/` in production and a temporary directory in
//! tests.
//!
//! A layout belongs to one agent. Athena, the first agent, keeps the layout
//! it had before there were others (`/etc/athena/<env>.env`,
//! `athena-serve@<env>`, user `athena`), so nothing on a running VM moves.
//! Every other agent `<a>` lives under `agents/<a>/`, runs as `athena-<a>`
//! and has its own units, `athena-<a>-serve@<env>`.

use super::Env;
use crate::spec::valid_name;
use std::path::{Path, PathBuf};

/// The agent whose layout predates the others.
pub const LEGACY_AGENT: &str = "athena";

/// An agent name the gate accepts: see [`valid_name`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Agent(String);

impl Agent {
    pub fn parse(word: &str) -> Option<Agent> {
        valid_name(word).then(|| Agent(word.to_string()))
    }

    /// Athena: what a key or command without `--agent` means.
    pub fn legacy() -> Agent {
        Agent(LEGACY_AGENT.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn is_legacy(&self) -> bool {
        self.0 == LEGACY_AGENT
    }
}

impl std::fmt::Display for Agent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

pub struct Layout {
    root: PathBuf,
    agent: Agent,
}

impl Layout {
    /// Athena's layout.
    pub fn new(root: impl Into<PathBuf>) -> Layout {
        Layout::for_agent(root, Agent::legacy())
    }

    pub fn for_agent(root: impl Into<PathBuf>, agent: Agent) -> Layout {
        Layout {
            root: root.into(),
            agent,
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn agent(&self) -> &Agent {
        &self.agent
    }

    fn at(&self, relative: &str) -> PathBuf {
        self.root.join(relative)
    }

    /// `<base>` for Athena, `<base>/agents/<a>` for any other agent.
    fn scoped(&self, base: &str) -> PathBuf {
        match self.agent.is_legacy() {
            true => self.at(base),
            false => self.at(base).join("agents").join(self.agent.as_str()),
        }
    }

    // ---- shared by every agent ----

    /// `/etc/athena/gate.env`: `ATHENA_REPO` (Athena's, and the gate's own),
    /// `ATHENA_KEEP_RELEASES`, `ORAS`.
    pub fn gate_env(&self) -> PathBuf {
        self.at("etc/athena/gate.env")
    }

    pub fn installed_gate(&self) -> PathBuf {
        self.at("opt/athena/bin/deploy-gate")
    }

    /// Where every agent's `/etc/athena/agents/<a>/agent.env` is.
    pub fn agents_dir(&self) -> PathBuf {
        self.at("etc/athena/agents")
    }

    /// Athena's lock, which `install-gate` also takes.
    pub fn global_lock(&self) -> PathBuf {
        self.at("var/lib/athena/.gate.lock")
    }

    // ---- this agent's ----

    /// `/etc/athena/agents/<a>/agent.env`: `ATHENA_REPO`, `PORT_STAGING`,
    /// `PORT_PROD`. An agent other than Athena is known only if it has one.
    pub fn agent_env(&self) -> PathBuf {
        self.agents_dir()
            .join(self.agent.as_str())
            .join("agent.env")
    }

    /// The services' settings and secrets.
    pub fn env_file(&self, env: Env) -> PathBuf {
        self.scoped("etc/athena").join(format!("{env}.env"))
    }

    pub fn releases(&self) -> PathBuf {
        self.scoped("opt/athena").join("releases")
    }

    pub fn release(&self, hex: &str) -> PathBuf {
        self.releases().join(hex)
    }

    /// The symlink the units run from.
    pub fn current(&self, env: Env) -> PathBuf {
        self.scoped("opt/athena").join(env.as_str()).join("current")
    }

    /// This agent's data: the directory under which each env's is.
    pub fn athena_data(&self) -> PathBuf {
        self.scoped("var/lib/athena")
    }

    /// Serialises this agent's deploys. Other agents deploy at the same time.
    pub fn lock(&self) -> PathBuf {
        match self.agent.is_legacy() {
            true => self.global_lock(),
            false => self.gate_dir().join(".lock"),
        }
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

    /// Root-only: the data directory is writable by the service, and
    /// `promote` trusts this file.
    pub fn state(&self, env: Env) -> PathBuf {
        self.gate_dir().join(format!("{env}.state.json"))
    }

    fn gate_dir(&self) -> PathBuf {
        match self.agent.is_legacy() {
            true => self.at("var/lib/athena/gate"),
            false => self.at("var/lib/athena/gate").join(self.agent.as_str()),
        }
    }

    /// The system user the services run as. Binaries from a release are
    /// only ever executed as this user, never as root.
    pub fn user(&self) -> String {
        match self.agent.is_legacy() {
            true => LEGACY_AGENT.into(),
            false => format!("athena-{}", self.agent),
        }
    }

    /// `athena` for Athena, `athena-<a>` for the others: the front of every
    /// unit name.
    fn unit_prefix(&self) -> String {
        self.user()
    }

    pub fn serve_unit(&self, env: Env) -> String {
        format!("{}-serve@{env}.service", self.unit_prefix())
    }

    pub fn telegram_unit(&self, env: Env) -> String {
        format!("{}-telegram@{env}.service", self.unit_prefix())
    }

    pub fn target(&self, env: Env) -> String {
        format!("{}@{env}.target", self.unit_prefix())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths(l: &Layout) -> Vec<String> {
        [
            l.gate_env(),
            l.env_file(Env::Prod),
            l.release("ab"),
            l.current(Env::Staging),
            l.installed_gate(),
            l.lock(),
            l.db(Env::Staging),
            l.backups(Env::Prod),
            l.state(Env::Staging),
            l.agent_env(),
            l.global_lock(),
        ]
        .iter()
        .map(|p| p.display().to_string())
        .collect()
    }

    #[test]
    fn athena_keeps_the_layout_it_had() {
        let l = Layout::new("/");
        assert_eq!(
            paths(&l),
            [
                "/etc/athena/gate.env",
                "/etc/athena/prod.env",
                "/opt/athena/releases/ab",
                "/opt/athena/staging/current",
                "/opt/athena/bin/deploy-gate",
                "/var/lib/athena/.gate.lock",
                "/var/lib/athena/staging/agent.db",
                "/var/lib/athena/prod/backups",
                "/var/lib/athena/gate/staging.state.json",
                "/etc/athena/agents/athena/agent.env",
                "/var/lib/athena/.gate.lock",
            ]
        );
        assert_eq!(l.root(), Path::new("/"));
        assert_eq!(l.user(), "athena");
        assert_eq!(l.serve_unit(Env::Staging), "athena-serve@staging.service");
        assert_eq!(l.telegram_unit(Env::Prod), "athena-telegram@prod.service");
        assert_eq!(l.target(Env::Prod), "athena@prod.target");
        assert!(l.agent().is_legacy());
    }

    #[test]
    fn other_agents_live_under_agents() {
        let l = Layout::for_agent("/", Agent::parse("notes").unwrap());
        assert_eq!(
            paths(&l),
            [
                "/etc/athena/gate.env",
                "/etc/athena/agents/notes/prod.env",
                "/opt/athena/agents/notes/releases/ab",
                "/opt/athena/agents/notes/staging/current",
                "/opt/athena/bin/deploy-gate",
                "/var/lib/athena/gate/notes/.lock",
                "/var/lib/athena/agents/notes/staging/agent.db",
                "/var/lib/athena/agents/notes/prod/backups",
                "/var/lib/athena/gate/notes/staging.state.json",
                "/etc/athena/agents/notes/agent.env",
                "/var/lib/athena/.gate.lock",
            ]
        );
        assert_eq!(l.user(), "athena-notes");
        assert_eq!(l.serve_unit(Env::Prod), "athena-notes-serve@prod.service");
        assert_eq!(
            l.telegram_unit(Env::Staging),
            "athena-notes-telegram@staging.service"
        );
        assert_eq!(l.target(Env::Staging), "athena-notes@staging.target");
        assert_eq!(l.agent().to_string(), "notes");
        assert_eq!(l.agents_dir(), Path::new("/etc/athena/agents"));
    }

    #[test]
    fn agent_names_are_checked() {
        assert_eq!(Agent::parse("notes").unwrap().as_str(), "notes");
        for bad in ["", "../etc", "a/b", "Notes", "my-agent"] {
            assert!(Agent::parse(bad).is_none(), "{bad}");
        }
        assert!(Agent::legacy().is_legacy());
        assert!(!Agent::parse("notes").unwrap().is_legacy());
    }
}
