//! `health_export`'s work: the user's raw points, from the store into a
//! SQLite file in their session's sandbox, without ever holding them all in
//! memory, writing them to the host's disk, or showing them to the model.
//!
//! 1. Rows are read from the store in batches by `id`, each bounded in rows
//!    and bytes, with the store's connection free between batches.
//! 2. Each batch is projected ([`super::data::project`]) and appended to
//!    NDJSON; a chunk of about [`CHUNK_BYTES`] is uploaded to
//!    `DATA_DIR/.export-<nonce>/chunk-NNNNN.ndjson` with
//!    [`Sandboxes::write_file`]. `write_file` buffers the body and the sandbox
//!    client gives each request 30 s, so a chunk is small enough to cross a
//!    slow link (4 MiB in 30 s is 1.1 Mbit/s).
//! 3. One Python script written by this crate (`export_loader.py`, uploaded
//!    as `load.py`) builds the database from the chunks and renames it into
//!    place in one step ([`DB_PATH`]), mode 0600, and removes the chunks.
//!
//! The result names the file and its schema, never a value.
use super::data::{MAX_EXPORT_BYTES, project};
use crate::sandbox::{Sandboxes, shell};
use crate::store::{BatchQuery, HealthWindow, PointRecord, Store};
use crate::untrusted;
use jiff::{Timestamp, tz::TimeZone};
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Duration;

/// Where exports live in a sandbox. Not [`crate::sandbox::INBOX_DIR`]:
/// that is for files the user sent.
pub const DATA_DIR: &str = "/tmp/athena-data";
/// The database an export builds, replaced by the next export.
pub const DB_PATH: &str = "/tmp/athena-data/health.sqlite";
/// An uploaded chunk's size, once it is at least this long.
pub const CHUNK_BYTES: usize = 4 * 1024 * 1024;
/// The most points, and bytes of stored values, read from the store at once.
pub const BATCH_ROWS: usize = 2000;
pub const BATCH_BYTES: usize = 2 * 1024 * 1024;
/// How long creating the directory may take.
const SETUP_TIMEOUT: Duration = Duration::from_secs(30);
/// How long building the database may take.
const LOAD_TIMEOUT: Duration = Duration::from_secs(15 * 60);
/// The loader, run once in the sandbox with python3 and nothing else.
pub const LOADER: &str = include_str!("export_loader.py");

/// What to export, and for whom.
pub struct Job {
    pub store: Store,
    pub sandboxes: Arc<Sandboxes>,
    pub session: String,
    pub owner: i64,
    pub zone: TimeZone,
    /// The data types to export: only ones that have points.
    pub types: Vec<&'static str>,
    pub window: HealthWindow,
    pub now: Timestamp,
}

/// The tables and views the loader builds, for the result. Kept here, next
/// to the loader's own copy, and checked against it by a test.
pub fn schema() -> Value {
    json!({
        "tables": {
            "points": ["data_type", "point_key", "start_ms", "end_ms", "civil_date",
                       "source", "value"],
            "meta": ["key", "value"],
        },
        "views": {
            "heart_rate": ["start_ms", "civil_date", "source", "bpm"],
            "steps": ["start_ms", "end_ms", "civil_date", "source", "count"],
            "weight": ["start_ms", "civil_date", "source", "kg"],
            "sleep": ["start_ms", "end_ms", "civil_date", "source", "minutes"],
            "hrv": ["start_ms", "civil_date", "source", "rmssd_ms"],
            "spo2": ["start_ms", "civil_date", "source", "percent"],
        },
        "columns": "start_ms and end_ms are UTC milliseconds since 1970 (a sample has both \
                    the same); civil_date is the user's local day, YYYY-MM-DD; value is the \
                    point's JSON as text (use json_extract), the data type's own fields only",
        "meta": "exported_at, zone, types, from, to, rows (rows per data type)",
    })
}

/// What the model is told about using the file.
const HOW_TO_QUERY: &str = "There is no sqlite3 command in the sandbox. Query with python3: \
    python3 -c \"import sqlite3; d=sqlite3.connect('/tmp/athena-data/health.sqlite'); \
    print(d.execute('select data_type, count(*) from points group by 1').fetchall())\", \
    or sqlite3 and pandas in run_code (pandas.read_sql_query on the views is fine). Use the \
    views (heart_rate, steps, weight, sleep, hrv, spo2) and aggregate in SQL; json_extract \
    reads the other types' value. Do not load every raw value into pandas at once: the \
    sandbox has 1 GiB of memory and minute-level heart rate is hundreds of thousands of \
    rows.";

/// Build the file. The error is what the model is told.
pub async fn run(job: &Job) -> Result<Value, String> {
    let dir = format!("{DATA_DIR}/.export-{}", untrusted::nonce());
    let built = build(job, &dir).await;
    if built.is_err() {
        // Best effort: the chunks are useless without the loader's cleanup.
        let rm = shell::command_line(&["rm", "-rf", &dir])
            .expect("an export directory is a fixed prefix and hex, with no NUL byte");
        let _ = job
            .sandboxes
            .command(&job.session, &rm, SETUP_TIMEOUT)
            .await;
    }
    built
}

/// One point as an NDJSON line, as the loader reads it.
fn line(row: &PointRecord) -> String {
    json!({"t": row.data_type, "k": row.key, "s": row.start_ms, "e": row.end_ms,
           "d": row.civil_date, "src": row.source, "v": project(&row.data_type, &row.value)})
    .to_string()
}

fn sandbox(e: crate::sandbox::Error) -> String {
    e.to_string()
}

async fn build(job: &Job, dir: &str) -> Result<Value, String> {
    let sandboxes = &job.sandboxes;
    let session = job.session.as_str();
    let prepare = format!(
        "mkdir -p {dir} && chmod 700 {data} {dir}",
        dir = shell::quote(dir).map_err(sandbox)?,
        data = shell::quote(DATA_DIR).map_err(sandbox)?
    );
    let made = sandboxes
        .command(session, &prepare, SETUP_TIMEOUT)
        .await
        .map_err(sandbox)?;
    if let Some(error) = made.error {
        return Err(format!("creating {dir} in the sandbox: {error}"));
    }
    let meta = json!({
        "exported_at": job.now.to_string(),
        "zone": crate::timezone::name(&job.zone),
        "types": job.types,
        "from": job.window.from,
        "to": job.window.to,
    });
    for (name, body) in [
        ("load.py", LOADER.to_string()),
        ("meta.json", meta.to_string()),
    ] {
        let path = format!("{dir}/{name}");
        sandboxes
            .write_file(session, &path, body.into_bytes())
            .await
            .map_err(sandbox)?;
    }

    let (mut after_id, mut chunks, mut sent) = (0i64, 0usize, 0usize);
    let mut buffer: Vec<u8> = Vec::new();
    loop {
        let (store, owner, types, window) = (
            job.store.clone(),
            job.owner,
            job.types.clone(),
            job.window.clone(),
        );
        let batch = store
            .call(move |s| {
                s.health_export_batch(
                    owner,
                    &BatchQuery {
                        data_types: &types,
                        window: &window,
                        after_id,
                        max_rows: BATCH_ROWS,
                        max_bytes: BATCH_BYTES,
                    },
                )
            })
            .await
            .map_err(|e| format!("reading the health data: {e:#}"))?;
        let Some(last) = batch.last() else { break };
        after_id = last.id;
        for row in &batch {
            buffer.extend_from_slice(line(row).as_bytes());
            buffer.push(b'\n');
        }
        if buffer.len() >= CHUNK_BYTES {
            chunks += 1;
            sent += upload(job, dir, chunks, &mut buffer, sent).await?;
        }
    }
    if !buffer.is_empty() {
        chunks += 1;
        upload(job, dir, chunks, &mut buffer, sent).await?;
    }

    let command = shell::command_line(&["python3", &format!("{dir}/load.py"), dir, DB_PATH])
        .map_err(sandbox)?;
    let loaded = sandboxes
        .command(session, &command, LOAD_TIMEOUT)
        .await
        .map_err(sandbox)?;
    if let Some(error) = loaded.error {
        return Err(format!(
            "building the database in the sandbox failed: {error} {}",
            crate::sandbox::stream::preview(&loaded.stderr)
        ));
    }
    let done: Value = loaded
        .stdout
        .lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .and_then(|l| serde_json::from_str(l).ok())
        .ok_or_else(|| {
            format!(
                "building the database in the sandbox gave no result: {}",
                crate::sandbox::stream::preview(&loaded.stderr)
            )
        })?;
    let total: i64 = done["rows"]
        .as_object()
        .map_or(0, |m| m.values().filter_map(Value::as_i64).sum());
    let mut out = json!({
        "path": DB_PATH,
        "from": job.window.from,
        "to": job.window.to,
        "timezone": crate::timezone::name(&job.zone),
        "rows": done["rows"],
        "total_rows": total,
        "file_bytes": done["bytes"],
        "how_to_query": HOW_TO_QUERY,
        "expires": "The file lives only in this conversation's sandbox. The sandbox is \
                    deleted after 30 idle minutes, and the file with it: export again if \
                    it is gone.",
        "privacy": "Your whole health history in this range is now in the sandbox, which \
                    has internet access. Do not send it anywhere.",
    });
    for (key, value) in schema().as_object().into_iter().flatten() {
        out[key] = value.clone();
    }
    Ok(out)
}

/// Upload `buffer` as chunk number `n`, emptying it. `sent` is what was
/// uploaded before; the export stops if the total would pass the limit the
/// size estimate allows (it is an estimate from a sample). Returns the bytes
/// sent.
async fn upload(
    job: &Job,
    dir: &str,
    n: usize,
    buffer: &mut Vec<u8>,
    sent: usize,
) -> Result<usize, String> {
    let size = buffer.len();
    if (sent + size) as u64 > MAX_EXPORT_BYTES {
        return Err(format!(
            "the data turned out larger than the {} MiB an export may hold; narrow `types` \
             or the dates",
            MAX_EXPORT_BYTES / 1024 / 1024
        ));
    }
    let path = format!("{dir}/chunk-{n:05}.ndjson");
    job.sandboxes
        .write_file(&job.session, &path, std::mem::take(buffer))
        .await
        .map_err(sandbox)?;
    Ok(size)
}

#[cfg(test)]
mod tests;
