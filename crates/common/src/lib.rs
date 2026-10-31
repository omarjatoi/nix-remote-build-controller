//! Process plumbing shared by the controller and the proxy: telemetry, HTTP
//! servers, shutdown handling, duration flags and error reports.

pub mod duration;
pub mod error;
pub mod http;
pub mod logging;
mod metrics_provider;
pub mod shutdown;
mod telemetry;
mod trace_context;

pub use metrics_provider::MetricsProvider;
pub use telemetry::{TRACE_ONLY_TARGET, Telemetry, record_error};
pub use trace_context::TraceContext;
