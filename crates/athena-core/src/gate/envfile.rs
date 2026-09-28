//! `KEY=VALUE` files: `/etc/athena/<env>.env` and `/etc/athena/gate.env`.
//!
//! The parser follows systemd's `EnvironmentFile=` for the subset these
//! files use: blank lines and `#`/`;` comments are skipped, whitespace
//! around the key and value is trimmed, and one pair of matching quotes is
//! removed. There is no `$VAR` expansion, as in systemd, so a secret
//! containing `$` reaches `athena backup` exactly as it reaches the service.

use super::{Context, Result, write_atomic};
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

pub type Vars = Vec<(String, String)>;

pub fn parse(text: &str) -> Vars {
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#') && !line.starts_with(';'))
        .filter_map(|line| line.split_once('='))
        .map(|(key, value)| (key.trim(), unquote(value.trim())))
        .filter(|(key, _)| !key.is_empty())
        .map(|(key, value)| (key.to_string(), value.to_string()))
        .collect()
}

fn unquote(value: &str) -> &str {
    for quote in ['"', '\''] {
        if let Some(inner) = value
            .strip_prefix(quote)
            .and_then(|rest| rest.strip_suffix(quote))
        {
            return inner;
        }
    }
    value
}

/// The last assignment wins, as in systemd.
pub fn get<'a>(vars: &'a Vars, key: &str) -> Option<&'a str> {
    vars.iter()
        .rev()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.as_str())
}

pub fn read(path: &Path) -> Result<Vars> {
    let text = fs::read_to_string(path).context(format!("reading {}", path.display()))?;
    Ok(parse(&text))
}

/// `text` with `key` set to `value`: the first assignment is replaced in
/// place, later ones are dropped, and it is appended if there was none.
/// Every other line is kept as it was.
pub fn with_var(text: &str, key: &str, value: &str) -> String {
    let mut lines = Vec::new();
    let mut set = false;
    for line in text.lines() {
        if !assigns(line, key) {
            lines.push(line.to_string());
        } else if !set {
            lines.push(format!("{key}={value}"));
            set = true;
        }
    }
    if !set {
        lines.push(format!("{key}={value}"));
    }
    lines.join("\n") + "\n"
}

fn assigns(line: &str, key: &str) -> bool {
    line.trim_start()
        .strip_prefix(key)
        .is_some_and(|rest| rest.trim_start().starts_with('='))
}

/// Set `key` in the file at `path`, keeping its mode and owner (the env
/// files are `root:athena 0640` and hold secrets). Returns the old value.
pub fn set_var(path: &Path, key: &str, value: &str) -> Result<Option<String>> {
    let what = || format!("updating {}", path.display());
    let text = fs::read_to_string(path).context(what())?;
    let meta = fs::metadata(path).context(what())?;
    let old = get(&parse(&text), key).map(str::to_string);
    let owner = Some((meta.uid(), meta.gid()));
    write_atomic(
        path,
        with_var(&text, key, value).as_bytes(),
        meta.mode() & 0o7777,
        owner,
    )?;
    Ok(old)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gate::testing::TempRoot;
    use std::os::unix::fs::PermissionsExt;

    fn pairs(list: &[(&str, &str)]) -> Vars {
        list.iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn parses_the_systemd_subset_without_expansion() {
        let text = "# comment\n; also a comment\n\n  A = 1 \nB=\"two words\"\nC='x'\nD=$HOME\nE=\"unbalanced\nnot a pair\n=novalue\nF=a=b\n";
        assert_eq!(
            parse(text),
            pairs(&[
                ("A", "1"),
                ("B", "two words"),
                ("C", "x"),
                ("D", "$HOME"),
                ("E", "\"unbalanced"),
                ("F", "a=b"),
            ])
        );
    }

    #[test]
    fn the_last_assignment_wins() {
        let vars = parse("A=1\nA=2\n");
        assert_eq!(get(&vars, "A"), Some("2"));
        assert_eq!(get(&vars, "B"), None);
    }

    #[test]
    fn setting_a_var_replaces_it_in_place_and_keeps_every_other_line() {
        let text = "# keep\nATHENA_VERSION=old\nATHENA_VERSION_X=keep\nOTHER=1\n  ATHENA_VERSION = older\n";
        assert_eq!(
            with_var(text, "ATHENA_VERSION", "abc"),
            "# keep\nATHENA_VERSION=abc\nATHENA_VERSION_X=keep\nOTHER=1\n"
        );
        assert_eq!(with_var("A=1", "B", "2"), "A=1\nB=2\n");
        assert_eq!(with_var("", "B", "2"), "B=2\n");
    }

    #[test]
    fn set_var_keeps_the_files_mode_and_returns_the_old_value() {
        let tmp = TempRoot::new();
        let path = tmp.path().join("staging.env");
        fs::write(&path, "SECRET=s\nATHENA_VERSION=old\n").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).unwrap();

        assert_eq!(
            set_var(&path, "ATHENA_VERSION", "new").unwrap(),
            Some("old".into())
        );
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "SECRET=s\nATHENA_VERSION=new\n"
        );
        assert_eq!(fs::metadata(&path).unwrap().mode() & 0o7777, 0o640);
        assert_eq!(
            read(&path).unwrap(),
            pairs(&[("SECRET", "s"), ("ATHENA_VERSION", "new")])
        );
    }

    #[test]
    fn a_missing_file_is_an_error_naming_it() {
        let tmp = TempRoot::new();
        let path = tmp.path().join("missing.env");
        let err = set_var(&path, "A", "1").unwrap_err().to_string();
        assert!(err.contains("missing.env"), "{err}");
        assert!(read(&path).unwrap_err().to_string().contains("reading"));
    }

    #[test]
    fn a_file_that_cannot_be_replaced_is_left_as_it_was() {
        let tmp = TempRoot::new();
        let path = tmp.path().join("prod.env");
        fs::write(&path, "A=1\n").unwrap();
        fs::create_dir(tmp.path().join("prod.env.tmp")).unwrap();
        let err = set_var(&path, "A", "2").unwrap_err().to_string();
        assert!(err.starts_with("writing"), "{err}");
        assert_eq!(fs::read_to_string(&path).unwrap(), "A=1\n");
    }
}
