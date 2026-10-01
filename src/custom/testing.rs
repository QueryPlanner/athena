//! Helpers for this module's unit tests.

use std::path::{Path, PathBuf};
use std::sync::Arc;

/// A directory in the system temp dir, removed on drop.
pub struct TempDir(PathBuf);

impl TempDir {
    pub fn new() -> Self {
        let dir = std::env::temp_dir().join(format!("athena-custom-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&dir).unwrap();
        // Resolved once, so tests can compare paths after symlinks are followed.
        Self(dir.canonicalize().unwrap())
    }

    pub fn path(&self) -> &Path {
        &self.0
    }

    pub fn join(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }

    /// Write `contents` to `name`, creating parent directories.
    pub fn write(&self, name: &str, contents: &str) -> PathBuf {
        self.write_bytes(name, contents.as_bytes())
    }

    pub fn write_bytes(&self, name: &str, contents: &[u8]) -> PathBuf {
        let path = self.0.join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, contents).unwrap();
        path
    }

    /// `skills/<name>/SKILL.md` with the given description and body.
    pub fn skill(&self, name: &str, description: &str, body: &str) -> PathBuf {
        let description = description.replace('\n', "\n  ");
        self.write(
            &format!("skills/{name}/SKILL.md"),
            &format!("---\nname: {name}\ndescription: {description}\n---\n{body}\n"),
        )
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Everything `run` logs on this thread, as text. The log goes to a file
/// that is read back, so no writer type of our own is needed.
pub fn capture_logs(run: impl FnOnce()) -> String {
    let dir = TempDir::new();
    let path = dir.join("log.txt");
    let file = Arc::new(std::fs::File::create(&path).unwrap());
    let subscriber = tracing_subscriber::fmt()
        .with_writer(file)
        .with_ansi(false)
        .finish();
    tracing::subscriber::with_default(subscriber, run);
    std::fs::read_to_string(path).unwrap()
}
