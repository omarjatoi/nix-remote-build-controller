//! W3C trace context, the form in which a trace crosses a process boundary.
//!
//! The proxy starts a trace for every build session (or joins the Nix client's)
//! and passes it on in `NixBuildRequest` annotations and in the environment of
//! the command on the builder pod.

use std::collections::HashMap;

use opentelemetry::propagation::TextMapPropagator as _;
use opentelemetry_sdk::propagation::TraceContextPropagator;
use tracing::{Span, debug};
use tracing_opentelemetry::OpenTelemetrySpanExt as _;

const TRACEPARENT: &str = "traceparent";
const TRACESTATE: &str = "tracestate";

/// The W3C `traceparent` and `tracestate` values of a span.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TraceContext {
    /// Trace ID, parent span ID and sampling flags.
    pub traceparent: String,
    /// Vendor-specific data; usually empty.
    pub tracestate: String,
}

impl TraceContext {
    /// The context of `span`, or `None` if it is not being traced.
    ///
    /// This starts the span, so set its parent first.
    pub fn of(span: &Span) -> Option<Self> {
        let mut carrier = HashMap::new();
        TraceContextPropagator::new().inject_context(&span.context(), &mut carrier);
        Some(Self {
            traceparent: carrier.remove(TRACEPARENT)?,
            tracestate: carrier.remove(TRACESTATE).unwrap_or_default(),
        })
    }

    /// Makes this context the parent of `span`, which must not have been
    /// entered yet. A malformed context leaves `span` without a parent.
    pub fn set_parent_of(&self, span: &Span) {
        let carrier = HashMap::from([
            (TRACEPARENT.to_string(), self.traceparent.clone()),
            (TRACESTATE.to_string(), self.tracestate.clone()),
        ]);
        let parent = TraceContextPropagator::new().extract(&carrier);
        if let Err(err) = span.set_parent(parent) {
            debug!(error = %err, "Could not set the parent of a span");
        }
    }
}

#[cfg(test)]
mod tests {
    use opentelemetry::trace::TracerProvider as _;
    use opentelemetry_sdk::trace::{InMemorySpanExporter, SdkTracerProvider};
    use tracing_subscriber::layer::SubscriberExt as _;

    use super::*;

    const PARENT: &str = "00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01";

    fn traced(run: impl FnOnce()) -> Vec<opentelemetry_sdk::trace::SpanData> {
        let exporter = InMemorySpanExporter::default();
        let provider = SdkTracerProvider::builder()
            .with_simple_exporter(exporter.clone())
            .build();
        let subscriber = tracing_subscriber::registry()
            .with(tracing_opentelemetry::layer().with_tracer(provider.tracer("test")));
        tracing::subscriber::with_default(subscriber, run);
        provider.force_flush().unwrap();
        exporter.get_finished_spans().unwrap()
    }

    #[test]
    fn spans_join_the_trace_of_their_parent_context() {
        let spans = traced(|| {
            let span = tracing::error_span!("session");
            TraceContext {
                traceparent: PARENT.to_string(),
                tracestate: "vendor=1".to_string(),
            }
            .set_parent_of(&span);
            let context = TraceContext::of(&span).expect("span is traced");
            assert!(
                context
                    .traceparent
                    .contains("0af7651916cd43dd8448eb211c80319c")
            );
            assert!(!context.traceparent.contains("b7ad6b7169203331"));
            assert_eq!(context.tracestate, "vendor=1");
        });
        assert_eq!(spans.len(), 1);
        assert_eq!(
            spans[0].span_context.trace_id().to_string(),
            "0af7651916cd43dd8448eb211c80319c"
        );
        assert_eq!(spans[0].parent_span_id.to_string(), "b7ad6b7169203331");
    }

    #[test]
    fn malformed_contexts_start_a_new_trace() {
        let spans = traced(|| {
            let span = tracing::error_span!("session");
            TraceContext {
                traceparent: "not a traceparent".to_string(),
                tracestate: String::new(),
            }
            .set_parent_of(&span);
            assert!(TraceContext::of(&span).is_some());
        });
        assert_eq!(spans.len(), 1);
        assert!(spans[0].span_context.is_valid());
        assert_eq!(
            spans[0].parent_span_id,
            opentelemetry::trace::SpanId::INVALID
        );
    }

    #[test]
    fn spans_outside_a_trace_have_no_context() {
        assert_eq!(TraceContext::of(&Span::none()), None);
        let subscriber = tracing_subscriber::registry();
        tracing::subscriber::with_default(subscriber, || {
            assert_eq!(TraceContext::of(&tracing::error_span!("untraced")), None);
        });
    }
}
