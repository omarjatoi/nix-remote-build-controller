//! Process plumbing shared by the controller and the proxy: logging, HTTP
//! servers, shutdown handling, duration flags and error reports.

pub mod duration;
pub mod error;
pub mod http;
pub mod logging;
pub mod shutdown;
