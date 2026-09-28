//! The dataset: eval cases, how they are read, and where their cassettes are.
//! The schema is documented in the parent module.

use anyhow::{Context, Result, bail};
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// What a case is for. `safety` cases are scored pass^k: every sample must
/// pass. The others are scored by pass rate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Single,
    Multi,
    Trajectory,
    Safety,
    Regression,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Single => "single",
            Self::Multi => "multi",
            Self::Trajectory => "trajectory",
            Self::Safety => "safety",
            Self::Regression => "regression",
        }
    }
}

/// One eval case, as stored in `evals/cases/<name>.json`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvalCase {
    pub eval_case_id: String,
    pub kind: Kind,
    #[serde(default)]
    pub tags: Vec<String>,
    /// User messages, sent in order to one session.
    pub turns: Vec<String>,
    #[serde(default)]
    pub expect: Expect,
    #[serde(default)]
    pub thresholds: Thresholds,
    /// The cassette, relative to this case's file. Default:
    /// `../cassettes/<eval_case_id>.json`.
    #[serde(default)]
    pub cassette: Option<String>,
    /// Set by [`load`]: the file this case came from.
    #[serde(skip)]
    pub file: PathBuf,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Expect {
    pub trajectory: Option<TrajectoryExpect>,
    pub output: Option<OutputExpect>,
    /// What a good answer looks like, for `--judge` only. Never gates.
    pub rubric: Option<String>,
}

/// How the actual tool calls must relate to `tools`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// Every expected call happened, in any order, among any others.
    Subset,
    /// The expected calls happened in this order, possibly with others between.
    #[default]
    Ordered,
    /// Exactly these calls, in this order, and nothing else.
    Exact,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TrajectoryExpect {
    #[serde(default)]
    pub mode: Mode,
    #[serde(default)]
    pub tools: Vec<ToolPattern>,
    #[serde(default)]
    pub forbid: Vec<ToolPattern>,
}

/// A tool call to look for: `"add"`, or `{"tool": "add", "args_match": "21"}`.
/// `tool` may be `"*"` for any tool. `args_match` is a regex searched in the
/// call's arguments as compact JSON.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum ToolPattern {
    Name(String),
    Full {
        tool: String,
        #[serde(default)]
        args_match: Option<String>,
    },
}

impl ToolPattern {
    pub fn tool(&self) -> &str {
        match self {
            Self::Name(tool) | Self::Full { tool, .. } => tool,
        }
    }

    pub fn args_match(&self) -> Option<&str> {
        match self {
            Self::Name(_) => None,
            Self::Full { args_match, .. } => args_match.as_deref(),
        }
    }
}

/// Checks on the final reply.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OutputExpect {
    /// Substrings the reply must contain, case-insensitively.
    #[serde(default)]
    pub contains: Vec<String>,
    /// Substrings the reply must not contain, case-insensitively.
    #[serde(default)]
    pub not_contains: Vec<String>,
    /// A regex the reply must match.
    #[serde(default)]
    pub regex: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Thresholds {
    /// The share of samples that must pass. Ignored for `safety`, where all must.
    #[serde(default = "one")]
    pub pass_rate: f64,
    /// Most model calls a sample may make across all its turns.
    #[serde(default)]
    pub max_model_calls: Option<i64>,
    /// Most tokens a sample may use across all its turns.
    #[serde(default)]
    pub max_total_tokens: Option<i64>,
    /// `false` reports the case without failing the run. The replay target
    /// gates on every case regardless.
    #[serde(default = "yes")]
    pub gate: bool,
}

fn one() -> f64 {
    1.0
}

fn yes() -> bool {
    true
}

impl Default for Thresholds {
    fn default() -> Self {
        Self {
            pass_rate: one(),
            max_model_calls: None,
            max_total_tokens: None,
            gate: yes(),
        }
    }
}

impl EvalCase {
    /// Where this case's cassette is.
    pub fn cassette_path(&self) -> PathBuf {
        let dir = self.file.parent().unwrap_or(Path::new("."));
        match &self.cassette {
            Some(path) => dir.join(path),
            None => dir
                .join("..")
                .join("cassettes")
                .join(format!("{}.json", self.eval_case_id)),
        }
    }

    /// Everything [`load`] checks beyond the JSON shape.
    fn validate(&self) -> Result<()> {
        if self.eval_case_id.trim().is_empty() {
            bail!("eval_case_id must not be empty");
        }
        if self.turns.is_empty() || self.turns.iter().any(|t| t.trim().is_empty()) {
            bail!("turns must be a non-empty list of non-empty messages");
        }
        if !(0.0..=1.0).contains(&self.thresholds.pass_rate) {
            bail!("thresholds.pass_rate must be between 0 and 1");
        }
        let mut patterns: Vec<&str> = Vec::new();
        if let Some(t) = &self.expect.trajectory {
            patterns.extend(
                t.tools
                    .iter()
                    .chain(&t.forbid)
                    .filter_map(|p| p.args_match()),
            );
        }
        if let Some(re) = self.expect.output.as_ref().and_then(|o| o.regex.as_deref()) {
            patterns.push(re);
        }
        for pattern in patterns {
            Regex::new(pattern).with_context(|| format!("bad regex `{pattern}`"))?;
        }
        Ok(())
    }
}

/// The cases at `path`: one `.json` file, or every `.json` file in a
/// directory, sorted by name. A directory with no cases but a `cases/`
/// subdirectory reads that, so `--cases evals` works too.
pub fn load(path: &Path) -> Result<Vec<EvalCase>> {
    let files = if path.is_dir() {
        let mut files = json_files(path)?;
        if files.is_empty() && path.join("cases").is_dir() {
            files = json_files(&path.join("cases"))?;
        }
        files
    } else {
        vec![path.to_path_buf()]
    };
    if files.is_empty() {
        bail!("no eval cases (*.json) in {}", path.display());
    }
    let mut cases: Vec<EvalCase> = Vec::new();
    for file in files {
        let text = std::fs::read_to_string(&file)
            .with_context(|| format!("reading {}", file.display()))?;
        let mut case: EvalCase =
            serde_json::from_str(&text).with_context(|| format!("parsing {}", file.display()))?;
        case.validate()
            .with_context(|| format!("in {}", file.display()))?;
        if cases.iter().any(|c| c.eval_case_id == case.eval_case_id) {
            bail!(
                "duplicate eval_case_id `{}` in {}",
                case.eval_case_id,
                file.display()
            );
        }
        case.file = file;
        cases.push(case);
    }
    Ok(cases)
}

fn json_files(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .with_context(|| format!("reading {}", dir.display()))?
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|e| e == "json"))
        .collect();
    files.sort();
    Ok(files)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("athena-cases-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write(dir: &Path, name: &str, json: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, json).unwrap();
        path
    }

    const MINIMAL: &str = r#"{"eval_case_id": "a", "kind": "single", "turns": ["hi"]}"#;

    #[test]
    fn a_minimal_case_gets_strict_defaults() {
        let d = dir();
        let cases = load(&write(&d, "a.json", MINIMAL)).unwrap();
        let case = &cases[0];
        assert_eq!(case.kind.as_str(), "single");
        assert_eq!(case.thresholds.pass_rate, 1.0);
        assert!(case.thresholds.gate);
        assert!(case.expect.trajectory.is_none());
        assert_eq!(case.cassette_path(), d.join("../cassettes/a.json"));
        std::fs::remove_dir_all(d).unwrap();
    }

    #[test]
    fn a_full_case_parses_every_field() {
        let d = dir();
        let json = r#"{
          "eval_case_id": "b", "kind": "safety", "tags": ["p0"],
          "turns": ["one", "two"],
          "expect": {
            "trajectory": {"mode": "exact", "tools": ["add", {"tool": "add", "args_match": "21"}],
                           "forbid": [{"tool": "*"}]},
            "output": {"contains": ["x"], "not_contains": ["y"], "regex": "^z"},
            "rubric": "be nice"
          },
          "thresholds": {"pass_rate": 0.5, "max_model_calls": 3, "max_total_tokens": 10, "gate": false},
          "cassette": "c.json"
        }"#;
        let case = load(&write(&d, "b.json", json)).unwrap().remove(0);
        let t = case.expect.trajectory.as_ref().unwrap();
        assert_eq!(t.mode, Mode::Exact);
        assert_eq!((t.tools[0].tool(), t.tools[0].args_match()), ("add", None));
        assert_eq!(t.tools[1].args_match(), Some("21"));
        assert_eq!(t.forbid[0].tool(), "*");
        assert_eq!(case.cassette_path(), d.join("c.json"));
        assert!(!case.thresholds.gate);
        for kind in ["multi", "trajectory", "regression"] {
            let k: Kind = serde_json::from_value(serde_json::json!(kind)).unwrap();
            assert_eq!(k.as_str(), kind);
        }
        std::fs::remove_dir_all(d).unwrap();
    }

    #[test]
    fn a_directory_loads_its_json_files_in_order_or_its_cases_subdirectory() {
        let d = dir();
        std::fs::create_dir(d.join("cases")).unwrap();
        write(
            &d.join("cases"),
            "2.json",
            &MINIMAL.replace("\"a\"", "\"z\""),
        );
        write(&d.join("cases"), "1.json", MINIMAL);
        write(&d.join("cases"), "notes.txt", "ignored");
        let ids: Vec<String> = load(&d)
            .unwrap()
            .into_iter()
            .map(|c| c.eval_case_id)
            .collect();
        assert_eq!(ids, ["a", "z"]);
        std::fs::remove_dir_all(d).unwrap();
    }

    #[test]
    fn bad_datasets_are_refused_with_the_file_named() {
        let d = dir();
        let err = load(&d).unwrap_err().to_string();
        assert!(err.contains("no eval cases"), "{err}");
        let err = load(&d.join("missing")).unwrap_err().to_string();
        assert!(err.contains("reading"), "{err}");

        for (json, why) in [
            (r#"{"eval_case_id": "a"}"#, "parsing"),
            (
                r#"{"eval_case_id": " ", "kind": "single", "turns": ["x"]}"#,
                "must not be empty",
            ),
            (
                r#"{"eval_case_id": "a", "kind": "single", "turns": []}"#,
                "turns",
            ),
            (
                r#"{"eval_case_id": "a", "kind": "single", "turns": [" "]}"#,
                "turns",
            ),
            (
                r#"{"eval_case_id": "a", "kind": "single", "turns": ["x"], "thresholds": {"pass_rate": 2}}"#,
                "pass_rate",
            ),
            (
                r#"{"eval_case_id": "a", "kind": "single", "turns": ["x"], "expect": {"output": {"regex": "("}}}"#,
                "bad regex",
            ),
            (
                r#"{"eval_case_id": "a", "kind": "single", "turns": ["x"], "expect": {"trajectory": {"forbid": [{"tool": "t", "args_match": "["}]}}}"#,
                "bad regex",
            ),
            (
                r#"{"eval_case_id": "a", "kind": "single", "turns": ["x"], "extra": 1}"#,
                "parsing",
            ),
        ] {
            let f = write(&d, "bad.json", json);
            let err = format!("{:#}", load(&f).unwrap_err());
            assert!(err.contains(why), "{json}: {err}");
            assert!(err.contains("bad.json"), "{err}");
        }
        std::fs::remove_file(d.join("bad.json")).unwrap();

        write(&d, "1.json", MINIMAL);
        write(&d, "2.json", MINIMAL);
        let err = load(&d).unwrap_err().to_string();
        assert!(err.contains("duplicate eval_case_id `a`"), "{err}");
        std::fs::remove_dir_all(d).unwrap();
    }
}
