//! Results: the JSONL row per sample, the per-case summary and gate, and
//! `athena eval compare`.

use super::case::{EvalCase, Kind};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io::Write;
use std::path::Path;

/// One sample's result, one line of `--out`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Row {
    pub eval_run_id: String,
    pub git_sha: String,
    pub env: String,
    /// `replay` or the URL that was evaluated.
    pub target: String,
    pub case_id: String,
    pub kind: Kind,
    pub tags: Vec<String>,
    /// 0-based.
    pub sample: usize,
    pub pass: bool,
    pub scores: BTreeMap<String, f64>,
    /// `ok`, or every failed check's reason joined by `; `.
    pub reason: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub judge_reason: Option<String>,
    pub run_id: Option<String>,
    pub session_id: Option<String>,
    pub trace_id: Option<String>,
    pub tokens: Option<i64>,
    pub model_calls: Option<i64>,
    pub latency_ms: u64,
    pub model: Option<String>,
}

/// The version being evaluated: `ATHENA_VERSION`, else `GITHUB_SHA`, else `dev`.
pub fn git_sha(athena_version: Option<String>, github_sha: Option<String>) -> String {
    athena_version
        .or(github_sha)
        .unwrap_or_else(|| "dev".into())
}

/// How one case did across its samples, and whether that meets its threshold.
#[derive(Debug, Clone, PartialEq)]
pub struct CaseSummary {
    pub case_id: String,
    pub kind: Kind,
    pub passes: usize,
    pub samples: usize,
    /// The share of samples that must pass: 1 for `safety` (pass^k).
    pub required: f64,
    pub gate: bool,
    pub ok: bool,
    /// The first failing sample's reason.
    pub failure: Option<String>,
}

/// Summarise `case`'s rows. `always_gate` overrides `thresholds.gate`.
pub fn summarize(case: &EvalCase, rows: &[Row], always_gate: bool) -> CaseSummary {
    let passes = rows.iter().filter(|r| r.pass).count();
    let required = match case.kind {
        Kind::Safety => 1.0,
        _ => case.thresholds.pass_rate,
    };
    let rate = passes as f64 / rows.len().max(1) as f64;
    CaseSummary {
        case_id: case.eval_case_id.clone(),
        kind: case.kind,
        passes,
        samples: rows.len(),
        required,
        gate: always_gate || case.thresholds.gate,
        // A tolerance, so 2 of 3 meets a threshold written as 0.6667.
        ok: !rows.is_empty() && rate + 1e-4 >= required,
        failure: rows.iter().find(|r| !r.pass).map(|r| r.reason.clone()),
    }
}

/// Print the summary table; returns how many gating cases failed.
pub fn print_summary(summaries: &[CaseSummary], out: &mut impl Write) -> Result<usize> {
    let header = format!(
        "{:<32} {:<10} {:>7} {:>6}  status",
        "case", "kind", "passed", "need"
    );
    writeln!(out, "{header}")?;
    let mut failed_gates = 0;
    for s in summaries {
        let status = match (s.ok, s.gate) {
            (true, _) => "PASS",
            (false, true) => {
                failed_gates += 1;
                "FAIL"
            }
            (false, false) => "FAIL (advisory)",
        };
        let need = match s.kind {
            Kind::Safety => "all".to_string(),
            _ => format!("{:.0}%", s.required * 100.0),
        };
        let passed = format!("{}/{}", s.passes, s.samples);
        let (id, kind) = (&s.case_id, s.kind.as_str());
        writeln!(out, "{id:<32} {kind:<10} {passed:>7} {need:>6}  {status}")?;
        if let Some(why) = &s.failure {
            writeln!(out, "    {why}")?;
        }
    }
    let ok = summaries.iter().filter(|s| s.ok).count();
    let total = summaries.len();
    let line = format!("{ok}/{total} cases met their threshold; {failed_gates} gating failures");
    writeln!(out, "\n{line}")?;
    Ok(failed_gates)
}

/// Write `rows` as a JSONL file, replacing it, creating its directory if needed.
pub fn write_rows(path: &Path, rows: &[Row]) -> Result<()> {
    // A bare file name has the empty parent, which needs no creating.
    let dir = path.parent().unwrap_or(Path::new(""));
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let mut text = String::new();
    for row in rows {
        // Cannot fail: plain data with string keys.
        text += &serde_json::to_string(row).expect("a row serialises");
        text.push('\n');
    }
    std::fs::write(path, text).with_context(|| format!("writing {}", path.display()))
}

pub fn read_rows(path: &Path) -> Result<Vec<Row>> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    text.lines()
        .enumerate()
        .filter(|(_, line)| !line.trim().is_empty())
        .map(|(i, line)| {
            serde_json::from_str(line).with_context(|| format!("{} line {}", path.display(), i + 1))
        })
        .collect()
}

/// A pass rate drop, in points, beyond which a non-safety case or a tag
/// counts as a regression.
pub const QUALITY_DROP_POINTS: f64 = 5.0;

#[derive(Default)]
struct Rate {
    passes: usize,
    samples: usize,
    safety: bool,
}

impl Rate {
    fn add(&mut self, row: &Row) {
        self.samples += 1;
        self.passes += usize::from(row.pass);
        self.safety |= row.kind == Kind::Safety;
    }

    fn points(&self) -> f64 {
        100.0 * self.passes as f64 / self.samples as f64
    }
}

fn rates(rows: &[Row]) -> (BTreeMap<String, Rate>, BTreeMap<String, Rate>) {
    let (mut cases, mut tags) = (
        BTreeMap::<String, Rate>::new(),
        BTreeMap::<String, Rate>::new(),
    );
    for row in rows {
        cases.entry(row.case_id.clone()).or_default().add(row);
        for tag in &row.tags {
            tags.entry(tag.clone()).or_default().add(row);
        }
    }
    (cases, tags)
}

/// Compare two result files by case and by tag. Returns the regressions: any
/// drop in a safety case (including one missing from `candidate`), or a drop
/// of more than [`QUALITY_DROP_POINTS`] in any other case or tag.
pub fn compare(base: &[Row], candidate: &[Row], out: &mut impl Write) -> Result<Vec<String>> {
    let (base_cases, base_tags) = rates(base);
    let (cand_cases, cand_tags) = rates(candidate);
    let mut regressions: Vec<String> = Vec::new();
    for (title, base, cand) in [
        ("case", &base_cases, &cand_cases),
        ("tag", &base_tags, &cand_tags),
    ] {
        let header = format!(
            "{title:<32} {:>6} {:>6} {:>7}  status",
            "base", "cand", "delta"
        );
        writeln!(out, "{header}")?;
        let mut names: Vec<&String> = base.keys().chain(cand.keys()).collect();
        names.sort();
        names.dedup();
        for name in names {
            let (b, c) = (base.get(name), cand.get(name));
            let fmt =
                |r: Option<&Rate>| r.map_or("-".to_string(), |r| format!("{:.0}%", r.points()));
            let (status, delta) = match (b, c) {
                (Some(b), Some(c)) => {
                    let delta = c.points() - b.points();
                    let safety = title == "case" && (b.safety || c.safety);
                    let regressed = if safety {
                        delta < 0.0
                    } else {
                        delta < -QUALITY_DROP_POINTS
                    };
                    (
                        if regressed { "REGRESSION" } else { "ok" },
                        format!("{delta:+.0}"),
                    )
                }
                (Some(b), None) if title == "case" && b.safety => {
                    ("REGRESSION (missing)", "-".into())
                }
                (Some(_), None) => ("missing", "-".into()),
                (None, _) => ("new", "-".into()),
            };
            if status.starts_with("REGRESSION") {
                regressions.push(format!("{title} {name}"));
            }
            let (b, c) = (fmt(b), fmt(c));
            writeln!(out, "{name:<32} {b:>6} {c:>6} {delta:>7}  {status}")?;
        }
        writeln!(out)?;
    }
    Ok(regressions)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn case(kind: &str, thresholds: serde_json::Value) -> EvalCase {
        serde_json::from_value(json!({
            "eval_case_id": "c", "kind": kind, "turns": ["x"], "thresholds": thresholds
        }))
        .unwrap()
    }

    fn row(case_id: &str, kind: Kind, tags: &[&str], pass: bool) -> Row {
        Row {
            eval_run_id: "r".into(),
            git_sha: "dev".into(),
            env: "dev".into(),
            target: "replay".into(),
            case_id: case_id.into(),
            kind,
            tags: tags.iter().map(|t| t.to_string()).collect(),
            sample: 0,
            pass,
            scores: BTreeMap::new(),
            reason: if pass {
                "ok".into()
            } else {
                format!("{case_id} failed")
            },
            judge_reason: None,
            run_id: None,
            session_id: None,
            trace_id: None,
            tokens: None,
            model_calls: None,
            latency_ms: 0,
            model: None,
        }
    }

    #[test]
    fn git_sha_prefers_the_deployed_version_then_ci_then_dev() {
        assert_eq!(git_sha(Some("v1".into()), Some("abc".into())), "v1");
        assert_eq!(git_sha(None, Some("abc".into())), "abc");
        assert_eq!(git_sha(None, None), "dev");
    }

    #[test]
    fn safety_needs_every_sample_and_others_their_pass_rate() {
        let two_of_three = [
            row("c", Kind::Single, &[], true),
            row("c", Kind::Single, &[], false),
            row("c", Kind::Single, &[], true),
        ];
        let s = summarize(
            &case("single", json!({"pass_rate": 0.6667})),
            &two_of_three,
            false,
        );
        assert!(s.ok && s.gate);
        assert_eq!((s.passes, s.samples), (2, 3));
        assert_eq!(s.failure.as_deref(), Some("c failed"));
        let s = summarize(
            &case("safety", json!({"pass_rate": 0.1})),
            &two_of_three,
            false,
        );
        assert!(!s.ok);
        assert_eq!(s.required, 1.0);
        let s = summarize(
            &case("single", json!({"gate": false})),
            &two_of_three,
            false,
        );
        assert!(!s.ok && !s.gate);
        assert!(summarize(&case("single", json!({"gate": false})), &[], true).gate);
        assert!(!summarize(&case("single", json!({})), &[], true).ok);
    }

    #[test]
    fn the_summary_counts_gating_failures_only() {
        let base = summarize(
            &case("single", json!({})),
            &[row("c", Kind::Single, &[], true)],
            false,
        );
        let failing = CaseSummary {
            ok: false,
            failure: Some("why".into()),
            ..base.clone()
        };
        let advisory = CaseSummary {
            gate: false,
            ..failing.clone()
        };
        let safety = CaseSummary {
            kind: Kind::Safety,
            ..base.clone()
        };
        let mut out = Vec::new();
        let failed = print_summary(&[base, failing, advisory, safety], &mut out).unwrap();
        let out = String::from_utf8(out).unwrap();
        assert_eq!(failed, 1);
        assert!(out.contains("  FAIL\n    why\n"), "{out}");
        assert!(out.contains("FAIL (advisory)"), "{out}");
        assert!(out.contains("   all  PASS"), "{out}");
        assert!(out.contains("100%  PASS"), "{out}");
        let tail = "2/4 cases met their threshold; 1 gating failures\n";
        assert!(out.ends_with(tail), "{out}");
    }

    #[test]
    fn rows_round_trip_through_jsonl() {
        let dir = std::env::temp_dir().join(format!("athena-rows-{}", uuid::Uuid::new_v4()));
        let path = dir.join("out/results.jsonl");
        let rows = vec![
            row("a", Kind::Safety, &["t"], true),
            row("b", Kind::Multi, &[], false),
        ];
        write_rows(&path, &rows).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text.lines().count(), 2);
        assert!(text.contains(r#""kind":"safety""#));
        std::fs::write(&path, text + "\n\nnot json\n").unwrap();
        let err = format!("{:#}", read_rows(&path).unwrap_err());
        let tail = "line 5: expected ident at line 1 column 2";
        assert!(err.ends_with(tail), "{err}");
        write_rows(&path, &rows[..1]).unwrap();
        assert_eq!(read_rows(&path).unwrap()[0].case_id, "a");

        assert!(read_rows(&dir.join("none")).is_err());
        let err = write_rows(&path.join("x"), &rows).unwrap_err();
        assert!(format!("{err:#}").contains("creating"));
        let err = write_rows(&dir, &rows).unwrap_err();
        assert!(format!("{err:#}").contains("writing"));
        // A bare file name has no directory to create.
        let err = write_rows(Path::new(""), &rows).unwrap_err();
        assert!(format!("{err:#}").contains("writing"));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn compare_flags_any_safety_drop_and_big_quality_drops() {
        use Kind::*;
        let base = vec![
            row("safe", Safety, &["p0"], true),
            row("safe", Safety, &["p0"], true),
            row("q", Single, &["core"], true),
            row("q", Single, &["core"], true),
            row("small", Single, &["x"], true),
            row("gone", Safety, &[], true),
            row("dropped", Single, &[], true),
        ];
        let mut out = Vec::new();
        assert!(compare(&base, &base, &mut out).unwrap().is_empty());

        let candidate = vec![
            row("safe", Safety, &["p0"], true),
            row("safe", Safety, &["p0"], false),
            row("q", Single, &["core"], true),
            row("q", Single, &["core"], false),
            row("small", Single, &["x"], true),
            row("fresh", Single, &[], true),
        ];
        let mut out = Vec::new();
        let regressions = compare(&base, &candidate, &mut out).unwrap();
        assert_eq!(
            regressions,
            ["case gone", "case q", "case safe", "tag core", "tag p0"]
        );
        let out = String::from_utf8(out).unwrap();
        assert!(out.contains("REGRESSION (missing)"), "{out}");
        assert!(out.contains("dropped") && out.contains("missing"), "{out}");
        assert!(out.contains("new"), "{out}");
        assert!(out.contains("   -50  REGRESSION"), "{out}");
    }

    #[test]
    fn a_small_safety_drop_regresses_where_a_small_quality_drop_does_not() {
        let many = |n_pass: usize, n: usize, kind: Kind| -> Vec<Row> {
            (0..n).map(|i| row("c", kind, &[], i < n_pass)).collect()
        };
        let mut out = Vec::new();
        let quality = compare(
            &many(50, 50, Kind::Single),
            &many(48, 50, Kind::Single),
            &mut out,
        );
        assert!(quality.unwrap().is_empty());
        let safety = compare(
            &many(50, 50, Kind::Safety),
            &many(49, 50, Kind::Safety),
            &mut out,
        );
        assert_eq!(safety.unwrap(), ["case c"]);
    }
}
