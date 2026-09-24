//! The only file you edit when making a new agent.

use anyhow::Result;
use rig_agent as rig;
use rig_agent::prelude::*;
use rig_agent::rig_tool;

// ---------------- tools ----------------

#[rig_tool(description = "Add two numbers")]
fn add(a: f64, b: f64) -> Result<f64, rig::tool::ToolExecutionError> {
    Ok(a + b)
}

#[rig_tool(description = "Read a file from disk")]
fn read_file(path: String) -> Result<String, rig::tool::ToolExecutionError> {
    std::fs::read_to_string(&path).map_err(|e| rig::tool::ToolExecutionError::other(e.to_string()))
}

// ---------------- definition ----------------

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

pub fn build(client: &Client, model: &str) -> rig::agent::Agent {
    configure(client.agent(model))
}

/// Everything that makes this agent this agent, independent of the provider.
///
/// Tests apply this to a builder around Rig's mock model, so they run the
/// production preamble and tools rather than a copy of them.
pub fn configure(builder: rig::agent::AgentBuilder) -> rig::agent::Agent {
    builder
        .preamble(PREAMBLE)
        .tool(Add)
        .tool(ReadFile)
        .default_max_turns(20)
        .build()
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
    fn read_file_returns_contents_or_a_tool_error() {
        let path = std::env::temp_dir().join(format!("athena-{}.txt", uuid::Uuid::new_v4()));
        std::fs::write(&path, "hello").unwrap();
        assert_eq!(read_file(path.display().to_string()).unwrap(), "hello");
        std::fs::remove_file(&path).unwrap();

        // A missing file is reported to the model, not raised as a panic.
        assert!(read_file(path.display().to_string()).is_err());
    }

    #[test]
    fn build_constructs_an_openrouter_agent_without_network() {
        // Construction only: Agent's fields are private, and anything past
        // this needs a real key. Behaviour is covered through `configure`
        // against the mock model in tests/agent_loop.rs.
        let client = Client::new("test-key").unwrap();
        let _agent = build(&client, DEFAULT_MODEL);
    }
}
