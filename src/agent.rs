//! The only file you edit when making a new agent: its instructions, its
//! own tools, and which of the core's tools and limits it uses. Everything
//! else is `athena_core`.

use athena_core::{AgentSpec, SandboxTools};
use rig_agent as rig;
use rig_agent::rig_tool;

// ---------------- tools ----------------

#[rig_tool(description = "Add two numbers")]
fn add(a: f64, b: f64) -> Result<f64, rig::tool::ToolExecutionError> {
    Ok(a + b)
}

// The sandbox and browser tools (`SandboxTools`) run in a sandbox, never on
// this host, and exist only when OPEN_SANDBOX_URL is set.

// ---------------- definition ----------------

pub const NAME: &str = "athena";
pub const PREAMBLE: &str = "You are a helpful assistant.";
pub const DEFAULT_MODEL: &str = "openai/gpt-5.6-luna";

pub fn spec() -> AgentSpec {
    AgentSpec {
        sandbox_tools: SandboxTools::All,
        tools: |b| b.tool(Add),
        ..AgentSpec::new(NAME, env!("CARGO_PKG_VERSION"), PREAMBLE, DEFAULT_MODEL)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_adds() {
        assert_eq!(add(21.0, 21.0).unwrap(), 42.0);
    }

    #[test]
    fn the_spec_is_valid() {
        let spec = spec();
        spec.validate().unwrap();
        assert_eq!(spec.name, NAME);
        assert_eq!(spec.default_model, DEFAULT_MODEL);
    }
}
