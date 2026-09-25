//! Settings from a `.env` file, for secrets that should not live in a shell
//! profile or a command line.

use anyhow::{Context, Result};
use std::path::Path;

/// Load `path` into the process environment, if it exists.
///
/// A variable already set in the environment wins over the file, so a
/// one-off `ATHENA_DB=/tmp/x.db athena ...` still works. A missing file is
/// fine; a malformed one is an error that names the file.
///
/// Call it before starting any threads: it sets environment variables,
/// which is only sound while nothing else can be reading them.
pub fn load(path: impl AsRef<Path>) -> Result<()> {
    let path = path.as_ref();
    match dotenvy::from_path(path) {
        Err(e) if e.not_found() => Ok(()),
        loaded => loaded.with_context(|| format!("reading {}", path.display())),
    }
}
