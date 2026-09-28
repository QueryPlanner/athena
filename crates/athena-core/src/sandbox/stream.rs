//! execd's output stream, and what the model is shown of it.
//!
//! The spec calls the stream SSE, but the execd image OpenSandbox 0.2.3
//! serves sends bare JSON objects separated by blank lines, with no `data:`
//! prefix. The parser accepts both: it strips an SSE `data:` prefix, skips
//! the other SSE fields, and reads each frame as a sequence of JSON values,
//! so objects separated by a single newline parse too.

use super::Error;
use serde::Deserialize;

/// The most output one tool call hands the model, in bytes. The rest is
/// counted and reported as omitted.
pub const MAX_OUTPUT_BYTES: usize = 16 * 1024;

/// One execd event. Unknown `type`s are kept and ignored.
#[derive(Debug, Deserialize, PartialEq)]
pub struct Event {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub text: Option<String>,
    /// MIME type to value, for `result` events from a code context.
    #[serde(default)]
    pub results: Option<serde_json::Map<String, serde_json::Value>>,
    #[serde(default)]
    pub error: Option<ExecError>,
}

#[derive(Debug, Deserialize, PartialEq)]
pub struct ExecError {
    #[serde(default)]
    pub ename: String,
    #[serde(default)]
    pub evalue: String,
    #[serde(default)]
    pub traceback: Vec<String>,
}

/// Turns the response body, in whatever chunks it arrives, into events.
#[derive(Default)]
pub struct Parser {
    /// Bytes after the last newline seen.
    line: Vec<u8>,
    /// The frame being collected: every line since the last blank one.
    frame: Vec<u8>,
}

impl Parser {
    /// Feed the next chunk; returns the events it completed.
    pub fn push(&mut self, chunk: &[u8]) -> Result<Vec<Event>, Error> {
        let mut events = Vec::new();
        for &byte in chunk {
            if byte == b'\n' {
                let line = std::mem::take(&mut self.line);
                self.line_done(&line, &mut events)?;
            } else {
                self.line.push(byte);
            }
        }
        Ok(events)
    }

    /// The body ended: whatever is left is the last frame.
    pub fn finish(mut self) -> Result<Vec<Event>, Error> {
        let mut events = Vec::new();
        let line = std::mem::take(&mut self.line);
        self.line_done(&line, &mut events)?;
        self.flush(&mut events)?;
        Ok(events)
    }

    fn line_done(&mut self, line: &[u8], events: &mut Vec<Event>) -> Result<(), Error> {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if line.iter().all(u8::is_ascii_whitespace) {
            return self.flush(events);
        }
        if let Some(data) = line.strip_prefix(b"data:") {
            self.frame.extend_from_slice(data);
        } else if is_sse_field(line) {
            return Ok(());
        } else {
            self.frame.extend_from_slice(line);
        }
        self.frame.push(b'\n');
        Ok(())
    }

    fn flush(&mut self, events: &mut Vec<Event>) -> Result<(), Error> {
        let frame = std::mem::take(&mut self.frame);
        for event in serde_json::Deserializer::from_slice(&frame).into_iter::<Event>() {
            events.push(event.map_err(|e| {
                Error::Protocol(format!(
                    "unreadable execd event ({e}): {}",
                    preview(&String::from_utf8_lossy(&frame))
                ))
            })?);
        }
        Ok(())
    }
}

/// An SSE comment (`: ...`) or a field other than `data`.
fn is_sse_field(line: &[u8]) -> bool {
    line.starts_with(b":")
        || [&b"event:"[..], b"id:", b"retry:"]
            .iter()
            .any(|field| line.starts_with(field))
}

/// At most 200 bytes of `text`, for error messages.
pub(crate) fn preview(text: &str) -> String {
    let (kept, omitted) = split_at_boundary(text.trim(), 200);
    if omitted == 0 {
        kept.to_string()
    } else {
        format!("{kept}...")
    }
}

/// The longest prefix of `text` within `limit` bytes that ends on a char
/// boundary, and how many bytes were left out.
fn split_at_boundary(text: &str, limit: usize) -> (&str, usize) {
    if text.len() <= limit {
        return (text, 0);
    }
    let mut end = limit;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    (&text[..end], text.len() - end)
}

/// What one execution printed and returned, capped at [`MAX_OUTPUT_BYTES`].
#[derive(Debug, Default, PartialEq)]
pub struct Output {
    pub stdout: String,
    pub stderr: String,
    /// `text/plain` values of `result` events (a code cell's last value).
    pub results: Vec<String>,
    pub error: Option<String>,
    /// An `execution_complete` event arrived.
    pub complete: bool,
    /// Bytes left out to stay under the cap.
    pub omitted: usize,
}

/// execd 1.x sends one event per output line with its newline stripped;
/// the spec's examples keep it. Either way each event ends one line.
fn line(mut text: String) -> String {
    if !text.ends_with('\n') {
        text.push('\n');
    }
    text
}

enum Channel {
    Stdout,
    Stderr,
    Result,
}

impl Output {
    fn used(&self) -> usize {
        self.stdout.len() + self.stderr.len() + self.results.iter().map(String::len).sum::<usize>()
    }

    /// Add `text` to a channel, keeping the total under the cap.
    fn append(&mut self, channel: Channel, text: &str) {
        let room = MAX_OUTPUT_BYTES.saturating_sub(self.used());
        let (kept, omitted) = split_at_boundary(text, room);
        self.omitted += omitted;
        match channel {
            Channel::Stdout => self.stdout.push_str(kept),
            Channel::Stderr => self.stderr.push_str(kept),
            Channel::Result => self.results.push(kept.to_string()),
        }
    }

    /// Fold one event in.
    pub fn apply(&mut self, event: Event) {
        let text = event.text.unwrap_or_default();
        match event.kind.as_str() {
            "stdout" => self.append(Channel::Stdout, &line(text)),
            "stderr" => self.append(Channel::Stderr, &line(text)),
            "result" => {
                let plain = event
                    .results
                    .as_ref()
                    .and_then(|r| r.get("text/plain"))
                    .and_then(|v| v.as_str())
                    .map(str::to_string)
                    .unwrap_or(text);
                if !plain.is_empty() {
                    self.append(Channel::Result, &plain);
                }
            }
            "error" => {
                let message = match event.error {
                    Some(e) => {
                        let mut message = format!("{}: {}", e.ename, e.evalue);
                        for line in e.traceback {
                            message.push('\n');
                            message.push_str(&line);
                        }
                        message
                    }
                    None => text,
                };
                let (kept, omitted) = split_at_boundary(&message, 4096);
                self.omitted += omitted;
                self.error = Some(kept.to_string());
            }
            "execution_complete" => self.complete = true,
            // init, ping, status, execution_count: nothing for the model.
            _ => {}
        }
    }

    /// The text the model sees.
    pub fn render(&self) -> String {
        let mut parts = Vec::new();
        if !self.stdout.is_empty() {
            parts.push(self.stdout.trim_end().to_string());
        }
        if !self.stderr.is_empty() {
            parts.push(format!("[stderr]\n{}", self.stderr.trim_end()));
        }
        for result in &self.results {
            parts.push(format!("[result]\n{}", result.trim_end()));
        }
        if let Some(error) = &self.error {
            parts.push(format!("[error]\n{error}"));
        }
        if !self.complete && self.error.is_none() {
            parts.push("[the execution did not report completion]".into());
        }
        if self.omitted > 0 {
            parts.push(format!(
                "[output truncated: {} bytes omitted]",
                self.omitted
            ));
        }
        if parts.is_empty() {
            return "(no output)".into();
        }
        parts.join("\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_in_chunks(body: &str, size: usize) -> Vec<Event> {
        let mut parser = Parser::default();
        let mut events = Vec::new();
        for chunk in body.as_bytes().chunks(size) {
            events.extend(parser.push(chunk).unwrap());
        }
        events.extend(parser.finish().unwrap());
        events
    }

    fn kinds(events: &[Event]) -> Vec<&str> {
        events.iter().map(|e| e.kind.as_str()).collect()
    }

    /// The shape the real execd sent during the probe.
    const PROBED: &str = "{\"type\":\"init\",\"text\":\"abc\",\"timestamp\":1}\n\n\
        {\"type\":\"ping\",\"text\":\"pong\"}\n\n\
        {\"type\":\"stdout\",\"text\":\"hé llo\\n\"}\n\n\
        {\"type\":\"stderr\",\"text\":\"warn\\n\"}\n\n\
        {\"type\":\"execution_complete\",\"execution_time\":3}\n\n";

    #[test]
    fn blank_line_separated_json_parses_whatever_the_chunking() {
        for size in [1, 2, 7, 64, PROBED.len()] {
            let events = parse_in_chunks(PROBED, size);
            let expected = ["init", "ping", "stdout", "stderr", "execution_complete"];
            assert_eq!(kinds(&events), expected);
            assert_eq!(events[2].text.as_deref(), Some("hé llo\n"));
        }
    }

    #[test]
    fn sse_framing_and_crlf_and_single_newlines_parse_too() {
        let sse = ": comment\r\nevent: message\r\nid: 1\r\nretry: 5\r\n\
                   data: {\"type\":\"stdout\",\"text\":\"a\"}\r\n\r\n";
        assert_eq!(kinds(&parse_in_chunks(sse, 3)), ["stdout"]);

        // Two objects with no blank line between them, and no trailing newline.
        let packed = "{\"type\":\"stdout\",\"text\":\"a\"}\n{\"type\":\"execution_complete\"}";
        assert_eq!(
            kinds(&parse_in_chunks(packed, 5)),
            ["stdout", "execution_complete"]
        );
    }

    #[test]
    fn a_frame_that_is_not_json_is_a_protocol_error() {
        let mut parser = Parser::default();
        let err = parser.push(b"<html>bad gateway</html>\n\n").unwrap_err();
        assert!(matches!(&err, Error::Protocol(m) if m.contains("bad gateway")));

        // And at the end of the body.
        let mut parser = Parser::default();
        parser.push(b"{\"type\":").unwrap();
        assert!(matches!(parser.finish(), Err(Error::Protocol(_))));
    }

    fn event(kind: &str, text: &str) -> Event {
        Event {
            kind: kind.into(),
            text: Some(text.into()),
            results: None,
            error: None,
        }
    }

    #[test]
    fn output_renders_every_channel_and_the_error() {
        let mut out = Output::default();
        out.apply(event("init", "id"));
        out.apply(event("stdout", "one\n"));
        out.apply(event("stderr", "two\n"));
        out.apply(Event {
            results: Some(
                serde_json::json!({"text/plain": "4", "text/html": "<b>4</b>"})
                    .as_object()
                    .unwrap()
                    .clone(),
            ),
            ..event("result", "")
        });
        out.apply(event("result", ""));
        out.apply(Event {
            error: Some(ExecError {
                ename: "NameError".into(),
                evalue: "x".into(),
                traceback: vec!["line 1".into()],
            }),
            ..event("error", "")
        });
        out.apply(event("execution_complete", ""));
        assert_eq!(
            out.render(),
            "one\n[stderr]\ntwo\n[result]\n4\n[error]\nNameError: x\nline 1"
        );
    }

    #[test]
    fn an_error_without_details_uses_its_text_and_long_errors_are_cut() {
        let mut out = Output::default();
        out.apply(event("error", "exit status 2"));
        assert_eq!(out.error.as_deref(), Some("exit status 2"));

        out.apply(event("error", &"e".repeat(5000)));
        assert_eq!(out.error.as_ref().unwrap().len(), 4096);
        assert_eq!(out.omitted, 904);
    }

    #[test]
    fn lines_without_their_newline_stay_separate_lines() {
        // As execd 1.x streams `echo a; echo b`.
        let mut out = Output::default();
        out.apply(event("stdout", "a"));
        out.apply(event("stdout", "b\n"));
        assert_eq!(out.stdout, "a\nb\n");
    }

    #[test]
    fn empty_and_unfinished_executions_say_so() {
        let mut out = Output::default();
        out.apply(event("execution_complete", ""));
        assert_eq!(out.render(), "(no output)");

        let mut out = Output::default();
        out.apply(event("stdout", "partial"));
        assert_eq!(
            out.render(),
            "partial\n[the execution did not report completion]"
        );
    }

    #[test]
    fn output_is_capped_on_a_char_boundary_and_the_rest_counted() {
        let mut out = Output::default();
        // With its newline, this leaves one byte of room.
        out.apply(event("stdout", &"a".repeat(MAX_OUTPUT_BYTES - 2)));
        // A two-byte char does not fit in the last byte of room.
        out.apply(event("stderr", "é and more"));
        out.apply(event("execution_complete", ""));
        assert_eq!(out.stdout.len(), MAX_OUTPUT_BYTES - 1);
        assert_eq!(out.stderr, "");
        assert_eq!(out.omitted, "é and more\n".len());
        assert!(
            out.render()
                .ends_with("[output truncated: 12 bytes omitted]")
        );
    }

    #[test]
    fn previews_are_short() {
        assert_eq!(preview("  short  "), "short");
        let long = preview(&"é".repeat(300));
        assert!(long.ends_with("...") && long.len() <= 203, "{long}");
    }
}
