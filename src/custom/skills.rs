//! Agent Skills (https://agentskills.io/specification): a directory per
//! skill, each holding a `SKILL.md` with `name` and `description` front
//! matter and instructions below it.
//!
//! [`Skills::load`] reads the skills directory once, at startup, and keeps
//! what it accepted in memory. The model sees each skill's name and
//! description in its system prompt ([`Skills::listing`]) and fetches the
//! instructions with the `read_skill` tool ([`ReadSkill`]) when one applies.
//!
//! Only `SKILL.md` is read. A skill's `scripts/`, `references/` and `assets/`
//! are not served and not copied anywhere.
//!
//! Skills are local files the owner put there. They are as trusted as the
//! owner's own instructions, and there is no registry and no fetching.
//!
//! `read_skill` can reach only what `load` accepted: it looks the name up in
//! that set and never builds a path from it. `load` refuses any `SKILL.md`
//! whose real path (symlinks resolved) is outside the skills directory.

use super::frontmatter::{self, FrontMatter};
use super::read_limited;
use rig_agent::tool::{Tool, ToolContext, ToolExecutionError};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::sync::Arc;

/// The specification's limits.
pub const MAX_NAME_CHARS: usize = 64;
pub const MAX_DESCRIPTION_CHARS: usize = 1024;
pub const MAX_COMPATIBILITY_CHARS: usize = 500;
/// The largest `SKILL.md`, front matter included. The specification
/// recommends under 5 000 tokens and 500 lines; this is the hard stop.
pub const MAX_SKILL_BYTES: usize = 64 * 1024;
/// The most the skills listing adds to every prompt, about 4 000 tokens.
/// Skills that do not fit are not loaded.
pub const MAX_LISTING_BYTES: usize = 16 * 1024;
/// The most skills loaded, which also bounds the memory their bodies take
/// (at most [`MAX_SKILLS`] times [`MAX_SKILL_BYTES`]).
pub const MAX_SKILLS: usize = 64;
/// The most entries of the skills directory examined, of any kind (files and
/// links count), so a huge directory cannot stall startup, fill memory or
/// flood the log. The scan stops there: it does not list the rest.
pub const MAX_ENTRIES: usize = 256;

#[derive(Debug, PartialEq, Eq)]
pub struct Skill {
    pub name: String,
    /// One line: the specification's description with its whitespace
    /// collapsed, so it cannot break the listing's layout.
    pub description: String,
    /// What follows the front matter.
    pub body: String,
}

/// The skills that passed validation, by name.
#[derive(Debug, Default)]
pub struct Skills(BTreeMap<String, Skill>);

impl Skills {
    /// Load every skill under `dir`. A skill that fails validation is left
    /// out and `warnings` says why; nothing here is fatal.
    pub fn load(dir: &Path, warnings: &mut Vec<String>) -> Self {
        let mut skills = Self::default();
        let root = match fs::canonicalize(dir) {
            Ok(root) => root,
            Err(e) => {
                warnings.push(format!("skills directory {}: {e}", dir.display()));
                return skills;
            }
        };
        let (names, truncated) = match directory_names(&root) {
            Ok(found) => found,
            Err(e) => {
                warnings.push(format!("skills directory {}: {e}", dir.display()));
                return skills;
            }
        };
        if truncated {
            warnings.push(format!(
                "the skills directory has more than {MAX_ENTRIES} entries; the rest were \
                 not examined (the file system decides which ones, so it can differ \
                 between starts)"
            ));
        }
        let mut listed = 0;
        for name in names {
            let skill = match read(&root, &name) {
                Ok(skill) => skill,
                Err(why) => {
                    warnings.push(format!("skill `{name}` skipped: {why}"));
                    continue;
                }
            };
            listed += skill.listing_line().len() + 1;
            if listed > MAX_LISTING_BYTES || skills.0.len() == MAX_SKILLS {
                warnings.push(format!(
                    "skill `{name}` and any skills after it were not loaded: the limit is \
                     {MAX_SKILLS} skills and {MAX_LISTING_BYTES} bytes of listing"
                ));
                break;
            }
            skills.0.insert(skill.name.clone(), skill);
        }
        skills
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn get(&self, name: &str) -> Option<&Skill> {
        self.0.get(name)
    }

    /// One `- name: description` line per skill, in name order.
    pub fn listing(&self) -> String {
        let lines: Vec<String> = self.0.values().map(Skill::listing_line).collect();
        lines.join("\n")
    }

    fn names(&self) -> Vec<&str> {
        self.0.keys().map(String::as_str).collect()
    }
}

impl Skill {
    fn listing_line(&self) -> String {
        format!("- {}: {}", self.name, self.description)
    }
}

/// The entries of `root` that could be skills (see [`scan`]), and whether
/// there were more entries than [`MAX_ENTRIES`].
fn directory_names(root: &Path) -> std::io::Result<(Vec<String>, bool)> {
    scan(
        fs::read_dir(root)?.map(|entry| entry.map(|e| e.path())),
        MAX_ENTRIES,
    )
}

/// The names among the first `cap` of `entries` that could be skills:
/// directories (symlinks to directories included), not hidden, in name
/// order. A stray file such as a README is not a skill and is left alone.
/// Every entry looked at counts toward `cap`, whatever it is, and `entries`
/// is not read past one entry beyond it, so time and memory are bounded by
/// `cap`. Directory order is up to the file system, so which entries are
/// examined past the cap is not defined. The `bool` is true when entries
/// were left unexamined.
fn scan(
    entries: impl Iterator<Item = std::io::Result<std::path::PathBuf>>,
    cap: usize,
) -> std::io::Result<(Vec<String>, bool)> {
    let mut names = Vec::new();
    for (examined, entry) in entries.enumerate() {
        if examined == cap {
            names.sort();
            return Ok((names, true));
        }
        let path = entry?;
        // A name that is not UTF-8 cannot be a skill's. `is_dir` follows
        // symlinks, and a broken one is not a directory.
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .map(str::to_string);
        if let Some(name) = name.filter(|n| !n.starts_with('.') && path.is_dir()) {
            names.push(name);
        }
    }
    names.sort();
    Ok((names, false))
}

/// The skill in directory `dir` of `root`, or why it is not one.
fn read(root: &Path, dir: &str) -> Result<Skill, String> {
    let file = match fs::canonicalize(root.join(dir).join("SKILL.md")) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err("it has no SKILL.md".into());
        }
        Err(e) => return Err(e.to_string()),
    };
    if !file.starts_with(root) {
        return Err("SKILL.md is a link to a file outside the skills directory".into());
    }
    let text = read_limited(&file, MAX_SKILL_BYTES).map_err(|why| format!("SKILL.md: {why}"))?;
    let (block, body) = frontmatter::split(&text)?;
    let front = frontmatter::parse(block)?;
    validate(&front, dir)?;
    Ok(Skill {
        name: front.name.unwrap_or_default(),
        description: front
            .description
            .unwrap_or_default()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" "),
        body: clean_body(body),
    })
}

/// `body` without the blank lines before its first text and the whitespace
/// after its last. The first text line keeps its indentation: in Markdown,
/// four spaces make a code block, and `    # x` is code, not a heading.
fn clean_body(body: &str) -> String {
    let blank_prefix: usize = body
        .split_inclusive('\n')
        .take_while(|line| line.trim().is_empty())
        .map(str::len)
        .sum();
    body[blank_prefix..].trim_end().to_string()
}

/// The specification's rules for the fields it defines.
fn validate(front: &FrontMatter, dir: &str) -> Result<(), String> {
    let name = front.name.as_deref().ok_or("`name` is missing")?;
    let description = front
        .description
        .as_deref()
        .ok_or("`description` is missing")?;
    check_name(name)?;
    if name != dir {
        return Err(format!(
            "`name` is `{name}` but the directory is `{dir}`; they must match"
        ));
    }
    let length = description.chars().count();
    if !(1..=MAX_DESCRIPTION_CHARS).contains(&length) {
        return Err(format!(
            "`description` is {length} characters; it must be 1 to {MAX_DESCRIPTION_CHARS}"
        ));
    }
    if let Some(compatibility) = &front.compatibility {
        let length = compatibility.chars().count();
        if !(1..=MAX_COMPATIBILITY_CHARS).contains(&length) {
            return Err(format!(
                "`compatibility` is {length} characters; it must be 1 to {MAX_COMPATIBILITY_CHARS}"
            ));
        }
    }
    Ok(())
}

/// 1 to 64 lowercase letters, digits and hyphens, with no hyphen at either
/// end or next to another. Letters are Unicode, as in the specification.
fn check_name(name: &str) -> Result<(), String> {
    let length = name.chars().count();
    if !(1..=MAX_NAME_CHARS).contains(&length) {
        return Err(format!(
            "`name` is {length} characters; it must be 1 to {MAX_NAME_CHARS}"
        ));
    }
    let lowercase = |c: char| c.to_lowercase().eq(std::iter::once(c));
    if !name
        .chars()
        .all(|c| c == '-' || (c.is_alphanumeric() && lowercase(c)))
    {
        return Err(format!(
            "`name` `{name}` may hold only lowercase letters, digits and hyphens"
        ));
    }
    if name.starts_with('-') || name.ends_with('-') || name.contains("--") {
        return Err(format!(
            "`name` `{name}` must not start or end with a hyphen or hold two in a row"
        ));
    }
    Ok(())
}

#[derive(Debug, Deserialize)]
pub struct ReadSkillArgs {
    pub name: String,
}

/// `read_skill(name)`: the instructions of one of the listed skills.
pub struct ReadSkill(pub Arc<Skills>);

impl Tool for ReadSkill {
    const NAME: &'static str = "read_skill";
    type Args = ReadSkillArgs;
    type Output = String;
    type Error = ToolExecutionError;

    fn description(&self) -> String {
        "Return the instructions (the SKILL.md) of one of the skills listed in your system \
         prompt. Files a skill mentions, such as scripts or references, are not available \
         through this tool. Call it before starting a task a skill covers, then follow \
         what it says."
            .into()
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "name": {"type": "string", "description": "A skill name from the list"}
            },
            "required": ["name"]
        })
    }

    async fn call(&self, _: &mut ToolContext, args: ReadSkillArgs) -> Result<String, Self::Error> {
        match self.0.get(&args.name) {
            Some(skill) if skill.body.is_empty() => {
                Ok("This skill has no instructions beyond its description.".into())
            }
            Some(skill) => Ok(skill.body.clone()),
            None => Err(ToolExecutionError::invalid_args(format!(
                "There is no skill named `{}`. The skills are: {}.",
                args.name,
                self.0.names().join(", ")
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::testing::TempDir;
    use super::*;
    use std::os::unix::fs::symlink;

    fn load(dir: &TempDir) -> (Skills, Vec<String>) {
        let mut warnings = Vec::new();
        let skills = Skills::load(&dir.join("skills"), &mut warnings);
        (skills, warnings)
    }

    /// The skill `name` with this front matter (a block of lines) and no body.
    fn with_front(dir: &TempDir, name: &str, front: &str) {
        dir.write(
            &format!("skills/{name}/SKILL.md"),
            &format!("---\n{front}\n---\n"),
        );
    }

    fn only_warning(warnings: &[String]) -> &str {
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        &warnings[0]
    }

    async fn read_skill(skills: Skills, name: &str) -> Result<String, ToolExecutionError> {
        let tool = ReadSkill(Arc::new(skills));
        let args = ReadSkillArgs { name: name.into() };
        tool.call(&mut ToolContext::new(), args).await
    }

    #[test]
    fn skills_load_with_name_description_and_body() {
        let dir = TempDir::new();
        dir.skill("pdf-tools", "Work with PDFs.", "# Steps\n\n1. Open it.");
        dir.skill("data-analysis", "Analyse data.", "Plot it.");
        let (skills, warnings) = load(&dir);
        assert!(warnings.is_empty(), "{warnings:?}");
        assert!(!skills.is_empty());
        assert_eq!(
            skills.get("pdf-tools"),
            Some(&Skill {
                name: "pdf-tools".into(),
                description: "Work with PDFs.".into(),
                body: "# Steps\n\n1. Open it.".into(),
            })
        );
        assert_eq!(skills.get("data-analysis").unwrap().body, "Plot it.");
        assert!(skills.get("other").is_none());
        assert_eq!(
            skills.listing(),
            "- data-analysis: Analyse data.\n- pdf-tools: Work with PDFs."
        );
    }

    #[test]
    fn every_other_field_of_the_specification_is_accepted_and_unknown_ones_ignored() {
        let dir = TempDir::new();
        with_front(
            &dir,
            "full",
            "name: full\ndescription: >-\n  Folds onto\n  one line.\nlicense: MIT\n\
             compatibility: Needs git\nallowed-tools: Bash(git:*) Read\n\
             metadata:\n  author: me\n  version: \"1.0\"\nx-vendor:\n  nested: [1, 2]\ntags:\n- a\n",
        );
        let (skills, warnings) = load(&dir);
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(
            skills.get("full").unwrap().description,
            "Folds onto one line."
        );
    }

    /// The body `load` makes of a `SKILL.md` whose text after the front
    /// matter's closing `---` line is `after`.
    fn body_after(after: &str) -> String {
        let dir = TempDir::new();
        let text = format!("---\nname: b\ndescription: d\n---\n{after}");
        dir.write("skills/b/SKILL.md", &text);
        let (skills, warnings) = load(&dir);
        assert!(warnings.is_empty(), "{warnings:?}");
        skills.get("b").unwrap().body.clone()
    }

    #[test]
    fn a_body_that_starts_with_an_indented_code_block_keeps_its_indentation() {
        let after = "    fn main() {\n        run();\n    }\n\ntext\n";
        assert_eq!(
            body_after(after),
            "    fn main() {\n        run();\n    }\n\ntext"
        );
        // Indentation makes it code, not a heading.
        assert_eq!(
            body_after("    # not a heading\n    more\n"),
            "    # not a heading\n    more"
        );
        assert_eq!(body_after("\tindented by a tab\n"), "\tindented by a tab");
    }

    #[test]
    fn blank_lines_after_the_front_matter_are_removed_and_trailing_space_too() {
        assert_eq!(
            body_after("\n\n  \n\t\n# Title\n\nbody\n\n  \n"),
            "# Title\n\nbody"
        );
        assert_eq!(body_after("\n\n   indented first\n"), "   indented first");
        assert_eq!(body_after("text   \n"), "text");
    }

    #[test]
    fn crlf_bodies_lose_blank_lines_and_keep_indentation() {
        let dir = TempDir::new();
        dir.write(
            "skills/b/SKILL.md",
            "---\r\nname: b\r\ndescription: d\r\n---\r\n\r\n\r\n    code\r\n    more\r\n\r\n",
        );
        let (skills, warnings) = load(&dir);
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(skills.get("b").unwrap().body, "    code\r\n    more");
    }

    #[test]
    fn a_body_of_only_whitespace_is_empty() {
        assert_eq!(body_after(""), "");
        assert_eq!(body_after("  \n\t\n\n   "), "");
        assert_eq!(body_after("\r\n \r\n"), "");
    }

    #[test]
    fn a_skill_with_no_body_is_valid() {
        let dir = TempDir::new();
        with_front(&dir, "bare", "name: bare\ndescription: Just metadata.");
        let (skills, warnings) = load(&dir);
        assert!(warnings.is_empty());
        assert_eq!(skills.get("bare").unwrap().body, "");
    }

    #[test]
    fn names_follow_the_specification() {
        for good in [
            "a",
            "pdf-processing",
            "v2",
            "a-1-b",
            "café",
            &"a".repeat(64),
        ] {
            assert_eq!(check_name(good), Ok(()), "{good}");
        }
        for (bad, why) in [
            ("", "0 characters"),
            ("PDF", "lowercase"),
            ("Pdf-processing", "lowercase"),
            ("pdf_processing", "lowercase"),
            ("pdf processing", "lowercase"),
            ("pdf/x", "lowercase"),
            ("..", "lowercase"),
            ("-pdf", "hyphen"),
            ("pdf-", "hyphen"),
            ("pdf--tools", "hyphen"),
            (&"a".repeat(65), "65 characters"),
        ] {
            assert!(check_name(bad).unwrap_err().contains(why), "{bad}");
        }
    }

    #[test]
    fn the_name_must_match_the_directory_and_both_fields_are_required() {
        let dir = TempDir::new();
        with_front(&dir, "dir-a", "name: other\ndescription: d");
        with_front(&dir, "dir-b", "description: no name");
        with_front(&dir, "dir-c", "name: dir-c");
        with_front(&dir, "dir-d", "name: dir-d\ndescription:");
        with_front(&dir, "dir-e", "name: dir-e\ndescription: x\n  \n");
        let (skills, warnings) = load(&dir);
        assert_eq!(skills.listing(), "- dir-e: x");
        assert_eq!(warnings.len(), 4, "{warnings:?}");
        assert!(
            warnings[0].contains("skill `dir-a` skipped") && warnings[0].contains("must match")
        );
        assert!(warnings[1].contains("`dir-b`") && warnings[1].contains("`name` is missing"));
        assert!(
            warnings[2].contains("`dir-c`") && warnings[2].contains("`description` is missing")
        );
        assert!(warnings[3].contains("`dir-d`") && warnings[3].contains("0 characters"));
    }

    #[test]
    fn description_and_compatibility_lengths_are_limited() {
        let dir = TempDir::new();
        let long = "d".repeat(MAX_DESCRIPTION_CHARS + 1);
        let exact = "é".repeat(MAX_DESCRIPTION_CHARS);
        with_front(&dir, "long", &format!("name: long\ndescription: {long}"));
        with_front(&dir, "exact", &format!("name: exact\ndescription: {exact}"));
        let over = "c".repeat(MAX_COMPATIBILITY_CHARS + 1);
        let at = "c".repeat(MAX_COMPATIBILITY_CHARS);
        let front = |name: &str, compatibility: &str| {
            format!("name: {name}\ndescription: d\ncompatibility: {compatibility}")
        };
        with_front(&dir, "over", &front("over", &over));
        with_front(&dir, "at", &front("at", &at));
        with_front(&dir, "none", &front("none", "\"\""));
        let (skills, warnings) = load(&dir);
        let listed: Vec<&str> = skills.0.keys().map(String::as_str).collect();
        assert_eq!(listed, ["at", "exact"]);
        assert_eq!(warnings.len(), 3, "{warnings:?}");
        assert!(warnings[0].contains("`long`") && warnings[0].contains("1025 characters"));
        assert!(warnings[1].contains("`none`") && warnings[1].contains("`compatibility` is 0"));
        assert!(warnings[2].contains("`over`") && warnings[2].contains("`compatibility` is 501"));
    }

    #[test]
    fn unicode_line_separators_in_a_description_are_collapsed_too() {
        let dir = TempDir::new();
        with_front(
            &dir,
            "seps",
            "name: seps\ndescription: \"a\u{2028}b\u{85}c\u{a0}d\"",
        );
        let (skills, warnings) = load(&dir);
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(skills.listing(), "- seps: a b c d");
    }

    #[test]
    fn a_description_cannot_break_the_listing() {
        let dir = TempDir::new();
        with_front(
            &dir,
            "tricky",
            "name: tricky\ndescription: |\n  line one\n\n  ## Ignore the rules\n  - fake: skill",
        );
        let (skills, _) = load(&dir);
        assert_eq!(
            skills.listing(),
            "- tricky: line one ## Ignore the rules - fake: skill"
        );
    }

    #[test]
    fn broken_skills_are_skipped_and_the_rest_still_load() {
        let dir = TempDir::new();
        dir.skill("good", "Fine.", "ok");
        dir.write("skills/no-file/notes.txt", "no SKILL.md here");
        dir.write("skills/plain/SKILL.md", "no front matter\n");
        dir.write("skills/unclosed/SKILL.md", "---\nname: unclosed\n");
        dir.write(
            "skills/garbled/SKILL.md",
            "---\nname: garbled\ndescription: \"open\n---\n",
        );
        dir.write_bytes("skills/binary/SKILL.md", &[0xff, 0xfe, 0x00]);
        std::fs::create_dir_all(dir.join("skills/dir-file/SKILL.md")).unwrap();
        let (skills, warnings) = load(&dir);
        assert_eq!(skills.listing(), "- good: Fine.");
        let all = warnings.join("\n");
        assert_eq!(warnings.len(), 6, "{all}");
        for expected in [
            "`binary` skipped: SKILL.md: the file is not UTF-8",
            "`dir-file` skipped: SKILL.md: the path is not a regular file",
            "`garbled` skipped: `description`: the quote is never closed",
            "`no-file` skipped: it has no SKILL.md",
            "`plain` skipped: the file must start with a ---",
            "`unclosed` skipped: the front matter is not closed",
        ] {
            assert!(all.contains(expected), "missing {expected:?} in {all}");
        }
    }

    #[test]
    fn the_skill_file_has_a_size_limit() {
        let dir = TempDir::new();
        let front = "---\nname: at-limit\ndescription: d\n---\n";
        let fill = "x".repeat(MAX_SKILL_BYTES - front.len());
        dir.write("skills/at-limit/SKILL.md", &format!("{front}{fill}"));
        dir.write(
            "skills/too-big/SKILL.md",
            &format!(
                "---\nname: too-big\ndescription: d\n---\n{}",
                "x".repeat(MAX_SKILL_BYTES)
            ),
        );
        let (skills, warnings) = load(&dir);
        assert_eq!(skills.get("at-limit").unwrap().body.len(), fill.len());
        assert!(skills.get("too-big").is_none());
        let warning = only_warning(&warnings);
        assert!(warning.contains("`too-big`") && warning.contains("over 65536 bytes"));
    }

    #[test]
    fn the_listing_has_a_size_limit_and_later_skills_are_not_loaded() {
        let dir = TempDir::new();
        let description = "d".repeat(MAX_DESCRIPTION_CHARS);
        // Each line is 1 024 + the name + 4 bytes, so fifteen fit in 16 KiB.
        for n in 0..20 {
            dir.skill(&format!("skill-{n:02}"), &description, "x");
        }
        let (skills, warnings) = load(&dir);
        assert_eq!(skills.0.len(), 15);
        assert!(skills.listing().len() <= MAX_LISTING_BYTES);
        assert!(skills.get("skill-14").is_some() && skills.get("skill-15").is_none());
        let warning = only_warning(&warnings);
        let expected = "`skill-15` and any skills after it";
        assert!(warning.contains(expected), "{warning}");
    }

    /// `count` skills `s-00`, `s-01`, ... each with a description of `len(n)`
    /// characters.
    fn numbered(dir: &TempDir, count: usize, len: impl Fn(usize) -> usize) {
        for n in 0..count {
            dir.skill(&format!("s-{n:02}"), &"d".repeat(len(n)), "x");
        }
    }

    #[test]
    fn the_listing_limit_is_exact() {
        // Each skill costs "- s-00: " (8 bytes), its description and a newline.
        let fits = MAX_LISTING_BYTES / 16 - 9;
        let dir = TempDir::new();
        numbered(&dir, 16, |_| fits);
        let (skills, warnings) = load(&dir);
        assert_eq!(skills.0.len(), 16);
        assert!(warnings.is_empty(), "{warnings:?}");

        let dir = TempDir::new();
        numbered(&dir, 16, |n| fits + usize::from(n == 15));
        let (skills, warnings) = load(&dir);
        assert_eq!(skills.0.len(), 15);
        assert!(only_warning(&warnings).contains("`s-15` and any skills after it"));
    }

    #[test]
    fn at_most_sixty_four_skills_load() {
        let dir = TempDir::new();
        numbered(&dir, MAX_SKILLS + 6, |_| 1);
        let (skills, warnings) = load(&dir);
        assert_eq!(skills.0.len(), MAX_SKILLS);
        assert!(skills.get("s-63").is_some() && skills.get("s-64").is_none());
        assert!(only_warning(&warnings).contains("`s-64` and any skills after it"));
    }

    #[test]
    fn a_huge_directory_is_examined_only_so_far() {
        let dir = TempDir::new();
        for n in 0..MAX_ENTRIES + 44 {
            std::fs::create_dir_all(dir.join(&format!("skills/d-{n:03}"))).unwrap();
        }
        let (skills, warnings) = load(&dir);
        assert!(skills.is_empty());
        // Which 256 are examined is up to the file system: count, don't name.
        assert_eq!(warnings.len(), MAX_ENTRIES + 1);
        let notice = warnings.iter().filter(|w| w.contains("not examined"));
        assert_eq!(notice.count(), 1);
        let skipped = warnings.iter().filter(|w| w.contains("` skipped: "));
        assert_eq!(skipped.count(), MAX_ENTRIES);
    }

    #[test]
    fn the_scan_stops_reading_entries_at_the_cap() {
        use std::cell::Cell;
        use std::path::PathBuf;
        let pulled = Cell::new(0);
        // A million plain entries, none a directory, counted as they are read.
        let entries = (0..1_000_000).map(|n| {
            pulled.set(pulled.get() + 1);
            Ok(PathBuf::from(format!("/nonexistent-skills/file-{n}")))
        });
        let (names, truncated) = scan(entries, 10).unwrap();
        assert!(names.is_empty() && truncated);
        // The ten examined and one more, to learn there were others.
        assert_eq!(pulled.get(), 11);
    }

    #[test]
    fn the_scan_counts_every_kind_of_entry_and_keeps_the_names_it_found_sorted() {
        let dir = TempDir::new();
        for name in ["b", "a", ".hidden"] {
            std::fs::create_dir_all(dir.join(name)).unwrap();
        }
        dir.write("c", "a file");
        let paths = || ["b", "c", "a", ".hidden"].map(|n| Ok(dir.join(n)));
        // Exactly the cap: nothing is left over.
        assert_eq!(
            scan(paths().into_iter(), 4).unwrap(),
            (vec!["a".into(), "b".into()], false)
        );
        // One short: the last entry (`.hidden`) is not examined.
        assert_eq!(
            scan(paths().into_iter(), 3).unwrap(),
            (vec!["a".into(), "b".into()], true)
        );
        // Two short: `a` is cut off too.
        assert_eq!(
            scan(paths().into_iter(), 2).unwrap(),
            (vec!["b".into()], true)
        );
    }

    #[test]
    fn a_read_error_during_the_scan_is_returned() {
        let failing = [Err(std::io::Error::other("disk gone"))];
        let error = scan(failing.into_iter(), 10).unwrap_err();
        assert_eq!(error.to_string(), "disk gone");
    }

    #[test]
    fn the_skills_directory_may_itself_be_a_symlink() {
        let dir = TempDir::new();
        dir.skill("real", "Real.", "x");
        symlink(dir.join("skills"), dir.join("shared")).unwrap();
        let mut warnings = Vec::new();
        let skills = Skills::load(&dir.join("shared"), &mut warnings);
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(skills.listing(), "- real: Real.");
    }

    #[test]
    fn a_named_pipe_as_skill_file_is_refused_not_waited_on() {
        let dir = TempDir::new();
        dir.skill("fine", "Fine.", "ok");
        std::fs::create_dir_all(dir.join("skills/pipe")).unwrap();
        let made = std::process::Command::new("mkfifo")
            .arg(dir.join("skills/pipe/SKILL.md"))
            .status()
            .unwrap();
        assert!(made.success());
        let (skills, warnings) = load(&dir);
        assert_eq!(skills.listing(), "- fine: Fine.");
        assert!(only_warning(&warnings).contains("not a regular file"));
    }

    #[test]
    fn stray_files_hidden_directories_and_dangling_links_are_not_skills() {
        let dir = TempDir::new();
        dir.skill("real", "Real.", "x");
        dir.write("skills/README.md", "about these skills");
        dir.write(
            "skills/.hidden/SKILL.md",
            "---\nname: hidden\ndescription: d\n---\n",
        );
        symlink(dir.join("nowhere"), dir.join("skills/dangling")).unwrap();
        let (skills, warnings) = load(&dir);
        assert_eq!(skills.listing(), "- real: Real.");
        assert!(warnings.is_empty(), "{warnings:?}");
    }

    #[test]
    fn a_missing_empty_or_non_directory_skills_path_loads_nothing() {
        let dir = TempDir::new();
        let (skills, warnings) = load(&dir);
        assert!(skills.is_empty());
        assert!(only_warning(&warnings).contains("skills directory"));

        std::fs::create_dir(dir.join("skills")).unwrap();
        let (skills, warnings) = load(&dir);
        assert!(skills.is_empty() && warnings.is_empty());

        let file = dir.write("a-file", "x");
        let mut warnings = Vec::new();
        assert!(Skills::load(&file, &mut warnings).is_empty());
        assert!(only_warning(&warnings).contains("skills directory"));
    }

    #[test]
    fn links_that_leave_the_skills_directory_are_refused() {
        let dir = TempDir::new();
        let secret = dir.write(
            "outside/secret.md",
            "---\nname: file-link\ndescription: leaked\n---\nSECRET",
        );
        // SKILL.md itself a link to a file outside.
        std::fs::create_dir_all(dir.join("skills/file-link")).unwrap();
        symlink(&secret, dir.join("skills/file-link/SKILL.md")).unwrap();
        // The skill directory a link to a directory outside.
        dir.write(
            "outside/dir-link/SKILL.md",
            "---\nname: dir-link\ndescription: leaked\n---\nSECRET",
        );
        symlink(dir.join("outside/dir-link"), dir.join("skills/dir-link")).unwrap();
        // A relative link up and out.
        dir.write(
            "outside/up/SKILL.md",
            "---\nname: up\ndescription: leaked\n---\nSECRET",
        );
        symlink("../outside/up", dir.join("skills/up")).unwrap();
        dir.skill("fine", "Fine.", "ok");
        let (skills, warnings) = load(&dir);
        assert_eq!(skills.listing(), "- fine: Fine.");
        assert_eq!(warnings.len(), 3, "{warnings:?}");
        assert!(
            warnings
                .iter()
                .all(|w| w.contains("outside the skills directory"))
        );
        assert!(!format!("{skills:?}").contains("SECRET"));
    }

    #[test]
    fn links_that_stay_inside_the_skills_directory_work() {
        let dir = TempDir::new();
        dir.write(
            "skills/linked/shared.md",
            "---\nname: linked\ndescription: Via a link.\n---\nbody",
        );
        symlink("shared.md", dir.join("skills/linked/SKILL.md")).unwrap();
        let (skills, warnings) = load(&dir);
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(skills.get("linked").unwrap().body, "body");
    }

    #[test]
    fn a_skill_file_that_cannot_be_resolved_is_skipped_with_the_reason() {
        let dir = TempDir::new();
        dir.skill("fine", "Fine.", "ok");
        std::fs::create_dir_all(dir.join("skills/looping")).unwrap();
        symlink("SKILL.md", dir.join("skills/looping/SKILL.md")).unwrap();
        let (skills, warnings) = load(&dir);
        assert_eq!(skills.listing(), "- fine: Fine.");
        let warning = only_warning(&warnings);
        assert!(warning.contains("`looping` skipped"));
    }

    #[test]
    fn a_directory_link_to_another_skill_must_still_match_by_name() {
        let dir = TempDir::new();
        dir.skill("real", "Real.", "x");
        symlink("real", dir.join("skills/alias")).unwrap();
        let (skills, warnings) = load(&dir);
        assert_eq!(skills.listing(), "- real: Real.");
        let warning = only_warning(&warnings);
        assert!(warning.contains("`alias` skipped") && warning.contains("must match"));
    }

    #[tokio::test]
    async fn read_skill_returns_the_body_without_the_front_matter() {
        let dir = TempDir::new();
        dir.skill("pdf-tools", "Work with PDFs.", "# Steps\n\nOpen it.");
        let (skills, _) = load(&dir);
        assert_eq!(
            read_skill(skills, "pdf-tools").await.unwrap(),
            "# Steps\n\nOpen it."
        );
    }

    #[tokio::test]
    async fn a_skill_without_a_body_says_so_instead_of_returning_nothing() {
        let dir = TempDir::new();
        with_front(&dir, "bare", "name: bare\ndescription: Just metadata.");
        let (skills, _) = load(&dir);
        let text = read_skill(skills, "bare").await.unwrap();
        assert_eq!(
            text,
            "This skill has no instructions beyond its description."
        );
    }

    #[tokio::test]
    async fn an_unknown_name_is_an_error_that_lists_the_skills() {
        let dir = TempDir::new();
        dir.skill("alpha", "A.", "x");
        dir.skill("beta", "B.", "y");
        let (skills, _) = load(&dir);
        let error = read_skill(skills, "gamma").await.unwrap_err();
        assert_eq!(error.kind().as_str(), "invalid_args");
        assert_eq!(
            error.message(),
            "There is no skill named `gamma`. The skills are: alpha, beta."
        );
    }

    #[tokio::test]
    async fn names_that_try_to_leave_the_directory_find_nothing() {
        let dir = TempDir::new();
        dir.skill("alpha", "A.", "x");
        dir.write(
            "outside/SKILL.md",
            "---\nname: outside\ndescription: d\n---\nSECRET",
        );
        dir.write("skills/alpha/references/REF.md", "SECRET");
        let absolute = dir.join("outside/SKILL.md").display().to_string();
        let attempts = [
            "../outside",
            "../outside/SKILL.md",
            "alpha/../../outside",
            "alpha/SKILL.md",
            "alpha/references/REF.md",
            "alpha/",
            "./alpha",
            "..",
            ".",
            "",
            "/etc/passwd",
            &absolute,
            "ALPHA",
        ];
        for attempt in attempts {
            let (skills, _) = load(&dir);
            let error = read_skill(skills, attempt).await.unwrap_err();
            assert!(!error.message().contains("SECRET"), "{attempt}");
            assert!(
                error.message().contains("There is no skill named"),
                "{attempt}"
            );
        }
    }

    #[test]
    fn the_tool_describes_itself_to_the_model() {
        let tool = ReadSkill(Arc::new(Skills::default()));
        assert_eq!(ReadSkill::NAME, "read_skill");
        assert!(tool.description().contains("skills listed"));
        let parameters = tool.parameters();
        assert_eq!(parameters["required"], json!(["name"]));
        assert_eq!(parameters["properties"]["name"]["type"], "string");
    }
}
