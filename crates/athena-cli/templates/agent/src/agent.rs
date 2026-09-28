//! The file you edit: this agent's instructions, its own tools, and which of
//! the runtime's tools and limits it uses. Everything else is `athena_core`.

use athena_core::{AgentSpec, SandboxTools};
use rig_agent as rig;
use rig_agent::rig_tool;

// ---------------- tools ----------------
//
// A tool is a function with `#[rig_tool]`. Its doc and argument names are
// what the model sees. Return `Err(ToolExecutionError::...)` to tell the
// model what went wrong; it can try again.

#[rig_tool(description = "Add two numbers")]
fn add(a: f64, b: f64) -> Result<f64, rig::tool::ToolExecutionError> {
    Ok(a + b)
}

// ---------------- definition ----------------

pub const NAME: &str = "@NAME@";
pub const DEFAULT_MODEL: &str = "openai/gpt-5.6-luna";

pub fn spec() -> AgentSpec {
    AgentSpec {
        // The runtime's shell, code, file and browser tools, which run in a
        // sandbox when OPEN_SANDBOX_URL is set. `SandboxTools::Only(&["shell"])`
        // picks some; `SandboxTools::None` turns them off.
        sandbox_tools: SandboxTools::All,
        // This agent's own tools.
        tools: |b| b.tool(Add),
        ..AgentSpec::new(
            NAME,
            env!("CARGO_PKG_VERSION"),
            include_str!("../prompts/system.md"),
            DEFAULT_MODEL,
        )
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
        spec().validate().unwrap();
    }
}
