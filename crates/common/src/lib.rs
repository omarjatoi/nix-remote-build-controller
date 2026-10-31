//! Process plumbing shared by the controller and the proxy: telemetry, HTTP
//! servers, shutdown handling, duration flags and error reports.

pub mod duration;
pub mod error;
pub mod http;
pub mod logging;
mod metrics_provider;
pub mod shutdown;
#[cfg(any(test, feature = "testing"))]
mod span_recorder;
mod telemetry;
mod trace_context;

pub use metrics_provider::MetricsProvider;
#[cfg(any(test, feature = "testing"))]
pub use span_recorder::SpanRecorder;
pub use telemetry::{TRACE_ONLY_TARGET, Telemetry, record_error};
pub use trace_context::TraceContext;
