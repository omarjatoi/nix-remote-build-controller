//! W3C trace context, the form in which a trace crosses a process boundary.
//!
//! The proxy starts a trace for every build session (or joins the Nix client's)
//! and passes it on in `NixBuildRequest` annotations and in the environment of
//! the command on the builder pod.

use opentelemetry::propagation::{Extractor, Injector, TextMapPropagator as _};
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
    #[must_use]
    pub fn of(span: &Span) -> Option<Self> {
        let mut context = Self::default();
        TraceContextPropagator::new().inject_context(&span.context(), &mut context);
        (!context.traceparent.is_empty()).then_some(context)
    }

    /// Makes this context the parent of `span`, which must not have been
    /// entered yet. A malformed context leaves `span` without a parent.
    pub fn make_parent_of(&self, span: &Span) {
        let parent = TraceContextPropagator::new().extract(self);
        if let Err(err) = span.set_parent(parent) {
            debug!(error = %err, "Could not set the parent of a span");
        }
    }
}

impl Injector for TraceContext {
    fn set(&mut self, key: &str, value: String) {
        match key {
            TRACEPARENT => self.traceparent = value,
            TRACESTATE => self.tracestate = value,
            _ => {}
        }
    }
}

impl Extractor for TraceContext {
    fn get(&self, key: &str) -> Option<&str> {
        match key {
            TRACEPARENT => Some(&self.traceparent),
            TRACESTATE => Some(&self.tracestate),
            _ => None,
        }
    }

    fn keys(&self) -> Vec<&str> {
        vec![TRACEPARENT, TRACESTATE]
    }
}

#[cfg(test)]
mod tests {
    use opentelemetry::trace::SpanId;

    use super::*;
    use crate::SpanRecorder;

    const TRACE: &str = "0af7651916cd43dd8448eb211c80319c";
    const PARENT_SPAN: &str = "b7ad6b7169203331";

    #[test]
    fn spans_join_the_trace_of_their_parent_context() {
        let recorder = SpanRecorder::new();
        let span = tracing::error_span!("session");
        TraceContext {
            traceparent: format!("00-{TRACE}-{PARENT_SPAN}-01"),
            tracestate: "vendor=1".to_string(),
        }
        .make_parent_of(&span);
        let context = TraceContext::of(&span).expect("span is traced");
        assert!(
            context.traceparent.contains(TRACE),
            "the span stays in its parent's trace: {context:?}"
        );
        assert!(
            !context.traceparent.contains(PARENT_SPAN),
            "the context names the span, not its parent: {context:?}"
        );
        assert_eq!(context.tracestate, "vendor=1");
        drop(span);

        let spans = recorder.finished_spans();
        assert_eq!(spans.len(), 1, "only the session was traced: {spans:?}");
        assert_eq!(spans[0].span_context.trace_id().to_string(), TRACE);
        assert_eq!(spans[0].parent_span_id.to_string(), PARENT_SPAN);
    }

    #[test]
    fn malformed_contexts_start_a_new_trace() {
        let recorder = SpanRecorder::new();
        let span = tracing::error_span!("session");
        TraceContext {
            traceparent: "not a traceparent".to_string(),
            tracestate: String::new(),
        }
        .make_parent_of(&span);
        assert!(
            TraceContext::of(&span).is_some(),
            "the span starts a trace of its own"
        );
        drop(span);

        let spans = recorder.finished_spans();
        assert_eq!(spans.len(), 1, "only the session was traced: {spans:?}");
        assert!(
            spans[0].span_context.is_valid(),
            "the span has generated IDs"
        );
        assert_eq!(spans[0].parent_span_id, SpanId::INVALID);
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
