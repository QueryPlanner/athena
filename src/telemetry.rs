//! Traces and logs: `tracing` everywhere, OpenTelemetry when configured.
//!
//! Every process logs through `tracing` to stderr in the human-readable
//! format. Spans and log events are also exported to up to two sinks, each
//! switched on by its own variable; with neither set, no OpenTelemetry code
//! runs at all:
//!
//! - `OTEL_EXPORTER_OTLP_ENDPOINT`: OTLP/HTTP (protobuf) straight to a
//!   backend, OpenObserve on the VM, with `OTEL_EXPORTER_OTLP_HEADERS`
//!   carrying its credentials;
//! - `ATHENA_TELEMETRY_DIR`: daily JSON Lines files for DuckDB
//!   (see [`jsonl`]).
//!
//! [`layers`] builds the subscriber's layers from [`Settings`] and injected
//! [`Sinks`], so tests capture spans in memory. [`init`] wires it to the
//! environment and installs it globally, which a process can do only once.
//!
//! What is exported is listed in README "Observability". Everything is: in
//! every environment rig records prompts, the system prompt, replies and
//! tool arguments and results on spans (`agent::configure` turns that on
//! unconditionally), and every sink exports them. Athena's own log events
//! never include prompt or reply text; that content lives on the spans.

pub mod jsonl;

use crate::service::User;
use anyhow::{Context as _, Result, anyhow};
use axum::http::{HeaderMap, HeaderName, HeaderValue};
use opentelemetry::propagation::TextMapPropagator;
use opentelemetry::trace::{TraceContextExt, TracerProvider as _};
use opentelemetry::{Context, KeyValue};
use opentelemetry_appender_tracing::layer::OpenTelemetryTracingBridge;
use opentelemetry_http::HeaderExtractor;
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::logs::{LogExporter, LoggerProviderBuilder, SdkLoggerProvider};
use opentelemetry_sdk::propagation::TraceContextPropagator;
use opentelemetry_sdk::trace::{SdkTracerProvider, SpanExporter, TracerProviderBuilder};
use sha2::{Digest, Sha256};
use std::io::Write;
use std::num::NonZeroU32;
use std::path::PathBuf;
use std::sync::Arc;
use tracing_opentelemetry::OpenTelemetrySpanExt;
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, Layer, Registry};

/// What stderr shows without `RUST_LOG`: Athena's own events from `info`,
/// everyone else's from `warn`. Spans are never printed.
pub const DEFAULT_LOG_FILTER: &str = "warn,athena=info";

/// What is exported. The HTTP stack is silenced so that exporting a batch
/// never produces events that are exported in the next one.
const EXPORT_FILTER: &str = "info,h2=off,hyper=off,hyper_util=off,reqwest=off,tower=off,\
                             opentelemetry=off,opentelemetry_sdk=off,opentelemetry_otlp=off,\
                             opentelemetry_http=off";

/// The instrumentation scope Athena's spans and logs are reported under.
const SCOPE: &str = "athena";

/// How long JSONL files are kept without `ATHENA_TELEMETRY_RETENTION_DAYS`.
pub const DEFAULT_RETENTION_DAYS: NonZeroU32 = NonZeroU32::new(30).expect("30 is not 0");

/// Which process is exporting: the subcommand, `cli` for everything else.
/// It names the JSONL files, so each file has one writer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Serve,
    Telegram,
    Cli,
}

impl Role {
    /// The role of `athena <args>` (the arguments after the program name).
    pub fn from_args(args: &[String]) -> Self {
        match args.first().map(String::as_str) {
            Some("serve") => Self::Serve,
            Some("telegram") => Self::Telegram,
            _ => Self::Cli,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Serve => "serve",
            Self::Telegram => "telegram",
            Self::Cli => "cli",
        }
    }
}

/// Everything telemetry is configured by. See [`Settings::from_env`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settings {
    /// `OTEL_EXPORTER_OTLP_ENDPOINT`. `None` means no OTLP export.
    pub endpoint: Option<String>,
    /// `ATHENA_TELEMETRY_DIR`. `None` means no JSONL files.
    pub telemetry_dir: Option<PathBuf>,
    /// `ATHENA_TELEMETRY_RETENTION_DAYS`, default [`DEFAULT_RETENTION_DAYS`].
    pub retention_days: NonZeroU32,
    /// `OTEL_SERVICE_NAME`, default `athena`.
    pub service_name: String,
    /// `ATHENA_VERSION`, else `<crate version>-dev`.
    pub version: String,
    /// `ATHENA_ENV`: `staging`, `prod`, or `None` in development.
    pub environment: Option<String>,
    /// `RUST_LOG`, else [`DEFAULT_LOG_FILTER`]. Applies to stderr only.
    pub log_filter: String,
}

impl Settings {
    pub fn from_env() -> Result<Self> {
        Self::from_vars(|name| std::env::var(name).ok())
    }

    /// Settings from a variable lookup. Empty values count as unset.
    pub fn from_vars(var: impl Fn(&str) -> Option<String>) -> Result<Self> {
        let set = |name: &str| var(name).filter(|value| !value.trim().is_empty());
        let retention_days = match set("ATHENA_TELEMETRY_RETENTION_DAYS") {
            None => DEFAULT_RETENTION_DAYS,
            Some(days) => days.trim().parse().map_err(|_| {
                anyhow!(
                    "ATHENA_TELEMETRY_RETENTION_DAYS must be a whole number of days, \
                     1 or more, got '{days}'"
                )
            })?,
        };
        Ok(Self {
            endpoint: set("OTEL_EXPORTER_OTLP_ENDPOINT"),
            telemetry_dir: set("ATHENA_TELEMETRY_DIR").map(PathBuf::from),
            retention_days,
            service_name: set("OTEL_SERVICE_NAME").unwrap_or_else(|| "athena".into()),
            version: version_or_dev(set("ATHENA_VERSION")),
            environment: set("ATHENA_ENV"),
            log_filter: set("RUST_LOG").unwrap_or_else(|| DEFAULT_LOG_FILTER.into()),
        })
    }

    /// The OpenTelemetry resource every span and log record carries.
    pub fn resource(&self) -> Resource {
        let mut attributes = vec![KeyValue::new("service.version", self.version.clone())];
        if let Some(environment) = &self.environment {
            attributes.push(KeyValue::new(
                "deployment.environment.name",
                environment.clone(),
            ));
        }
        Resource::builder()
            .with_service_name(self.service_name.clone())
            .with_attributes(attributes)
            .build()
    }
}

/// `ATHENA_VERSION` as written by deploy-gate, or this build's crate
/// version marked as a development build.
fn version_or_dev(configured: Option<String>) -> String {
    configured.unwrap_or_else(|| format!("{}-dev", env!("CARGO_PKG_VERSION")))
}

/// A span exporter and a log exporter that go together.
pub struct Exporters<S, L> {
    pub spans: S,
    pub logs: L,
}

/// OTLP/HTTP protobuf exporters. They read the endpoint, headers and
/// timeout from the standard `OTEL_EXPORTER_OTLP_*` variables and append
/// `/v1/traces` and `/v1/logs` to the endpoint, so for OpenObserve it is
/// `http://<host>:5080/api/<org>`. `OTEL_EXPORTER_OTLP_HEADERS` is
/// `key=value` pairs separated by commas, with `%XX` escapes decoded, e.g.
/// `Authorization=Basic%20<base64 of user:password>`. Nothing connects
/// until the first export.
pub fn otlp_exporters()
-> Result<Exporters<opentelemetry_otlp::SpanExporter, opentelemetry_otlp::LogExporter>> {
    use opentelemetry_otlp::{Protocol, WithExportConfig};
    Ok(Exporters {
        spans: opentelemetry_otlp::SpanExporter::builder()
            .with_http()
            .with_protocol(Protocol::HttpBinary)
            .build()
            .context("building the OTLP span exporter")?,
        logs: opentelemetry_otlp::LogExporter::builder()
            .with_http()
            .with_protocol(Protocol::HttpBinary)
            .build()
            .context("building the OTLP log exporter")?,
    })
}

/// Where spans and logs are exported to, each sink behind its own batch
/// processor (and thread). None means OpenTelemetry is off.
pub struct Sinks {
    count: usize,
    tracer: TracerProviderBuilder,
    logger: LoggerProviderBuilder,
}

impl Default for Sinks {
    /// No sinks yet.
    fn default() -> Self {
        Self {
            count: 0,
            tracer: SdkTracerProvider::builder(),
            logger: SdkLoggerProvider::builder(),
        }
    }
}

impl Sinks {
    /// The sinks `settings` switch on for process `role`: OTLP, JSONL
    /// files, both or neither.
    pub fn from_settings(settings: &Settings, role: Role) -> Result<Self> {
        let mut sinks = Self::default();
        if settings.endpoint.is_some() {
            sinks = sinks.with(otlp_exporters()?);
        }
        if let Some(dir) = &settings.telemetry_dir {
            let clock = Arc::new(jsonl::SystemClock);
            let files = jsonl::exporters(dir, role, settings.retention_days, clock)
                .with_context(|| format!("creating ATHENA_TELEMETRY_DIR {}", dir.display()))?;
            sinks = sinks.with(files);
        }
        Ok(sinks)
    }

    /// Add a sink.
    pub fn with<S, L>(self, exporters: Exporters<S, L>) -> Self
    where
        S: SpanExporter + 'static,
        L: LogExporter + 'static,
    {
        Self {
            count: self.count + 1,
            tracer: self.tracer.with_batch_exporter(exporters.spans),
            logger: self.logger.with_batch_exporter(exporters.logs),
        }
    }

    /// How many sinks there are.
    pub fn len(&self) -> usize {
        self.count
    }

    pub fn is_empty(&self) -> bool {
        self.count == 0
    }
}

type BoxedLayer = Box<dyn Layer<Registry> + Send + Sync>;

/// The OpenTelemetry providers behind the layers. Shut them down once the
/// process has finished its work, so the last batch is exported.
#[must_use = "dropping it without `shutdown` can lose the last batch"]
pub struct Telemetry {
    tracer: Option<SdkTracerProvider>,
    logger: Option<SdkLoggerProvider>,
}

impl Telemetry {
    /// Whether spans and logs are exported.
    pub fn exporting(&self) -> bool {
        self.tracer.is_some()
    }

    /// Export what is still buffered and stop the exporters, reporting a
    /// failure on stderr: the logger itself is gone by then.
    pub fn shutdown(self) {
        self.shutdown_to(&mut std::io::stderr());
    }

    fn shutdown_to(self, out: &mut impl Write) {
        let traces = self.tracer.map(|p| p.shutdown());
        let logs = self.logger.map(|p| p.shutdown());
        for (what, result) in [("traces", traces), ("logs", logs)] {
            if let Some(Err(e)) = result {
                // Nowhere left to report a failure to write to stderr.
                let _ = writeln!(out, "warning: exporting the last {what} failed: {e}");
            }
        }
    }
}

/// The subscriber's layers: a human-readable log on `stderr`, filtered by
/// `settings.log_filter`, and with any `sinks`, the OpenTelemetry span and
/// log layers over them.
pub fn layers<W>(settings: &Settings, sinks: Sinks, stderr: W) -> (Vec<BoxedLayer>, Telemetry)
where
    W: for<'w> MakeWriter<'w> + Send + Sync + 'static,
{
    let log = tracing_subscriber::fmt::layer()
        .with_writer(stderr)
        .with_ansi(false)
        .with_target(false)
        .with_filter(EnvFilter::new(&settings.log_filter))
        .boxed();
    if sinks.is_empty() {
        let off = Telemetry {
            tracer: None,
            logger: None,
        };
        return (vec![log], off);
    }

    let tracer = sinks.tracer.with_resource(settings.resource()).build();
    let logger = sinks.logger.with_resource(settings.resource()).build();
    let spans = tracing_opentelemetry::layer()
        .with_tracer(tracer.tracer(SCOPE))
        .with_filter(EnvFilter::new(EXPORT_FILTER))
        .boxed();
    let events = OpenTelemetryTracingBridge::new(&logger)
        .with_filter(EnvFilter::new(EXPORT_FILTER))
        .boxed();
    let on = Telemetry {
        tracer: Some(tracer),
        logger: Some(logger),
    };
    (vec![log, spans, events], on)
}

/// Configure telemetry from the environment and install it for the whole
/// process, which runs as `role`. Call once, before anything logs.
pub fn init(role: Role) -> Result<Telemetry> {
    let settings = Settings::from_env()?;
    let sinks = Sinks::from_settings(&settings, role)?;
    let (layers, telemetry) = layers(&settings, sinks, std::io::stderr);
    tracing_subscriber::registry()
        .with(layers)
        .try_init()
        .context("installing the tracing subscriber")?;
    Ok(telemetry)
}

/// A stable pseudonym for `user`, for `enduser.pseudo.id`: the first 16
/// bytes of SHA-256 over `transport:external_id`, in hex.
///
/// It keeps raw chat ids out of the telemetry backend and still lets one
/// person's turns be grouped. It is unsalted, so anyone who can enumerate
/// ids (Telegram ids are numbers) can reverse it; treat it as internal.
pub fn pseudonym(user: &User) -> String {
    let digest = Sha256::digest(format!("{}:{}", user.transport(), user.external_id()));
    digest[..16].iter().map(|b| format!("{b:02x}")).collect()
}

/// The incoming W3C trace context (`traceparent`, `tracestate`) in
/// `headers`, as a parent for the request's span.
pub fn remote_context(headers: &HeaderMap) -> Context {
    TraceContextPropagator::new().extract(&HeaderExtractor(headers))
}

/// The response header naming the trace a request was recorded in.
pub const TRACE_ID_HEADER: &str = "x-trace-id";

/// The trace id of `span` as `x-trace-id` and its W3C `traceparent`, when
/// the span is being exported. Nothing when OpenTelemetry is off.
pub fn trace_headers(span: &tracing::Span) -> Vec<(HeaderName, HeaderValue)> {
    let context = span.context();
    let span = context.span();
    let ids = span.span_context();
    if !ids.is_valid() {
        return Vec::new();
    }
    let traceparent = format!(
        "00-{}-{}-{:02x}",
        ids.trace_id(),
        ids.span_id(),
        ids.trace_flags().to_u8()
    );
    // Hex digits and dashes only, so both are always valid header values.
    vec![
        (
            HeaderName::from_static(TRACE_ID_HEADER),
            HeaderValue::from_str(&ids.trace_id().to_string()).expect("hex is a header value"),
        ),
        (
            HeaderName::from_static("traceparent"),
            HeaderValue::from_str(&traceparent).expect("hex is a header value"),
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use opentelemetry_sdk::logs::InMemoryLogExporter;
    use opentelemetry_sdk::trace::InMemorySpanExporter;

    fn vars<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |name| {
            pairs
                .iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| value.to_string())
        }
    }

    fn settings(pairs: &[(&str, &str)]) -> Settings {
        Settings::from_vars(vars(pairs)).unwrap()
    }

    fn in_memory() -> Exporters<InMemorySpanExporter, InMemoryLogExporter> {
        Exporters {
            spans: InMemorySpanExporter::default(),
            logs: InMemoryLogExporter::default(),
        }
    }

    /// A fresh directory under the system temp dir, removed on drop.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            let path =
                std::env::temp_dir().join(format!("athena-telemetry-{}", uuid::Uuid::new_v4()));
            Self(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn without_an_endpoint_or_a_directory_telemetry_is_off_and_has_defaults() {
        let settings = settings(&[
            ("OTEL_EXPORTER_OTLP_ENDPOINT", " "),
            ("ATHENA_TELEMETRY_DIR", ""),
        ]);
        assert_eq!(
            settings,
            Settings {
                endpoint: None,
                telemetry_dir: None,
                retention_days: DEFAULT_RETENTION_DAYS,
                service_name: "athena".into(),
                version: format!("{}-dev", env!("CARGO_PKG_VERSION")),
                environment: None,
                log_filter: DEFAULT_LOG_FILTER.into(),
            }
        );
        assert!(
            Sinks::from_settings(&settings, Role::Cli)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn every_setting_comes_from_its_variable() {
        let settings = settings(&[
            (
                "OTEL_EXPORTER_OTLP_ENDPOINT",
                "http://127.0.0.1:5080/api/default",
            ),
            ("ATHENA_TELEMETRY_DIR", "/var/lib/athena/staging/telemetry"),
            ("ATHENA_TELEMETRY_RETENTION_DAYS", " 7 "),
            ("OTEL_SERVICE_NAME", "athena-staging"),
            ("ATHENA_VERSION", "v1.2.3+abc"),
            ("ATHENA_ENV", "staging"),
            ("RUST_LOG", "debug"),
        ]);
        assert_eq!(
            settings.endpoint.as_deref(),
            Some("http://127.0.0.1:5080/api/default")
        );
        assert_eq!(
            settings.telemetry_dir,
            Some(PathBuf::from("/var/lib/athena/staging/telemetry"))
        );
        assert_eq!(settings.retention_days.get(), 7);
        assert_eq!(settings.service_name, "athena-staging");
        assert_eq!(settings.version, "v1.2.3+abc");
        assert_eq!(settings.environment.as_deref(), Some("staging"));
        assert_eq!(settings.log_filter, "debug");
    }

    #[test]
    fn a_retention_that_is_not_a_positive_number_is_refused() {
        for bad in ["0", "-1", "thirty", "1.5"] {
            let error = Settings::from_vars(vars(&[("ATHENA_TELEMETRY_RETENTION_DAYS", bad)]))
                .unwrap_err()
                .to_string();
            assert!(error.contains("ATHENA_TELEMETRY_RETENTION_DAYS"), "{error}");
            assert!(error.contains(&format!("'{bad}'")), "{error}");
        }
    }

    #[test]
    fn the_resource_names_the_service_version_and_environment() {
        let attribute = |resource: &Resource, key: &'static str| {
            resource
                .get(&opentelemetry::Key::from_static_str(key))
                .map(|v| v.to_string())
        };
        let staging =
            settings(&[("ATHENA_VERSION", "abc123"), ("ATHENA_ENV", "staging")]).resource();
        assert_eq!(
            attribute(&staging, "service.name").as_deref(),
            Some("athena")
        );
        assert_eq!(
            attribute(&staging, "service.version").as_deref(),
            Some("abc123")
        );
        assert_eq!(
            attribute(&staging, "deployment.environment.name").as_deref(),
            Some("staging")
        );

        let dev = settings(&[]).resource();
        assert_eq!(attribute(&dev, "deployment.environment.name"), None);
    }

    #[test]
    fn the_role_is_the_subcommand() {
        let role = |args: &[&str]| {
            let args: Vec<String> = args.iter().map(|a| a.to_string()).collect();
            Role::from_args(&args).as_str()
        };
        assert_eq!(role(&["serve", "--addr", "x"]), "serve");
        assert_eq!(role(&["telegram"]), "telegram");
        assert_eq!(role(&["eval", "run"]), "cli");
        assert_eq!(role(&[]), "cli");
    }

    #[test]
    fn the_otlp_exporters_build_without_a_backend() {
        assert!(otlp_exporters().is_ok());
    }

    #[test]
    fn each_configured_sink_is_added_and_the_directory_is_created() {
        let dir = TempDir::new();
        let path = dir.0.join("telemetry");
        let path = path.to_str().unwrap();
        let endpoint = (
            "OTEL_EXPORTER_OTLP_ENDPOINT",
            "http://127.0.0.1:1/api/default",
        );
        let files = ("ATHENA_TELEMETRY_DIR", path);

        assert_eq!(
            Sinks::from_settings(&settings(&[endpoint]), Role::Serve)
                .unwrap()
                .len(),
            1
        );
        assert!(!dir.0.exists());
        assert_eq!(
            Sinks::from_settings(&settings(&[files]), Role::Serve)
                .unwrap()
                .len(),
            1
        );
        assert!(dir.0.join("telemetry").is_dir());
        let both = Sinks::from_settings(&settings(&[endpoint, files]), Role::Serve).unwrap();
        assert_eq!(both.len(), 2);
        assert!(!both.is_empty());
    }

    #[test]
    fn a_telemetry_directory_that_cannot_be_created_is_an_error() {
        let dir = TempDir::new();
        std::fs::write(&dir.0, "a file, not a directory").unwrap();
        let below = dir.0.join("telemetry");
        let files = settings(&[("ATHENA_TELEMETRY_DIR", below.to_str().unwrap())]);
        let error = Sinks::from_settings(&files, Role::Telegram)
            .err()
            .expect("a directory under a file")
            .to_string();
        assert!(error.contains("creating ATHENA_TELEMETRY_DIR"), "{error}");
        let _ = std::fs::remove_file(&dir.0);
    }

    /// What stderr would have shown.
    #[derive(Clone, Default)]
    struct Stderr(Arc<std::sync::Mutex<Vec<u8>>>);

    impl Write for Stderr {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().write(buf)
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// Emit one event with a content field and one without, and return the
    /// bodies of the log records exported and what stderr showed, for prod.
    fn logged_in_prod() -> (Vec<String>, String) {
        let settings = settings(&[("ATHENA_ENV", "prod")]);
        let exporters = in_memory();
        let logs = exporters.logs.clone();
        let stderr = Stderr::default();
        let writer = stderr.clone();
        let (layers, telemetry) = layers(&settings, Sinks::default().with(exporters), move || {
            writer.clone()
        });
        let subscriber = tracing_subscriber::registry().with(layers);
        tracing::subscriber::with_default(subscriber, || {
            tracing::warn!(gen_ai.prompt = "what the user typed", "with content");
            tracing::warn!(gen_ai.usage.input_tokens = 12, "without content");
        });
        telemetry.logger.as_ref().unwrap().force_flush().unwrap();
        let bodies = logs
            .get_emitted_logs()
            .unwrap()
            .iter()
            .map(|log| format!("{:?}", log.record.body()))
            .collect();
        telemetry.shutdown_to(&mut Vec::new());
        // The fmt layer never flushes; the helper's flush must still work.
        stderr.clone().flush().unwrap();
        let shown = String::from_utf8(stderr.0.lock().unwrap().clone()).unwrap();
        (bodies, shown)
    }

    #[test]
    fn prod_exports_and_prints_log_events_with_content_fields() {
        let (exported, shown) = logged_in_prod();
        assert_eq!(exported.len(), 2, "{exported:?}");
        assert!(exported[0].contains("with content"), "{exported:?}");
        assert!(exported[1].contains("without content"), "{exported:?}");
        assert!(shown.contains("what the user typed"), "{shown}");
        assert!(shown.contains("without content"), "{shown}");
    }

    #[test]
    fn a_failed_final_export_is_reported_not_raised() {
        let settings = settings(&[]);
        let (_, telemetry) = layers(&settings, Sinks::default().with(in_memory()), std::io::sink);
        assert!(telemetry.exporting());
        // Shut down behind its back: the second shutdown fails.
        telemetry.tracer.as_ref().unwrap().shutdown().unwrap();
        telemetry.logger.as_ref().unwrap().shutdown().unwrap();

        let mut out = Vec::new();
        telemetry.shutdown_to(&mut out);

        let out = String::from_utf8(out).unwrap();
        assert!(out.contains("the last traces failed"), "{out}");
        assert!(out.contains("the last logs failed"), "{out}");
        assert!(out.starts_with("warning: exporting"), "{out}");
    }

    #[test]
    fn switched_off_telemetry_shuts_down_silently() {
        let settings = settings(&[]);
        let (layers, telemetry) = layers(&settings, Sinks::default(), std::io::sink);
        assert_eq!(layers.len(), 1);
        assert!(!telemetry.exporting());
        let mut out = Vec::new();
        telemetry.shutdown_to(&mut out);
        assert!(out.is_empty());
    }
}
