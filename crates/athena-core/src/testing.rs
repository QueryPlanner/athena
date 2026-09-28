//! For tests of this crate and of agents built on it (feature `testing`).

use crate::spec::AgentSpec;
use rig_agent as rig;
use rig_agent::rig_tool;

#[rig_tool(description = "Add two numbers")]
fn add(a: f64, b: f64) -> Result<f64, rig::tool::ToolExecutionError> {
    Ok(a + b)
}

/// The preamble of [`spec`], for tests that look for it in a request.
pub const PREAMBLE: &str = "You are a helpful assistant.";

/// An agent like the starter one: named `athena`, one `add` tool.
pub fn spec() -> AgentSpec {
    AgentSpec {
        tools: |b| b.tool(Add),
        ..AgentSpec::new(
            "athena",
            env!("CARGO_PKG_VERSION"),
            PREAMBLE,
            "openai/gpt-5.6-luna",
        )
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn add_adds() {
        assert_eq!(super::add(21.0, 21.0).unwrap(), 42.0);
    }
}
