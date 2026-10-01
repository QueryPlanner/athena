//! What the person running Athena adds to the agent without editing code:
//! a file of instructions and a directory of skills.
//!
//! - `ATHENA_INSTRUCTIONS`: the path of a text file. Its contents are
//!   appended to the base preamble ([`crate::agent::PREAMBLE`]).
//! - `ATHENA_SKILLS_DIR`: the path of a directory of Agent Skills
//!   (see [`skills`]). Each skill is listed in the preamble by name and
//!   description, and the `read_skill` tool returns one on demand.
//!
//! Both are optional, read once when the agent is built, and never fatal: a
//! missing file, an unreadable file or an invalid skill is a warning and
//! the agent starts without it. Edits take effect on the next start.
//!
//! These files are code-equivalent: the model follows them with the same
//! authority as the base preamble. They are local only. Nothing here fetches
//! from the network.

pub mod frontmatter;
pub mod skills;

use rig_agent::agent::{AgentBuilder, WithBuilderTools};
use skills::{ReadSkill, Skills};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// The largest instructions file. Every prompt carries it, so it is capped
/// at about 4 000 tokens; a larger file is ignored, not cut short, because
/// half a set of instructions can mean something else.
pub const MAX_INSTRUCTIONS_BYTES: usize = 16 * 1024;

/// Where the files are, from the environment.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Config {
    pub instructions: Option<PathBuf>,
    pub skills_dir: Option<PathBuf>,
}

impl Config {
    /// `ATHENA_INSTRUCTIONS` and `ATHENA_SKILLS_DIR`; unset or empty means
    /// none.
    pub fn from_env() -> Self {
        let var = |name| std::env::var(name).ok();
        Self::parse(var("ATHENA_INSTRUCTIONS"), var("ATHENA_SKILLS_DIR"))
    }

    fn parse(instructions: Option<String>, skills_dir: Option<String>) -> Self {
        let path = |v: Option<String>| v.filter(|v| !v.trim().is_empty()).map(PathBuf::from);
        Self {
            instructions: path(instructions),
            skills_dir: path(skills_dir),
        }
    }
}

/// The loaded instructions and skills.
#[derive(Debug, Default)]
pub struct Custom {
    instructions: Option<String>,
    skills: Arc<Skills>,
}

impl Custom {
    /// [`Custom::load`] with the environment's paths, each warning logged.
    pub fn from_env() -> Self {
        Self::load_logged(&Config::from_env())
    }

    fn load_logged(config: &Config) -> Self {
        let (custom, warnings) = Self::load(config);
        for warning in warnings {
            tracing::warn!("{warning}");
        }
        custom
    }

    /// Read what `config` points at. The warnings say what was left out.
    pub fn load(config: &Config) -> (Self, Vec<String>) {
        let mut warnings = Vec::new();
        let instructions = config.instructions.as_deref().and_then(|path| {
            read_instructions(path).unwrap_or_else(|why| {
                warnings.push(format!("instructions ignored: {why}"));
                None
            })
        });
        let skills = config
            .skills_dir
            .as_deref()
            .map(|dir| Skills::load(dir, &mut warnings))
            .unwrap_or_default();
        let custom = Self {
            instructions,
            skills: Arc::new(skills),
        };
        // Names and paths come from the file system, so keep control
        // characters from forging log lines.
        (custom, warnings.iter().map(|w| printable(w)).collect())
    }

    /// `base` followed by the instructions and the list of skills. With
    /// neither, `base` unchanged.
    pub fn preamble(&self, base: &str) -> String {
        let mut text = base.to_string();
        if let Some(instructions) = &self.instructions {
            text.push_str("\n\n## Custom instructions\n\n");
            text.push_str(instructions);
        }
        if !self.skills.is_empty() {
            text.push_str(
                "\n\n## Skills\n\nA skill holds instructions for one kind of task, put \
                 here by your owner. When a task matches one of these, call read_skill \
                 with its name before you start, then follow what it returns. Unlike a \
                 web page or a file you open, a skill is not untrusted.\n\n",
            );
            text.push_str(&self.skills.listing());
        }
        text
    }

    /// Add `read_skill` when there are skills to read.
    pub fn register(
        &self,
        builder: AgentBuilder<WithBuilderTools>,
    ) -> AgentBuilder<WithBuilderTools> {
        if self.skills.is_empty() {
            builder
        } else {
            builder.tool(ReadSkill(self.skills.clone()))
        }
    }
}

/// `text` with each control character (a newline, an escape) written out
/// as an escape sequence such as `\n`.
fn printable(text: &str) -> String {
    let shown = |c: char| match c.is_control() {
        true => c.escape_default().to_string(),
        false => c.to_string(),
    };
    text.chars().map(shown).collect()
}

fn read_instructions(path: &Path) -> Result<Option<String>, String> {
    let text = read_limited(path, MAX_INSTRUCTIONS_BYTES)
        .map_err(|why| format!("{}: {why}", path.display()))?;
    Ok(Some(text.trim().to_string()).filter(|text| !text.is_empty()))
}

/// The text of the regular file at `path` (a link to one is fine), reading
/// at most `limit` bytes of it. Checking the kind first keeps a named pipe
/// from blocking startup.
fn read_limited(path: &Path, limit: usize) -> Result<String, String> {
    if !std::fs::metadata(path)
        .map_err(|e| e.to_string())?
        .is_file()
    {
        return Err("the path is not a regular file".into());
    }
    let mut bytes = Vec::new();
    std::fs::File::open(path)
        .and_then(|file| file.take(limit as u64 + 1).read_to_end(&mut bytes))
        .map_err(|e| e.to_string())?;
    if bytes.len() > limit {
        return Err(format!("the file is over {limit} bytes"));
    }
    String::from_utf8(bytes).map_err(|_| "the file is not UTF-8 text".into())
}

#[cfg(test)]
pub(crate) mod testing;

#[cfg(test)]
mod tests {
    use super::testing::*;
    use super::*;
    use rig_core::test_utils::MockCompletionModel;

    fn config(instructions: Option<&Path>, skills: Option<&Path>) -> Config {
        Config {
            instructions: instructions.map(Path::to_path_buf),
            skills_dir: skills.map(Path::to_path_buf),
        }
    }

    #[test]
    fn paths_come_from_the_environment_and_empty_means_unset() {
        let got = Config::parse(Some("/a/i.md".into()), Some("/a/skills".into()));
        assert_eq!(got.instructions.unwrap(), Path::new("/a/i.md"));
        assert_eq!(got.skills_dir.unwrap(), Path::new("/a/skills"));
        assert_eq!(Config::parse(None, None), Config::default());
        assert_eq!(
            Config::parse(Some("".into()), Some("  ".into())),
            Config::default()
        );
    }

    #[tokio::test]
    async fn without_files_the_preamble_is_the_base_and_there_is_no_tool() {
        let (custom, warnings) = Custom::load(&Config::default());
        assert!(warnings.is_empty());
        assert_eq!(custom.preamble("base"), "base");
        assert!(tool_names(&custom).await.is_empty());
    }

    #[tokio::test]
    async fn instructions_follow_the_base_preamble() {
        let dir = TempDir::new();
        let file = dir.write("instructions.md", "\n  Call me Boss.\nBe brief.  \n\n");
        let (custom, warnings) = Custom::load(&config(Some(&file), None));
        assert!(warnings.is_empty());
        assert_eq!(
            custom.preamble("base"),
            "base\n\n## Custom instructions\n\nCall me Boss.\nBe brief."
        );
        assert!(tool_names(&custom).await.is_empty());
    }

    #[tokio::test]
    async fn skills_are_listed_after_the_instructions_and_come_with_read_skill() {
        let dir = TempDir::new();
        let file = dir.write("i.md", "Be brief.");
        dir.skill("pdf-tools", "Work with PDFs.\nUse for forms.", "Open it.");
        dir.skill("a-first", "Sorts first.", "A.");
        let (custom, warnings) = Custom::load(&config(Some(&file), Some(&dir.join("skills"))));
        assert!(warnings.is_empty(), "{warnings:?}");
        let preamble = custom.preamble("base");
        assert!(preamble.starts_with("base\n\n## Custom instructions\n\nBe brief.\n\n## Skills\n"));
        assert!(
            preamble.ends_with(
                "\n\n- a-first: Sorts first.\n- pdf-tools: Work with PDFs. Use for forms."
            )
        );
        assert_eq!(tool_names(&custom).await, ["read_skill"]);
    }

    #[test]
    fn a_missing_file_or_directory_is_a_warning_not_a_failure() {
        let dir = TempDir::new();
        let (custom, warnings) = Custom::load(&config(
            Some(&dir.join("nope.md")),
            Some(&dir.join("no-skills")),
        ));
        assert_eq!(custom.preamble("base"), "base");
        assert_eq!(warnings.len(), 2, "{warnings:?}");
        let (instructions, skills) = (&warnings[0], &warnings[1]);
        assert!(instructions.starts_with("instructions ignored: "));
        assert!(instructions.contains("nope.md"));
        assert!(skills.starts_with("skills directory "));
    }

    #[test]
    fn unusable_instructions_are_ignored_with_the_reason() {
        let dir = TempDir::new();
        let cases = [
            (dir.path().to_path_buf(), "not a regular file"),
            (dir.write_bytes("binary.md", &[0xff, 0xfe]), "not UTF-8"),
            (
                dir.write("big.md", &"x".repeat(MAX_INSTRUCTIONS_BYTES + 1)),
                &format!("over {MAX_INSTRUCTIONS_BYTES} bytes"),
            ),
        ];
        for (path, why) in cases {
            let (custom, warnings) = Custom::load(&config(Some(&path), None));
            assert_eq!(custom.preamble("base"), "base", "{why}");
            assert!(warnings[0].contains(why), "{warnings:?}");
        }
    }

    #[test]
    fn instructions_at_the_size_limit_are_used_and_blank_ones_are_not() {
        let dir = TempDir::new();
        let exact = "y".repeat(MAX_INSTRUCTIONS_BYTES);
        let file = dir.write("exact.md", &exact);
        let (custom, warnings) = Custom::load(&config(Some(&file), None));
        assert!(warnings.is_empty());
        assert!(custom.preamble("b").ends_with(&exact));

        let blank = dir.write("blank.md", " \n\t\n");
        let (custom, warnings) = Custom::load(&config(Some(&blank), None));
        assert!(warnings.is_empty());
        assert_eq!(custom.preamble("b"), "b");
    }

    #[test]
    fn a_named_pipe_as_instructions_is_refused_not_waited_on() {
        let dir = TempDir::new();
        let pipe = dir.join("pipe.md");
        let made = std::process::Command::new("mkfifo").arg(&pipe).status();
        assert!(made.unwrap().success());
        let (custom, warnings) = Custom::load(&config(Some(&pipe), None));
        assert_eq!(custom.preamble("base"), "base");
        assert!(warnings[0].contains("not a regular file"));
    }

    #[test]
    fn control_characters_in_a_name_cannot_forge_log_lines() {
        let dir = TempDir::new();
        std::fs::create_dir_all(dir.join("skills/bad\nWARN fake\x1b[0m")).unwrap();
        let (_, warnings) = Custom::load(&config(None, Some(&dir.join("skills"))));
        assert_eq!(warnings.len(), 1);
        let warning = &warnings[0];
        assert!(warning.contains("`bad\\nWARN fake\\u{1b}[0m` skipped"));
        assert!(!warning.contains(['\n', '\x1b']));
    }

    #[test]
    fn load_logged_logs_each_warning() {
        let dir = TempDir::new();
        let logged = capture_logs(|| {
            Custom::load_logged(&config(Some(&dir.join("nope.md")), Some(&dir.join("nope"))));
        });
        assert!(logged.contains("instructions ignored"), "{logged}");
        assert!(logged.contains("skills directory"), "{logged}");
    }

    /// The tools the production agent has beyond its host tool `add`, built
    /// by the same `configure_custom` that `build_with` calls.
    async fn tool_names(custom: &Custom) -> Vec<String> {
        let builder = AgentBuilder::new(MockCompletionModel::new([]));
        let agent = crate::agent::configure_custom(builder, None, custom);
        let defs = agent.tool_definitions(None).await.unwrap();
        let mut names: Vec<String> = defs.into_iter().map(|d| d.name).collect();
        names.retain(|n| n != "add");
        names.sort();
        names
    }
}
