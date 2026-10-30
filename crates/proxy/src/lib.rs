//! The SSH server that Nix clients connect to.
//!
//! For each remote build, a Nix client opens an SSH session and runs
//! `nix-store --serve --write`. The proxy handles every session on its own: it
//! creates a `NixBuildRequest`, waits until the controller has started a
//! builder pod, and forwards the session to that pod. Sessions do not wait for
//! each other, so independent builds run at the same time.

mod auth;
mod config;
pub mod keys;
mod server;
mod session;
mod stream;
mod tunnel;

pub use auth::{ClientAuthorizer, fingerprint};
pub use config::*;
pub use server::SshProxy;
pub use session::{
    ANNOTATION_PROXY_POD, LABEL_MANAGED_BY, MANAGED_BY_VALUE, Session, SessionResult,
    build_request_name,
};

#[cfg(test)]
mod tests;
