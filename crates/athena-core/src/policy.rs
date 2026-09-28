//! Limits on what one run may ask its tools to do.
//!
//! [`ToolPolicy`] is a Rig hook that sees every tool call before it runs. A
//! call over a limit is not run: the model gets the reason as the tool's
//! result instead and can change course. Output size is limited by the
//! tools themselves (`sandbox::stream::MAX_OUTPUT_BYTES`).

use rig_agent::agent::{AgentHook, HookContext, ToolCall, ToolCallAction};

/// Tool calls one run may make. The agent allows 20 model turns, and a
/// turn can ask for several tools at once.
pub const MAX_TOOL_CALLS: usize = 40;
/// The largest arguments one tool call may carry, in bytes of JSON. Room
/// for `write_file` with a sizeable source file.
pub const MAX_ARGUMENT_BYTES: usize = 128 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ToolPolicy {
    pub max_calls: usize,
    pub max_argument_bytes: usize,
}

impl Default for ToolPolicy {
    fn default() -> Self {
        Self {
            max_calls: MAX_TOOL_CALLS,
            max_argument_bytes: MAX_ARGUMENT_BYTES,
        }
    }
}

/// Calls this run has asked for so far, kept in the run's scratchpad.
#[derive(Debug, Clone, Default)]
struct CallsSoFar(usize);

impl ToolPolicy {
    /// Whether the `nth` call of a run (counting from 1), with arguments of
    /// `argument_bytes`, may run.
    pub fn decide(&self, nth: usize, argument_bytes: usize) -> ToolCallAction {
        if nth > self.max_calls {
            return ToolCallAction::skip(format!(
                "Not run: this turn has used its {} tool calls. Answer with what you have.",
                self.max_calls
            ));
        }
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
    async fn on_tool_call(&self, ctx: &HookContext, event: ToolCall<'_>) -> ToolCallAction {
        // Skipped calls count too: a model that keeps asking is still asking.
        let nth = ctx.scratchpad().update::<CallsSoFar, _>(|calls| {
            calls.0 += 1;
            calls.0
        });
        self.decide(nth, event.args.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn calls_within_both_limits_run() {
        let policy = ToolPolicy {
            max_calls: 2,
            max_argument_bytes: 10,
        };
        assert_eq!(policy.decide(1, 0), ToolCallAction::Run);
        assert_eq!(policy.decide(2, 10), ToolCallAction::Run);
    }

    #[test]
    fn a_call_over_the_budget_is_skipped_with_the_reason() {
        let policy = ToolPolicy {
            max_calls: 2,
            max_argument_bytes: 10,
        };
        let action = policy.decide(3, 1);
        assert!(matches!(&action, ToolCallAction::Skip(why) if why.contains("2 tool calls")));
    }

    #[test]
    fn oversized_arguments_are_skipped_with_the_reason() {
        let policy = ToolPolicy::default();
        let action = policy.decide(1, MAX_ARGUMENT_BYTES + 1);
        let size = (MAX_ARGUMENT_BYTES + 1).to_string();
        assert!(matches!(&action, ToolCallAction::Skip(why) if why.contains(&size)));
    }
}
