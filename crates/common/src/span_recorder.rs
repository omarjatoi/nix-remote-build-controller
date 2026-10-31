//! Records finished spans, for tests.

use opentelemetry_sdk::trace::{InMemorySpanExporter, SdkTracerProvider, SpanData};
use tracing::Dispatch;
use tracing::subscriber::DefaultGuard;

use crate::telemetry;

/// Sends the spans of the current thread through the production trace filter
/// to memory, until dropped. This also covers tasks on the runtime of a
/// `#[tokio::test]`, which run on the test's thread.
pub struct SpanRecorder {
    exporter: InMemorySpanExporter,
    provider: SdkTracerProvider,
    _subscriber: DefaultGuard,
    _second_dispatch: Dispatch,
}

impl SpanRecorder {
    /// Starts recording.
    pub fn start() -> Self {
        use opentelemetry::trace::TracerProvider as _;
        use tracing_subscriber::Layer as _;
        use tracing_subscriber::layer::SubscriberExt as _;

        // tracing caches whether a callsite is enabled. With a single dispatch
        // it asks the default of whichever thread gets there first, which can
        // be a test that does not record. A second one makes it ask them all.
        let second_dispatch = Dispatch::new(tracing_subscriber::registry());
        let exporter = InMemorySpanExporter::default();
        let provider = SdkTracerProvider::builder()
            .with_simple_exporter(exporter.clone())
            .build();
        let subscriber = tracing_subscriber::registry().with(
            tracing_opentelemetry::layer()
                .with_tracer(provider.tracer("test"))
                .with_filter(telemetry::trace_filter()),
        );
        Self {
            exporter,
            provider,
            _subscriber: tracing::subscriber::set_default(subscriber),
            _second_dispatch: second_dispatch,
        }
    }

    /// The spans that have ended so far.
    pub fn finished_spans(&self) -> Vec<SpanData> {
        self.provider.force_flush().expect("flush spans");
        self.exporter.get_finished_spans().expect("read spans")
    }
}
