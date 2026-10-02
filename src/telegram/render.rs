//! Markdown to Telegram messages: plain text plus entities.
//!
//! The model writes Markdown. Telegram's classic messages have no lists,
//! headings or tables, only styled spans, so a reply is turned into text plus
//! a [`MessageEntity`] list (`sendMessage`'s `entities`), and Telegram never
//! parses markup itself. A stray `*` in the text therefore cannot make
//! Telegram refuse a message, which is what the `MarkdownV2` and HTML parse
//! modes do. [`render`] is pure: Markdown in, chunks out.
//!
//! Layout, by construct:
//!
//! - `**bold**`, `*italic*`, `~~strike~~`: the matching entity.
//! - `` `code` ``: `code`. A fence: `pre`, with its language.
//! - `[text](url)`: `text_link` for http, https and mailto, except in a
//!   blockquote or a table. Any other link stays visible as `text (url)`,
//!   never silently dropped.
//! - `> quote`: `blockquote`.
//! - Headings are bold with a blank line before. Bullets are `• ` text and
//!   ordered lists keep their numbers; nested lists indent two spaces a level.
//! - A table is an aligned grid in one `pre` entity. A grid wider than
//!   [`TABLE_WIDTH`] columns is written as one `Header: value` block per row.
//! - A rule is a line of box-drawing characters. An image is its alt text and
//!   its URL. Raw HTML, and anything that does not parse as Markdown, stays
//!   literal text.
//!
//! The rules Telegram applies, from the Bot API's "Formatting options":
//! offsets and lengths count UTF-16 code units; entities that share
//! characters must nest; `bold`, `italic`, `underline`, `strikethrough` and
//! `spoiler` may contain other entities but not `code` or `pre`; everything
//! else (`code`, `pre`, links, blockquotes) may not contain other entities
//! (blockquotes not even each other). An entity's range must not end in
//! whitespace. The 4096 limit counts the text after parsing.
//!
//! Model Markdown nests freely (`**`code`**`, `> see [this](u)`), so
//! [`legalize`] flattens it: where an outer link or blockquote meets
//! something it may not hold, the outer one wins; a styled span is cut into
//! pieces around the `code` or `pre` inside it.

use super::{MESSAGE_LIMIT, split_ranges, utf16_len};
use pulldown_cmark::{Alignment, CodeBlockKind, Event, LinkType, Options, Parser, Tag, TagEnd};
use std::cmp::Reverse;
use teloxide::types::{MessageEntity, MessageEntityKind};
use url::Url;

/// The widest table written as a grid, in monospace columns. Wider ones do
/// not fit a phone's screen, and are written as one block per row instead.
pub const TABLE_WIDTH: usize = 60;

/// What a horizontal rule is shown as.
const RULE: &str = "────────────";

/// One message to send: its text, and the entities that format it, with
/// offsets in UTF-16 code units from the start of `text`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chunk {
    pub text: String,
    pub entities: Vec<MessageEntity>,
}

impl Chunk {
    /// A message with no formatting.
    pub fn plain(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            entities: Vec::new(),
        }
    }
}

/// Render a Markdown reply as messages of at most [`MESSAGE_LIMIT`] UTF-16
/// code units. A reply with nothing visible in it gives no messages.
pub fn render(markdown: &str) -> Vec<Chunk> {
    render_within(markdown, MESSAGE_LIMIT)
}

/// [`render`] with another message size, for tests that cut small texts.
pub fn render_within(markdown: &str, limit: usize) -> Vec<Chunk> {
    let (text, spans) = convert(markdown);
    let entities = legalize(&text, spans);
    chunk(&text, &entities, limit)
}

// ---------------- Markdown to text and spans ----------------

/// A styled range of the text, in UTF-16 code units.
#[derive(Debug)]
struct Span {
    kind: MessageEntityKind,
    start: usize,
    end: usize,
}

fn convert(markdown: &str) -> (String, Vec<Span>) {
    let options =
        Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS;
    let mut renderer = Renderer::default();
    for event in Parser::new_ext(markdown, options) {
        renderer.event(event);
    }
    (renderer.out, renderer.spans)
}

/// The blank space owed before the next block.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Gap {
    #[default]
    None,
    Line,
    Blank,
}

/// A link being read.
struct Link {
    dest: String,
    /// Whether a `text_link` span is open for it.
    entity: bool,
    /// How much text there was when it began, to tell an empty link.
    at: usize,
}

#[derive(Default)]
struct Renderer {
    out: String,
    /// `out` in UTF-16 code units.
    units: usize,
    /// Every span, in the order it opened: outer before inner.
    spans: Vec<Span>,
    /// Indexes into `spans` of the spans still open.
    open: Vec<usize>,
    /// Separation owed before the next thing written. Written lazily so a
    /// reply never ends in blank lines, and a span starts after them.
    pending: Gap,
    /// Nothing has been written in the container (quote, list item) just
    /// opened, so its first block needs no separation.
    fresh: bool,
    /// The open lists: the next number of an ordered one.
    lists: Vec<Option<u64>>,
    /// How many blockquotes are open.
    quotes: usize,
    links: Vec<Link>,
    /// Open images: their URL and how much text there was at the start.
    images: Vec<(String, usize)>,
    table: Option<Table>,
}

impl Renderer {
    fn event(&mut self, event: Event<'_>) {
        match event {
            Event::Start(tag) => self.start(tag),
            Event::End(tag) => self.end(tag),
            // Text, and what Markdown cannot say in entities, as it was written.
            Event::Text(t)
            | Event::Html(t)
            | Event::InlineHtml(t)
            | Event::InlineMath(t)
            | Event::DisplayMath(t)
            | Event::FootnoteReference(t) => self.write(&t),
            Event::Code(t) => {
                self.open(MessageEntityKind::Code);
                self.write(&t);
                self.close();
            }
            Event::SoftBreak | Event::HardBreak => self.write("\n"),
            Event::Rule => {
                self.block(self.gap());
                self.write(RULE);
            }
            Event::TaskListMarker(done) => self.write(if done { "☑ " } else { "☐ " }),
        }
    }

    fn start(&mut self, tag: Tag<'_>) {
        use MessageEntityKind::{Blockquote, Bold, Italic, Pre, Strikethrough};
        match tag {
            Tag::Paragraph | Tag::HtmlBlock => self.block(self.gap()),
            Tag::Heading { .. } => {
                self.block(Gap::Blank);
                self.open(Bold);
            }
            Tag::BlockQuote(_) => {
                self.block(self.gap());
                self.open(Blockquote);
                self.quotes += 1;
                self.fresh = true;
            }
            Tag::CodeBlock(kind) => {
                self.block(self.gap());
                self.open(Pre {
                    language: language(&kind),
                });
            }
            Tag::List(first) => {
                self.block(self.gap());
                self.lists.push(first);
            }
            Tag::Item => self.item(),
            Tag::Table(aligns) => {
                self.block(self.gap());
                self.table = Some(Table::new(aligns));
            }
            Tag::Emphasis => self.open(Italic),
            Tag::Strong => self.open(Bold),
            Tag::Strikethrough => self.open(Strikethrough),
            Tag::Link {
                link_type,
                dest_url,
                ..
            } => {
                // An email address is a link to its mailto: address.
                match link_type {
                    LinkType::Email => self.start_link(&format!("mailto:{dest_url}")),
                    _ => self.start_link(&dest_url),
                }
            }
            Tag::Image { dest_url, .. } => {
                let at = self.len();
                self.images.push((dest_url.to_string(), at));
            }
            // Table parts start empty. The rest need parser options that are
            // not on, so they cannot occur.
            _ => {}
        }
    }

    fn end(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Heading(_) => self.close(),
            TagEnd::BlockQuote(_) => {
                self.quotes -= 1;
                self.fresh = false;
                self.close();
            }
            TagEnd::Item => self.fresh = false,
            TagEnd::Emphasis | TagEnd::Strong | TagEnd::Strikethrough => self.close(),
            TagEnd::CodeBlock => {
                let floor = self.spans[self.open[self.open.len() - 1]].start;
                self.trim_newlines(floor);
                self.close();
            }
            TagEnd::HtmlBlock => self.trim_newlines(0),
            TagEnd::List(_) => {
                self.lists.pop();
            }
            TagEnd::Table => self.end_table(),
            TagEnd::TableCell => {
                let cell = std::mem::take(&mut self.table().cell);
                self.table().row.push(cell);
            }
            TagEnd::TableHead | TagEnd::TableRow => {
                let row = std::mem::take(&mut self.table().row);
                self.table().rows.push(row);
            }
            TagEnd::Link => self.end_link(),
            TagEnd::Image => self.end_image(),
            // Paragraphs add nothing at their end: the next block brings its
            // own separation.
            _ => {}
        }
    }

    /// The separation between blocks: blank lines, except inside a list,
    /// where blocks follow each other on the next line.
    fn gap(&self) -> Gap {
        if self.lists.is_empty() {
            Gap::Blank
        } else {
            Gap::Line
        }
    }

    /// A block starts: owe `gap` before it, unless it is the first thing in
    /// its container.
    fn block(&mut self, gap: Gap) {
        if !self.fresh {
            self.pending = self.pending.max(gap);
        }
    }

    /// Write the separation owed, less the line breaks the text already ends
    /// in (an empty block leaves its separation behind), and none before the
    /// first thing written.
    fn flush(&mut self) {
        let owed: usize = match std::mem::take(&mut self.pending) {
            _ if self.out.is_empty() => 0,
            Gap::None => 0,
            Gap::Line => 1,
            Gap::Blank => 2,
        };
        let have = self.out.chars().rev().take_while(|&c| c == '\n').count();
        let newlines = owed.saturating_sub(have);
        self.out.push_str(&"\n".repeat(newlines));
        self.units += newlines;
    }

    /// Write text. In a table it goes to the cell being read.
    fn write(&mut self, text: &str) {
        if let Some(table) = &mut self.table {
            table.cell.push_str(text);
            return;
        }
        self.flush();
        self.fresh = false;
        self.out.push_str(text);
        self.units += utf16_len(text);
    }

    /// How much has been written, in bytes: a mark to tell if anything was.
    fn len(&self) -> usize {
        self.table
            .as_ref()
            .map_or(self.out.len(), |table| table.cell.len())
    }

    /// Start a span here. A table cell is plain text, so it has none.
    fn open(&mut self, kind: MessageEntityKind) {
        if self.table.is_some() {
            return;
        }
        self.flush();
        self.open.push(self.spans.len());
        self.spans.push(Span {
            kind,
            start: self.units,
            end: self.units,
        });
    }

    /// End the span opened last.
    fn close(&mut self) {
        if self.table.is_some() {
            return;
        }
        let span = self.open.pop().expect("a span is open");
        self.spans[span].end = self.units;
    }

    /// Drop line breaks from the end of the text, down to `floor` units.
    fn trim_newlines(&mut self, floor: usize) {
        while self.units > floor && self.out.ends_with('\n') {
            self.out.pop();
            self.units -= 1;
        }
    }

    fn item(&mut self) {
        self.block(Gap::Line);
        let indent = "  ".repeat(self.lists.len() - 1);
        let marker = match self.lists.last_mut() {
            Some(Some(number)) => {
                let marker = format!("{number}. ");
                *number += 1;
                marker
            }
            _ => "• ".to_string(),
        };
        self.write(&format!("{indent}{marker}"));
        self.fresh = true;
    }

    /// A link is a `text_link` entity only where Telegram allows one: not in
    /// a table cell (plain text) and not in a blockquote, which may hold only
    /// styling.
    fn start_link(&mut self, dest: &str) {
        let url = linkable(dest).filter(|_| self.table.is_none() && self.quotes == 0);
        let entity = url.is_some();
        if let Some(url) = url {
            self.open(MessageEntityKind::TextLink { url });
        }
        let at = self.len();
        self.links.push(Link {
            dest: dest.to_string(),
            entity,
            at,
        });
    }

    /// A link with no text shows its address; one that cannot be a
    /// `text_link` shows it after the text.
    fn end_link(&mut self) {
        let link = self.links.pop().expect("a link is open");
        if link.at == self.len() {
            self.write(&link.dest);
        } else if !link.entity {
            self.write(&format!(" ({})", link.dest));
        }
        if link.entity {
            self.close();
        }
    }

    /// An image is shown as its alt text, already written, and its address.
    fn end_image(&mut self) {
        let (dest, at) = self.images.pop().expect("an image is open");
        if at == self.len() {
            self.write(&dest);
        } else {
            self.write(&format!(" ({dest})"));
        }
    }

    fn table(&mut self) -> &mut Table {
        self.table
            .as_mut()
            .expect("table parts come inside a table")
    }

    fn end_table(&mut self) {
        let table = self.table.take().expect("a table is open");
        match table.grid() {
            Some(grid) => {
                self.open(MessageEntityKind::Pre { language: None });
                self.write(&grid);
                self.close();
            }
            None => self.write(&table.records()),
        }
    }
}

/// The language of a fence: the first word of its info string.
fn language(kind: &CodeBlockKind<'_>) -> Option<String> {
    match kind {
        CodeBlockKind::Indented => None,
        CodeBlockKind::Fenced(info) => info
            .split(|c: char| c.is_whitespace() || c == ',' || c == '{')
            .next()
            .filter(|word| !word.is_empty())
            .map(str::to_string),
    }
}

/// The destination as a URL Telegram will open from a `text_link`.
fn linkable(dest: &str) -> Option<Url> {
    Url::parse(dest)
        .ok()
        .filter(|url| matches!(url.scheme(), "http" | "https" | "mailto"))
}

// ---------------- tables ----------------

/// A table being read: finished rows, the row and the cell in progress.
struct Table {
    aligns: Vec<Alignment>,
    /// The header first.
    rows: Vec<Vec<String>>,
    row: Vec<String>,
    cell: String,
}

impl Table {
    fn new(aligns: Vec<Alignment>) -> Self {
        Self {
            aligns,
            rows: Vec::new(),
            row: Vec::new(),
            cell: String::new(),
        }
    }

    /// The table as a grid of aligned columns, or `None` if it is wider
    /// than [`TABLE_WIDTH`].
    fn grid(&self) -> Option<String> {
        let columns = self.rows.iter().map(Vec::len).max().unwrap_or(0);
        let widths: Vec<usize> = (0..columns)
            .map(|c| {
                let cells = self.rows.iter().filter_map(|row| row.get(c));
                cells
                    .map(|cell| display_width(cell))
                    .max()
                    .unwrap_or(0)
                    .max(1)
            })
            .collect();
        if widths.iter().sum::<usize>() + 3 * columns.saturating_sub(1) > TABLE_WIDTH {
            return None;
        }
        let mut lines = Vec::new();
        for (n, row) in self.rows.iter().enumerate() {
            let cells: Vec<String> = widths
                .iter()
                .enumerate()
                .map(|(c, &width)| {
                    let cell = row.get(c).map_or("", String::as_str);
                    pad(cell, width, self.aligns.get(c))
                })
                .collect();
            lines.push(cells.join(" | ").trim_end().to_string());
            if n == 0 {
                let rule: Vec<String> = widths.iter().map(|&w| "-".repeat(w)).collect();
                lines.push(rule.join("-+-"));
            }
        }
        Some(lines.join("\n"))
    }

    /// The table as plain text, a `Header: value` line per cell and a blank
    /// line between rows. A table of only a header is that header.
    fn records(&self) -> String {
        let (head, body) = self.rows.split_first().expect("a table has a header");
        if body.is_empty() {
            return head.join(" | ");
        }
        let records: Vec<String> = body
            .iter()
            .map(|row| {
                let lines: Vec<String> = head
                    .iter()
                    .zip(row)
                    .filter(|(_, value)| !value.is_empty())
                    .map(|(name, value)| match name.is_empty() {
                        true => value.clone(),
                        false => format!("{name}: {value}"),
                    })
                    .collect();
                lines.join("\n")
            })
            .filter(|record| !record.is_empty())
            .collect();
        records.join("\n\n")
    }
}

/// `cell` padded with spaces to `width` columns as `align` says.
fn pad(cell: &str, width: usize, align: Option<&Alignment>) -> String {
    let spare = width.saturating_sub(display_width(cell));
    let (left, right) = match align {
        Some(Alignment::Right) => (spare, 0),
        Some(Alignment::Center) => (spare / 2, spare - spare / 2),
        _ => (0, spare),
    };
    format!("{}{cell}{}", " ".repeat(left), " ".repeat(right))
}

/// How many monospace columns `text` takes.
fn display_width(text: &str) -> usize {
    text.chars().map(char_width).sum()
}

/// Columns one character takes in a monospace font: none for combining marks
/// and joiners, two for East Asian wide characters and emoji, else one. A
/// close approximation of Unicode's East Asian Width, not a copy of it.
fn char_width(c: char) -> usize {
    match u32::from(c) {
        0x0300..=0x036F | 0x200B..=0x200F | 0x20D0..=0x20FF | 0xFE00..=0xFE0F => 0,
        0x1100..=0x115F
        | 0x2E80..=0xA4CF
        | 0xAC00..=0xD7A3
        | 0xF900..=0xFAFF
        | 0xFE30..=0xFE6F
        | 0xFF00..=0xFF60
        | 0xFFE0..=0xFFE6
        | 0x1F300..=0x1F64F
        | 0x1F900..=0x1F9FF
        | 0x20000..=0x3FFFD => 2,
        _ => 1,
    }
}

// ---------------- making the entities legal ----------------

fn is_style(kind: &MessageEntityKind) -> bool {
    use MessageEntityKind::{Bold, Italic, Spoiler, Strikethrough, Underline};
    matches!(kind, Bold | Italic | Underline | Strikethrough | Spoiler)
}

fn is_mono(kind: &MessageEntityKind) -> bool {
    matches!(
        kind,
        MessageEntityKind::Code | MessageEntityKind::Pre { .. }
    )
}

/// Whether Telegram lets an `outer` entity contain an `inner` one. Styled
/// spans hold anything, though pieces of `code` and `pre` are cut out of
/// them (see [`legalize`]); a repeat of the same style is pointless. Links
/// and blockquotes hold only styled spans, `code` and `pre` nothing.
fn holds(outer: &MessageEntityKind, inner: &MessageEntityKind) -> bool {
    if is_mono(outer) {
        false
    } else if is_style(outer) {
        outer != inner
    } else {
        is_style(inner)
    }
}

fn is_space(unit: u16) -> bool {
    char::from_u32(u32::from(unit)).is_some_and(char::is_whitespace)
}

/// The spans as entities Telegram accepts, outer before inner.
///
/// Drops spans that are empty or only whitespace, trims whitespace off the
/// end of the rest, drops what an outer entity may not hold (see [`holds`]),
/// and cuts each styled span around the `code` and `pre` inside it, since a
/// styled span may not contain those.
fn legalize(text: &str, spans: Vec<Span>) -> Vec<MessageEntity> {
    let units: Vec<u16> = text.encode_utf16().collect();
    let mut kept: Vec<Span> = Vec::new();
    // The kept spans around the one being looked at, as indexes into `kept`.
    let mut around: Vec<usize> = Vec::new();
    for mut span in spans {
        while span.end > span.start && is_space(units[span.end - 1]) {
            span.end -= 1;
        }
        if span.end <= span.start {
            continue;
        }
        while around.last().is_some_and(|&i| kept[i].end <= span.start) {
            around.pop();
        }
        if around.iter().all(|&i| holds(&kept[i].kind, &span.kind)) {
            around.push(kept.len());
            kept.push(span);
        }
    }

    let holes: Vec<(usize, usize)> = kept
        .iter()
        .filter(|span| is_mono(&span.kind))
        .map(|span| (span.start, span.end))
        .collect();
    let mut entities = Vec::new();
    let mut piece = |kind: &MessageEntityKind, from: usize, mut to: usize| {
        while to > from && is_space(units[to - 1]) {
            to -= 1;
        }
        if to > from {
            entities.push(MessageEntity::new(kind.clone(), from, to - from));
        }
    };
    for span in &kept {
        let mut from = span.start;
        if is_style(&span.kind) {
            let inside = holes
                .iter()
                .filter(|&&(start, end)| start >= span.start && end <= span.end);
            for &(start, end) in inside {
                piece(&span.kind, from, start);
                from = end;
            }
        }
        piece(&span.kind, from, span.end);
    }
    entities.sort_by_key(|e| (e.offset, Reverse(e.length)));
    entities
}

// ---------------- cutting into messages ----------------

/// Cut the text into messages of at most `limit` UTF-16 code units, at the
/// places [`split_ranges`] chooses. An entity that crosses a cut is clipped
/// and reopened in the next message, its offsets counted from that message's
/// start. Leading line breaks and trailing whitespace are trimmed off each
/// message, so no entity ends in whitespace.
fn chunk(text: &str, entities: &[MessageEntity], limit: usize) -> Vec<Chunk> {
    split_ranges(text, limit)
        .into_iter()
        .map(|range| {
            let body = text[range.clone()].trim_start_matches('\n');
            let start = range.end - body.len();
            let body = body.trim_end();
            let from = utf16_len(&text[..start]);
            let to = from + utf16_len(body);
            Chunk {
                text: body.to_string(),
                entities: entities.iter().filter_map(|e| clip(e, from, to)).collect(),
            }
        })
        .collect()
}

/// The part of `e` inside `from..to`, counted from `from`.
fn clip(e: &MessageEntity, from: usize, to: usize) -> Option<MessageEntity> {
    let start = e.offset.max(from);
    let end = (e.offset + e.length).min(to);
    (start < end).then(|| MessageEntity::new(e.kind.clone(), start - from, end - start))
}

#[cfg(test)]
mod tests;
