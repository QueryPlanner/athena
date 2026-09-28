//! `athena-cli new`: a new agent's repository, from the template in
//! `templates/agent/`, compiled into this binary so it works anywhere.

use std::fs;
use std::io;
use std::path::Path;

/// `(path in the new repository, contents)`. `@NAME@` and `@CORE_DEP@` are
/// filled in by [`render`].
pub const FILES: &[(&str, &str)] = &[
    (
        "Cargo.toml",
        include_str!("../templates/agent/Cargo.toml.in"),
    ),
    (
        "src/main.rs",
        include_str!("../templates/agent/src/main.rs"),
    ),
    ("src/lib.rs", include_str!("../templates/agent/src/lib.rs")),
    (
        "src/agent.rs",
        include_str!("../templates/agent/src/agent.rs"),
    ),
    (
        "prompts/system.md",
        include_str!("../templates/agent/prompts/system.md"),
    ),
    (
        "tests/agent.rs",
        include_str!("../templates/agent/tests/agent.rs"),
    ),
    ("evals/cases/.gitkeep", ""),
    (
        ".github/workflows/ci-cd.yml",
        include_str!("../templates/agent/.github/workflows/ci-cd.yml"),
    ),
    // The same action Athena's own CI deploys with.
    (
        ".github/actions/gate/action.yml",
        include_str!("../../../.github/actions/gate/action.yml"),
    ),
    ("AGENTS.md", include_str!("../templates/agent/AGENTS.md")),
    (
        ".agents/skills/athena-agent/SKILL.md",
        include_str!("../templates/agent/.agents/skills/athena-agent/SKILL.md"),
    ),
    ("README.md", include_str!("../templates/agent/README.md")),
    (".gitignore", include_str!("../templates/agent/.gitignore")),
    (
        ".env.example",
        include_str!("../templates/agent/.env.example"),
    ),
];

/// Where the new agent gets `athena-core` from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Core {
    /// A git repository at a branch; `Cargo.lock` then pins the commit.
    Branch { git: String, branch: String },
    /// A git repository at one commit.
    Rev { git: String, rev: String },
    /// A local checkout, for trying a change to the runtime.
    Path(String),
}

pub const DEFAULT_GIT: &str = "https://github.com/QueryPlanner/athena";

impl Default for Core {
    fn default() -> Self {
        Core::Branch {
            git: DEFAULT_GIT.into(),
            branch: "main".into(),
        }
    }
}

impl Core {
    /// The inside of the `athena-core = { ... }` dependency.
    pub fn dependency(&self) -> String {
        match self {
            Core::Branch { git, branch } => format!("git = {git:?}, branch = {branch:?}"),
            Core::Rev { git, rev } => format!("git = {git:?}, rev = {rev:?}"),
            Core::Path(path) => format!("path = {path:?}"),
        }
    }
}

/// A name that works as a crate, a path component, a user (`athena-<name>`)
/// and a systemd unit: the same rule as `athena_core::spec::valid_name`.
/// Athena itself is taken.
pub fn check_name(name: &str) -> Result<(), String> {
    let mut chars = name.chars();
    let valid = chars.next().is_some_and(|c| c.is_ascii_lowercase())
        && name.len() <= 24
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit());
    if !valid {
        return Err(format!(
            "`{name}` is not an agent name: a lowercase letter, then up to 23 lowercase letters or digits"
        ));
    }
    if name == "athena" {
        return Err("`athena` is the first agent's name; pick another".into());
    }
    Ok(())
}

pub fn render(template: &str, name: &str, core: &Core) -> String {
    template
        .replace("@NAME@", name)
        .replace("@CORE_DEP@", &core.dependency())
}

/// Write the new agent's files into `dir`, which must not exist or be
/// empty. Returns the paths written, relative to `dir`.
pub fn write(dir: &Path, name: &str, core: &Core) -> io::Result<Vec<&'static str>> {
    if dir.exists() && fs::read_dir(dir)?.next().is_some() {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("{} exists and is not empty", dir.display()),
        ));
    }
    let mut written = Vec::new();
    for (path, template) in FILES {
        let target = dir.join(path);
        fs::create_dir_all(target.parent().unwrap_or(dir))?;
        fs::write(&target, render(template, name, core))?;
        written.push(*path);
    }
    Ok(written)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "athena-cli-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn names_follow_the_vm_rule_and_athena_is_taken() {
        assert!(check_name("notes").is_ok());
        assert!(check_name("a1").is_ok());
        for bad in [
            "",
            "1a",
            "Notes",
            "my-agent",
            "a_b",
            "../x",
            &"a".repeat(25),
        ] {
            let refused = check_name(bad)
                .unwrap_err()
                .contains("is not an agent name");
            assert!(refused, "{bad}");
        }
        assert!(check_name("athena").unwrap_err().contains("first agent"));
    }

    #[test]
    fn the_core_dependency_has_three_forms() {
        assert_eq!(
            Core::default().dependency(),
            r#"git = "https://github.com/QueryPlanner/athena", branch = "main""#
        );
        let rev = Core::Rev {
            git: "https://x/y".into(),
            rev: "abc".into(),
        };
        assert_eq!(rev.dependency(), r#"git = "https://x/y", rev = "abc""#);
        assert_eq!(
            Core::Path("../core".into()).dependency(),
            r#"path = "../core""#
        );
    }

    #[test]
    fn every_placeholder_is_filled() {
        for (path, template) in FILES {
            let out = render(template, "notes", &Core::default());
            let filled = !out.contains("@NAME@") && !out.contains("@CORE_DEP@");
            assert!(filled, "{path}");
        }
        let cargo = render(FILES[0].1, "notes", &Core::default());
        assert!(cargo.contains("name = \"notes\""), "{cargo}");
        assert!(cargo.contains("athena-core = { git = "), "{cargo}");
    }

    #[test]
    fn write_makes_the_repository() {
        let dir = temp("write");
        let written = write(&dir, "notes", &Core::Path("/core".into())).unwrap();
        assert_eq!(written.len(), FILES.len());
        let main = fs::read_to_string(dir.join("src/main.rs")).unwrap();
        assert!(main.contains("notes::agent::spec()"), "{main}");
        let gate = fs::read_to_string(dir.join(".github/actions/gate/action.yml")).unwrap();
        assert!(gate.contains("name: deploy-gate"));
        assert!(dir.join("evals/cases/.gitkeep").is_file());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn write_refuses_a_directory_with_files_but_takes_an_empty_one() {
        let dir = temp("refuse");
        fs::create_dir_all(&dir).unwrap();
        write(&dir, "notes", &Core::default()).unwrap();
        let err = write(&dir, "notes", &Core::default()).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
        assert!(err.to_string().contains("is not empty"));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn write_reports_a_directory_it_cannot_read() {
        let dir = temp("unreadable");
        fs::write(&dir, "a file").unwrap();
        assert!(write(&dir, "notes", &Core::default()).is_err());
        fs::remove_file(&dir).unwrap();
    }

    #[test]
    fn write_reports_a_path_it_cannot_create() {
        // The target's parent is a file, so nothing can be created under it.
        let file = temp("blocked");
        fs::write(&file, "").unwrap();
        assert!(write(&file.join("repo"), "notes", &Core::default()).is_err());
        fs::remove_file(&file).unwrap();
    }
}
