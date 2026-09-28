//! The agent side of a turn: how one is run, and what it cost.
//!
//! Loading and saving the transcript is not here. Rig does both through the
//! conversation memory the agent was built with (`store::SqliteMemory`);
//! `service::Service::send` and `send_stream` wrap the run with ownership,
//! locking and telemetry.

use crate::media::{File, Outbox};
use crate::store::{RunRecord, now_millis};
use rig_agent::agent::{Agent, PromptResponse, StreamingResult};
use rig_agent::completion::PromptError;
use rig_agent::prelude::{Prompt, StreamingPrompt};
use rig_agent::tool::ToolContext;
use rig_core::message::{Message, UserContent};
use std::future::{Future, IntoFuture};

/// The conversation (session id) a run belongs to. Every tool call in the
/// run finds it in its [`ToolContext`]; the model never sees or sets it.
/// The sandbox tools use it to pick the session's sandbox.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Conversation(pub String);

/// The tool context for one run in `conversation`. With an `outbox`, the
/// run's tools can send the user files through it.
pub fn tool_context(conversation: &str, outbox: Option<&Outbox>) -> ToolContext {
    let mut context = ToolContext::new();
    context.insert(Conversation(conversation.to_string()));
    if let Some(outbox) = outbox {
        context.insert(outbox.clone());
    }
    context
}

/// What one turn is asked: the user's text and the files they sent with
/// it, and where the turn's tools may send files back.
#[derive(Debug, Clone, Default)]
pub struct Request {
    pub text: String,
    pub files: Vec<File>,
    pub outbox: Option<Outbox>,
}

impl From<&str> for Request {
    fn from(text: &str) -> Self {
        Self {
            text: text.to_string(),
            ..Self::default()
        }
    }
}

impl From<String> for Request {
    fn from(text: String) -> Self {
        Self {
            text,
            ..Self::default()
        }
    }
}

impl From<&String> for Request {
    fn from(text: &String) -> Self {
        Self::from(text.as_str())
    }
}

impl Request {
    /// Whether there is nothing to answer: no text and no files.
    pub fn is_empty(&self) -> bool {
        self.text.trim().is_empty() && self.files.is_empty()
    }

    /// The user message the model is given: the text, a note on each file
    /// saying what it is and where the sandbox has it, and each file shown
    /// as an image (see [`crate::media::shown`]), as an image.
    pub fn message(&self) -> Message {
        let mut text = self.text.clone();
        let mut images = Vec::new();
        for file in &self.files {
            let mut note = vec![format!(
                "The user attached `{}` ({}, {} bytes).",
                file.name,
                file.mime.as_deref().unwrap_or("type unknown"),
                file.size,
            )];
            note.push(match &file.saved {
                Ok(path) => format!("It is in your sandbox at {path}."),
                Err(why) => format!("It is not in your sandbox: {why}."),
            });
            match &file.image {
                Some(Ok(image)) => {
                    images.push(UserContent::Image(image.clone()));
                    note.push("It is shown to you below.".into());
                }
                Some(Err(why)) => note.push(format!("It is not shown to you: {why}.")),
                None => {}
            }
            if !text.is_empty() {
                text.push_str("\n\n");
            }
            text.push_str(&format!("[{}]", note.join(" ")));
        }
        let mut content = vec![UserContent::text(text)];
        content.extend(images);
        Message::User { content }
    }
}

/// One agent run in a conversation, returning everything Rig reports rather
/// than just the reply.
///
/// `Chat::chat` is Rig's convenience surface: it hands back the assistant text
/// and drops `usage`, `completion_calls` and `content` on the way out. This
/// template needs those for the `runs` table, so it drives the underlying
/// request directly. The trait exists so the service stays testable without
/// a provider behind it.
///
/// `Send + Sync` and a `Send` future, so a transport can share one agent
/// across tasks and spawn turns on it.
pub trait Run: Send + Sync {
    /// Run `request` in `conversation`, the session id. The agent's memory
    /// loads that conversation's history first and appends the turn after.
    fn run(
        &self,
        request: &Request,
        conversation: &str,
    ) -> impl Future<Output = Result<PromptResponse, PromptError>> + Send;
}

impl Run for Agent {
    async fn run(
        &self,
        request: &Request,
        conversation: &str,
    ) -> Result<PromptResponse, PromptError> {
        self.prompt(request.message())
            .conversation(conversation)
            .tool_context(tool_context(conversation, request.outbox.as_ref()))
            .extended_details()
            .await
    }
}

/// The streaming counterpart of [`Run`]: the same turn, reported as it
/// happens. The stream's last item is Rig's `FinalResponse`, or an error.
///
/// The returned future must not touch the conversation memory until it is
/// first polled: the service starts the turn's memory receipt in between.
/// Rig's own request is lazy that way, and so is any `async fn`.
pub trait RunStream: Send + Sync {
    /// Stream `prompt` in `conversation`. The memory loads the history when
    /// the future is polled and appends the turn before the final item.
    fn stream(
        &self,
        prompt: &str,
        conversation: &str,
    ) -> impl Future<Output = StreamingResult> + Send;
}

impl RunStream for Agent {
    fn stream(
        &self,
        prompt: &str,
        conversation: &str,
    ) -> impl Future<Output = StreamingResult> + Send {
        self.stream_prompt(prompt)
            .conversation(conversation)
            .tool_context(tool_context(conversation, None))
            .into_future()
    }
}

/// Whether to keep each completion call's raw provider response.
///
/// The raw payload carries provider detail Rig does not model (OpenRouter
/// reports upstream routing and cost there), but it is an unredacted copy of
/// the response and it dominates row size. On by default, off via
/// `RUNS_STORE_RAW=0` for deployments where either matters.
fn store_raw() -> bool {
    keep_raw(std::env::var("RUNS_STORE_RAW").ok().as_deref())
}

fn keep_raw(setting: Option<&str>) -> bool {
    !matches!(setting, Some("0" | "false"))
}

/// Serialise the run's completion calls, dropping `raw` unless it is wanted.
fn calls_json(response: &PromptResponse, keep_raw: bool) -> String {
    // Cannot fail: CompletionCall is plain structs, numbers and strings, and
    // `raw` is already a serde_json::Value. Every map key is a string.
    let mut value = serde_json::to_value(&response.completion_calls)
        .expect("CompletionCall always serialises to JSON");
    if !keep_raw && let Some(calls) = value.as_array_mut() {
        for call in calls {
            if let Some(obj) = call.as_object_mut() {
                obj.remove("raw");
            }
        }
    }
    value.to_string()
}

/// The fields of a run record that do not depend on how it ended.
pub(crate) struct RunStart<'a> {
    pub run_id: String,
    pub session_id: &'a str,
    pub model: &'a str,
    pub started_at: i64,
    /// The seq the run's first message would get.
    pub first_seq: i64,
}

/// The telemetry row for one finished run.
///
/// `response` is what the model returned, if it returned; its usage is
/// recorded even when the transcript then failed to save, because those
/// tokens were paid for. `stored` is the last seq written, or why the run
/// wrote nothing.
pub(crate) fn record(
    start: RunStart<'_>,
    response: Option<&PromptResponse>,
    stored: Result<i64, String>,
) -> RunRecord {
    let mut rec = RunRecord {
        run_id: start.run_id,
        session_id: start.session_id.to_string(),
        started_at: start.started_at,
        ended_at: now_millis(),
        model: start.model.to_string(),
        status: "ok".into(),
        error: None,
        first_seq: start.first_seq,
        // A run that appends nothing leaves last_seq below first_seq.
        last_seq: start.first_seq - 1,
        input_tokens: 0,
        output_tokens: 0,
        total_tokens: 0,
        cached_input_tokens: 0,
        cache_creation_input_tokens: 0,
        reasoning_tokens: 0,
        tool_use_prompt_tokens: 0,
        model_calls: 0,
        calls_json: "[]".into(),
    };

    if let Some(response) = response {
        let u = &response.usage;
        rec.input_tokens = u.input_tokens as i64;
        rec.output_tokens = u.output_tokens as i64;
        rec.total_tokens = u.total_tokens as i64;
        rec.cached_input_tokens = u.cached_input_tokens as i64;
        rec.cache_creation_input_tokens = u.cache_creation_input_tokens as i64;
        rec.reasoning_tokens = u.reasoning_tokens as i64;
        rec.tool_use_prompt_tokens = u.tool_use_prompt_tokens as i64;
        rec.model_calls = response.completion_calls.len() as i64;
        rec.calls_json = calls_json(response, store_raw());
    }
    match stored {
        Ok(last_seq) => rec.last_seq = last_seq,
        Err(e) => {
            rec.status = "error".into();
            rec.error = Some(e);
        }
    }
    rec
}

#[cfg(test)]
mod tests {
    use super::*;
    use rig_agent::agent::CompletionCall;
    use rig_agent::prelude::Message;
    use rig_core::completion::Usage;

    fn usage() -> Usage {
        Usage {
            input_tokens: 100,
            output_tokens: 20,
            total_tokens: 120,
            cached_input_tokens: 64,
            cache_creation_input_tokens: 4,
            tool_use_prompt_tokens: 2,
            reasoning_tokens: 8,
        }
    }

    /// A two-model-call run appending four messages — the `add 21 and 21`
    /// shape: prompt, tool call, tool result, reply.
    fn tool_using_response() -> PromptResponse {
        let mut response = PromptResponse::new("42", usage());
        response.messages = Some(vec![
            Message::user("add 21 and 21"),
            Message::assistant("<tool call>"),
            Message::user("<tool result>"),
            Message::assistant("42"),
        ]);
        response.completion_calls = vec![
            CompletionCall::new(0, usage()).with_raw(serde_json::json!({"secret": "payload"})),
            CompletionCall::new(1, usage()).with_raw(serde_json::json!({"secret": "payload"})),
        ];
        response
    }

    fn start(first_seq: i64) -> RunStart<'static> {
        RunStart {
            run_id: "run".into(),
            session_id: "s",
            model: "test/model",
            started_at: 0,
            first_seq,
        }
    }

    #[test]
    fn record_copies_every_usage_field() {
        let rec = record(start(0), Some(&tool_using_response()), Ok(3));
        assert_eq!(rec.status, "ok");
        assert_eq!(rec.input_tokens, 100);
        assert_eq!(rec.output_tokens, 20);
        assert_eq!(rec.total_tokens, 120);
        assert_eq!(rec.cached_input_tokens, 64);
        assert_eq!(rec.cache_creation_input_tokens, 4);
        assert_eq!(rec.tool_use_prompt_tokens, 2);
        assert_eq!(rec.reasoning_tokens, 8);
        assert_eq!(rec.model, "test/model");
        assert_eq!(rec.session_id, "s");
    }

    #[test]
    fn model_calls_are_counted_separately_from_messages() {
        let rec = record(start(10), Some(&tool_using_response()), Ok(13));
        // Two HTTP requests produced four transcript rows. Conflating the two
        // is the mistake the runs table exists to prevent.
        assert_eq!(rec.model_calls, 2);
        assert_eq!((rec.first_seq, rec.last_seq), (10, 13));
    }

    #[test]
    fn a_failed_run_is_recorded_with_its_error_and_no_messages() {
        let rec = record(start(7), None, Err("max turns reached".into()));
        assert_eq!(rec.status, "error");
        assert_eq!(rec.error.as_deref(), Some("max turns reached"));
        assert_eq!(rec.model_calls, 0);
        // Nothing was appended, so the range is empty rather than one row.
        assert!(rec.last_seq < rec.first_seq);
    }

    #[test]
    fn a_reply_whose_transcript_was_lost_still_records_what_it_cost() {
        let rec = record(
            start(4),
            Some(&tool_using_response()),
            Err("transcript not saved".into()),
        );
        assert_eq!(rec.status, "error");
        assert_eq!((rec.input_tokens, rec.model_calls), (100, 2));
        assert_eq!((rec.first_seq, rec.last_seq), (4, 3));
    }

    use crate::media;

    const PNG: &[u8] = b"\x89PNG\r\n\x1a\nrest";

    fn file(name: &str, mime: Option<&str>, bytes: &[u8], saved: Result<&str, &str>) -> File {
        File {
            name: name.into(),
            mime: mime.map(str::to_string),
            size: bytes.len() as u64,
            image: media::shown(bytes, mime),
            saved: saved.map(str::to_string).map_err(str::to_string),
        }
    }

    #[test]
    fn a_request_is_text_or_files() {
        assert!(Request::from("  ").is_empty());
        assert!(!Request::from(String::from("hi")).is_empty());
        assert_eq!(Request::from(&String::from("hi")).text, "hi");
        let files = Request {
            files: vec![file("a.txt", None, b"a", Ok("/x/a.txt"))],
            ..Request::default()
        };
        assert!(!files.is_empty());
    }

    #[test]
    fn a_text_request_is_a_plain_user_message() {
        assert_eq!(Request::from("hello").message(), Message::user("hello"));
    }

    #[test]
    fn files_are_described_and_images_are_shown() {
        let request = Request {
            text: "what is this?".into(),
            files: vec![
                file("shot.png", Some("image/png"), PNG, Ok("/tmp/in/shot.png")),
                file(
                    "notes.pdf",
                    Some("application/pdf"),
                    b"%PDF",
                    Err("no sandbox"),
                ),
                file(
                    "bad.jpg",
                    Some("image/jpeg"),
                    b"not really",
                    Ok("/tmp/in/bad.jpg"),
                ),
                file("blob", None, b"??", Ok("/tmp/in/blob")),
            ],
            outbox: None,
        };
        let text = "what is this?\n\n\
            [The user attached `shot.png` (image/png, 12 bytes). It is in your sandbox at \
            /tmp/in/shot.png. It is shown to you below.]\n\n\
            [The user attached `notes.pdf` (application/pdf, 4 bytes). It is not in your \
            sandbox: no sandbox.]\n\n\
            [The user attached `bad.jpg` (image/jpeg, 10 bytes). It is in your sandbox at \
            /tmp/in/bad.jpg. It is not shown to you: not a PNG, JPEG, GIF or WebP image.]\n\n\
            [The user attached `blob` (type unknown, 2 bytes). It is in your sandbox at \
            /tmp/in/blob.]";
        let expected = Message::User {
            content: vec![
                UserContent::text(text),
                UserContent::Image(media::image(PNG).unwrap()),
            ],
        };
        assert_eq!(request.message(), expected);
    }

    #[test]
    fn a_caption_less_file_is_only_its_note() {
        let request = Request {
            files: vec![file("a.txt", Some("text/plain"), b"a", Ok("/x/a.txt"))],
            ..Request::default()
        };
        assert_eq!(
            request.message(),
            Message::user(
                "[The user attached `a.txt` (text/plain, 1 bytes). It is in your sandbox at /x/a.txt.]"
            )
        );
    }

    #[test]
    fn the_tool_context_carries_the_outbox_only_when_there_is_one() {
        let without = tool_context("s", None);
        assert_eq!(
            without.get::<Conversation>(),
            Some(&Conversation("s".into()))
        );
        assert!(without.get::<Outbox>().is_none());
        let outbox = Outbox::default();
        assert!(tool_context("s", Some(&outbox)).get::<Outbox>().is_some());
    }

    #[test]
    fn runs_store_raw_is_on_unless_set_to_0_or_false() {
        assert!(keep_raw(None));
        assert!(keep_raw(Some("1")));
        assert!(keep_raw(Some("true")));
        assert!(!keep_raw(Some("0")));
        assert!(!keep_raw(Some("false")));
    }

    #[test]
    fn raw_payloads_are_kept_when_enabled_and_dropped_when_not() {
        let response = tool_using_response();

        let kept = calls_json(&response, true);
        assert!(kept.contains("secret"));

        let dropped = calls_json(&response, false);
        assert!(!dropped.contains("secret"));
        // Dropping raw must not drop the calls themselves or their usage.
        let parsed: serde_json::Value = serde_json::from_str(&dropped).unwrap();
        assert_eq!(parsed.as_array().unwrap().len(), 2);
        assert_eq!(parsed[0]["usage"]["input_tokens"], 100);
    }
}
