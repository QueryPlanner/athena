//! Limits on what one run may ask its tools to do.
//!
//! [`ToolPolicy`] is a Rig hook that sees every tool call before it runs. A
//! call over a limit is not run: the model gets the reason as the tool's
//! result instead and can change course. Output size is limited by the
//! tools themselves (`sandbox::stream::MAX_OUTPUT_BYTES`).
//!
//! There is no limit on how many model turns or tool calls one run makes:
//! a task takes as many steps as it needs.

use rig_agent::agent::{AgentHook, HookContext, ToolCall, ToolCallAction};

/// The largest arguments one tool call may carry, in bytes of JSON. Room
/// for `write_file` with a sizeable source file.
pub const MAX_ARGUMENT_BYTES: usize = 128 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ToolPolicy {
    pub max_argument_bytes: usize,
}

impl Default for ToolPolicy {
    fn default() -> Self {
        Self {
            max_argument_bytes: MAX_ARGUMENT_BYTES,
        }
    }
}

impl ToolPolicy {
    /// Whether a call with arguments of `argument_bytes` may run.
    pub fn decide(&self, argument_bytes: usize) -> ToolCallAction {
        if argument_bytes > self.max_argument_bytes {
            return ToolCallAction::skip(format!(
                "Not run: the arguments are {argument_bytes} bytes, over the limit of {}. \
                 Split the work into smaller calls.",
                self.max_argument_bytes
            ));
        }
        ToolCallAction::run()
    }
}

impl AgentHook for ToolPolicy {
    async fn on_tool_call(&self, _: &HookContext, event: ToolCall<'_>) -> ToolCallAction {
        self.decide(event.args.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn calls_within_the_limit_run() {
        let policy = ToolPolicy {
            max_argument_bytes: 10,
        };
        assert_eq!(policy.decide(0), ToolCallAction::Run);
        assert_eq!(policy.decide(10), ToolCallAction::Run);
    }

    #[test]
    fn oversized_arguments_are_skipped_with_the_reason() {
        let policy = ToolPolicy::default();
        let action = policy.decide(MAX_ARGUMENT_BYTES + 1);
        let size = (MAX_ARGUMENT_BYTES + 1).to_string();
        assert!(matches!(&action, ToolCallAction::Skip(why) if why.contains(&size)));
    }
}
