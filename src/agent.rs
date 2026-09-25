//! The only file you edit when making a new agent.

use crate::policy::ToolPolicy;
use crate::sandbox::{self, Sandboxes};
use crate::store::{SqliteMemory, Store};
use anyhow::Result;
use rig_agent as rig;
use rig_agent::prelude::*;
use rig_agent::rig_tool;
use std::sync::Arc;

// ---------------- tools ----------------

#[rig_tool(description = "Add two numbers")]
fn add(a: f64, b: f64) -> Result<f64, rig::tool::ToolExecutionError> {
    Ok(a + b)
}

// Everything else runs in a sandbox, never on this host: see
// `sandbox::tools`. Those tools exist only when OPEN_SANDBOX_URL is set.

// ---------------- definition ----------------

/// Reported as `gen_ai.agent.name` on every turn's span.
pub const NAME: &str = "athena";
pub const PREAMBLE: &str = "You are a helpful assistant.";
pub const DEFAULT_MODEL: &str = "openai/gpt-5.6-luna";

pub type Client = rig::core::providers::openrouter::Client;

/// The model this process will use, after the `AGENT_MODEL` override.
///
/// Recorded on every run so a session's cost can be attributed to the model
/// that produced it.
pub fn model() -> String {
    model_or_default(std::env::var("AGENT_MODEL").ok())
}

fn model_or_default(configured: Option<String>) -> String {
    configured.unwrap_or_else(|| DEFAULT_MODEL.into())
}

/// The OpenRouter client, keyed by `OPENROUTER_API_KEY`. Fails without it.
pub fn client() -> Result<Client> {
    Ok(Client::from_env()?)
}

/// A bare model on the same provider, without the agent around it: what
/// `athena eval record` records and `athena eval run --judge` asks.
pub fn provider_model(model: &str) -> Result<rig::core::providers::openrouter::CompletionModel> {
    Ok(client()?.completion_model(model))
}

/// The production agent. `memory` is where Rig loads and saves each
/// conversation: `service.memory()`. Its store also records each session's
/// sandbox when the sandbox settings (`OPEN_SANDBOX_URL`) are present.
pub fn build(client: &Client, model: &str, memory: SqliteMemory) -> Result<rig::agent::Agent> {
    let sandboxes = sandboxes(sandbox::Config::from_env()?, memory.store());
    Ok(configure_with(
        client.agent(model).memory(memory),
        sandboxes,
    ))
}

fn sandboxes(config: Option<sandbox::Config>, store: &Store) -> Option<Arc<Sandboxes>> {
    config.map(|config| Arc::new(Sandboxes::new(config, store.clone())))
}

/// Everything that makes this agent this agent, independent of the provider.
///
/// Tests apply this to a builder around Rig's mock model, so they run the
/// production preamble and tools rather than a copy of them.
pub fn configure(builder: rig::agent::AgentBuilder) -> rig::agent::Agent {
    configure_with(builder, None)
}

/// [`configure`], plus the sandbox and browser tools when there is a sandbox
/// server to run them on.
pub fn configure_with(
    builder: rig::agent::AgentBuilder,
    sandboxes: Option<Arc<Sandboxes>>,
) -> rig::agent::Agent {
    let builder = builder
        .name(NAME)
        // Prompt and reply text on spans, only with ATHENA_RECORD_CONTENT=1.
        .record_content_telemetry(crate::telemetry::record_content())
        .preamble(PREAMBLE)
        .tool(Add)
        .add_hook(ToolPolicy::default())
        .default_max_turns(20);
    match sandboxes {
        Some(sandboxes) => sandbox::tools::register(builder, sandboxes).build(),
        None => builder.build(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_model_overrides_the_default() {
        assert_eq!(model_or_default(Some("x/y".into())), "x/y");
        assert_eq!(model_or_default(None), DEFAULT_MODEL);
    }

    #[test]
    fn add_adds() {
        assert_eq!(add(21.0, 21.0).unwrap(), 42.0);
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
}
