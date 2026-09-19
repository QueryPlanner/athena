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

pub fn build() -> Result<impl Chat> {
    let model = std::env::var("AGENT_MODEL").unwrap_or_else(|_| DEFAULT_MODEL.into());
    Ok(rig::core::providers::openrouter::Client::from_env()?
        .agent(&model)
        .preamble(PREAMBLE)
        .tool(Add)
        .tool(ReadFile)
        .default_max_turns(20)
        .build())
}
