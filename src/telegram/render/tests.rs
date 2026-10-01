use super::*;
use MessageEntityKind::{Blockquote, Bold, Code, Italic, Spoiler, Strikethrough, Underline};

fn utf16(text: &str) -> Vec<u16> {
    text.encode_utf16().collect()
}

/// The text an entity covers.
fn covered(text: &str, e: &MessageEntity) -> String {
    String::from_utf16(&utf16(text)[e.offset..e.offset + e.length]).unwrap()
}

fn label(kind: &MessageEntityKind) -> String {
    match kind {
        Bold => "bold".into(),
        Italic => "italic".into(),
        Strikethrough => "strike".into(),
        Code => "code".into(),
        Blockquote => "quote".into(),
        MessageEntityKind::Pre { language } => {
            format!("pre:{}", language.as_deref().unwrap_or(""))
        }
        MessageEntityKind::TextLink { url } => format!("link:{url}"),
        other => panic!("unexpected entity {other:?}"),
    }
}

/// One chunk's entities as `(kind, text covered)`.
fn shown(chunk: &Chunk) -> Vec<(String, String)> {
    chunk
        .entities
        .iter()
        .map(|e| (label(&e.kind), covered(&chunk.text, e)))
        .collect()
}

/// A reply that fits one message: its text and entities.
fn show(markdown: &str) -> (String, Vec<(String, String)>) {
    let mut chunks = render(markdown);
    assert_eq!(chunks.len(), 1, "{markdown:?} gave {chunks:#?}");
    let chunk = chunks.remove(0);
    assert_valid(&chunk, MESSAGE_LIMIT);
    (chunk.text.clone(), shown(&chunk))
}

fn pair(kind: &str, text: &str) -> (String, String) {
    (kind.to_string(), text.to_string())
}

/// The entity kinds of `entities`, sorted.
fn kinds(entities: &[(String, String)]) -> Vec<&str> {
    let mut kinds: Vec<&str> = entities.iter().map(|e| e.0.as_str()).collect();
    kinds.sort_unstable();
    kinds
}

/// Telegram's nesting rules, written out again from the Bot API's
/// "Formatting options" so the tests do not trust [`holds`].
fn legal(outer: &MessageEntityKind, inner: &MessageEntityKind) -> bool {
    let style =
        |k: &MessageEntityKind| matches!(k, Bold | Italic | Underline | Strikethrough | Spoiler);
    let mono = |k: &MessageEntityKind| matches!(k, Code | MessageEntityKind::Pre { .. });
    if mono(outer) {
        false
    } else if style(outer) {
        !mono(inner)
    } else {
        style(inner)
    }
}

/// Everything Telegram requires of a message: it fits, no entity is empty,
/// out of range or ends in whitespace, and entities that share characters
/// nest legally. Entities must also be sorted, outer first.
fn assert_valid(chunk: &Chunk, limit: usize) {
    let units = utf16(&chunk.text);
    assert!(!units.is_empty() && units.len() <= limit, "{chunk:?}");
    assert!(!is_space(units[units.len() - 1]), "{chunk:?}");
    let es = &chunk.entities;
    for e in es {
        assert!(e.length > 0, "{chunk:?}");
        assert!(e.offset + e.length <= units.len(), "{chunk:?}");
        assert!(!is_space(units[e.offset + e.length - 1]), "{chunk:?}");
    }
    for (i, a) in es.iter().enumerate() {
        for b in &es[i + 1..] {
            let order = |e: &MessageEntity| (e.offset, Reverse(e.length));
            assert!(order(a) <= order(b), "not sorted: {chunk:?}");
            if b.offset >= a.offset + a.length {
                continue;
            }
            assert!(
                b.offset + b.length <= a.offset + a.length,
                "overlap without nesting: {chunk:?}"
            );
            assert!(legal(&a.kind, &b.kind), "illegal nesting: {chunk:?}");
        }
    }
}

fn squash(s: &str) -> String {
    s.split_whitespace().collect()
}

#[test]
fn styles_become_entities_and_their_markers_vanish() {
    let (text, entities) = show("**bold** and *it* and ~~gone~~ and `code`");
    assert_eq!(text, "bold and it and gone and code");
    assert_eq!(
        entities,
        [
            pair("bold", "bold"),
            pair("italic", "it"),
            pair("strike", "gone"),
            pair("code", "code"),
        ]
    );
}

#[test]
fn a_fence_is_pre_with_its_language_and_no_trailing_newline() {
    let (text, entities) = show("Intro\n\n```py\nprint(1)\n```\n\nOutro");
    assert_eq!(text, "Intro\n\nprint(1)\n\nOutro");
    assert_eq!(entities, [pair("pre:py", "print(1)")]);

    // Only the first word of the info string is the language.
    let (_, entities) = show("```rust,ignore\nx\n```");
    assert_eq!(entities, [pair("pre:rust", "x")]);
    let (_, entities) = show("```\nx\n```");
    assert_eq!(entities, [pair("pre:", "x")]);
    let (text, entities) = show("para\n\n    indented\n    code");
    assert_eq!(text, "para\n\nindented\ncode");
    assert_eq!(entities, [pair("pre:", "indented\ncode")]);

    // Blank lines at the end of a fence are not part of it.
    let (text, entities) = show("```\ncode\n\n\n```\nafter");
    assert_eq!(text, "code\n\nafter");
    assert_eq!(entities, [pair("pre:", "code")]);
}

#[test]
fn an_empty_fence_leaves_no_entity_and_no_extra_blank_lines() {
    let (text, entities) = show("a\n\n```\n```\n\nb");
    assert_eq!(text, "a\n\nb");
    assert!(entities.is_empty());
}

#[test]
fn web_and_mail_links_become_text_links() {
    let (text, entities) = show("see [the docs](https://example.com/a) now");
    assert_eq!(text, "see the docs now");
    assert_eq!(entities, [pair("link:https://example.com/a", "the docs")]);

    let (text, entities) = show("[mail](mailto:a@b.co) <https://example.com/y> <a@b.co>");
    assert_eq!(text, "mail https://example.com/y a@b.co");
    assert_eq!(
        entities,
        [
            pair("link:mailto:a@b.co", "mail"),
            pair("link:https://example.com/y", "https://example.com/y"),
            pair("link:mailto:a@b.co", "a@b.co"),
        ]
    );

    let (text, entities) = show("[a][r]\n\n[r]: https://example.com/r");
    assert_eq!(text, "a");
    assert_eq!(entities, [pair("link:https://example.com/r", "a")]);
}

#[test]
fn a_link_telegram_cannot_open_keeps_its_address_visible() {
    for (markdown, expected) in [
        ("[docs](#install)", "docs (#install)"),
        ("[a](javascript:alert(1))", "a (javascript:alert(1))"),
        ("[a](/relative/path)", "a (/relative/path)"),
    ] {
        let (text, entities) = show(markdown);
        assert_eq!(text, expected);
        assert!(entities.is_empty(), "{entities:?}");
    }
}

#[test]
fn a_link_with_no_text_shows_its_address() {
    let (text, entities) = show("[](https://example.com/x)");
    assert_eq!(text, "https://example.com/x");
    assert_eq!(
        entities,
        [pair("link:https://example.com/x", "https://example.com/x")]
    );
    let (text, entities) = show("[](#x)");
    assert_eq!(text, "#x");
    assert!(entities.is_empty());
}

#[test]
fn images_are_their_alt_text_and_address() {
    let (text, entities) = show("![logo](https://example.com/l.png)");
    assert_eq!(text, "logo (https://example.com/l.png)");
    assert!(entities.is_empty());
    let (text, _) = show("![](https://example.com/l.png)");
    assert_eq!(text, "https://example.com/l.png");
    let (text, entities) = show("[![l](https://e.com/i.png)](https://e.com/p)");
    assert_eq!(text, "l (https://e.com/i.png)");
    assert_eq!(
        entities,
        [pair("link:https://e.com/p", "l (https://e.com/i.png)")]
    );
}

#[test]
fn nested_lists_indent_and_ordered_lists_keep_their_numbers() {
    let (text, _) = show("- a\n  - b\n    - c\n- d");
    assert_eq!(text, "• a\n  • b\n    • c\n• d");
    let (text, _) = show("3. x\n4. y\n   - z");
    assert_eq!(text, "3. x\n4. y\n  • z");
    let (text, _) = show("para\n\n- a\n- b\n\nnext");
    assert_eq!(text, "para\n\n• a\n• b\n\nnext");
}

#[test]
fn loose_lists_and_blocks_inside_items_stay_compact() {
    let (text, _) = show("- a\n\n- b");
    assert_eq!(text, "• a\n• b");
    let (text, _) = show("- a\n\n  more\n- b");
    assert_eq!(text, "• a\nmore\n• b");
    let (text, entities) = show("- a\n\n  ```\n  x\n  ```\n- b");
    assert_eq!(text, "• a\nx\n• b");
    assert_eq!(entities, [pair("pre:", "x")]);
    let (text, _) = show("- [x] done\n- [ ] todo");
    assert_eq!(text, "• ☑ done\n• ☐ todo");
    let (text, entities) = show("- **bold** item");
    assert_eq!(text, "• bold item");
    assert_eq!(entities, [pair("bold", "bold")]);
}

#[test]
fn an_empty_item_keeps_its_marker_and_does_not_glue_onto_the_next_block() {
    for (markdown, expected) in [
        ("- \n- b", "• \n• b"),
        ("- a\n-\n\npara", "• a\n• \n\npara"),
        ("1. \n2. x", "1. \n2. x"),
        ("- >\n- b", "• \n• b"),
        ("- ```\n  ```\n- b", "• \n• b"),
        ("> -\n\npara", "• \n\npara"),
    ] {
        let (text, _) = show(markdown);
        assert_eq!(text, expected, "{markdown:?}");
    }
}

#[test]
fn a_quote_after_an_item_starts_a_new_line() {
    let (text, entities) = show("- a\n\n  > q\n- b");
    assert_eq!(text, "• a\nq\n• b");
    assert_eq!(entities, [pair("quote", "q")]);
}

#[test]
fn a_single_tilde_between_words_is_not_struck_through() {
    for text in ["foo~bar~baz", "about ~5 or ~10 people"] {
        let (shown, entities) = show(text);
        assert_eq!(shown, text);
        assert!(entities.is_empty(), "{text:?}: {entities:?}");
    }
}

#[test]
fn headings_are_bold_with_a_blank_line_before() {
    let (text, entities) = show("# Title\n\ntext\n\n## Sub\nmore");
    assert_eq!(text, "Title\n\ntext\n\nSub\n\nmore");
    assert_eq!(entities, [pair("bold", "Title"), pair("bold", "Sub")]);
    // Bold inside a bold heading adds nothing.
    let (text, entities) = show("# Title with **bold**");
    assert_eq!(text, "Title with bold");
    assert_eq!(entities, [pair("bold", "Title with bold")]);
}

#[test]
fn a_blockquote_holds_styling() {
    let (text, entities) = show("intro\n\n> quoted *x*\n> second\n\nafter");
    assert_eq!(text, "intro\n\nquoted x\nsecond\n\nafter");
    assert_eq!(
        entities,
        [pair("quote", "quoted x\nsecond"), pair("italic", "x")]
    );
    // Paragraphs and a list inside one quote.
    let (text, entities) = show("> a\n>\n> b\n>\n> - c");
    assert_eq!(text, "a\n\nb\n\n• c");
    assert_eq!(entities, [pair("quote", "a\n\nb\n\n• c")]);
    // A list in a quote after other text has no stray blank line.
    let (text, _) = show("para\n\n> - c\n> - d");
    assert_eq!(text, "para\n\n• c\n• d");
}

#[test]
fn a_table_is_an_aligned_grid_in_one_pre() {
    let (text, entities) =
        show("| Name | Qty |\n|:-----|----:|\n| apple | 3 |\n| kiwi | 12 |\n\nafter");
    let grid = "Name  | Qty\n------+----\napple |   3\nkiwi  |  12";
    assert_eq!(text, format!("{grid}\n\nafter"));
    assert_eq!(entities, [pair("pre:", grid)]);

    let (text, _) = show("| a |\n|:-:|\n| xyz |");
    assert_eq!(text, " a\n---\nxyz");
    // A row shorter than the header is padded.
    let (text, _) = show("| a | b |\n|---|---|\n| c |");
    assert_eq!(text, "a | b\n--+--\nc |");
}

#[test]
fn grid_columns_are_padded_by_display_width() {
    let (text, _) = show("| 名前 | n |\n|---|---|\n| あ | 1 |");
    assert_eq!(text, "名前 | n\n-----+--\nあ   | 1");
    let (text, _) = show("| x | y |\n|---|---|\n| 😀 | e\u{301} |");
    assert_eq!(text, "x  | y\n---+--\n😀 | e\u{301}");
}

#[test]
fn styling_inside_a_cell_is_plain_and_links_keep_their_address() {
    let (text, entities) =
        show("| h |\n|---|\n| **b** `c` *i* |\n| [x](https://example.com/z) |\n| a<br>b |");
    let rule = "-".repeat("x (https://example.com/z)".len());
    assert_eq!(
        text,
        format!("h\n{rule}\nb c i\nx (https://example.com/z)\na<br>b")
    );
    assert_eq!(kinds(&entities), ["pre:"]);
}

#[test]
fn a_table_too_wide_for_a_screen_is_one_block_per_row() {
    let cell = "x".repeat(30);
    let (text, entities) = show(&format!(
        "| A | B | C |\n|---|---|---|\n| {cell} | {cell} | |\n| | | |\n| 1 | | |\n\n\
         | | n |\n|---|---|\n| {cell} | {cell} |"
    ));
    // The row with nothing in it has nothing to say, and no blank lines.
    assert_eq!(
        text,
        format!("A: {cell}\nB: {cell}\n\nA: 1\n\n{cell}\nn: {cell}")
    );
    assert!(entities.is_empty());
}

#[test]
fn a_wide_table_with_only_a_header_is_its_header() {
    let (a, b) = ("a".repeat(30), "b".repeat(30));
    let (text, entities) = show(&format!("| {a} | {b} |\n|---|---|"));
    assert_eq!(text, format!("{a} | {b}"));
    assert!(entities.is_empty());
}

#[test]
fn widths_count_wide_characters_twice_and_combining_marks_not_at_all() {
    assert_eq!(display_width("abc"), 3);
    assert_eq!(display_width("名前"), 4);
    assert_eq!(display_width("😀"), 2);
    assert_eq!(display_width("e\u{301}"), 1);
    assert_eq!(display_width("a\u{200d}b"), 2);
    assert_eq!(display_width(""), 0);
    // One from each wide range: Hangul Jamo and syllables, CJK, kana,
    // compatibility ideographs, fullwidth forms, emoji and extension B.
    for wide in "ᄀ한豈︰Ａ￠🌀🤖𠀀、ぁ".chars() {
        assert_eq!(char_width(wide), 2, "{wide:?}");
    }
    for narrow in "aé~·€\u{ff61}".chars() {
        assert_eq!(char_width(narrow), 1, "{narrow:?}");
    }
    for zero in "\u{300}\u{200b}\u{20d0}\u{fe0f}".chars() {
        assert_eq!(char_width(zero), 0, "{zero:?}");
    }
}

#[test]
fn what_telegram_lets_an_entity_hold() {
    let link = || MessageEntityKind::TextLink {
        url: Url::parse("https://example.com/").unwrap(),
    };
    let pre = || MessageEntityKind::Pre { language: None };
    // Code and pre hold nothing.
    for inner in [Bold, Code, Blockquote, link()] {
        assert!(!holds(&Code, &inner));
        assert!(!holds(&pre(), &inner));
    }
    // A style holds anything but itself again; code and pre are cut out of
    // it afterwards.
    assert!(holds(&Bold, &Italic) && holds(&Bold, &Code) && holds(&Bold, &link()));
    assert!(holds(&Bold, &pre()) && holds(&Bold, &Blockquote));
    assert!(!holds(&Bold, &Bold));
    // Links and blockquotes hold styling only.
    for outer in [link(), Blockquote] {
        assert!(holds(&outer, &Italic));
        for inner in [Code, pre(), Blockquote, link()] {
            assert!(!holds(&outer, &inner));
        }
    }
}

#[test]
fn events_that_need_parser_options_are_written_as_they_came() {
    // The parser is not asked for math or footnotes, so only a direct call
    // reaches these.
    for event in [
        Event::InlineMath("a".into()),
        Event::DisplayMath("a".into()),
        Event::FootnoteReference("a".into()),
    ] {
        let mut renderer = Renderer::default();
        renderer.event(event);
        assert_eq!(renderer.out, "a");
        assert_eq!(renderer.units, 1);
    }
}

#[test]
fn offsets_count_utf16_units_so_emoji_and_astral_characters_shift_them() {
    let chunk = &render("😀 **bold** 𝒳 `c`")[0];
    assert_eq!(chunk.text, "😀 bold 𝒳 c");
    let at: Vec<_> = chunk
        .entities
        .iter()
        .map(|e| (e.offset, e.length))
        .collect();
    // 😀 and its space are 3 units, "bold " 5, 𝒳 and its space 3.
    assert_eq!(at, [(3, 4), (11, 1)]);
    assert_eq!(shown(chunk), [pair("bold", "bold"), pair("code", "c")]);
}

#[test]
fn bold_around_code_is_cut_into_pieces_around_it() {
    let (text, entities) = show("**bold `code` bold**");
    assert_eq!(text, "bold code bold");
    assert_eq!(
        entities,
        [
            pair("bold", "bold"),
            pair("code", "code"),
            pair("bold", " bold")
        ]
    );
    // Nothing is left of a span that is all code.
    let (_, entities) = show("**`code`**");
    assert_eq!(entities, [pair("code", "code")]);
    // Several styles around one piece of code.
    let (_, entities) = show("***a `b` c***");
    assert_eq!(
        kinds(&entities),
        ["bold", "bold", "code", "italic", "italic"]
    );
}

#[test]
fn a_link_or_quote_keeps_what_it_may_and_loses_the_code_inside_it() {
    let (text, entities) = show("[`code`](https://example.com/a)");
    assert_eq!(text, "code");
    assert_eq!(entities, [pair("link:https://example.com/a", "code")]);

    // A link in a quote cannot be a text_link, so its address is shown.
    let (text, entities) = show("> see `x` and [l](https://example.com/b)");
    assert_eq!(text, "see x and l (https://example.com/b)");
    assert_eq!(entities, [pair("quote", text.as_str())]);

    let (_, entities) = show("> ```\n> code\n> ```");
    assert_eq!(entities, [pair("quote", "code")]);
    // A quote inside a quote is one quote.
    let (_, entities) = show("> a\n>\n> > b");
    assert_eq!(entities, [pair("quote", "a\n\nb")]);
}

#[test]
fn styles_and_links_nest_both_ways() {
    let (_, entities) = show("[**bold**](https://example.com/c)");
    assert_eq!(
        entities,
        [
            pair("link:https://example.com/c", "bold"),
            pair("bold", "bold")
        ]
    );
    let (_, entities) = show("**[l](https://example.com/d)**");
    assert_eq!(
        entities,
        [pair("bold", "l"), pair("link:https://example.com/d", "l")]
    );
    let (_, entities) = show("***both***");
    assert_eq!(kinds(&entities), ["bold", "italic"]);
}

#[test]
fn stray_markers_are_left_alone() {
    for text in [
        "2 * 3 * 4",
        "snake_case_name and more_snake_case",
        "5 > 3 and 4 < 5",
        "unclosed **bold",
        "unclosed `code",
        "**",
        "a _ b _ c",
        "tilde ~ 5 ~ here",
        "cost: $5 * 2 = $10",
    ] {
        let (shown, entities) = show(text);
        assert_eq!(shown, text);
        assert!(entities.is_empty(), "{text:?}: {entities:?}");
    }
}

#[test]
fn escapes_html_and_breaks_stay_as_written() {
    let (text, entities) = show(r"\*not italic\* and <b>x</b>");
    assert_eq!(text, "*not italic* and <b>x</b>");
    assert!(entities.is_empty());
    let (text, _) = show("<div>\nhi\n</div>\n\nafter");
    assert_eq!(text, "<div>\nhi\n</div>\n\nafter");
    let (text, _) = show("a\nb  \nc");
    assert_eq!(text, "a\nb\nc");
    let (text, _) = show("a\n\n---\n\nb");
    assert_eq!(text, "a\n\n────────────\n\nb");
}

#[test]
fn whitespace_at_the_end_of_an_entity_is_left_out_of_it() {
    let (text, entities) = show("a `x ` b");
    assert_eq!(text, "a x  b");
    assert_eq!(entities, [pair("code", "x")]);
    // A span of only whitespace is dropped.
    let (_, entities) = show("a `  ` b");
    assert!(entities.is_empty());
}

#[test]
fn nothing_visible_is_no_message() {
    assert!(render("").is_empty());
    assert!(render("  \n\n ").is_empty());
    assert!(render("```\n```").is_empty());
    assert_eq!(Chunk::plain("x").entities, []);
}

#[test]
fn a_cut_prefers_a_paragraph_boundary() {
    let chunks = render_within("aaaa aaaa aaaa\n\nbbbb bbbb\ncccc", 20);
    let texts: Vec<_> = chunks.iter().map(|c| c.text.as_str()).collect();
    assert_eq!(texts, ["aaaa aaaa aaaa", "bbbb bbbb\ncccc"]);
}

#[test]
fn an_entity_crossing_a_cut_is_reopened_in_the_next_message() {
    let words = ["word"; 10].join(" ");
    let chunks = render_within(&format!("**{words}**"), 20);
    assert!(chunks.len() >= 3);
    for chunk in &chunks {
        assert_valid(chunk, 20);
        // Every message is wholly bold, counted from its own start.
        assert_eq!(chunk.entities.len(), 1);
        assert_eq!(covered(&chunk.text, &chunk.entities[0]), chunk.text);
    }
    let joined: Vec<_> = chunks.iter().map(|c| c.text.as_str()).collect();
    assert_eq!(joined.join(" "), words);
}

#[test]
fn offsets_are_rebased_per_message_and_survive_astral_text() {
    let text = "😀😀 one two **three four** five six 😀 seven";
    let chunks = render_within(text, 14);
    assert!(chunks.len() > 2);
    let mut bold = String::new();
    for chunk in &chunks {
        assert_valid(chunk, 14);
        for e in &chunk.entities {
            bold.push_str(&covered(&chunk.text, e));
        }
    }
    assert_eq!(squash(&bold), "threefour");
}

/// Lines of code, as the model would write them.
fn code_lines(n: usize) -> String {
    (0..n)
        .map(|i| format!("let value_{i} = compute({i}, \"item\");"))
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn a_ten_thousand_character_reply_keeps_its_code_block_across_the_cut() {
    let code = code_lines(260);
    let reply = format!(
        "Here is the code.\n\n```rust\n{code}\n```\n\nThat is all, {}.",
        "and then some words ".repeat(40)
    );
    assert!(reply.len() > 10_000);
    let chunks = render(&reply);
    assert!(chunks.len() >= 3, "{}", chunks.len());

    let mut pre = Vec::new();
    for chunk in &chunks {
        assert_valid(chunk, MESSAGE_LIMIT);
        for e in &chunk.entities {
            assert_eq!(label(&e.kind), "pre:rust");
            pre.push(covered(&chunk.text, e));
        }
    }
    // The block is reopened in each message it spans, and none of it is
    // lost: its pieces are cut at line breaks.
    assert!(pre.len() >= 2, "{pre:?}");
    assert_eq!(pre.join("\n"), code);
    // The words after it are in the last message, outside the block.
    let last = chunks.last().unwrap();
    assert!(
        last.text.ends_with("and then some words ."),
        "{}",
        last.text
    );
}

#[test]
fn every_reply_renders_to_messages_telegram_accepts() {
    // Every string of up to four of the first twelve pieces, then
    // pseudo-random longer ones, each cut at several sizes.
    let pieces = [
        "*",
        "**",
        "_",
        "`",
        "```\n",
        "~~",
        "[a](https://x.y/z)",
        "[b](#c)",
        "![i](u)",
        "\n",
        "\n\n",
        "> ",
        "- ",
        "1. ",
        "# ",
        "| a | b |\n|---|---|\n| c | d |\n",
        "---",
        "😀",
        "a",
        " ",
        "<b>",
        "\\",
        "<https://q.r/s>",
    ];
    let mut inputs: Vec<String> = Vec::new();
    let mut stack = vec![(String::new(), 0)];
    while let Some((prefix, depth)) = stack.pop() {
        if depth < 4 {
            for piece in &pieces[..12] {
                stack.push((format!("{prefix}{piece}"), depth + 1));
            }
        }
        inputs.push(prefix);
    }
    let mut state = 0x2545_f491_4f6c_dd1du64;
    let mut next = move |n: usize| {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        (state % n as u64) as usize
    };
    for _ in 0..4000 {
        let len = 1 + next(30);
        inputs.push((0..len).map(|_| pieces[next(pieces.len())]).collect());
    }
    assert!(inputs.len() > 20_000);

    let text =
        |chunks: &[Chunk]| squash(&chunks.iter().map(|c| c.text.as_str()).collect::<String>());
    for input in &inputs {
        let whole = render(input);
        for chunk in &whole {
            assert_valid(chunk, MESSAGE_LIMIT);
        }
        for limit in [40, 7] {
            let chunks = render_within(input, limit);
            for chunk in &chunks {
                assert_valid(chunk, limit);
            }
            // Cutting only removes the whitespace it cuts at.
            assert_eq!(text(&chunks), text(&whole), "{input:?} at {limit}");
        }
    }
}
