//! The SSH front door for Nix remote builds.
//!
//! A Nix client configured with a builder such as
//!
//! ```text
//! ssh://nixbld@<proxy> x86_64-linux - 100 1
//! ```
//!
//! runs one build-remote hook per derivation it wants built remotely. Each
//! hook opens its own SSH connection (or, with ControlMaster, its own channel
//! on a shared connection) and executes `nix-store --serve --write`. The proxy
//! treats every session channel as an independent build session: it creates a
//! `NixBuildRequest`, waits for the controller to bring up a dedicated builder
//! pod and then splices the channel onto an SSH session on that pod. Sessions
//! never wait on each other, so independent derivations build concurrently as
//! long as the client advertises enough remote slots (the "100" above).

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
    ANNOTATION_PROXY_POD, BuildRequestClient, KubeBuildRequestClient, LABEL_MANAGED_BY,
    MANAGED_BY_VALUE, Session, SessionResult, build_request_name,
};

#[cfg(test)]
mod tests;
