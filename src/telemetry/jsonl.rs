//! Spans and log records as JSON Lines files, for DuckDB
//! (`analytics/queries/`).
//!
//! With `ATHENA_TELEMETRY_DIR` set, every exported batch is appended to
//! `traces-YYYYMMDD.jsonl` or `logs-YYYYMMDD.jsonl` in that directory, named
//! after the UTC day the batch was written. The first write of a day deletes
//! files of that signal older than the retention: with 30 days, today's file
//! and the 29 before it are kept.
//!
//! The schema is Athena's own and flat, not OTLP/JSON: one object per line.
//! Spans:
//!
//! | field | JSON type | |
//! |---|---|---|
//! | `trace_id`, `span_id` | string | lowercase hex |
//! | `parent_span_id` | string or null | null for a root span |
//! | `name` | string | |
//! | `kind` | string | `internal`, `server`, `client`, `producer`, `consumer` |
//! | `start_unix_nano`, `end_unix_nano` | integer | |
//! | `duration_ms` | number | |
//! | `status` | string | `unset`, `ok` or `error` |
//! | `status_message` | string or null | the error description |
//! | `attributes` | object | attribute name to value |
//! | `events` | array | `{name, time_unix_nano, attributes}` |
//! | `scope` | string | instrumentation scope, e.g. `athena` or rig's |
//! | `resource` | object | `service.name`, `service.version`, `deployment.environment.name` |
//!
//! Log records: `time_unix_nano`, `severity` (`INFO`...), `severity_number`,
//! `target`, `body`, `trace_id` and `span_id` (null outside a span),
//! `attributes`, `scope`, `resource`.
//!
//! A batch goes to the file of the day it is written, so a span that ended
//! just before midnight UTC can land in the next day's file.
//!
//! Exporters run on the batch processors' own threads, so the blocking file
//! writes never run on the async runtime. Each batch is written with one
//! `write_all` to a file opened with `O_APPEND`: on a local Linux file system
//! (ext4, xfs) that is one `write` call, which the kernel keeps whole, so
//! prod's two processes (serve and telegram) can share a day's file without
//! splitting each other's lines. POSIX does not promise it for every size
//! and file system.
//!
//! Pruning never costs data: a batch is written first, and a failed prune is
//! reported as the export's error once, on the day's first write.

use opentelemetry::logs::AnyValue;
use opentelemetry::trace::{SpanId, SpanKind, Status};
use opentelemetry::{KeyValue, Value as OtelValue};
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::error::{OTelSdkError, OTelSdkResult};
use opentelemetry_sdk::logs::{LogBatch, LogExporter, SdkLogRecord};
use opentelemetry_sdk::trace::{SpanData, SpanExporter};
use serde_json::{Map, Value, json};
use std::fmt::Debug;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::num::NonZeroU32;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Where the current time comes from; tests move it by hand.
pub trait Clock: Send + Sync + Debug {
    fn now(&self) -> SystemTime;
}

/// The system clock.
#[derive(Debug)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> SystemTime {
        SystemTime::now()
    }
}

/// The span and log exporters writing to one directory.
pub fn exporters(
    dir: &Path,
    retention_days: NonZeroU32,
    clock: Arc<dyn Clock>,
) -> io::Result<super::Exporters<SpanFiles, LogFiles>> {
    fs::create_dir_all(dir)?;
    let files = |prefix| DailyFiles {
        dir: dir.to_path_buf(),
        prefix,
        retention_days,
        clock: clock.clone(),
        open: Mutex::new(None),
    };
    Ok(super::Exporters {
        spans: SpanFiles {
            files: files("traces"),
            resource: Map::new(),
        },
        logs: LogFiles {
            files: files("logs"),
            resource: Map::new(),
        },
    })
}

/// Appends lines to `<prefix>-YYYYMMDD.jsonl`, one file per UTC day.
#[derive(Debug)]
struct DailyFiles {
    dir: PathBuf,
    prefix: &'static str,
    retention_days: NonZeroU32,
    clock: Arc<dyn Clock>,
    /// The day (days since the epoch) and file written to last.
    open: Mutex<Option<(u64, File)>>,
}

impl DailyFiles {
    fn append(&self, lines: &str) -> io::Result<()> {
        if lines.is_empty() {
            return Ok(());
        }
        let today = day(self.clock.now());
        let mut open = self.open.lock().unwrap_or_else(PoisonError::into_inner);
        let mut pruned = Ok(());
        if !matches!(&*open, Some((day, _)) if *day == today) {
            // Yesterday's file closes here.
            *open = None;
            let path = self
                .dir
                .join(format!("{}-{}.jsonl", self.prefix, date(today)));
            let file = OpenOptions::new()
                .create(true)
                .append(true)
                .mode(0o640)
                .open(path)?;
            *open = Some((today, file));
            pruned = self.prune(today);
        }
        let (_, file) = open.as_mut().expect("opened above");
        file.write_all(lines.as_bytes())?;
        pruned
    }

    /// Delete this signal's files dated before the retention window.
    fn prune(&self, today: u64) -> io::Result<()> {
        let oldest_kept = date(today.saturating_sub(u64::from(self.retention_days.get() - 1)));
        for entry in fs::read_dir(&self.dir)? {
            self.prune_entry(&entry?.path(), &oldest_kept)?;
        }
        Ok(())
    }

    /// Delete `path` if it is this signal's file for a day before
    /// `oldest_kept` (`YYYYMMDD`).
    fn prune_entry(&self, path: &Path, oldest_kept: &str) -> io::Result<()> {
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        let dated = name
            .strip_prefix(self.prefix)
            .and_then(|rest| rest.strip_prefix('-'))
            .and_then(|rest| rest.strip_suffix(".jsonl"))
            .filter(|d| d.len() == 8 && d.bytes().all(|b| b.is_ascii_digit()));
        // YYYYMMDD strings sort like the dates they name.
        if dated.is_none_or(|d| d >= oldest_kept) {
            return Ok(());
        }
        match fs::remove_file(path) {
            // The other process of this env may have pruned it first.
            Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        }
    }

    fn write(&self, lines: String) -> OTelSdkResult {
        self.append(&lines).map_err(|e| {
            OTelSdkError::InternalFailure(format!("writing {}: {e}", self.dir.display()))
        })
    }
}

fn since_epoch(time: SystemTime) -> Duration {
    time.duration_since(UNIX_EPOCH).unwrap_or_default()
}

fn nanos(time: SystemTime) -> u64 {
    u64::try_from(since_epoch(time).as_nanos()).unwrap_or(u64::MAX)
}

/// Days since the Unix epoch, in UTC.
fn day(time: SystemTime) -> u64 {
    since_epoch(time).as_secs() / 86_400
}

/// `YYYYMMDD` for a day number (Howard Hinnant's `civil_from_days`).
fn date(day: u64) -> String {
    let z = day + 719_468;
    let era = z / 146_097;
    let doe = z % 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + u64::from(m <= 2);
    format!("{y:04}{m:02}{d:02}")
}

fn resource_map(resource: &Resource) -> Map<String, Value> {
    resource
        .iter()
        .map(|(k, v)| (k.to_string(), attribute_value(v)))
        .collect()
}

fn attribute_value(value: &OtelValue) -> Value {
    match value {
        OtelValue::Bool(b) => json!(b),
        OtelValue::I64(i) => json!(i),
        // NaN and infinities become null.
        OtelValue::F64(f) => json!(f),
        OtelValue::String(s) => json!(s.as_str()),
        other => json!(other.to_string()),
    }
}

fn attributes(attributes: &[KeyValue]) -> Map<String, Value> {
    attributes
        .iter()
        .map(|kv| (kv.key.to_string(), attribute_value(&kv.value)))
        .collect()
}

fn any_value(value: &AnyValue) -> Value {
    match value {
        AnyValue::Int(i) => json!(i),
        AnyValue::Double(f) => json!(f),
        AnyValue::String(s) => json!(s.as_str()),
        AnyValue::Boolean(b) => json!(b),
        other => json!(format!("{other:?}")),
    }
}

fn kind(kind: &SpanKind) -> &'static str {
    match kind {
        SpanKind::Internal => "internal",
        SpanKind::Server => "server",
        SpanKind::Client => "client",
        SpanKind::Producer => "producer",
        SpanKind::Consumer => "consumer",
    }
}

fn span_line(span: &SpanData, resource: &Map<String, Value>) -> Value {
    let (status, message) = match &span.status {
        Status::Unset => ("unset", None),
        Status::Ok => ("ok", None),
        Status::Error { description } => ("error", Some(description.to_string())),
    };
    let duration = span.end_time.duration_since(span.start_time);
    let events: Vec<Value> = span
        .events
        .iter()
        .map(|event| {
            json!({
                "name": event.name,
                "time_unix_nano": nanos(event.timestamp),
                "attributes": attributes(&event.attributes),
            })
        })
        .collect();
    json!({
        "trace_id": span.span_context.trace_id().to_string(),
        "span_id": span.span_context.span_id().to_string(),
        "parent_span_id": (span.parent_span_id != SpanId::INVALID)
            .then(|| span.parent_span_id.to_string()),
        "name": span.name,
        "kind": kind(&span.span_kind),
        "start_unix_nano": nanos(span.start_time),
        "end_unix_nano": nanos(span.end_time),
        "duration_ms": duration.unwrap_or_default().as_secs_f64() * 1000.0,
        "status": status,
        "status_message": message,
        "attributes": attributes(&span.attributes),
        "events": events,
        "scope": span.instrumentation_scope.name(),
        "resource": resource,
    })
}

fn log_line(record: &SdkLogRecord, scope: &str, resource: &Map<String, Value>) -> Value {
    let time = record.timestamp().or(record.observed_timestamp());
    let trace = record.trace_context();
    let attributes: Map<String, Value> = record
        .attributes_iter()
        .map(|(k, v)| (k.to_string(), any_value(v)))
        .collect();
    json!({
        "time_unix_nano": time.map(nanos),
        "severity": record.severity_text(),
        "severity_number": record.severity_number().map(|s| s as i32),
        "target": record.target().map(|t| t.to_string()),
        "body": record.body().map(any_value),
        "trace_id": trace.map(|t| t.trace_id.to_string()),
        "span_id": trace.map(|t| t.span_id.to_string()),
        "attributes": attributes,
        "scope": scope,
        "resource": resource,
    })
}

fn lines(values: impl Iterator<Item = Value>) -> String {
    values.map(|v| format!("{v}\n")).collect()
}

/// Writes spans to `traces-YYYYMMDD.jsonl`.
#[derive(Debug)]
pub struct SpanFiles {
    files: DailyFiles,
    resource: Map<String, Value>,
}

impl SpanExporter for SpanFiles {
    fn export(
        &self,
        batch: Vec<SpanData>,
    ) -> impl std::future::Future<Output = OTelSdkResult> + Send {
        let text = lines(batch.iter().map(|span| span_line(span, &self.resource)));
        std::future::ready(self.files.write(text))
    }

    fn set_resource(&mut self, resource: &Resource) {
        self.resource = resource_map(resource);
    }
}

/// Writes log records to `logs-YYYYMMDD.jsonl`.
#[derive(Debug)]
pub struct LogFiles {
    files: DailyFiles,
    resource: Map<String, Value>,
}

impl LogExporter for LogFiles {
    fn export(
        &self,
        batch: LogBatch<'_>,
    ) -> impl std::future::Future<Output = OTelSdkResult> + Send {
        let text = lines(
            batch
                .iter()
                .map(|(record, scope)| log_line(record, scope.name(), &self.resource)),
        );
        std::future::ready(self.files.write(text))
    }

    fn set_resource(&mut self, resource: &Resource) {
        self.resource = resource_map(resource);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use opentelemetry::logs::{LogRecord as _, Logger as _, LoggerProvider as _, Severity};
    use opentelemetry::trace::{Event, SpanContext, TraceFlags, TraceId, TraceState};
    use opentelemetry::{Array, InstrumentationScope};
    use opentelemetry_sdk::logs::SdkLoggerProvider;
    use opentelemetry_sdk::trace::{SpanEvents, SpanLinks};
    use std::collections::BTreeSet;

    /// 2026-09-25T12:00:00Z.
    const NOON: u64 = 1_790_337_600;
    const DAY: u64 = 86_400;

    fn days(n: u32) -> NonZeroU32 {
        NonZeroU32::new(n).unwrap()
    }

    #[derive(Debug)]
    struct FakeClock(Mutex<SystemTime>);

    impl FakeClock {
        fn at(secs: u64) -> Arc<Self> {
            Arc::new(Self(Mutex::new(UNIX_EPOCH + Duration::from_secs(secs))))
        }

        fn advance(&self, secs: u64) {
            *self.0.lock().unwrap() += Duration::from_secs(secs);
        }
    }

    impl Clock for FakeClock {
        fn now(&self) -> SystemTime {
            *self.0.lock().unwrap()
        }
    }

    /// A fresh directory under the system temp dir, removed on drop.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            let name = format!("athena-jsonl-{}", uuid::Uuid::new_v4());
            Self(std::env::temp_dir().join(name))
        }

        fn files(&self) -> BTreeSet<String> {
            fs::read_dir(&self.0)
                .unwrap()
                .map(|e| e.unwrap().file_name().into_string().unwrap())
                .collect()
        }

        fn lines(&self, name: &str) -> Vec<Value> {
            fs::read_to_string(self.0.join(name))
                .unwrap()
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect()
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn at(secs: u64, millis: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(secs) + Duration::from_millis(millis)
    }

    fn span(name: &'static str, parent: u64, status: Status) -> SpanData {
        let mut events = SpanEvents::default();
        events.events.push(Event::new(
            "retry",
            at(NOON, 50),
            vec![KeyValue::new("attempt", 2)],
            0,
        ));
        SpanData {
            span_context: SpanContext::new(
                TraceId::from(0x0af7_6519_16cd_43dd_8448_eb21_1c80_319c),
                SpanId::from(0xb7ad_6b71_6920_3331),
                TraceFlags::SAMPLED,
                false,
                TraceState::default(),
            ),
            parent_span_id: SpanId::from(parent),
            parent_span_is_remote: false,
            span_kind: SpanKind::Client,
            name: name.into(),
            start_time: at(NOON, 0),
            end_time: at(NOON, 1500),
            attributes: vec![
                KeyValue::new("gen_ai.tool.name", "add"),
                KeyValue::new("gen_ai.usage.input_tokens", 12),
                KeyValue::new("sampled", true),
                KeyValue::new("ratio", 0.5),
            ],
            dropped_attributes_count: 0,
            events,
            links: SpanLinks::default(),
            status,
            instrumentation_scope: InstrumentationScope::builder("rig").build(),
        }
    }

    fn resource() -> Resource {
        Resource::builder_empty()
            .with_attributes([
                KeyValue::new("service.name", "athena"),
                KeyValue::new("service.version", "v1.2.3"),
                KeyValue::new("deployment.environment.name", "prod"),
            ])
            .build()
    }

    fn export_spans(exporter: &SpanFiles, batch: Vec<SpanData>) -> OTelSdkResult {
        futures_util::FutureExt::now_or_never(exporter.export(batch)).expect("synchronous")
    }

    #[test]
    fn dates_are_utc_calendar_days() {
        assert_eq!(date(0), "19700101");
        assert_eq!(date(1095), "19721231");
        assert_eq!(date(11_017), "20000301");
        assert_eq!(date(19_782), "20240229");
        assert_eq!(date(20_721), "20260925");
        assert_eq!(date(47_482), "21000101");
        assert_eq!(day(at(NOON, 0)), 20_721);
        // Before the epoch counts as the epoch.
        let before = UNIX_EPOCH - Duration::from_secs(5);
        assert_eq!((day(before), nanos(before)), (0, 0));
        assert!(SystemClock.now() > at(NOON - 365 * DAY, 0));
    }

    #[test]
    fn a_span_is_one_flat_json_line() {
        let dir = TempDir::new();
        let mut files = exporters(&dir.0, days(30), FakeClock::at(NOON)).unwrap();
        files.spans.set_resource(&resource());
        let failed = span("execute_tool", 0x00f0_67aa_0ba9_02b7, Status::error("boom"));
        let root = span("invoke_agent athena", 0, Status::Ok);
        let unset = span("chat", 1, Status::Unset);
        export_spans(&files.spans, vec![failed, root, unset]).unwrap();

        let lines = dir.lines("traces-20260925.jsonl");
        assert_eq!(
            lines[0],
            json!({
                "trace_id": "0af7651916cd43dd8448eb211c80319c",
                "span_id": "b7ad6b7169203331",
                "parent_span_id": "00f067aa0ba902b7",
                "name": "execute_tool",
                "kind": "client",
                "start_unix_nano": 1_790_337_600_000_000_000_u64,
                "end_unix_nano": 1_790_337_601_500_000_000_u64,
                "duration_ms": 1500.0,
                "status": "error",
                "status_message": "boom",
                "attributes": {
                    "gen_ai.tool.name": "add",
                    "gen_ai.usage.input_tokens": 12,
                    "sampled": true,
                    "ratio": 0.5,
                },
                "events": [{
                    "name": "retry",
                    "time_unix_nano": 1_790_337_600_050_000_000_u64,
                    "attributes": {"attempt": 2},
                }],
                "scope": "rig",
                "resource": {
                    "service.name": "athena",
                    "service.version": "v1.2.3",
                    "deployment.environment.name": "prod",
                },
            })
        );
        assert_eq!(lines[1]["parent_span_id"], Value::Null);
        assert_eq!(lines[1]["status"], "ok");
        assert_eq!(lines[1]["status_message"], Value::Null);
        assert_eq!(lines[2]["status"], "unset");
        assert_eq!(lines.len(), 3);
    }

    #[test]
    fn every_kind_and_value_type_has_a_json_form() {
        let kinds = [
            SpanKind::Internal,
            SpanKind::Server,
            SpanKind::Client,
            SpanKind::Producer,
            SpanKind::Consumer,
        ];
        let names: Vec<&str> = kinds.iter().map(kind).collect();
        assert_eq!(
            names,
            ["internal", "server", "client", "producer", "consumer"]
        );

        assert_eq!(attribute_value(&OtelValue::F64(f64::NAN)), Value::Null);
        let list = OtelValue::Array(Array::I64(vec![1, 2]));
        assert_eq!(attribute_value(&list), json!("[1,2]"));

        assert_eq!(any_value(&AnyValue::Int(3)), json!(3));
        assert_eq!(any_value(&AnyValue::Double(0.25)), json!(0.25));
        assert_eq!(any_value(&AnyValue::Boolean(false)), json!(false));
        assert_eq!(any_value(&AnyValue::String("hi".into())), json!("hi"));
        let bytes = AnyValue::Bytes(Box::new(vec![1]));
        assert_eq!(any_value(&bytes), json!("Bytes([1])"));

        // A span that ends before it starts has no duration, not a panic.
        let mut backwards = span("s", 0, Status::Unset);
        backwards.end_time = UNIX_EPOCH;
        assert_eq!(span_line(&backwards, &Map::new())["duration_ms"], 0.0);
    }

    #[test]
    fn a_log_record_is_one_flat_json_line() {
        let dir = TempDir::new();
        let files = exporters(&dir.0, days(30), FakeClock::at(NOON)).unwrap();
        let provider = SdkLoggerProvider::builder()
            .with_resource(resource())
            .with_simple_exporter(files.logs)
            .build();
        let logger = provider.logger("athena");

        let mut record = logger.create_log_record();
        record.set_timestamp(at(NOON, 7));
        record.set_severity_number(Severity::Error);
        record.set_severity_text("ERROR");
        record.set_target("athena::http");
        record.set_body(AnyValue::from("turn failed"));
        record.add_attribute("error.type", "model");
        record.set_trace_context(
            TraceId::from(0x0af7_6519_16cd_43dd_8448_eb21_1c80_319c),
            SpanId::from(0xb7ad_6b71_6920_3331),
            None,
        );
        logger.emit(record);
        logger.emit(logger.create_log_record());
        provider.shutdown().unwrap();

        let lines = dir.lines("logs-20260925.jsonl");
        assert_eq!(
            lines[0],
            json!({
                "time_unix_nano": 1_790_337_600_007_000_000_u64,
                "severity": "ERROR",
                "severity_number": 17,
                "target": "athena::http",
                "body": "turn failed",
                "trace_id": "0af7651916cd43dd8448eb211c80319c",
                "span_id": "b7ad6b7169203331",
                "attributes": {"error.type": "model"},
                "scope": "athena",
                "resource": {
                    "service.name": "athena",
                    "service.version": "v1.2.3",
                    "deployment.environment.name": "prod",
                },
            })
        );
        // Everything unset: the SDK stamps the observed time.
        let bare = &lines[1];
        assert!(bare["time_unix_nano"].is_u64(), "{bare}");
        for field in [
            "severity",
            "severity_number",
            "target",
            "body",
            "trace_id",
            "span_id",
        ] {
            assert_eq!(bare[field], Value::Null, "{field}: {bare}");
        }
    }

    #[test]
    fn a_new_day_opens_a_new_file_and_prunes_files_past_the_retention() {
        let dir = TempDir::new();
        let clock = FakeClock::at(NOON);
        let files = exporters(&dir.0, days(30), clock.clone()).unwrap();
        let keep = [
            "traces-20260827.jsonl", // 29 days before: inside 30 days
            "logs-20200101.jsonl",   // another signal's file
            "traces-2026082.jsonl",  // not a date
            "traces-2026082x.jsonl",
            "traces.jsonl",
            "notes.txt",
        ];
        let expired = ["traces-20260826.jsonl", "traces-19991231.jsonl"];
        for name in keep.iter().chain(&expired) {
            fs::write(dir.0.join(name), "{}\n").unwrap();
        }

        // Nothing to write: no file, no pruning.
        export_spans(&files.spans, Vec::new()).unwrap();
        assert!(dir.0.join(expired[0]).exists());

        export_spans(&files.spans, vec![span("a", 0, Status::Unset)]).unwrap();
        export_spans(&files.spans, vec![span("b", 0, Status::Unset)]).unwrap();
        let mut want: BTreeSet<String> = keep.iter().map(|s| s.to_string()).collect();
        want.insert("traces-20260925.jsonl".into());
        assert_eq!(dir.files(), want);

        clock.advance(DAY);
        export_spans(&files.spans, vec![span("c", 0, Status::Unset)]).unwrap();
        // The day change pruned again: 20260827 is now 30 days old.
        want.remove("traces-20260827.jsonl");
        want.insert("traces-20260926.jsonl".into());
        assert_eq!(dir.files(), want);

        let names = |file| {
            dir.lines(file)
                .iter()
                .map(|l| l["name"].clone())
                .collect::<Vec<_>>()
        };
        assert_eq!(names("traces-20260925.jsonl"), ["a", "b"]);
        assert_eq!(names("traces-20260926.jsonl"), ["c"]);
        let mode = fs::metadata(dir.0.join("traces-20260925.jsonl"))
            .unwrap()
            .permissions();
        assert_eq!(
            std::os::unix::fs::PermissionsExt::mode(&mode) & 0o777,
            0o640
        );
    }

    #[test]
    fn a_failed_prune_is_reported_once_and_loses_no_spans() {
        let dir = TempDir::new();
        let files = exporters(&dir.0, days(1), FakeClock::at(NOON)).unwrap();
        // A directory where an expired file would be: removing it fails.
        fs::create_dir(dir.0.join("traces-20260924.jsonl")).unwrap();
        let error = export_spans(&files.spans, vec![span("a", 0, Status::Unset)])
            .unwrap_err()
            .to_string();
        assert!(error.contains("writing"), "{error}");
        // The rest of the day is not pruned again, so it writes cleanly.
        export_spans(&files.spans, vec![span("b", 0, Status::Unset)]).unwrap();
        let names: Vec<Value> = dir
            .lines("traces-20260925.jsonl")
            .iter()
            .map(|l| l["name"].clone())
            .collect();
        assert_eq!(names, ["a", "b"]);
    }

    #[test]
    fn a_directory_that_is_gone_fails_the_export() {
        let dir = TempDir::new();
        let files = exporters(&dir.0, days(1), FakeClock::at(NOON)).unwrap();

        fs::remove_dir_all(&dir.0).unwrap();
        let error = export_spans(&files.spans, vec![span("a", 0, Status::Unset)])
            .unwrap_err()
            .to_string();
        assert!(error.contains(&dir.0.display().to_string()), "{error}");
    }

    #[test]
    fn a_file_the_other_process_pruned_first_is_not_an_error() {
        let dir = TempDir::new();
        let files = exporters(&dir.0, days(1), FakeClock::at(NOON)).unwrap();
        let old = dir.0.join("traces-20260924.jsonl");
        fs::write(&old, "{}\n").unwrap();
        files.spans.files.prune_entry(&old, "20260925").unwrap();
        assert!(!old.exists());
        // Gone already, as when serve and telegram both prune.
        files.spans.files.prune_entry(&old, "20260925").unwrap();
    }

    /// The analytics fixtures are what this exporter writes: same fields.
    #[test]
    fn the_analytics_fixtures_have_the_exporters_fields() {
        let keys = |v: &Value| {
            v.as_object()
                .unwrap()
                .keys()
                .cloned()
                .collect::<BTreeSet<_>>()
        };
        let want = keys(&span_line(&span("s", 0, Status::Unset), &Map::new()));
        let testdata = Path::new(env!("CARGO_MANIFEST_DIR")).join("analytics/testdata");
        let mut checked = 0;
        for entry in fs::read_dir(testdata).unwrap() {
            let path = entry.unwrap().path();
            let name = path.file_name().unwrap().to_str().unwrap().to_string();
            if !name.starts_with("traces-") {
                continue;
            }
            for line in fs::read_to_string(&path).unwrap().lines() {
                let line: Value = serde_json::from_str(line).unwrap();
                assert_eq!(keys(&line), want, "{name}: {line}");
                checked += 1;
            }
        }
        assert!(checked > 0, "no traces-*.jsonl fixtures");
    }
}
