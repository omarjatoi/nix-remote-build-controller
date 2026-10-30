//! Leader election with a Kubernetes Lease (coordination.k8s.io/v1).
//!
//! Expiry is judged against the local clock at which the lease record was last
//! seen to change, never against the timestamps written in the lease, so clock
//! skew between replicas cannot cause two leaders.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use k8s_openapi::api::coordination::v1::{Lease, LeaseSpec};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{MicroTime, ObjectMeta};
use kube::api::PostParams;
use kube::{Api, Client};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::kube_util::{is_already_exists, is_conflict, is_not_found};
use crate::lock;

/// Leader election timings and identity.
#[derive(Debug, Clone)]
pub struct LeaderElectionConfig {
    /// Namespace of the Lease.
    pub namespace: String,
    /// Name of the Lease.
    pub name: String,
    /// Unique identity of this replica.
    pub identity: String,
    /// How long non-leaders wait after the last observed renewal before taking
    /// over.
    pub lease_duration: Duration,
    /// How long the leader keeps retrying a failing renewal before giving up.
    pub renew_deadline: Duration,
    /// Interval between acquire/renew attempts.
    pub retry_period: Duration,
}

impl LeaderElectionConfig {
    /// Defaults: 15s lease, 10s renew deadline, 2s retry.
    pub fn new(
        namespace: impl Into<String>,
        name: impl Into<String>,
        identity: impl Into<String>,
    ) -> Self {
        Self {
            namespace: namespace.into(),
            name: name.into(),
            identity: identity.into(),
            lease_duration: Duration::from_secs(15),
            renew_deadline: Duration::from_secs(10),
            retry_period: Duration::from_secs(2),
        }
    }
}

/// Leadership was lost while it was held.
#[derive(Debug, thiserror::Error)]
#[error("leader election lost: {0}")]
pub struct LeadershipLost(String);

/// Outcome of one acquire-or-renew attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Attempt {
    /// This replica holds the lease.
    Held,
    /// Another replica holds an unexpired lease.
    HeldByOther,
    /// The lease changed between read and write; try again.
    Conflict,
}

struct Observed {
    spec: Option<LeaseSpec>,
    at: Instant,
}

/// Acquires and holds a Lease.
pub struct LeaderElector {
    api: Api<Lease>,
    cfg: LeaderElectionConfig,
    observed: Mutex<Option<Observed>>,
}

impl LeaderElector {
    /// Creates an elector for the lease described by `cfg`.
    pub fn new(client: Client, cfg: LeaderElectionConfig) -> Self {
        Self {
            api: Api::namespaced(client, &cfg.namespace),
            cfg,
            observed: Mutex::new(None),
        }
    }

    /// Blocks until leadership is acquired (`true`) or `cancel` fires (`false`).
    pub async fn acquire(&self, cancel: &CancellationToken) -> bool {
        info!(lease = %format!("{}/{}", self.cfg.namespace, self.cfg.name), identity = %self.cfg.identity, "Attempting to acquire leader lease");
        loop {
            match self.try_acquire_or_renew().await {
                Ok(Attempt::Held) => {
                    info!(identity = %self.cfg.identity, "Acquired leader lease");
                    return true;
                }
                Ok(Attempt::HeldByOther) => debug!("Leader lease is held by another replica"),
                Ok(Attempt::Conflict) => debug!("Leader lease changed while acquiring, retrying"),
                Err(err) => warn!(error = %err, "Failed to acquire leader lease"),
            }
            tokio::select! {
                () = cancel.cancelled() => return false,
                () = tokio::time::sleep(self.cfg.retry_period) => {}
            }
        }
    }

    /// Renews the lease every retry period. Returns only when leadership is
    /// lost: the lease was taken, or could not be renewed within the renew
    /// deadline. Drop the future to stop renewing.
    pub async fn hold(&self) -> LeadershipLost {
        let mut last_renewed = Instant::now();
        loop {
            tokio::time::sleep(self.cfg.retry_period).await;
            let attempt =
                tokio::time::timeout(self.cfg.renew_deadline, self.try_acquire_or_renew()).await;
            match attempt {
                Ok(Ok(Attempt::Held)) => last_renewed = Instant::now(),
                Ok(Ok(Attempt::HeldByOther)) => {
                    return LeadershipLost("lease was taken by another replica".into());
                }
                // Retried until the renew deadline.
                Ok(Ok(Attempt::Conflict)) => {
                    debug!("Leader lease changed while renewing, retrying")
                }
                Ok(Err(err)) => warn!(error = %err, "Failed to renew leader lease"),
                Err(_) => warn!("Timed out renewing leader lease"),
            }
            if last_renewed.elapsed() > self.cfg.renew_deadline {
                return LeadershipLost(format!(
                    "lease not renewed within {}",
                    crate::duration::format(self.cfg.renew_deadline)
                ));
            }
        }
    }

    /// Gives up the lease if this replica still holds it, so a standby can take
    /// over without waiting for it to expire. Call only after reconciling has
    /// stopped.
    pub async fn release(&self) {
        let lease = match self.api.get(&self.cfg.name).await {
            Ok(lease) => lease,
            Err(err) => {
                warn!(error = %err, "Failed to read leader lease for release");
                return;
            }
        };
        let holder = lease
            .spec
            .as_ref()
            .and_then(|s| s.holder_identity.as_deref());
        if holder != Some(self.cfg.identity.as_str()) {
            return;
        }
        let mut released = lease.clone();
        let spec = released.spec.get_or_insert_with(LeaseSpec::default);
        spec.holder_identity = None;
        spec.lease_duration_seconds = Some(1);
        spec.renew_time = Some(now());
        match self
            .api
            .replace(&self.cfg.name, &PostParams::default(), &released)
            .await
        {
            Ok(_) => info!("Released leader lease"),
            Err(err) => warn!(error = %err, "Failed to release leader lease"),
        }
    }

    async fn try_acquire_or_renew(&self) -> kube::Result<Attempt> {
        let lease_secs = i32::try_from(self.cfg.lease_duration.as_secs())
            .unwrap_or(i32::MAX)
            .max(1);
        let existing = match self.api.get(&self.cfg.name).await {
            Ok(lease) => lease,
            Err(err) if is_not_found(&err) => {
                let lease = Lease {
                    metadata: ObjectMeta {
                        name: Some(self.cfg.name.clone()),
                        namespace: Some(self.cfg.namespace.clone()),
                        ..Default::default()
                    },
                    spec: Some(LeaseSpec {
                        holder_identity: Some(self.cfg.identity.clone()),
                        lease_duration_seconds: Some(lease_secs),
                        acquire_time: Some(now()),
                        renew_time: Some(now()),
                        lease_transitions: Some(0),
                        ..Default::default()
                    }),
                };
                return match self.api.create(&PostParams::default(), &lease).await {
                    Ok(created) => {
                        self.observe(created.spec);
                        Ok(Attempt::Held)
                    }
                    Err(err) if is_already_exists(&err) || is_conflict(&err) => {
                        Ok(Attempt::Conflict)
                    }
                    Err(err) => Err(err),
                };
            }
            Err(err) => return Err(err),
        };

        let spec = existing.spec.clone().unwrap_or_default();
        let observed_at = self.observe(existing.spec.clone());
        let holder = spec.holder_identity.clone().unwrap_or_default();
        let held_by_us = holder == self.cfg.identity;
        if !holder.is_empty() && !held_by_us {
            let duration = spec
                .lease_duration_seconds
                .and_then(|s| u64::try_from(s).ok())
                .map_or(self.cfg.lease_duration, Duration::from_secs);
            if observed_at.elapsed() < duration {
                return Ok(Attempt::HeldByOther);
            }
        }

        let mut updated = existing.clone();
        let new_spec = updated.spec.get_or_insert_with(LeaseSpec::default);
        new_spec.holder_identity = Some(self.cfg.identity.clone());
        new_spec.lease_duration_seconds = Some(lease_secs);
        new_spec.renew_time = Some(now());
        if !held_by_us {
            new_spec.acquire_time = Some(now());
            new_spec.lease_transitions =
                Some(spec.lease_transitions.unwrap_or(0).saturating_add(1));
        }
        match self
            .api
            .replace(&self.cfg.name, &PostParams::default(), &updated)
            .await
        {
            Ok(replaced) => {
                self.observe(replaced.spec);
                Ok(Attempt::Held)
            }
            Err(err) if is_conflict(&err) => Ok(Attempt::Conflict),
            Err(err) => Err(err),
        }
    }

    /// Records the lease record and returns when it was first seen in this
    /// exact state.
    fn observe(&self, spec: Option<LeaseSpec>) -> Instant {
        let mut observed = lock(&self.observed);
        match observed.as_ref() {
            Some(o) if o.spec == spec => o.at,
            _ => {
                let at = Instant::now();
                *observed = Some(Observed { spec, at });
                at
            }
        }
    }
}

fn now() -> MicroTime {
    MicroTime(k8s_openapi::jiff::Timestamp::now())
}

#[cfg(test)]
mod tests {
    use std::convert::Infallible;
    use std::sync::{Arc, Mutex};

    use http::{Request, Response, StatusCode};
    use kube::client::Body;
    use serde_json::{Value, json};

    use super::*;

    const NS: &str = "default";
    const NAME: &str = "test-lease";

    /// A minimal in-memory Lease endpoint with resourceVersion concurrency.
    #[derive(Clone, Default)]
    struct LeaseServer {
        lease: Arc<Mutex<Option<Value>>>,
    }

    impl LeaseServer {
        fn client(&self) -> Client {
            let server = self.clone();
            let service = tower::service_fn(move |req: Request<Body>| {
                let server = server.clone();
                async move { Ok::<_, Infallible>(server.handle(req).await) }
            });
            Client::new(service, NS)
        }

        fn holder(&self) -> Option<String> {
            let lease = self.lease.lock().unwrap();
            lease.as_ref()?["spec"]["holderIdentity"]
                .as_str()
                .map(str::to_string)
        }

        /// Another replica writes the lease directly.
        fn set_holder(&self, holder: &str) {
            let mut lease = self.lease.lock().unwrap();
            let lease = lease.as_mut().unwrap();
            lease["spec"]["holderIdentity"] = json!(holder);
            lease["spec"]["renewTime"] = json!(now());
            bump(lease);
        }

        async fn handle(&self, req: Request<Body>) -> Response<Body> {
            let method = req.method().clone();
            let path = req.uri().path().to_string();
            let body = req.into_body().collect_bytes().await.unwrap();
            let mut stored = self.lease.lock().unwrap();
            let base = format!("/apis/coordination.k8s.io/v1/namespaces/{NS}/leases");
            match (method.as_str(), path.as_str()) {
                ("GET", p) if p == format!("{base}/{NAME}") => match stored.as_ref() {
                    Some(lease) => respond(StatusCode::OK, lease.clone()),
                    None => status(404, "NotFound"),
                },
                ("POST", p) if p == base => {
                    if stored.is_some() {
                        return status(409, "AlreadyExists");
                    }
                    let mut lease: Value = serde_json::from_slice(&body).unwrap();
                    lease["metadata"]["resourceVersion"] = json!("1");
                    *stored = Some(lease.clone());
                    respond(StatusCode::CREATED, lease)
                }
                ("PUT", p) if p == format!("{base}/{NAME}") => {
                    let mut lease: Value = serde_json::from_slice(&body).unwrap();
                    let current = stored
                        .as_ref()
                        .map(|l| l["metadata"]["resourceVersion"].clone());
                    if current != Some(lease["metadata"]["resourceVersion"].clone()) {
                        return status(409, "Conflict");
                    }
                    bump(&mut lease);
                    *stored = Some(lease.clone());
                    respond(StatusCode::OK, lease)
                }
                _ => status(405, "MethodNotAllowed"),
            }
        }
    }

    fn bump(lease: &mut Value) {
        let rv: u64 = lease["metadata"]["resourceVersion"]
            .as_str()
            .unwrap()
            .parse()
            .unwrap();
        lease["metadata"]["resourceVersion"] = json!((rv + 1).to_string());
    }

    fn respond(code: StatusCode, body: Value) -> Response<Body> {
        Response::builder()
            .status(code)
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(&body).unwrap()))
            .unwrap()
    }

    fn status(code: u16, reason: &str) -> Response<Body> {
        respond(
            StatusCode::from_u16(code).unwrap(),
            json!({"kind": "Status", "apiVersion": "v1", "status": "Failure", "reason": reason, "code": code, "message": reason}),
        )
    }

    fn elector(server: &LeaseServer, identity: &str) -> Arc<LeaderElector> {
        let mut cfg = LeaderElectionConfig::new(NS, NAME, identity);
        cfg.lease_duration = Duration::from_millis(400);
        cfg.renew_deadline = Duration::from_millis(250);
        cfg.retry_period = Duration::from_millis(50);
        Arc::new(LeaderElector::new(server.client(), cfg))
    }

    async fn acquire_within(elector: &LeaderElector, within: Duration) -> bool {
        let cancel = CancellationToken::new();
        let cancel_later = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(within).await;
            cancel_later.cancel();
        });
        elector.acquire(&cancel).await
    }

    #[tokio::test]
    async fn one_leader_at_a_time_with_release_and_expiry() {
        let server = LeaseServer::default();
        let a = elector(&server, "a");
        let b = elector(&server, "b");

        assert!(acquire_within(&a, Duration::from_secs(2)).await);
        assert_eq!(server.holder().as_deref(), Some("a"));

        // A renews; B stays a standby well past the lease duration.
        let holding = {
            let a = Arc::clone(&a);
            tokio::spawn(async move { a.hold().await })
        };
        assert!(!acquire_within(&b, Duration::from_millis(900)).await);
        assert_eq!(server.holder().as_deref(), Some("a"));
        assert!(!holding.is_finished(), "A still holds the lease");

        // A stops renewing without releasing: B takes over once it expires.
        holding.abort();
        let start = Instant::now();
        assert!(acquire_within(&b, Duration::from_secs(3)).await);
        assert!(
            start.elapsed() >= Duration::from_millis(300),
            "took over before expiry"
        );
        assert_eq!(server.holder().as_deref(), Some("b"));

        // B releases: A acquires immediately.
        b.release().await;
        let start = Instant::now();
        assert!(acquire_within(&a, Duration::from_secs(2)).await);
        assert!(
            start.elapsed() < Duration::from_millis(200),
            "release did not hand over promptly"
        );
        assert_eq!(server.holder().as_deref(), Some("a"));
    }

    #[tokio::test]
    async fn holder_notices_when_the_lease_is_taken() {
        let server = LeaseServer::default();
        let a = elector(&server, "a");
        assert!(acquire_within(&a, Duration::from_secs(2)).await);
        server.set_holder("intruder");
        let lost = tokio::time::timeout(Duration::from_secs(2), a.hold())
            .await
            .expect("hold returns when the lease is taken");
        assert!(lost.to_string().contains("taken by another replica"));
        // A release by a non-holder leaves the lease alone.
        a.release().await;
        assert_eq!(server.holder().as_deref(), Some("intruder"));
    }
}
