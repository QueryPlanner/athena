//! What makes one agent that agent: its [`AgentSpec`]. An agent's own crate
//! writes one (`src/agent.rs`) and hands it to [`crate::app::main`]; the
//! core builds the Rig agent from it for every transport, the evals and the
//! tests.

use crate::policy::ToolPolicy;
use crate::sandbox::{self, Sandboxes};
use crate::store::{SqliteMemory, Store};
use anyhow::{Result, bail};
use rig_agent as rig;
use rig_agent::agent::{AgentBuilder, WithBuilderTools};
use rig_agent::prelude::*;
use std::sync::Arc;

pub type Client = rig::core::providers::openrouter::Client;
pub type CompletionModel = rig::core::providers::openrouter::CompletionModel;

/// Adds an agent's own tools to the builder: `|b| b.tool(Add).tool(Search)`.
pub type Tools = fn(AgentBuilder<WithBuilderTools>) -> AgentBuilder<WithBuilderTools>;

/// Which of the core's sandbox and browser tools an agent gets. They are
/// only ever registered when there is a sandbox server (`OPEN_SANDBOX_URL`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SandboxTools {
    All,
    None,
    /// These names only, from [`sandbox::tools::NAMES`].
    Only(&'static [&'static str]),
}

impl SandboxTools {
    pub fn includes(&self, name: &str) -> bool {
        match self {
            SandboxTools::All => true,
            SandboxTools::None => false,
            SandboxTools::Only(names) => names.contains(&name),
        }
    }
}

/// Everything an agent's crate decides. The core decides how it runs.
#[derive(Debug, Clone)]
pub struct AgentSpec {
    /// `[a-z][a-z0-9]{0,23}`: the name on the VM (paths, units) and on every
    /// turn's span (`gen_ai.agent.name`).
    pub name: &'static str,
    /// The agent crate's `env!("CARGO_PKG_VERSION")`: what `--version`,
    /// `/version` and telemetry report when `ATHENA_VERSION` is not set.
    pub version: &'static str,
    pub preamble: &'static str,
    /// The model unless `AGENT_MODEL` says otherwise.
    pub default_model: &'static str,
    pub max_turns: usize,
    pub policy: ToolPolicy,
    pub sandbox_tools: SandboxTools,
    pub tools: Tools,
}

/// The tools of an agent that has none of its own.
pub fn no_tools(builder: AgentBuilder<WithBuilderTools>) -> AgentBuilder<WithBuilderTools> {
    builder
}

impl AgentSpec {
    /// A spec with the defaults: all sandbox tools, the default policy, 20
    /// turns, no tools of its own.
    pub fn new(
        name: &'static str,
        version: &'static str,
        preamble: &'static str,
        default_model: &'static str,
    ) -> Self {
        AgentSpec {
            name,
            version,
            preamble,
            default_model,
            max_turns: 20,
            policy: ToolPolicy::default(),
            sandbox_tools: SandboxTools::All,
            tools: no_tools,
        }
    }

    /// Refuses a spec the VM or the tools could not use. Checked before
    /// anything starts.
    pub fn validate(&self) -> Result<()> {
        if !valid_name(self.name) {
            bail!(
                "agent name `{}` must be a lowercase letter then up to 23 lowercase letters or digits",
                self.name
            );
        }
        if let SandboxTools::Only(names) = self.sandbox_tools
            && let Some(unknown) = names.iter().find(|n| !sandbox::tools::NAMES.contains(n))
        {
            bail!(
                "unknown sandbox tool `{unknown}`; the sandbox tools are {}",
                sandbox::tools::NAMES.join(", ")
            );
        }
        if self.max_turns == 0 {
            bail!("max_turns must be at least 1");
        }
        Ok(())
    }

    /// The model this process will use, after the `AGENT_MODEL` override.
    ///
    /// Recorded on every run so a session's cost can be attributed to the
    /// model that produced it.
    pub fn model(&self) -> String {
        self.model_or_default(std::env::var("AGENT_MODEL").ok())
    }

    fn model_or_default(&self, configured: Option<String>) -> String {
        configured.unwrap_or_else(|| self.default_model.into())
    }

    /// The production agent. `memory` is where Rig loads and saves each
    /// conversation: `service.memory()`. Its store also records each
    /// session's sandbox when the sandbox settings (`OPEN_SANDBOX_URL`) are
    /// present.
    pub fn build(
        &self,
        client: &Client,
        model: &str,
        memory: SqliteMemory,
    ) -> Result<rig::agent::Agent> {
        let sandboxes = sandboxes(sandbox::Config::from_env()?, memory.store());
        Ok(self.configure_with(client.agent(model).memory(memory), sandboxes))
    }

    /// Everything that makes this agent this agent, independent of the
    /// provider.
    ///
    /// Tests apply this to a builder around Rig's mock model, so they run
    /// the production preamble and tools rather than a copy of them.
    pub fn configure(&self, builder: AgentBuilder) -> rig::agent::Agent {
        self.configure_with(builder, None)
    }

    /// [`Self::configure`], plus the selected sandbox and browser tools when
    /// there is a sandbox server to run them on.
    pub fn configure_with(
        &self,
        builder: AgentBuilder,
        sandboxes: Option<Arc<Sandboxes>>,
    ) -> rig::agent::Agent {
        let builder = builder
            .name(self.name)
            // Always: every prompt, system prompt, reply and tool call goes
            // on spans, so the telemetry sinks export all of it.
            .record_content_telemetry(true)
            .preamble(self.preamble)
            .add_hook(self.policy)
            .default_max_turns(self.max_turns)
            // Into the state that takes tools, with none yet, so an agent
            // without tools of its own builds the same way.
            .dynamic_tools(Vec::new());
        let builder = (self.tools)(builder);
        match sandboxes {
            Some(sandboxes) => sandbox::tools::register(builder, sandboxes, self.sandbox_tools),
            None => builder,
        }
        .build()
    }
}

/// A name that is safe as a path component, a systemd instance part and a
/// GHCR repository: no `-`, so a unit's escaped instance `<name>-<env>` has
/// one spelling.
pub fn valid_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars.next().is_some_and(|c| c.is_ascii_lowercase())
        && name.len() <= 24
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
}

fn sandboxes(config: Option<sandbox::Config>, store: &Store) -> Option<Arc<Sandboxes>> {
    config.map(|config| Arc::new(Sandboxes::new(config, store.clone())))
}

/// The OpenRouter client, keyed by `OPENROUTER_API_KEY`. Fails without it.
pub fn client() -> Result<Client> {
    Ok(Client::from_env()?)
}

/// A bare model on the same provider, without the agent around it: what
/// `eval record` records and `eval run --judge` asks.
pub fn provider_model(model: &str) -> Result<CompletionModel> {
    Ok(client()?.completion_model(model))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing;

    #[test]
    fn agent_model_overrides_the_default() {
        let spec = testing::spec();
        assert_eq!(spec.model_or_default(Some("x/y".into())), "x/y");
        assert_eq!(spec.model_or_default(None), spec.default_model);
    }

    #[test]
    fn sandbox_tools_need_a_sandbox_server() {
        let store = Store::open_in_memory().unwrap();
        assert!(sandboxes(None, &store).is_none());
        let config =
            sandbox::Config::parse(Some("http://sandbox:9090".into()), None, None, None, None)
                .unwrap()
                .unwrap();
        assert!(sandboxes(Some(config), &store).is_some());
    }

    #[test]
    fn names_are_safe_on_the_vm() {
        for good in ["a", "athena", "notes2", "abcdefghijklmnopqrstuvwx"] {
            assert!(valid_name(good), "{good}");
        }
        for bad in [
            "",
            "2x",
            "Notes",
            "my-agent",
            "a_b",
            "../x",
            "a.b",
            "abcdefghijklmnopqrstuvwxy",
        ] {
            assert!(!valid_name(bad), "{bad}");
        }
    }

    #[test]
    fn a_spec_is_checked_before_anything_runs() {
        assert!(testing::spec().validate().is_ok());
        let mut bad = testing::spec();
        bad.name = "Bad-Name";
        assert!(
            bad.validate()
                .unwrap_err()
                .to_string()
                .contains("agent name `Bad-Name`")
        );

        let mut bad = testing::spec();
        bad.sandbox_tools = SandboxTools::Only(&["shell", "rm_rf"]);
        let err = bad.validate().unwrap_err().to_string();
        assert!(err.contains("unknown sandbox tool `rm_rf`"), "{err}");

        let mut bad = testing::spec();
        bad.max_turns = 0;
        assert!(
            bad.validate()
                .unwrap_err()
                .to_string()
                .contains("max_turns")
        );
    }

    #[test]
    fn sandbox_tool_selection() {
        assert!(SandboxTools::All.includes("shell"));
        assert!(!SandboxTools::None.includes("shell"));
        let only = SandboxTools::Only(&["read_file"]);
        assert!(only.includes("read_file"));
        assert!(!only.includes("shell"));
    }

    #[test]
    fn the_defaults() {
        let spec = AgentSpec::new("x", "1.2.3", "p", "m/m");
        assert_eq!(spec.max_turns, 20);
        assert_eq!(spec.sandbox_tools, SandboxTools::All);
        assert_eq!(spec.policy, ToolPolicy::default());
        let builder =
            rig::agent::AgentBuilder::new(rig_core::test_utils::MockCompletionModel::new([]))
                .dynamic_tools(Vec::new());
        let _ = (spec.tools)(builder).build();
    }
}
