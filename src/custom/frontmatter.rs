//! The front matter of a `SKILL.md`: the block of YAML between two `---`
//! lines at the top of the file.
//!
//! The Agent Skills specification (https://agentskills.io/specification) uses
//! a small subset of YAML, so this parses that subset and no YAML library is
//! needed:
//!
//! - top-level `key: value` lines, the value a plain scalar, a `"double"` or
//!   `'single'` quoted scalar (a comment may follow it), or a `|` / `>`
//!   block scalar on the lines below. A plain scalar may continue on
//!   indented lines, which are joined with spaces, until a comment line
//!   ends it (text after the comment is an error);
//! - `metadata:` followed by indented `key: value` lines, every value a
//!   string;
//! - full-line `#` comments, indented or not, and blank lines. Inside a
//!   `|` or `>` block scalar a line starting with `#` is content, as in YAML.
//!
//! Anything else on a known key is an error, so a skill the parser cannot
//! read is skipped loudly instead of read wrongly. Unknown keys are ignored,
//! whatever their value looks like. Two deliberate differences from YAML: a
//! `#` inside a value is kept (so `Fix issue #12` stays whole, and
//! `name: x # note` fails the name check instead of silently shrinking), and
//! a plain value may contain `: `. Surrounding whitespace is always trimmed.

use std::collections::{BTreeMap, BTreeSet};

/// The fields the specification defines. A field the file lacks is `None`.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct FrontMatter {
    pub name: Option<String>,
    pub description: Option<String>,
    pub license: Option<String>,
    pub compatibility: Option<String>,
    pub metadata: BTreeMap<String, String>,
    pub allowed_tools: Option<String>,
}

/// Split a `SKILL.md` into its front matter block and its body.
pub fn split(text: &str) -> Result<(&str, &str), String> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut offset = 0;
    let mut start = None;
    for line in text.split_inclusive('\n') {
        let end = offset + line.len();
        match start {
            None if line.trim_end() == "---" => start = Some(end),
            None => break,
            Some(start) if line.trim_end() == "---" => {
                return Ok((&text[start..offset], &text[end..]));
            }
            Some(_) => {}
        }
        offset = end;
    }
    Err(match start {
        None => "the file must start with a --- line that opens the front matter",
        Some(_) => "the front matter is not closed by a --- line",
    }
    .into())
}

/// One top-level key and everything indented under it.
struct Entry<'a> {
    key: &'a str,
    value: &'a str,
    rest: Vec<&'a str>,
}

fn entries(block: &str) -> Result<Vec<Entry<'_>>, String> {
    let mut found: Vec<Entry> = Vec::new();
    for line in block.lines().map(str::trim_end) {
        if line.starts_with('#') {
            continue;
        }
        if line.is_empty() {
            // Blank lines are content inside a block scalar.
            if let Some(last) = found.last_mut() {
                last.rest.push("");
            }
        } else if line.starts_with([' ', '\t']) || line == "-" || line.starts_with("- ") {
            let owner = found
                .last_mut()
                .ok_or("an indented line comes before any key")?;
            owner.rest.push(line);
        } else {
            let (key, value) = key_value(line)?;
            found.push(Entry {
                key,
                value,
                rest: Vec::new(),
            });
        }
    }
    let mut seen = BTreeSet::new();
    for entry in &found {
        if !seen.insert(entry.key) {
            return Err(format!("`{}` appears twice", entry.key));
        }
    }
    Ok(found)
}

fn key_value(line: &str) -> Result<(&str, &str), String> {
    let (key, value) = match line.split_once(": ") {
        Some(pair) => pair,
        None => (
            line.strip_suffix(':')
                .ok_or_else(|| format!("`{line}` is not `key: value`"))?,
            "",
        ),
    };
    match key.trim() {
        "" => Err(format!("`{line}` has no key")),
        key => Ok((key, value.trim())),
    }
}

/// Read the front matter block (the text between the `---` lines).
pub fn parse(block: &str) -> Result<FrontMatter, String> {
    let mut front = FrontMatter::default();
    for Entry { key, value, rest } in entries(block)? {
        let field = match key {
            "name" => &mut front.name,
            "description" => &mut front.description,
            "license" => &mut front.license,
            "compatibility" => &mut front.compatibility,
            "allowed-tools" => &mut front.allowed_tools,
            "metadata" => {
                front.metadata = string_map(value, &rest)?;
                continue;
            }
            _ => continue,
        };
        *field = Some(scalar(key, value, &rest)?);
    }
    Ok(front)
}

fn scalar(key: &str, value: &str, rest: &[&str]) -> Result<String, String> {
    let text = match value.chars().next() {
        Some('|') => block_scalar(key, value, rest, true)?,
        Some('>') => block_scalar(key, value, rest, false)?,
        Some(quote @ ('"' | '\'')) => {
            if rest
                .iter()
                .any(|line| !line.is_empty() && !is_comment(line))
            {
                return Err(format!("`{key}`: a quoted value must stay on one line"));
            }
            quoted(key, value, quote)?
        }
        _ => plain(key, value, rest)?,
    };
    Ok(text.trim().to_string())
}

/// Whether the first character of `line` after its indentation is `#`: a
/// full-line comment. (Not inside a block scalar, where it is content.)
fn is_comment(line: &str) -> bool {
    line.trim_start().starts_with('#')
}

/// A plain scalar: `value` and the indented lines that continue it, joined
/// with spaces. As in YAML, a comment line ends the scalar, and text after
/// it is refused. A comment before the first text (`key:` then a comment
/// then the value) ends nothing.
fn plain(key: &str, value: &str, rest: &[&str]) -> Result<String, String> {
    let mut parts = vec![value];
    let mut ended = false;
    for line in rest {
        let text = line.trim();
        if is_comment(line) {
            ended = parts.iter().any(|part| !part.is_empty());
        } else if !text.is_empty() {
            if ended {
                return Err(format!("`{key}`: text after a comment; put it before"));
            }
            parts.push(text);
        }
    }
    parts.retain(|part| !part.is_empty());
    Ok(parts.join(" "))
}

/// A `|` (line breaks kept) or `>` (the lines of a paragraph joined with
/// spaces, paragraphs separated by a line break) scalar. The chomping and
/// indentation indicators after the `|` or `>` are accepted and ignored: the
/// result is trimmed anyway.
fn block_scalar(key: &str, header: &str, rest: &[&str], literal: bool) -> Result<String, String> {
    if !header[1..]
        .chars()
        .all(|c| matches!(c, '-' | '+' | '0'..='9'))
    {
        return Err(format!("`{key}`: `{header}` is not a block scalar header"));
    }
    let margin = rest
        .iter()
        .filter(|line| !line.is_empty())
        .map(|line| indent(line))
        .min()
        .unwrap_or(0);
    let lines: Vec<&str> = rest.iter().map(|l| l.get(margin..).unwrap_or("")).collect();
    Ok(if literal {
        lines.join("\n")
    } else {
        lines
            .split(|line| line.is_empty())
            .filter(|paragraph| !paragraph.is_empty())
            .map(|paragraph| paragraph.join(" "))
            .collect::<Vec<_>>()
            .join("\n")
    })
}

/// How many spaces and tabs start `line`. Only those count as indentation
/// (as in YAML), so the margin it gives is always a character boundary.
fn indent(line: &str) -> usize {
    line.len() - line.trim_start_matches([' ', '\t']).len()
}

fn quoted(key: &str, value: &str, quote: char) -> Result<String, String> {
    let mut out = String::new();
    let mut chars = value[1..].chars();
    while let Some(c) = chars.next() {
        match (c, quote) {
            ('\\', '"') => out.push(match chars.next() {
                Some('n') => '\n',
                Some('t') => '\t',
                Some(c @ ('"' | '\\' | '/')) => c,
                other => {
                    return Err(format!(
                        "`{key}`: unsupported escape `\\{}`",
                        other.map(String::from).unwrap_or_default()
                    ));
                }
            }),
            ('\'', '\'') if chars.as_str().starts_with('\'') => {
                chars.next();
                out.push('\'');
            }
            (c, quote) if c == quote => {
                // Only a comment may follow the closing quote.
                return match chars.as_str().trim() {
                    "" => Ok(out),
                    after if after.starts_with('#') => Ok(out),
                    after => Err(format!("`{key}`: unexpected `{after}` after the quotes")),
                };
            }
            (c, _) => out.push(c),
        }
    }
    Err(format!("`{key}`: the quote is never closed"))
}

/// `metadata`: indented `key: value` lines, every value a string.
fn string_map(value: &str, rest: &[&str]) -> Result<BTreeMap<String, String>, String> {
    let mut map = BTreeMap::new();
    if !matches!(value, "" | "{}") {
        return Err("`metadata` must be a list of indented `key: value` lines".into());
    }
    let lines: Vec<&str> = rest
        .iter()
        .copied()
        .filter(|line| !line.trim().is_empty() && !line.trim_start().starts_with('#'))
        .collect();
    let margin = lines.first().map_or(0, |line| indent(line));
    for line in lines {
        if indent(line) != margin {
            return Err(format!(
                "`metadata`: `{}` is nested, but every value must be a string",
                line.trim()
            ));
        }
        let (key, value) = key_value(line.trim())?;
        if map
            .insert(key.to_string(), scalar(key, value, &[])?)
            .is_some()
        {
            return Err(format!("`metadata` has `{key}` twice"));
        }
    }
    Ok(map)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn front(block: &str) -> FrontMatter {
        parse(block).unwrap()
    }

    fn error(block: &str) -> String {
        parse(block).unwrap_err()
    }

    #[test]
    fn a_file_splits_into_front_matter_and_body() {
        let (block, body) = split("---\nname: a\n---\n# Body\n\ntext\n").unwrap();
        assert_eq!((block, body), ("name: a\n", "# Body\n\ntext\n"));
    }

    #[test]
    fn a_byte_order_mark_and_windows_line_endings_are_accepted() {
        let (block, body) = split("\u{feff}---\r\nname: a\r\n---\r\nbody\r\n").unwrap();
        assert_eq!(front(block).name.as_deref(), Some("a"));
        assert_eq!(body, "body\r\n");
    }

    #[test]
    fn a_body_may_hold_its_own_rules_and_may_be_absent() {
        assert_eq!(split("---\na: b\n---\nx\n---\ny").unwrap().1, "x\n---\ny");
        assert_eq!(split("---\na: b\n---").unwrap(), ("a: b\n", ""));
    }

    #[test]
    fn a_file_without_closed_front_matter_is_refused() {
        for text in [
            "",
            "name: a\n---\n",
            "\n---\nname: a\n---\n",
            "--- x\n---\n",
        ] {
            assert!(split(text).unwrap_err().contains("must start"), "{text:?}");
        }
        assert!(split("---\nname: a\n").unwrap_err().contains("not closed"));
    }

    #[test]
    fn every_specified_field_is_read() {
        let got = front(
            "name: pdf-processing\n\
             description: Extracts text. Use for PDFs.\n\
             license: Apache-2.0\n\
             compatibility: Requires git and jq\n\
             allowed-tools: Bash(git:*) Read\n\
             metadata:\n  author: example-org\n  version: \"1.0\"\n",
        );
        assert_eq!(
            got,
            FrontMatter {
                name: Some("pdf-processing".into()),
                description: Some("Extracts text. Use for PDFs.".into()),
                license: Some("Apache-2.0".into()),
                compatibility: Some("Requires git and jq".into()),
                metadata: BTreeMap::from([
                    ("author".into(), "example-org".into()),
                    ("version".into(), "1.0".into())
                ]),
                allowed_tools: Some("Bash(git:*) Read".into()),
            }
        );
    }

    #[test]
    fn unknown_keys_are_ignored_whatever_their_shape() {
        let got = front(
            "name: a\nowner: me\nversion: 3\ntags:\n  - x\n  - y\nhooks:\n  pre:\n    run: z\n\
             list:\n- one\n- two\n-\nweird: [1, {2: 3}]\ndescription: d\n",
        );
        assert_eq!(
            (got.name.as_deref(), got.description.as_deref()),
            (Some("a"), Some("d"))
        );
    }

    #[test]
    fn quoted_scalars_keep_their_punctuation() {
        assert_eq!(front("name: \"a: b # c\"").name.unwrap(), "a: b # c");
        assert_eq!(front("name: 'it''s'").name.unwrap(), "it's");
        assert_eq!(
            front("name: \"q\\\"b\\\\s\\/\\n\\t.\"  ").name.unwrap(),
            "q\"b\\s/\n\t."
        );
        assert_eq!(front("name: ''").name.unwrap(), "");
    }

    #[test]
    fn a_comment_may_follow_a_quoted_value() {
        assert_eq!(front("name: \"a\" # why").name.unwrap(), "a");
        assert_eq!(front("name: 'a'#x").name.unwrap(), "a");
    }

    #[test]
    fn block_scalar_content_is_never_dropped_for_odd_whitespace() {
        // Only spaces and tabs indent; a no-break space is content.
        let got = front("description: |\n  a\n \u{a0}b\n");
        assert_eq!(got.description.unwrap(), "a\n\u{a0}b");
        let got = front("description: |\n  é\n  ü\n");
        assert_eq!(got.description.unwrap(), "é\nü");
    }

    #[test]
    fn bad_quotes_are_refused() {
        assert!(error("name: \"open").contains("never closed"));
        assert!(error("name: 'open").contains("never closed"));
        assert!(error("name: \"a\" b").contains("unexpected `b`"));
        assert!(error("name: \"a\\x\"").contains("unsupported escape `\\x`"));
        assert!(error("name: \"a\\").contains("unsupported escape `\\`"));
        assert!(error("name: \"a\"\n  more").contains("one line"));
    }

    #[test]
    fn plain_values_keep_hashes_and_colons_and_continue_on_indented_lines() {
        assert_eq!(
            front("description: Fix issue #12: now # ok")
                .description
                .unwrap(),
            "Fix issue #12: now # ok"
        );
        assert_eq!(
            front("description: one\n  two\n\n  three\nname: n")
                .description
                .unwrap(),
            "one two three"
        );
        assert_eq!(
            front("description:\n  starts below").description.unwrap(),
            "starts below"
        );
        assert_eq!(front("name:").name.unwrap(), "");
    }

    #[test]
    fn folded_and_literal_block_scalars_are_read() {
        let folded =
            front("description: >-\n  first line\n  same paragraph\n\n  next paragraph\nname: n");
        assert_eq!(
            folded.description.unwrap(),
            "first line same paragraph\nnext paragraph"
        );
        let literal = front("description: |\n    a\n      b\n\n    c\n");
        assert_eq!(literal.description.unwrap(), "a\n  b\n\nc");
    }

    #[test]
    fn block_scalar_headers_are_checked() {
        assert!(error("description: >>").contains("block scalar header"));
        assert!(error("description: |x").contains("block scalar header"));
        assert_eq!(front("description: |2+\n  a").description.unwrap(), "a");
        assert_eq!(front("description: |").description.unwrap(), "");
    }

    #[test]
    fn comments_and_blank_lines_are_skipped() {
        let got = front("# top\n\nname: a\n# middle\n\n\ndescription: b\n");
        assert_eq!(
            (got.name.unwrap(), got.description.unwrap()),
            ("a".into(), "b".into())
        );
    }

    #[test]
    fn an_indented_comment_between_keys_is_ignored() {
        let got = front("name: foo\n  # why\ndescription: bar\n\t# tab\nlicense: MIT\n");
        assert_eq!(got.name.as_deref(), Some("foo"));
        assert_eq!(got.description.as_deref(), Some("bar"));
        assert_eq!(got.license.as_deref(), Some("MIT"));
    }

    #[test]
    fn an_indented_comment_ends_a_plain_value_as_in_yaml() {
        // A comment before the value's first line does not end anything.
        assert_eq!(front("name:\n  # c\n  text").name.unwrap(), "text");
        // After the value has begun, the comment ends it...
        assert_eq!(front("name: a\n  b\n  # c\n\n").name.unwrap(), "a b");
        // ...so more text after it is refused rather than guessed at.
        assert!(error("name: a\n  # c\n  b").contains("after a comment"));
        // A blank line alone is not a comment.
        assert_eq!(front("name: a\n\n  b").name.unwrap(), "a b");
    }

    #[test]
    fn an_indented_comment_after_a_quoted_value_is_ignored() {
        assert_eq!(front("name: \"a\"\n  # c\n").name.unwrap(), "a");
        assert!(error("name: \"a\"\n  # c\n  b").contains("one line"));
    }

    #[test]
    fn an_indented_comment_inside_metadata_is_ignored() {
        let got = front("metadata:\n  a: 1\n    # deeper\n  # same\n\t# tab\n  b: 2\n");
        assert_eq!(
            got.metadata,
            BTreeMap::from([("a".into(), "1".into()), ("b".into(), "2".into())])
        );
    }

    #[test]
    fn a_hash_line_inside_a_block_scalar_is_content_not_a_comment() {
        let got = front("description: |\n  one\n  # two\n    # three\n  four\nname: n");
        assert_eq!(got.description.unwrap(), "one\n# two\n  # three\nfour");
        let folded = front("description: >\n  one\n  # two\n");
        assert_eq!(folded.description.unwrap(), "one # two");
        assert_eq!(folded.name, None);
    }

    #[test]
    fn hashes_inside_values_are_still_kept() {
        assert_eq!(front("name: Fix issue #12").name.unwrap(), "Fix issue #12");
        assert_eq!(
            front("name: a\n  Fix #12 now").name.unwrap(),
            "a Fix #12 now"
        );
    }

    #[test]
    fn metadata_is_a_map_of_strings() {
        assert!(front("metadata: {}").metadata.is_empty());
        assert!(front("metadata:").metadata.is_empty());
        let got = front("metadata:\n  # note\n  a: 1\n  'b': two words\n  c:\nname: n");
        assert_eq!(
            got.metadata,
            BTreeMap::from([
                ("a".into(), "1".into()),
                ("'b'".into(), "two words".into()),
                ("c".into(), "".into()),
            ])
        );
    }

    #[test]
    fn malformed_metadata_is_refused() {
        assert!(error("metadata: text").contains("indented"));
        assert!(error("metadata:\n  a:\n    b: 1").contains("nested"));
        assert!(error("metadata:\n  a: 1\n  a: 2").contains("`a` twice"));
        assert!(error("metadata:\n  not a pair").contains("not `key: value`"));
    }

    #[test]
    fn structural_mistakes_are_refused() {
        assert!(error("name: a\nname: b").contains("`name` appears twice"));
        assert!(error("owner: a\nowner: b").contains("`owner` appears twice"));
        assert!(error("  indented: first").contains("before any key"));
        assert!(error("just text").contains("not `key: value`"));
        assert!(error(": value").contains("no key"));
    }
}
