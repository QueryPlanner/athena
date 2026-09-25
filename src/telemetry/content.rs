//! The in-process backstop that keeps prompt and tool content out of prod
//! telemetry, whatever `ATHENA_RECORD_CONTENT` says.
//!
//! Content reaches telemetry only as the GenAI attributes in
//! [`CONTENT_KEYS`]; rig records them when content capture is on. For prod,
//! [`StripContent`] removes them from every span and span event before any
//! exporter sees the span, and [`carries_content`] lets the log layer drop a
//! log event that has one as a field. Span links and names are left alone:
//! tracing-opentelemetry gives links no attributes, and names are code.

use opentelemetry::KeyValue;
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::error::OTelSdkResult;
use opentelemetry_sdk::trace::{SpanData, SpanExporter};
use std::time::Duration;

/// Attributes that hold what users typed, what the model answered, or what
/// tools were given and returned.
pub const CONTENT_KEYS: &[&str] = &[
    "gen_ai.input.messages",
    "gen_ai.output.messages",
    "gen_ai.system_instructions",
    "gen_ai.tool.call.arguments",
    "gen_ai.tool.call.result",
    "gen_ai.prompt",
    "gen_ai.completion",
];

fn is_content(key: &str) -> bool {
    CONTENT_KEYS.contains(&key)
}

/// Whether a `tracing` event has a content field.
pub fn carries_content(metadata: &tracing::Metadata<'_>) -> bool {
    metadata
        .fields()
        .iter()
        .any(|field| is_content(field.name()))
}

fn strip(attributes: &mut Vec<KeyValue>) {
    attributes.retain(|kv| !is_content(kv.key.as_str()));
}

/// A span exporter that, when `enabled`, removes [`CONTENT_KEYS`] from each
/// span and its events before handing the batch to `inner`.
#[derive(Debug)]
pub struct StripContent<E> {
    inner: E,
    enabled: bool,
}

impl<E> StripContent<E> {
    pub fn new(inner: E, enabled: bool) -> Self {
        Self { inner, enabled }
    }
}

impl<E: SpanExporter> SpanExporter for StripContent<E> {
    fn export(
        &self,
        mut batch: Vec<SpanData>,
    ) -> impl std::future::Future<Output = OTelSdkResult> + Send {
        if self.enabled {
            for span in &mut batch {
                strip(&mut span.attributes);
                for event in &mut span.events.events {
                    strip(&mut event.attributes);
                }
            }
        }
        self.inner.export(batch)
    }

    fn shutdown_with_timeout(&self, timeout: Duration) -> OTelSdkResult {
        self.inner.shutdown_with_timeout(timeout)
    }

    fn force_flush(&self) -> OTelSdkResult {
        self.inner.force_flush()
    }

    fn set_resource(&mut self, resource: &Resource) {
        self.inner.set_resource(resource);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use opentelemetry::InstrumentationScope;
    use opentelemetry::trace::{
        Event, SpanContext, SpanId, SpanKind, Status, TraceFlags, TraceId, TraceState,
    };
    use opentelemetry_sdk::trace::{InMemorySpanExporter, SpanEvents, SpanLinks};
    use std::time::SystemTime;

    fn content_and_counts() -> Vec<KeyValue> {
        let mut attributes: Vec<KeyValue> = CONTENT_KEYS
            .iter()
            .map(|key| KeyValue::new(*key, "what the user typed"))
            .collect();
        attributes.push(KeyValue::new("gen_ai.usage.input_tokens", 12));
        attributes.push(KeyValue::new("gen_ai.tool.name", "add"));
        attributes
    }

    fn span() -> SpanData {
        let mut events = SpanEvents::default();
        events.events.push(Event::new(
            "tool output",
            SystemTime::UNIX_EPOCH,
            content_and_counts(),
            0,
        ));
        SpanData {
            span_context: SpanContext::new(
                TraceId::from(1),
                SpanId::from(2),
                TraceFlags::SAMPLED,
                false,
                TraceState::default(),
            ),
            parent_span_id: SpanId::INVALID,
            parent_span_is_remote: false,
            span_kind: SpanKind::Internal,
            name: "chat".into(),
            start_time: SystemTime::UNIX_EPOCH,
            end_time: SystemTime::UNIX_EPOCH,
            attributes: content_and_counts(),
            dropped_attributes_count: 0,
            events,
            links: SpanLinks::default(),
            status: Status::Unset,
            instrumentation_scope: InstrumentationScope::builder("athena").build(),
        }
    }

    fn keys(attributes: &[KeyValue]) -> Vec<&str> {
        attributes.iter().map(|kv| kv.key.as_str()).collect()
    }

    fn export(enabled: bool) -> SpanData {
        let memory = InMemorySpanExporter::default();
        let exporter = StripContent::new(memory.clone(), enabled);
        futures_util::FutureExt::now_or_never(exporter.export(vec![span()]))
            .expect("the in-memory exporter is synchronous")
            .unwrap();
        memory.get_finished_spans().unwrap().remove(0)
    }

    #[test]
    fn enabled_it_removes_content_from_spans_and_their_events_and_keeps_the_rest() {
        let span = export(true);
        let kept = ["gen_ai.usage.input_tokens", "gen_ai.tool.name"];
        assert_eq!(keys(&span.attributes), kept);
        assert_eq!(keys(&span.events.events[0].attributes), kept);
    }

    #[test]
    fn disabled_it_passes_spans_through_untouched() {
        let span = export(false);
        assert_eq!(span.attributes, content_and_counts());
        assert_eq!(span.events.events[0].attributes, content_and_counts());
    }

    #[test]
    fn it_forwards_resource_flush_and_shutdown_to_the_wrapped_exporter() {
        let memory = InMemorySpanExporter::default();
        let mut exporter = StripContent::new(memory.clone(), true);
        exporter.set_resource(&Resource::builder_empty().build());
        assert!(exporter.force_flush().is_ok());
        assert!(exporter.shutdown().is_ok());
        // The in-memory exporter forgets its spans on shutdown.
        futures_util::FutureExt::now_or_never(exporter.export(vec![span()]))
            .unwrap()
            .unwrap();
        assert_eq!(memory.get_finished_spans().unwrap().len(), 1);
    }

    #[test]
    fn a_log_event_carries_content_only_with_a_content_field() {
        // Metadata is static; a subscriber that enables everything hands it out.
        let (with, without) =
            tracing::subscriber::with_default(tracing_subscriber::registry(), || {
                let with = tracing::info_span!("x", gen_ai.prompt = "hi", n = 1);
                let without = tracing::info_span!("x", gen_ai.tool.name = "add");
                (with.metadata().unwrap(), without.metadata().unwrap())
            });
        assert!(carries_content(with));
        assert!(!carries_content(without));
    }
}
