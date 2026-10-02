//! Limits on what one run may ask its tools to do.
//!
//! [`ToolPolicy`] is a Rig hook that sees every tool call before it runs. A
//! call over a limit is not run: the model gets the reason as the tool's
//! result instead and can change course. Output size is limited by the
//! sandbox tools themselves (`sandbox::stream::MAX_OUTPUT_BYTES`); the tools
//! of MCP servers (`mcp.rs`) are not ours, so the policy cuts their results
//! down to [`MAX_RESULT_BYTES`] in all, and leaves out images the model
//! cannot be shown (`media::MAX_IMAGE_BYTES`) or has no room for.
//!
//! There is no limit on how many model turns or tool calls one run makes:
//! a task takes as many steps as it needs.

use crate::media::{MAX_IMAGE_BYTES, MAX_IMAGES_PER_REQUEST};
use crate::sandbox::stream::split_at_boundary;
use rig_agent::agent::{
    AgentHook, HookContext, ToolCall, ToolCallAction, ToolResultAction, ToolResultEvent,
};
use rig_core::message::{DocumentSourceKind, ToolResultContent};
use rig_core::tool::ToolOutput;
use std::collections::HashSet;

/// The largest arguments one tool call may carry, in bytes of JSON. Room
/// for `write_file` with a sizeable source file.
pub const MAX_ARGUMENT_BYTES: usize = 128 * 1024;

/// The most a limited tool's result may carry, in bytes of text: what
/// `read_file` returns, about 16 000 tokens. A block with no text, and an
/// image, count as one byte each, so a result cannot grow by having many
/// blocks. An image over `media::MAX_IMAGE_BYTES` is left out.
pub const MAX_RESULT_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolPolicy {
    pub max_argument_bytes: usize,
    pub max_result_bytes: usize,
    /// The tools whose results are cut to `max_result_bytes`.
    limited_results: HashSet<String>,
}

impl Default for ToolPolicy {
    fn default() -> Self {
        Self {
            max_argument_bytes: MAX_ARGUMENT_BYTES,
            max_result_bytes: MAX_RESULT_BYTES,
            limited_results: HashSet::new(),
        }
    }
}

impl ToolPolicy {
    /// Also cut the results of the tools named in `names` to
    /// `max_result_bytes`: the tools this process does not implement.
    pub fn limiting_results_of(mut self, names: HashSet<String>) -> Self {
        self.limited_results = names;
        self
    }

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

    /// What the model sees of `tool`'s `output`: all of it, or the first
    /// blocks that fit in `max_result_bytes`.
    ///
    /// Every block costs its text in bytes (a JSON block, its serialised
    /// form), an image and an empty block one byte, so the number of blocks
    /// is bounded too. Blocks are kept in order until the budget is spent;
    /// the block that crosses it keeps its first bytes, and the rest are
    /// left out. At most [`MAX_IMAGES_PER_REQUEST`] images are kept, and none
    /// over `MAX_IMAGE_BYTES`. A result that lost anything ends with one
    /// notice block saying what, whatever the number of blocks left out. The
    /// notice is a fixed few dozen bytes and is not part of the budget.
    pub fn limit_result(&self, tool: &str, output: &ToolOutput) -> ToolResultAction {
        if !self.limited_results.contains(tool) {
            return ToolResultAction::Keep;
        }
        let mut room = self.max_result_bytes;
        let mut images = 0;
        let (mut text_cut, mut images_cut) = (false, false);
        let mut kept = Vec::new();
        for block in output.as_content() {
            if room == 0 {
                text_cut = true;
                break;
            }
            let text = match block {
                ToolResultContent::Text(text) => text.text.clone(),
                ToolResultContent::Json { value } => value.to_string(),
                ToolResultContent::Image(image) => {
                    room -= 1;
                    if images == MAX_IMAGES_PER_REQUEST || too_big(image) {
                        images_cut = true;
                    } else {
                        images += 1;
                        kept.push(block.clone());
                    }
                    continue;
                }
            };
            let cost = text.len().max(1);
            if cost <= room {
                room -= cost;
                kept.push(block.clone());
            } else {
                text_cut = true;
                let (head, _) = split_at_boundary(&text, room);
                if !head.is_empty() {
                    kept.push(ToolResultContent::text(head));
                }
                break;
            }
        }
        let mut notes = Vec::new();
        if text_cut {
            notes.push(format!(
                "[cut: the result is over {} bytes]",
                self.max_result_bytes
            ));
        }
        if images_cut {
            notes.push(format!(
                "[image left out: more than {MAX_IMAGES_PER_REQUEST} to a result, or over \
                 the {MAX_IMAGE_BYTES} bytes the model can be shown]"
            ));
        }
        if notes.is_empty() {
            return ToolResultAction::Keep;
        }
        kept.push(ToolResultContent::text(notes.join("\n")));
        // `kept` ends with the notice, so it is not empty.
        ToolOutput::content(kept)
            .ok()
            .map_or(ToolResultAction::Keep, ToolResultAction::rewrite_output)
    }
}

/// Whether `image`, as base64, holds more than `MAX_IMAGE_BYTES` bytes.
fn too_big(image: &rig_core::message::Image) -> bool {
    matches!(&image.data, DocumentSourceKind::Base64(data)
        if data.len() > MAX_IMAGE_BYTES.div_ceil(3) * 4)
}

impl AgentHook for ToolPolicy {
    async fn on_tool_call(&self, _: &HookContext, event: ToolCall<'_>) -> ToolCallAction {
        self.decide(event.args.len())
    }

    async fn on_tool_result(
        &self,
        _: &HookContext,
        event: ToolResultEvent<'_>,
    ) -> ToolResultAction {
        self.limit_result(event.tool_name, event.presentation)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn calls_within_the_limit_run() {
        let policy = ToolPolicy {
            max_argument_bytes: 10,
            ..ToolPolicy::default()
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

    fn limited(max: usize, names: &[&str]) -> ToolPolicy {
        ToolPolicy {
            max_result_bytes: max,
            ..ToolPolicy::default()
        }
        .limiting_results_of(names.iter().map(|n| n.to_string()).collect())
    }

    fn rewritten(action: ToolResultAction) -> Option<ToolOutput> {
        match action {
            ToolResultAction::Rewrite(output) => Some(output),
            _ => None,
        }
    }

    fn texts(action: ToolResultAction) -> Vec<String> {
        let output = rewritten(action).unwrap();
        let text = |block: &ToolResultContent| block.as_text().unwrap_or("<not text>").to_string();
        output.as_content().iter().map(text).collect()
    }

    #[test]
    fn a_result_within_the_limit_or_of_another_tool_is_kept() {
        let policy = limited(10, &["mcp_tool"]);
        assert_eq!(
            policy.limit_result("mcp_tool", &ToolOutput::text("0123456789")),
            ToolResultAction::Keep
        );
        // Not in the list: whatever its size, the tool limits itself.
        assert_eq!(
            policy.limit_result("shell", &ToolOutput::text("x".repeat(100))),
            ToolResultAction::Keep
        );
        // The default policy limits nobody.
        assert_eq!(
            ToolPolicy::default().limit_result(
                "mcp_tool",
                &ToolOutput::text("x".repeat(MAX_RESULT_BYTES + 1))
            ),
            ToolResultAction::Keep
        );
    }

    #[test]
    fn a_long_text_result_is_cut_on_a_character_boundary_with_a_note() {
        // Each of these is two bytes: a cut at 5 falls inside the third.
        let policy = limited(5, &["t"]);
        let kept = texts(policy.limit_result("t", &ToolOutput::text("\u{e9}".repeat(10))));
        assert_eq!(kept, ["\u{e9}\u{e9}", "[cut: the result is over 5 bytes]"]);
    }

    #[test]
    fn the_limit_is_shared_by_the_blocks_of_one_result() {
        let policy = limited(8, &["t"]);
        let output = ToolOutput::content(vec![
            ToolResultContent::text("12345"),
            ToolResultContent::json(serde_json::json!({"k": "long value"})),
            ToolResultContent::text("never seen"),
        ])
        .unwrap();
        let kept = texts(policy.limit_result("t", &output));
        // Five bytes of room used, three left for the JSON, none for the rest,
        // and one notice.
        assert_eq!(kept, ["12345", "{\"k", "[cut: the result is over 8 bytes]"]);
    }

    #[test]
    fn an_image_too_big_to_show_is_left_out_with_a_note() {
        let policy = limited(1000, &["t"]);
        let big = "A".repeat(MAX_IMAGE_BYTES.div_ceil(3) * 4 + 1);
        let output = ToolOutput::content(vec![
            ToolResultContent::text("caption"),
            ToolResultContent::image_base64(big, None, None),
        ])
        .unwrap();
        let kept = texts(policy.limit_result("t", &output));
        assert_eq!(kept[0], "caption");
        assert_eq!(kept.len(), 2, "the image is replaced by the note");
        assert!(kept[1].starts_with("[image left out: "), "{kept:?}");

        // One that is exactly the limit stays.
        let edge = "A".repeat(MAX_IMAGE_BYTES.div_ceil(3) * 4);
        let output = ToolOutput::one(ToolResultContent::image_base64(edge, None, None));
        assert_eq!(rewritten(policy.limit_result("t", &output)), None);
    }

    /// What the model is sent of a result: bytes of text, and blocks.
    fn size(output: &ToolOutput) -> (usize, usize) {
        let blocks = output.as_content();
        let text = |b: &ToolResultContent| b.as_text().map_or(0, str::len);
        (blocks.iter().map(text).sum(), blocks.len())
    }

    #[test]
    fn a_flood_of_tiny_blocks_is_cut_once_the_budget_is_spent() {
        let max = 64 * 1024;
        let policy = limited(max, &["t"]);
        let flood = vec![ToolResultContent::text("x"); 100_000];
        let output = policy.limit_result("t", &ToolOutput::content(flood).unwrap());
        let cut = rewritten(output).unwrap();

        let (bytes, blocks) = size(&cut);
        let kept = cut.as_content();
        // The budget's worth of blocks, then exactly one notice.
        assert_eq!(blocks, max + 1);
        assert_eq!(kept[max - 1].as_text(), Some("x"));
        let notice = kept[max].as_text().unwrap();
        assert_eq!(notice, "[cut: the result is over 65536 bytes]");
        // The notice is not part of the budget; it is a fixed few dozen bytes.
        assert_eq!(bytes, max + notice.len());
    }

    #[test]
    fn empty_blocks_are_not_free() {
        let policy = limited(10, &["t"]);
        let blocks = vec![ToolResultContent::text(""); 1000];
        let cut = rewritten(policy.limit_result("t", &ToolOutput::content(blocks).unwrap()));
        assert_eq!(size(&cut.unwrap()).1, 11);
    }

    #[test]
    fn images_count_toward_the_budget_and_are_limited_in_number() {
        let image = ToolResultContent::image_base64("A".repeat(100), None, None);

        // Past the cap on images, the rest are left out, with one note.
        let policy = limited(1000, &["t"]);
        let many = ToolOutput::content(vec![image.clone(); 50]).unwrap();
        let cut = rewritten(policy.limit_result("t", &many)).unwrap();
        let kept = cut.as_content();
        assert_eq!(kept.len(), crate::media::MAX_IMAGES_PER_REQUEST + 1);
        assert!(kept[..4].iter().all(|b| *b == image));
        let note = kept[4].as_text().unwrap();
        assert!(note.starts_with("[image left out: "), "{note}");

        // A flood of images with a small budget stops with the budget.
        let policy = limited(3, &["t"]);
        let mixed = vec![image.clone(); 100_000];
        let cut = rewritten(policy.limit_result("t", &ToolOutput::content(mixed).unwrap()));
        let cut = cut.unwrap();
        assert_eq!(cut.as_content().len(), 3 + 1);
        assert!(cut.as_content()[3].as_text().unwrap().starts_with("[cut: "));

        // Text and images share the budget, and both notices can appear.
        let policy = limited(6, &["t"]);
        let mut blocks = vec![ToolResultContent::text("abcd"), image.clone()];
        blocks.extend(vec![image.clone(); 10]);
        blocks.push(ToolResultContent::text("tail"));
        let cut = rewritten(policy.limit_result("t", &ToolOutput::content(blocks).unwrap()));
        let cut = cut.unwrap();
        let kept = cut.as_content();
        // "abcd" (4), an image (1), another (1): the budget is spent.
        assert_eq!(kept.len(), 4);
        assert_eq!(kept[2], image);
        assert_eq!(kept[3].as_text(), Some("[cut: the result is over 6 bytes]"));
    }

    #[test]
    fn json_and_images_within_the_limit_stay_as_they_are() {
        let policy = limited(20, &["t"]);
        let image = ToolResultContent::image_base64("A".repeat(1000), None, None);
        let json = ToolResultContent::json(serde_json::json!({"a": 1}));
        let output = ToolOutput::content(vec![json.clone(), image.clone()]).unwrap();
        assert_eq!(rewritten(policy.limit_result("t", &output)), None);

        // Cut text after an image leaves the image as it was; the 50 bytes
        // get the 19 that are left (the image took one).
        let output =
            ToolOutput::content(vec![image.clone(), ToolResultContent::text("x".repeat(50))])
                .unwrap();
        let cut = rewritten(policy.limit_result("t", &output)).unwrap();
        assert_eq!(cut.as_content()[0], image);
        assert_eq!(cut.as_content()[1].as_text(), Some("x".repeat(19).as_str()));
        assert_eq!(cut.as_content().len(), 3);
    }
}
