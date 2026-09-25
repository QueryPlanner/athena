//! Traces and logs: `tracing` everywhere, OpenTelemetry when configured.
//!
//! Every process logs through `tracing` to stderr in the human-readable
//! format. When `OTEL_EXPORTER_OTLP_ENDPOINT` is set, spans and log events
//! are also exported over OTLP/HTTP (protobuf) to that endpoint; unset, no
//! OpenTelemetry code runs at all.
//!
//! [`layers`] builds the subscriber's layers from [`Settings`] and injected
//! exporters, so tests capture spans in memory. [`init`] wires it to the
//! environment and installs it globally, which a process can do only once.
//!
//! What is exported is listed in README "Observability". Prompt and reply
//! text never goes into a log event; on spans only with
//! `ATHENA_RECORD_CONTENT=1` (see [`record_content`]).

use crate::service::User;
use anyhow::{Context as _, Result};
use axum::http::{HeaderMap, HeaderName, HeaderValue};
use opentelemetry::propagation::TextMapPropagator;
use opentelemetry::trace::{TraceContextExt, TracerProvider as _};
use opentelemetry::{Context, KeyValue};
use opentelemetry_appender_tracing::layer::OpenTelemetryTracingBridge;
use opentelemetry_http::HeaderExtractor;
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::logs::{LogExporter, SdkLoggerProvider};
use opentelemetry_sdk::propagation::TraceContextPropagator;
use opentelemetry_sdk::trace::{SdkTracerProvider, SpanExporter};
use sha2::{Digest, Sha256};
use std::io::Write;
use tracing_opentelemetry::OpenTelemetrySpanExt;
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, Layer, Registry};

/// What stderr shows without `RUST_LOG`: Athena's own events from `info`,
/// everyone else's from `warn`. Spans are never printed.
pub const DEFAULT_LOG_FILTER: &str = "warn,athena=info";

/// What is exported over OTLP. The HTTP stack is silenced so that exporting
/// a batch never produces events that are exported in the next one.
const EXPORT_FILTER: &str = "info,h2=off,hyper=off,hyper_util=off,reqwest=off,tower=off,\
                             opentelemetry=off,opentelemetry_sdk=off,opentelemetry_otlp=off,\
                             opentelemetry_http=off";

/// The instrumentation scope Athena's spans and logs are reported under.
const SCOPE: &str = "athena";

/// Everything telemetry is configured by. See [`Settings::from_env`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settings {
    /// `OTEL_EXPORTER_OTLP_ENDPOINT`. `None` means OpenTelemetry is off.
    pub endpoint: Option<String>,
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
    pub fn from_env() -> Self {
        Self::from_vars(|name| std::env::var(name).ok())
    }

    /// Settings from a variable lookup. Empty values count as unset.
    pub fn from_vars(var: impl Fn(&str) -> Option<String>) -> Self {
        let set = |name: &str| var(name).filter(|value| !value.trim().is_empty());
        Self {
            endpoint: set("OTEL_EXPORTER_OTLP_ENDPOINT"),
            service_name: set("OTEL_SERVICE_NAME").unwrap_or_else(|| "athena".into()),
            version: version_or_dev(set("ATHENA_VERSION")),
            environment: set("ATHENA_ENV"),
            log_filter: set("RUST_LOG").unwrap_or_else(|| DEFAULT_LOG_FILTER.into()),
        }
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

/// Whether `ATHENA_RECORD_CONTENT=1` asks for prompt and response content
/// on spans. Off by default: that content is whatever users typed.
pub fn record_content() -> bool {
    record_content_from(std::env::var("ATHENA_RECORD_CONTENT").ok().as_deref())
}

fn record_content_from(setting: Option<&str>) -> bool {
    matches!(setting, Some("1" | "true"))
}

/// The span and log exporters OpenTelemetry sends through.
pub struct Exporters<S, L> {
    pub spans: S,
    pub logs: L,
}

/// OTLP/HTTP protobuf exporters. They read the endpoint, headers and
/// timeout from the standard `OTEL_EXPORTER_OTLP_*` variables and append
/// `/v1/traces` and `/v1/logs` to the endpoint. Nothing connects until the
/// first export.
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
/// `settings.log_filter`, and with `exporters`, the OpenTelemetry span and
/// log layers over them.
pub fn layers<S, L, W>(
    settings: &Settings,
    exporters: Option<Exporters<S, L>>,
    stderr: W,
) -> (Vec<BoxedLayer>, Telemetry)
where
    S: SpanExporter + 'static,
    L: LogExporter + 'static,
    W: for<'w> MakeWriter<'w> + Send + Sync + 'static,
{
    let log = tracing_subscriber::fmt::layer()
        .with_writer(stderr)
        .with_ansi(false)
        .with_target(false)
        .with_filter(EnvFilter::new(&settings.log_filter))
        .boxed();
    let Some(exporters) = exporters else {
        let off = Telemetry {
            tracer: None,
            logger: None,
        };
        return (vec![log], off);
    };

    let tracer = SdkTracerProvider::builder()
        .with_resource(settings.resource())
        .with_batch_exporter(exporters.spans)
        .build();
    let logger = SdkLoggerProvider::builder()
        .with_resource(settings.resource())
        .with_batch_exporter(exporters.logs)
        .build();
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
/// process. Call once, before anything logs.
pub fn init() -> Result<Telemetry> {
    let settings = Settings::from_env();
    let exporters = settings.endpoint.as_ref().map(|_| otlp_exporters());
    let (layers, telemetry) = layers(&settings, exporters.transpose()?, std::io::stderr);
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

    #[test]
    fn without_an_endpoint_telemetry_is_off_and_has_defaults() {
        let settings = Settings::from_vars(vars(&[("OTEL_EXPORTER_OTLP_ENDPOINT", " ")]));
        assert_eq!(
            settings,
            Settings {
                endpoint: None,
                service_name: "athena".into(),
                version: format!("{}-dev", env!("CARGO_PKG_VERSION")),
                environment: None,
                log_filter: DEFAULT_LOG_FILTER.into(),
            }
        );
    }

    #[test]
    fn every_setting_comes_from_its_variable() {
        let settings = Settings::from_vars(vars(&[
            ("OTEL_EXPORTER_OTLP_ENDPOINT", "http://127.0.0.1:4318"),
            ("OTEL_SERVICE_NAME", "athena-staging"),
            ("ATHENA_VERSION", "v1.2.3+abc"),
            ("ATHENA_ENV", "staging"),
            ("RUST_LOG", "debug"),
        ]));
        assert_eq!(settings.endpoint.as_deref(), Some("http://127.0.0.1:4318"));
        assert_eq!(settings.service_name, "athena-staging");
        assert_eq!(settings.version, "v1.2.3+abc");
        assert_eq!(settings.environment.as_deref(), Some("staging"));
        assert_eq!(settings.log_filter, "debug");
    }

    #[test]
    fn the_resource_names_the_service_version_and_environment() {
        let attribute = |resource: &Resource, key: &'static str| {
            resource
                .get(&opentelemetry::Key::from_static_str(key))
                .map(|v| v.to_string())
        };
        let staging = Settings::from_vars(vars(&[
            ("ATHENA_VERSION", "abc123"),
            ("ATHENA_ENV", "staging"),
        ]))
        .resource();
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

        let dev = Settings::from_vars(vars(&[])).resource();
        assert_eq!(attribute(&dev, "deployment.environment.name"), None);
    }

    #[test]
    fn content_is_recorded_only_when_asked_for() {
        assert!(!record_content_from(None));
        assert!(!record_content_from(Some("0")));
        assert!(!record_content_from(Some("yes")));
        assert!(record_content_from(Some("1")));
        assert!(record_content_from(Some("true")));
        // The env-reading wrapper; tests never set ATHENA_RECORD_CONTENT.
        assert!(!record_content());
    }

    #[test]
    fn the_otlp_exporters_build_without_a_collector() {
        assert!(otlp_exporters().is_ok());
    }

    #[test]
    fn a_failed_final_export_is_reported_not_raised() {
        let settings = Settings::from_vars(vars(&[]));
        let exporters = Exporters {
            spans: InMemorySpanExporter::default(),
            logs: InMemoryLogExporter::default(),
        };
        let (_, telemetry) = layers(&settings, Some(exporters), std::io::sink);
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
        let settings = Settings::from_vars(vars(&[]));
        let none: Option<Exporters<InMemorySpanExporter, InMemoryLogExporter>> = None;
        let (layers, telemetry) = layers(&settings, none, std::io::sink);
        assert_eq!(layers.len(), 1);
        assert!(!telemetry.exporting());
        let mut out = Vec::new();
        telemetry.shutdown_to(&mut out);
        assert!(out.is_empty());
    }
}
