//! Reconciler tests against the in-memory Kubernetes API.

use std::collections::BTreeMap;
use std::time::Duration;

use k8s_openapi::api::core::v1::{
    ContainerState, ContainerStateWaiting, ContainerStatus, Pod, PodCondition, PodStatus,
    ResourceRequirements,
};
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{ObjectMeta, Time};
use kube::ResourceExt;
use kube::runtime::events::EventType;

use api::fake::FakeKube;
use api::{
    BuildPhase, ControllerClient, NixBuildRequest, NixBuildRequestSpec, NixBuildRequestStatus,
};
use common::SpanRecorder;

use super::*;
use crate::reconciler::{REQUEUE_CONFLICT, REQUEUE_CREATING, REQUEUE_RUNNING, pod_failure_reason};

const NS: &str = "default";

fn reconciler(fake: &FakeKube) -> Reconciler<FakeKube> {
    Reconciler::new(
        fake.clone(),
        ReconcilerConfig {
            builder_image: "ghcr.io/example/builder:v1".into(),
            builder_image_pull_policy: Some("IfNotPresent".into()),
            remote_port: 22,
            nix_config_map: Some("nix-builder-config".into()),
            ssh_key_secret: "nix-builder-ssh-keys".into(),
            mount_builder_host_key: true,
            completed_ttl: Duration::from_secs(60),
            default_resources: ResourceRequirements {
                requests: Some(BTreeMap::from([("cpu".into(), Quantity("1".into()))])),
                ..Default::default()
            },
            builder_service_account: None,
        },
        metrics(),
    )
}

fn metrics() -> Metrics {
    Metrics::new(&opentelemetry::global::meter("test"))
}

fn new_request(session: &str) -> NixBuildRequest {
    let mut req = NixBuildRequest::new(
        &format!("build-{session}"),
        NixBuildRequestSpec {
            session_id: session.into(),
            ..Default::default()
        },
    );
    req.metadata.namespace = Some(NS.into());
    req.metadata.uid = Some(format!("uid-{session}"));
    req
}

fn with_finalizer(mut req: NixBuildRequest) -> NixBuildRequest {
    req.metadata.finalizers = Some(vec![FINALIZER_NAME.into()]);
    req
}

async fn reconcile(r: &Reconciler<FakeKube>, fake: &FakeKube, name: &str) -> ReconcileAction {
    let req = fake.request(NS, name).expect("request exists");
    r.reconcile(&req)
        .await
        .unwrap_or_else(|e| panic!("reconcile {name}: {e}"))
}

fn status(fake: &FakeKube, name: &str) -> NixBuildRequestStatus {
    fake.request(NS, name)
        .expect("request exists")
        .status
        .unwrap_or_default()
}

#[tokio::test]
async fn pending_request_creates_pod_and_moves_to_creating() {
    let fake = FakeKube::new();
    fake.insert_request(new_request("s1"));
    let r = reconciler(&fake);

    // The first pass adds the finalizer only.
    reconcile(&r, &fake, "build-s1").await;
    assert!(
        fake.request(NS, "build-s1")
            .unwrap()
            .finalizers()
            .contains(&FINALIZER_NAME.to_string())
    );
    assert!(fake.pod(NS, "nix-builder-s1").is_none());

    let action = reconcile(&r, &fake, "build-s1").await;
    assert!(
        action.requeue_after().is_some(),
        "expected requeue while pod starts"
    );
    let st = status(&fake, "build-s1");
    assert_eq!(st.phase, Some(BuildPhase::Creating));
    assert_eq!(st.pod_name, "nix-builder-s1");
    assert!(st.start_time.is_some());

    let pod = fake.pod(NS, "nix-builder-s1").expect("pod created");
    let owners = pod.metadata.owner_references.clone().unwrap();
    assert_eq!(owners.len(), 1);
    assert_eq!(owners[0].uid, "uid-s1");
    assert_eq!(owners[0].kind, "NixBuildRequest");
    assert_eq!(owners[0].api_version, "nix.io/v1alpha1");
    assert_eq!(owners[0].controller, Some(true));
    assert_eq!(owners[0].block_owner_deletion, Some(true));

    let spec = pod.spec.unwrap();
    let container = &spec.containers[0];
    assert_eq!(
        container.image.as_deref(),
        Some("ghcr.io/example/builder:v1")
    );
    assert_eq!(container.image_pull_policy.as_deref(), Some("IfNotPresent"));
    assert_eq!(
        spec.active_deadline_seconds,
        Some(DEFAULT_BUILD_TIMEOUT_SECONDS)
    );
    assert_eq!(spec.restart_policy.as_deref(), Some("Never"));
    assert_eq!(
        container
            .resources
            .as_ref()
            .unwrap()
            .requests
            .as_ref()
            .unwrap()["cpu"],
        Quantity("1".into()),
        "default resources not applied"
    );
    let labels = pod.metadata.labels.unwrap();
    assert_eq!(labels[LABEL_APP], LABEL_APP_VALUE);
    assert_eq!(labels[api::LABEL_SESSION_ID], "s1");
    assert_eq!(labels[LABEL_BUILD_REQUEST], "build-s1");

    let events = fake.events();
    assert!(
        events.iter().any(|e| e.reason == "PodCreated"
            && e.type_ == EventType::Normal
            && e.action == "CreatePod"),
        "expected a PodCreated event, got {events:?}"
    );
}

#[tokio::test]
async fn pod_spec_honours_request_overrides() {
    let fake = FakeKube::new();
    let r = reconciler(&fake);
    let mut req = new_request("s2");
    req.spec.image = "custom/builder:dev".into();
    req.spec.timeout_seconds = Some(120);
    req.spec.node_selector = Some(BTreeMap::from([(
        "kubernetes.io/arch".into(),
        "arm64".into(),
    )]));
    req.spec.resources = ResourceRequirements {
        limits: Some(BTreeMap::from([("memory".into(), Quantity("4Gi".into()))])),
        ..Default::default()
    };

    let pod = r.builder_pod(&req);
    let spec = pod.spec.as_ref().unwrap();
    let container = &spec.containers[0];
    assert_eq!(container.image.as_deref(), Some("custom/builder:dev"));
    assert_eq!(spec.active_deadline_seconds, Some(120));
    assert_eq!(
        spec.node_selector.as_ref().unwrap()["kubernetes.io/arch"],
        "arm64"
    );
    let resources = container.resources.as_ref().unwrap();
    assert_eq!(
        resources.limits.as_ref().unwrap()["memory"],
        Quantity("4Gi".into())
    );
    assert!(
        resources.requests.is_none(),
        "spec resources replace the defaults entirely"
    );

    let mounts: BTreeMap<String, Option<String>> = container
        .volume_mounts
        .iter()
        .flatten()
        .map(|m| (m.mount_path.clone(), m.sub_path.clone()))
        .collect();
    assert!(mounts.contains_key("/etc/nix"));
    assert_eq!(
        mounts["/home/nixbld/.ssh/authorized_keys"].as_deref(),
        Some("public")
    );
    assert_eq!(
        mounts[BUILDER_HOST_KEY_MOUNT_PATH].as_deref(),
        Some("ssh_host_ed25519_key")
    );
    assert!(
        container
            .volume_mounts
            .iter()
            .flatten()
            .all(|m| m.read_only == Some(true))
    );
    let probe = container.readiness_probe.as_ref().unwrap();
    assert_eq!(probe.period_seconds, Some(1));
    assert_eq!(probe.failure_threshold, Some(60));

    // A non-positive timeout means "no deadline"; Kubernetes rejects
    // activeDeadlineSeconds=0, so the field must be absent rather than 0.
    for timeout in [-5, 0] {
        req.spec.timeout_seconds = Some(timeout);
        assert_eq!(
            r.builder_pod(&req).spec.unwrap().active_deadline_seconds,
            None,
            "timeout {timeout}"
        );
    }
}

#[tokio::test]
async fn reconciles_join_the_trace_of_their_session() {
    const TRACE: &str = "0af7651916cd43dd8448eb211c80319c";
    const SESSION_SPAN: &str = "b7ad6b7169203331";
    let recorder = SpanRecorder::new();
    let fake = FakeKube::new();
    let r = reconciler(&fake);
    let mut req = new_request("traced");
    req.metadata.annotations = Some(BTreeMap::from([(
        api::ANNOTATION_TRACEPARENT.to_string(),
        format!("00-{TRACE}-{SESSION_SPAN}-01"),
    )]));
    fake.insert_request(req);
    reconcile(&r, &fake, "build-traced").await;

    let spans = recorder.finished_spans();
    assert_eq!(spans.len(), 1, "one reconcile ran: {spans:?}");
    assert_eq!(spans[0].name, "reconcile");
    assert_eq!(spans[0].span_context.trace_id().to_string(), TRACE);
    assert_eq!(spans[0].parent_span_id.to_string(), SESSION_SPAN);
}

#[tokio::test]
async fn optional_mounts_are_omitted_when_not_configured() {
    let fake = FakeKube::new();
    let r = Reconciler::new(
        fake,
        ReconcilerConfig {
            builder_image: "img".into(),
            remote_port: 2022,
            ssh_key_secret: "keys".into(),
            ..Default::default()
        },
        metrics(),
    );
    let pod = r.builder_pod(&new_request("s"));
    let spec = pod.spec.unwrap();
    assert_eq!(spec.volumes.as_ref().unwrap().len(), 1);
    let container = &spec.containers[0];
    assert_eq!(container.volume_mounts.as_ref().unwrap().len(), 1);
    assert_eq!(container.image_pull_policy, None);
    assert_eq!(spec.service_account_name, None);
    assert_eq!(container.ports.as_ref().unwrap()[0].container_port, 2022);
    let resources = container.resources.as_ref().unwrap();
    assert!(resources.requests.is_none() && resources.limits.is_none());
}

fn mark_ready(fake: &FakeKube, pod: &str, ip: &str) {
    fake.set_pod_status(
        NS,
        pod,
        PodStatus {
            phase: Some("Running".into()),
            pod_ip: Some(ip.into()),
            conditions: Some(vec![PodCondition {
                type_: "ContainersReady".into(),
                status: "True".into(),
                ..Default::default()
            }]),
            ..Default::default()
        },
    );
}

#[tokio::test]
async fn creating_moves_to_running_when_pod_has_ip() {
    let fake = FakeKube::new();
    fake.insert_request(with_finalizer(new_request("s3")));
    let r = reconciler(&fake);
    reconcile(&r, &fake, "build-s3").await; // Pending -> Creating, pod created

    // Not ready yet: stays Creating and surfaces the pending reason.
    fake.set_pod_status(
        NS,
        "nix-builder-s3",
        PodStatus {
            phase: Some("Pending".into()),
            conditions: Some(vec![PodCondition {
                type_: "PodScheduled".into(),
                status: "False".into(),
                reason: Some("Unschedulable".into()),
                message: Some("0/1 nodes".into()),
                ..Default::default()
            }]),
            ..Default::default()
        },
    );
    let action = reconcile(&r, &fake, "build-s3").await;
    assert_eq!(action, ReconcileAction::Requeue(REQUEUE_CREATING));
    let st = status(&fake, "build-s3");
    assert_eq!(st.phase, Some(BuildPhase::Creating));
    assert_eq!(
        st.message,
        "Builder pod not scheduled: Unschedulable: 0/1 nodes"
    );

    // An unchanged reason is not rewritten.
    let version = fake
        .request(NS, "build-s3")
        .unwrap()
        .metadata
        .resource_version;
    reconcile(&r, &fake, "build-s3").await;
    assert_eq!(
        fake.request(NS, "build-s3")
            .unwrap()
            .metadata
            .resource_version,
        version
    );

    mark_ready(&fake, "nix-builder-s3", "10.1.2.3");
    let action = reconcile(&r, &fake, "build-s3").await;
    assert_eq!(action, ReconcileAction::Requeue(REQUEUE_RUNNING));
    let st = status(&fake, "build-s3");
    assert_eq!(st.phase, Some(BuildPhase::Running));
    assert_eq!(st.pod_ip, "10.1.2.3");
    assert!(fake.events().iter().any(|e| e.reason == "PodReady"));
}

#[tokio::test]
async fn image_pull_problems_are_surfaced() {
    let fake = FakeKube::new();
    fake.insert_request(with_finalizer(new_request("img")));
    let r = reconciler(&fake);
    reconcile(&r, &fake, "build-img").await;
    fake.set_pod_status(
        NS,
        "nix-builder-img",
        PodStatus {
            phase: Some("Pending".into()),
            container_statuses: Some(vec![ContainerStatus {
                name: "nix-builder".into(),
                state: Some(ContainerState {
                    waiting: Some(ContainerStateWaiting {
                        reason: Some("ErrImagePull".into()),
                        message: Some("not found".into()),
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            }]),
            ..Default::default()
        },
    );
    reconcile(&r, &fake, "build-img").await;
    assert_eq!(
        status(&fake, "build-img").message,
        "Builder pod waiting: ErrImagePull: not found"
    );
}

#[tokio::test]
async fn pod_failure_marks_request_failed() {
    let fake = FakeKube::new();
    fake.insert_request(with_finalizer(new_request("s4")));
    let r = reconciler(&fake);
    reconcile(&r, &fake, "build-s4").await;

    fake.set_pod_status(
        NS,
        "nix-builder-s4",
        PodStatus {
            phase: Some("Failed".into()),
            reason: Some("DeadlineExceeded".into()),
            message: Some("Pod was active on the node longer than the specified deadline".into()),
            ..Default::default()
        },
    );
    let action = reconcile(&r, &fake, "build-s4").await;
    assert_eq!(action, ReconcileAction::Requeue(Duration::from_secs(60)));
    let st = status(&fake, "build-s4");
    assert_eq!(st.phase, Some(BuildPhase::Failed));
    assert!(st.completion_time.is_some());
    assert!(
        st.message.contains("DeadlineExceeded"),
        "message = {}",
        st.message
    );
    assert!(
        fake.events()
            .iter()
            .any(|e| e.reason == "PodFailed" && e.type_ == EventType::Warning)
    );
}

#[tokio::test]
async fn pod_that_ends_before_it_is_ready_fails_the_request() {
    let fake = FakeKube::new();
    let r = reconciler(&fake);
    for session in ["exited", "gone"] {
        fake.insert_request(with_finalizer(new_request(session)));
        reconcile(&r, &fake, &format!("build-{session}")).await; // -> Creating
    }
    fake.set_pod_status(
        NS,
        "nix-builder-exited",
        PodStatus {
            phase: Some("Succeeded".into()),
            ..Default::default()
        },
    );
    fake.delete_pod(NS, "nix-builder-gone").await.unwrap();

    for (session, reason, message) in [
        (
            "exited",
            "PodExited",
            "Builder pod exited before it became ready",
        ),
        (
            "gone",
            "PodMissing",
            "Builder pod was deleted before it became ready",
        ),
    ] {
        let name = format!("build-{session}");
        let action = reconcile(&r, &fake, &name).await;
        assert_eq!(action, ReconcileAction::Requeue(Duration::from_secs(60)));
        let st = status(&fake, &name);
        assert_eq!(st.phase, Some(BuildPhase::Failed), "{session}");
        assert_eq!(st.message, message);
        assert!(st.completion_time.is_some());
        assert!(fake.events().iter().any(|e| e.reason == reason));
    }
}

#[tokio::test]
async fn pod_that_exits_while_running_completes_the_request() {
    let fake = FakeKube::new();
    let mut req = with_finalizer(new_request("done"));
    req.status = Some(NixBuildRequestStatus {
        phase: Some(BuildPhase::Running),
        pod_name: "nix-builder-done".into(),
        pod_ip: "10.0.0.9".into(),
        ..Default::default()
    });
    fake.insert_request(req);
    fake.insert_pod(Pod {
        metadata: ObjectMeta {
            name: Some("nix-builder-done".into()),
            namespace: Some(NS.into()),
            ..Default::default()
        },
        status: Some(PodStatus {
            phase: Some("Succeeded".into()),
            ..Default::default()
        }),
        ..Default::default()
    });
    let r = reconciler(&fake);
    reconcile(&r, &fake, "build-done").await;
    let st = status(&fake, "build-done");
    assert_eq!(st.phase, Some(BuildPhase::Completed));
    assert_eq!(st.message, "Builder pod exited");
    assert!(st.completion_time.is_some());
}

#[tokio::test]
async fn missing_pod_while_running_marks_failed() {
    let fake = FakeKube::new();
    let mut req = with_finalizer(new_request("s5"));
    req.status = Some(NixBuildRequestStatus {
        phase: Some(BuildPhase::Running),
        pod_name: "nix-builder-s5".into(),
        ..Default::default()
    });
    fake.insert_request(req);
    let r = reconciler(&fake);
    reconcile(&r, &fake, "build-s5").await;
    let st = status(&fake, "build-s5");
    assert_eq!(st.phase, Some(BuildPhase::Failed));
    assert_eq!(st.message, "Builder pod was deleted while running");
}

#[tokio::test]
async fn deletion_cleans_up_pod_and_removes_finalizer() {
    let fake = FakeKube::new();
    fake.insert_request(with_finalizer(new_request("s6")));
    let r = reconciler(&fake);
    reconcile(&r, &fake, "build-s6").await;
    assert!(fake.pod(NS, "nix-builder-s6").is_some());

    fake.delete_request(NS, "build-s6").await.unwrap();
    assert!(
        fake.request(NS, "build-s6")
            .unwrap()
            .metadata
            .deletion_timestamp
            .is_some(),
        "finalizer holds the request back"
    );
    reconcile(&r, &fake, "build-s6").await;
    assert!(
        fake.pod(NS, "nix-builder-s6").is_none(),
        "pod should have been deleted"
    );
    assert!(
        fake.request(NS, "build-s6").is_none(),
        "request should be gone once the finalizer is removed"
    );
}

#[tokio::test]
async fn deletion_before_status_still_deletes_deterministic_pod() {
    let fake = FakeKube::new();
    let mut req = with_finalizer(new_request("s7"));
    req.metadata.deletion_timestamp = Some(Time(k8s_openapi::jiff::Timestamp::now()));
    fake.insert_request(req);
    fake.insert_pod(Pod {
        metadata: ObjectMeta {
            name: Some("nix-builder-s7".into()),
            namespace: Some(NS.into()),
            ..Default::default()
        },
        ..Default::default()
    });
    let r = reconciler(&fake);
    reconcile(&r, &fake, "build-s7").await;
    assert!(
        fake.pod(NS, "nix-builder-s7").is_none(),
        "pod should be deleted even without status.podName"
    );
    assert!(fake.request(NS, "build-s7").is_none());
}

#[tokio::test]
async fn deletion_without_our_finalizer_is_ignored() {
    let fake = FakeKube::new();
    let mut req = new_request("other");
    req.metadata.finalizers = Some(vec!["someone.else/finalizer".into()]);
    req.metadata.deletion_timestamp = Some(Time(k8s_openapi::jiff::Timestamp::now()));
    fake.insert_request(req);
    let r = reconciler(&fake);
    let action = reconcile(&r, &fake, "build-other").await;
    assert_eq!(action, ReconcileAction::AwaitChange);
    assert!(fake.request(NS, "build-other").is_some());
}

#[tokio::test]
async fn completed_request_is_reaped_after_ttl() {
    let fake = FakeKube::new();
    let mut old = with_finalizer(new_request("s8"));
    old.status = Some(NixBuildRequestStatus {
        phase: Some(BuildPhase::Failed),
        completion_time: Some(Time(
            k8s_openapi::jiff::Timestamp::now() - k8s_openapi::jiff::SignedDuration::from_secs(120),
        )),
        ..Default::default()
    });
    fake.insert_request(old);
    let r = reconciler(&fake);
    reconcile(&r, &fake, "build-s8").await;
    assert!(
        fake.request(NS, "build-s8")
            .unwrap()
            .metadata
            .deletion_timestamp
            .is_some(),
        "expected the request to be deleted after its TTL"
    );

    let mut fresh = with_finalizer(new_request("s9"));
    fresh.status = Some(NixBuildRequestStatus {
        phase: Some(BuildPhase::Completed),
        completion_time: Some(api::now()),
        ..Default::default()
    });
    fake.insert_request(fresh);
    let action = reconcile(&r, &fake, "build-s9").await;
    let after = action.requeue_after().expect("requeue until TTL");
    assert!(
        after > Duration::ZERO && after <= Duration::from_secs(60),
        "requeue after {after:?}"
    );
    assert!(
        fake.request(NS, "build-s9")
            .unwrap()
            .metadata
            .deletion_timestamp
            .is_none()
    );
}

#[tokio::test]
async fn finished_request_without_completion_time_is_reaped_by_age() {
    let fake = FakeKube::new();
    let mut req = with_finalizer(new_request("nct"));
    req.metadata.creation_timestamp = Some(Time(
        k8s_openapi::jiff::Timestamp::now() - k8s_openapi::jiff::SignedDuration::from_secs(600),
    ));
    req.status = Some(NixBuildRequestStatus {
        phase: Some(BuildPhase::Completed),
        ..Default::default()
    });
    fake.insert_request(req);
    let r = reconciler(&fake);
    reconcile(&r, &fake, "build-nct").await;
    assert!(
        fake.request(NS, "build-nct")
            .unwrap()
            .metadata
            .deletion_timestamp
            .is_some()
    );
}

#[tokio::test]
async fn stale_writes_requeue_promptly_instead_of_failing() {
    let fake = FakeKube::new();
    fake.insert_request(with_finalizer(new_request("c1")));
    let r = reconciler(&fake);
    let stale = fake.request(NS, "build-c1").unwrap();
    // Someone else (the proxy) writes the object first.
    fake.set_request_status(NS, "build-c1", NixBuildRequestStatus::default());
    let action = r.reconcile(&stale).await.expect("conflict is not an error");
    assert_eq!(action, ReconcileAction::Requeue(REQUEUE_CONFLICT));
    // The pod was still created; the next pass adopts it.
    assert!(fake.pod(NS, "nix-builder-c1").is_some());
    reconcile(&r, &fake, "build-c1").await;
    assert_eq!(status(&fake, "build-c1").phase, Some(BuildPhase::Creating));
}

#[tokio::test]
async fn existing_pod_is_adopted() {
    let fake = FakeKube::new();
    fake.insert_request(with_finalizer(new_request("dup")));
    let r = reconciler(&fake);
    fake.insert_pod(r.builder_pod(&fake.request(NS, "build-dup").unwrap()));
    reconcile(&r, &fake, "build-dup").await;
    let st = status(&fake, "build-dup");
    assert_eq!(st.phase, Some(BuildPhase::Creating));
    assert_eq!(st.pod_name, "nix-builder-dup");
    assert!(
        !fake
            .events()
            .iter()
            .any(|e| e.reason == "PodCreated" || e.reason == "PodCreateFailed")
    );
}

#[tokio::test]
async fn invalid_pod_fails_the_request() {
    let fake = FakeKube::new();
    fake.insert_request(with_finalizer(new_request("bad")));
    let r = reconciler(&fake);
    fake.fail_next_pod_create(422, "Invalid", "metadata.name: Invalid value");

    reconcile(&r, &fake, "build-bad").await;
    let st = status(&fake, "build-bad");
    assert_eq!(st.phase, Some(BuildPhase::Failed), "retrying cannot help");
    assert!(
        st.message.starts_with("Builder pod is invalid: "),
        "message = {}",
        st.message
    );
}

#[tokio::test]
async fn pod_create_failure_is_reported_and_retried() {
    let fake = FakeKube::new();
    fake.insert_request(with_finalizer(new_request("quota")));
    let r = reconciler(&fake);
    fake.fail_next_pod_create(403, "Forbidden", "exceeded quota: pods");

    let req = fake.request(NS, "build-quota").unwrap();
    let err = r
        .reconcile(&req)
        .await
        .expect_err("create failure is retried with backoff");
    assert!(err.to_string().contains("exceeded quota"), "error = {err}");
    let st = status(&fake, "build-quota");
    assert_eq!(st.phase, None, "the request is not failed yet");
    assert!(
        st.message.starts_with("Failed to create builder pod: "),
        "message = {}",
        st.message
    );
    assert!(st.message.contains("exceeded quota"));
    assert!(
        fake.events()
            .iter()
            .any(|e| e.reason == "PodCreateFailed" && e.type_ == EventType::Warning)
    );

    // The same failure again does not write the status again: that write
    // would trigger another reconcile and bypass the retry backoff.
    let version = fake
        .request(NS, "build-quota")
        .unwrap()
        .metadata
        .resource_version;
    fake.fail_next_pod_create(403, "Forbidden", "exceeded quota: pods");
    let req = fake.request(NS, "build-quota").unwrap();
    let err = r.reconcile(&req).await.expect_err("still failing");
    assert!(err.to_string().contains("exceeded quota"), "error = {err}");
    assert_eq!(
        fake.request(NS, "build-quota")
            .unwrap()
            .metadata
            .resource_version,
        version
    );

    // The retry succeeds once the API accepts the pod.
    reconcile(&r, &fake, "build-quota").await;
    assert_eq!(
        status(&fake, "build-quota").phase,
        Some(BuildPhase::Creating)
    );
    assert!(fake.pod(NS, "nix-builder-quota").is_some());
}

#[tokio::test]
async fn unknown_phase_is_left_alone() {
    let fake = FakeKube::new();
    let mut req = with_finalizer(new_request("u"));
    req.status = Some(NixBuildRequestStatus {
        phase: Some(BuildPhase::Unknown),
        ..Default::default()
    });
    fake.insert_request(req);
    let r = reconciler(&fake);
    let before = fake
        .request(NS, "build-u")
        .unwrap()
        .metadata
        .resource_version;
    assert_eq!(
        reconcile(&r, &fake, "build-u").await,
        ReconcileAction::AwaitChange
    );
    assert_eq!(
        fake.request(NS, "build-u")
            .unwrap()
            .metadata
            .resource_version,
        before
    );
}

#[test]
fn failure_reasons_are_summarised() {
    let mut pod = Pod::default();
    assert_eq!(pod_failure_reason(&pod), "unknown");
    pod.status = Some(PodStatus {
        container_statuses: Some(vec![ContainerStatus {
            name: "nix-builder".into(),
            state: Some(ContainerState {
                terminated: Some(k8s_openapi::api::core::v1::ContainerStateTerminated {
                    exit_code: 1,
                    reason: Some("Error".into()),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        }]),
        ..Default::default()
    });
    assert_eq!(
        pod_failure_reason(&pod),
        "container nix-builder exited with code 1 (Error)"
    );
    pod.status.as_mut().unwrap().reason = Some("Evicted".into());
    assert_eq!(pod_failure_reason(&pod), "Evicted");
    pod.status.as_mut().unwrap().message = Some("low memory".into());
    assert_eq!(pod_failure_reason(&pod), "Evicted: low memory");
}
