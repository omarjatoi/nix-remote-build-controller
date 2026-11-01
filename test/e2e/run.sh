#!/usr/bin/env bash
# End-to-end test for nix-remote-build-controller.
#
# Creates a Kind cluster, deploys the controller and proxy, and runs a real
# `nix build` of a synthetic uncached DAG (top -> {dep-a, dep-b}) through the
# proxy. Asserts that the two dependencies build concurrently in separate pods,
# that top waits for them, that the result transfers back, that a failing
# derivation fails cleanly, and that all pods and requests are cleaned up.
#
# HA=1 runs on a three-node cluster with two controller replicas using leader
# election, and checks that a standby takes over when the leader pod is deleted.
#
# OTEL=1 adds an OpenTelemetry collector, and checks that the local Nix client,
# the proxy, the controller and Nix on the builder pods all end up in one trace,
# with both ssh:// and ssh-ng:// builders. It needs Determinate Nix on the client.
#
# HELM=1 deploys with the Helm chart, in a namespace of its own, instead of with
# kustomize. It works with HA=1, but not with OTEL=1.
#
# Requires on PATH: kind, kubectl, docker, nix, jq, ssh, ssh-keygen, perl, and
# helm for HELM=1.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
CLUSTER="nix-remote-build-e2e"
KEEP="${KEEP:-0}"
HA="${HA:-0}"
OTEL="${OTEL:-0}"
HELM="${HELM:-0}"
if [ "$HELM" = "1" ]; then
  NS="nix-builds"
  RELEASE="nix-remote-build-controller"
  CONTROLLER="${RELEASE}-controller"
  PROXY="${RELEASE}-proxy"
  COMPONENT_LABEL="app.kubernetes.io/component"
else
  NS="default"
  CONTROLLER="controller"
  PROXY="proxy"
  COMPONENT_LABEL="component"
fi
LEASE="nix-remote-build-controller.nix.io"
SLEEP_SECONDS="${SLEEP_SECONDS:-30}"
PROXY_PORT="${PROXY_LOCAL_PORT:-2222}"
MARKER="e2e-$(date +%s)-$$"
DAG="${REPO_ROOT}/test/e2e/dag.nix"
WORK="$(mktemp -d "${TMPDIR:-/tmp}/nrbc-e2e.XXXXXX")"
PF_PID=""
OTEL_PF_PID=""
WATCH_PID=""

log()  { printf '\n\033[1;34m==> %s\033[0m\n' "$*"; }
info() { printf '    %s\n' "$*"; }
fail() { printf '\n\033[1;31mFAIL: %s\033[0m\n' "$*" >&2; exit 1; }

cleanup() {
  local ec=$?
  [ -n "$PF_PID" ] && kill "$PF_PID" 2>/dev/null || true
  [ -n "$OTEL_PF_PID" ] && kill "$OTEL_PF_PID" 2>/dev/null || true
  [ -n "$WATCH_PID" ] && kill "$WATCH_PID" 2>/dev/null || true
  if [ "$KEEP" = "1" ]; then
    info "KEEP=1 set; leaving cluster '$CLUSTER' and workdir $WORK"
  else
    log "Tearing down"
    kind delete cluster --name "$CLUSTER" >/dev/null 2>&1 || true
    chmod -R u+w "${STORE:-/nonexistent}" 2>/dev/null || true
    rm -rf "$WORK" "${STORE:-}"
  fi
  exit $ec
}
trap cleanup EXIT

for t in kind kubectl docker nix jq ssh ssh-keygen; do
  command -v "$t" >/dev/null 2>&1 || fail "missing required tool: $t"
done
[ "$HELM" = "1" ] && { command -v helm >/dev/null 2>&1 || fail "missing required tool: helm"; }

# Nix ignores a client-specified `builders` setting for untrusted users. Rather
# than edit /etc/nix and restart the daemon, build into a private chroot store:
# the invoking user is always trusted for a store it created. The store must have
# no symlink ancestors, so `pwd -P` resolves macOS's /var and /tmp symlinks.
STORE="$(cd "$(mktemp -d "${TMPDIR:-/tmp}/nrbc-store.XXXXXX")" && pwd -P)"
[ "$HA" = "1" ] && [ "$OTEL" = "1" ] && fail "HA=1 and OTEL=1 cannot be combined"
[ "$HELM" = "1" ] && [ "$OTEL" = "1" ] && fail "HELM=1 and OTEL=1 cannot be combined"

[ "$(nix store info --store "$STORE" --json 2>/dev/null | jq -r '.trusted // false')" = "true" ] ||
  fail "not trusted even in a private store; cannot honor --builders"

case "$(uname -m)" in
  arm64 | aarch64) BUILD_SYSTEM="aarch64-linux" ;;
  x86_64 | amd64)  BUILD_SYSTEM="x86_64-linux" ;;
  *) fail "unsupported host arch $(uname -m)" ;;
esac
SPEC="ssh://nixbld@nrbc-proxy ${BUILD_SYSTEM} - 100 1"
info "store=$STORE system=$BUILD_SYSTEM marker=$MARKER"

# Build a DAG attribute remotely through the proxy. Args: <attr> <logfile> [extra nix args...]
remote_build() {
  local attr="$1" logf="$2"
  shift 2
  nix build --store "$STORE" --file "$DAG" "$attr" \
    --arg nixpkgs "$NP" --argstr system "$BUILD_SYSTEM" --argstr marker "$MARKER" \
    --max-jobs 0 --builders "$SPEC" --builders-use-substitutes --no-link -L "$@" \
    2> "$logf"
}

# First "@@ <name> <START|END>" timestamp in a build log (empty if absent). The
# marker and timestamp are matched in two stages so trailing padding between them
# does not matter; `|| true` keeps a missing marker from tripping `set -e`.
mark_ts() { grep -aE -m1 "@@ $1 $2 " "$3" 2>/dev/null | grep -oE -m1 '[0-9-]{10}T[0-9:]{8}Z' || true; }
epoch() { date -u -j -f "%Y-%m-%dT%H:%M:%SZ" "$1" +%s 2>/dev/null || date -u -d "$1" +%s; }
# Number of objects of a kind (with optional selectors).
count() { kubectl -n "$NS" get "$@" --no-headers 2>/dev/null | wc -l | tr -d ' '; }
# Run a command with a time limit (portable: macOS has no `timeout`).
with_timeout() { local secs="$1"; shift; perl -e 'alarm shift; exec @ARGV' "$secs" "$@"; }
# Prometheus metrics of a pod, read through the API server. Args: <pod>
pod_metrics() { kubectl get --raw "/api/v1/namespaces/${NS}/pods/${1}:8082/proxy/metrics"; }
# Name of the first pod of a component. Args: <component>
pod_of() {
  kubectl -n "$NS" get pods -l "${COMPONENT_LABEL}=$1" \
    -o jsonpath='{.items[?(@.status.phase=="Running")].metadata.name}' | cut -d' ' -f1
}
# Fail unless a metric of a pod has a sample above zero. Args: <pod> <metric regex>
want_metric() {
  pod_metrics "$1" > "${WORK}/metrics"
  grep -E "^$2" "${WORK}/metrics" | grep -vE ' 0$' >/dev/null || fail "$1 reports no $2"
  info "PASS: $1 reports $2"
}
# What the collector has logged. Later greps read this file: `pipefail` would
# fail a pipeline whose `grep -q` stopped reading early.
collector_log() { kubectl -n "$NS" logs deploy/otel-collector --tail=-1 > "${WORK}/collector.log"; }
# The spans the collector has received, one per line: service, trace ID, name.
collector_spans() {
  collector_log
  awk '
    /service\.name: Str\(/ { service = $0; sub(/.*Str\(/, "", service); sub(/\).*/, "", service) }
    /^ *Trace ID +:/ { trace = $NF }
    /^ *Name +:/ { name = $0; sub(/^ *Name +: /, "", name); print service, trace, name }' \
    "${WORK}/collector.log"
}
# Print a trace that has all of the given "service span name" spans, waiting
# for them to be exported.
find_trace() {
  local spans="${WORK}/spans" trace want missing
  for _ in $(seq 1 60); do
    collector_spans > "$spans"
    while read -r trace; do
      missing=0
      for want in "$@"; do
        grep -qF "${want%% *} $trace ${want#* }" "$spans" || missing=1
      done
      [ "$missing" = 0 ] && { echo "$trace"; return 0; }
    done < <(awk '{print $2}' "$spans" | sort -u)
    sleep 2
  done
  return 1
}

########################################################################
log "1/8 Building and loading images"
NP="$(nix eval --raw --impure --expr "builtins.toString (builtins.getFlake (toString $REPO_ROOT)).inputs.nixpkgs")"
for img in controller proxy builder; do
  nix build "${REPO_ROOT}#packages.${BUILD_SYSTEM}.${img}-image" -o "${WORK}/${img}" >/dev/null
done

log "2/8 Creating Kind cluster '$CLUSTER'"
kind delete cluster --name "$CLUSTER" >/dev/null 2>&1 || true
if [ "$HA" = "1" ]; then
  KIND_CONFIG="${REPO_ROOT}/test/e2e/kind-config-ha.yaml"
  MANIFESTS="${REPO_ROOT}/test/e2e/manifests-ha"
else
  KIND_CONFIG="${REPO_ROOT}/test/e2e/kind-config.yaml"
  MANIFESTS="${REPO_ROOT}/test/e2e/manifests"
  [ "$OTEL" = "1" ] && MANIFESTS="${REPO_ROOT}/test/e2e/manifests-otel"
fi
kind create cluster --name "$CLUSTER" --config "$KIND_CONFIG" >/dev/null
export KUBECONFIG="${WORK}/kubeconfig"
kind get kubeconfig --name "$CLUSTER" > "$KUBECONFIG"
kubectl wait --for=condition=Ready nodes --all --timeout=120s >/dev/null

for img in controller proxy builder; do
  tag="ghcr.io/omarjatoi/nix-remote-build-controller/${img}:e2e"
  docker tag "$(docker load < "${WORK}/${img}" | sed -n 's/^Loaded image: //p' | head -1)" "$tag"
  kind load docker-image "$tag" --name "$CLUSTER" >/dev/null
done

########################################################################
log "3/8 Creating SSH keys and secret"
for k in nixbld client builder-host; do
  ssh-keygen -t ed25519 -f "${WORK}/${k}" -N "" -C "$k" -q
done
kubectl create namespace "$NS" --dry-run=client -o yaml | kubectl apply -f - >/dev/null
kubectl create secret generic nix-builder-ssh-keys -n "$NS" \
  --from-file=private="${WORK}/nixbld" \
  --from-file=public="${WORK}/nixbld.pub" \
  --from-file=builder-host-key="${WORK}/builder-host" \
  --from-file=client-authorized-keys="${WORK}/client.pub" \
  --dry-run=client -o yaml | kubectl apply -f - >/dev/null

log "4/8 Deploying controller and proxy"
if [ "$HELM" = "1" ]; then
  REPLICAS=1
  [ "$HA" = "1" ] && REPLICAS=2
  helm install "$RELEASE" "${REPO_ROOT}/charts/nix-remote-build-controller" -n "$NS" \
    --values "${REPO_ROOT}/test/e2e/helm-values.yaml" \
    --set "controller.replicaCount=${REPLICAS}" >/dev/null
else
  kubectl apply -k "$MANIFESTS" >/dev/null
fi
kubectl -n "$NS" rollout status "deploy/${CONTROLLER}" --timeout=120s
kubectl -n "$NS" rollout status "deploy/${PROXY}" --timeout=120s
if [ "$OTEL" = "1" ]; then
  kubectl -n "$NS" rollout status deploy/otel-collector --timeout=180s
  kubectl -n "$NS" port-forward svc/otel-collector 4318:4318 >/dev/null 2>&1 &
  OTEL_PF_PID=$!
  # The local Nix client exports to the collector too.
  export OTEL_EXPORTER_OTLP_ENDPOINT="http://127.0.0.1:4318"
fi

# The pod holding the controller's leader lease (empty if none).
leader_pod() {
  kubectl -n "$NS" get lease "$LEASE" -o jsonpath='{.spec.holderIdentity}' 2>/dev/null | sed 's/_.*//'
}
if [ "$HA" = "1" ]; then
  for i in $(seq 1 60); do
    LEADER="$(leader_pod)"
    [ -n "$LEADER" ] && break
    [ "$i" = 60 ] && fail "no controller replica acquired the leader lease"
    sleep 1
  done
  info "PASS: controller leader is ${LEADER}"
fi

log "5/8 Port-forwarding proxy to localhost:${PROXY_PORT}"
kubectl -n "$NS" port-forward "svc/${PROXY}" "${PROXY_PORT}:22" >/dev/null 2>&1 &
PF_PID=$!
for i in $(seq 1 30); do
  (exec 3<>"/dev/tcp/127.0.0.1/${PROXY_PORT}") 2>/dev/null && { exec 3>&- 3<&-; break; }
  [ "$i" = 30 ] && fail "port-forward to proxy never came up"
  sleep 1
done
cat > "${WORK}/ssh_config" <<SSH
Host nrbc-proxy
  HostName 127.0.0.1
  Port ${PROXY_PORT}
  User nixbld
  IdentityFile ${WORK}/client
  IdentitiesOnly yes
  StrictHostKeyChecking no
  UserKnownHostsFile /dev/null
  LogLevel ERROR
  SendEnv TRACEPARENT
SSH
export NIX_SSHOPTS="-F ${WORK}/ssh_config"

# Sanity: one plain SSH session reaches a builder pod and can run nix.
ssh -F "${WORK}/ssh_config" nrbc-proxy "nix-store --version" >/dev/null 2>&1 ||
  fail "could not run nix-store on a builder pod through the proxy"

# Builder pods must reach HTTPS substituters themselves. Builds still pass if
# they cannot, because the client then uploads the dependencies, so check it
# directly.
ssh -F "${WORK}/ssh_config" nrbc-proxy "nix store info --store https://cache.nixos.org" >/dev/null 2>&1 ||
  fail "a builder pod could not reach https://cache.nixos.org"
info "PASS: builder pods can reach HTTPS substituters"

# Flow control: the builder writes a large stdout before reading any stdin while
# the client streams a large stdin. Both directions must keep moving; a proxy
# that lets one stalled direction block the other deadlocks here.
STREAM_BYTES=$((32 * 1024 * 1024))
STREAM_ERR="${WORK}/stream.err"
stream_out="$(head -c "$STREAM_BYTES" /dev/zero |
  with_timeout 180 ssh -F "${WORK}/ssh_config" nrbc-proxy \
    "head -c $STREAM_BYTES /dev/zero; wc -c >&2" 2> "$STREAM_ERR" | wc -c | tr -d ' ')" ||
  fail "bidirectional stream through the proxy failed or deadlocked"
stream_in="$(tr -dc '0-9' < "$STREAM_ERR")"
[ "$stream_out" = "$STREAM_BYTES" ] && [ "$stream_in" = "$STREAM_BYTES" ] ||
  fail "bidirectional stream lost data (stdout=${stream_out} stdin=${stream_in}, want ${STREAM_BYTES})"
info "PASS: ${STREAM_BYTES} bytes streamed each way concurrently"

# Hung build: the remote command never reads its input. After the client
# leaves, the proxy must still end the session and remove the builder pod.
head -c "$STREAM_BYTES" /dev/zero |
  with_timeout 20 ssh -F "${WORK}/ssh_config" nrbc-proxy "sleep 600" >/dev/null 2>&1 || true
for i in $(seq 1 60); do
  [ "$(count pods -l app=nix-builder)" = "0" ] && [ "$(count nixbuildrequests)" = "0" ] && break
  [ "$i" = 60 ] && fail "session with a hung build was not cleaned up after the client left"
  sleep 3
done
info "PASS: hung build cleaned up after the client left"

########################################################################
log "6/8 Building the DAG while watching builder pods"
POD_EVENTS="${WORK}/pods.jsonl"
: > "$POD_EVENTS"
(
  while true; do
    kubectl -n "$NS" get pods -l app=nix-builder -o json 2>/dev/null |
      jq -c '{pods: [.items[].metadata.labels["nix.io/session-id"]]}' >> "$POD_EVENTS" 2>/dev/null || true
    sleep 0.5
  done
) &
WATCH_PID=$!

BUILD_LOG="${WORK}/build.log"
if ! TOP_OUT="$(remote_build top "$BUILD_LOG" --argstr sleepSeconds "$SLEEP_SECONDS" --print-out-paths)"; then
  cat "$BUILD_LOG"
  fail "nix build of top failed"
fi
kill "$WATCH_PID" 2>/dev/null || true
WATCH_PID=""

########################################################################
log "7/8 Verifying concurrency, ordering, and result transfer"

# Result transfer: the physical path lives inside the chroot store.
combined="$(cat "${STORE}${TOP_OUT}/combined" 2>/dev/null || true)"
case "$combined" in
  *"dep-a:${MARKER}"*"dep-b:${MARKER}"* | *"dep-b:${MARKER}"*"dep-a:${MARKER}"*) ;;
  *) fail "top result did not contain both dependencies" ;;
esac
info "PASS: result transferred back with both dependencies"

# Concurrency: dep-a and dep-b execution intervals overlap.
a_start="$(mark_ts dep-a START "$BUILD_LOG")"; a_end="$(mark_ts dep-a END "$BUILD_LOG")"
b_start="$(mark_ts dep-b START "$BUILD_LOG")"; b_end="$(mark_ts dep-b END "$BUILD_LOG")"
top_start="$(mark_ts top START "$BUILD_LOG")"
for ts in "$a_start" "$a_end" "$b_start" "$b_end" "$top_start"; do
  [ -n "$ts" ] || fail "could not extract build timestamps from $BUILD_LOG"
done
as=$(epoch "$a_start"); ae=$(epoch "$a_end"); bs=$(epoch "$b_start"); be=$(epoch "$b_end")
overlap=$(( (ae < be ? ae : be) - (as > bs ? as : bs) ))
info "dep-a: $a_start..$a_end  dep-b: $b_start..$b_end"
[ "$overlap" -gt 0 ] || fail "dep-a and dep-b did not overlap (serialized)"
info "PASS: dep-a and dep-b overlapped by ${overlap}s"

# Ordering: top starts only after both dependencies end.
ts=$(epoch "$top_start")
{ [ "$ts" -ge "$ae" ] && [ "$ts" -ge "$be" ]; } || fail "top started before its dependencies finished"
info "PASS: top started at $top_start, after both dependencies"

# Concurrency from the cluster side: >=2 pods at once, >=3 distinct sessions.
max_pods=$(jq -s '[.[].pods | length] | max // 0' "$POD_EVENTS")
sessions=$(jq -rs '[.[].pods[]] | unique | length' "$POD_EVENTS")
[ "${max_pods:-0}" -ge 2 ] || fail "expected >=2 concurrent builder pods, saw ${max_pods:-0}"
[ "${sessions:-0}" -ge 3 ] || fail "expected >=3 distinct sessions, saw ${sessions:-0}"
info "PASS: peak ${max_pods} builder pods across ${sessions} sessions"

# Prometheus: both programs report the builds that just ran.
PROXY_POD="$(pod_of proxy)"
CONTROLLER_POD="${LEADER:-$(pod_of controller)}"
want_metric "$PROXY_POD" 'nix_remote_build_proxy_sessions_total\{outcome="success"\}'
want_metric "$PROXY_POD" 'nix_remote_build_proxy_tunnel_transferred_bytes_total\{direction="builder_to_client"\}'
want_metric "$CONTROLLER_POD" 'nix_remote_build_controller_reconcile_total'
want_metric "$CONTROLLER_POD" 'nix_remote_build_controller_builder_pods_created_total'
want_metric "$CONTROLLER_POD" 'nix_remote_build_controller_leader'

# OpenTelemetry: one build shows up as one trace across all the processes.
if [ "$OTEL" = "1" ]; then
  trace="$(find_trace 'nix nix build' \
    'nix-remote-build-proxy session' \
    'nix-remote-build-proxy tunnel' \
    'nix-remote-build-controller reconcile' \
    'nix-store nix-store')" || {
    collector_spans | awk '{$2 = ""; print}' | sort | uniq -c | sort -rn | head -40
    fail "no trace covers the client, proxy, controller and builder"
  }
  info "PASS: trace ${trace} covers the client, proxy, controller and builder"
  grep -E "^nix-remote-build-controller $trace " "${WORK}/spans" | grep -v ' reconcile$' >/dev/null ||
    fail "trace ${trace} has no Kubernetes API calls from the controller"
  info "PASS: the trace shows the controller's Kubernetes API calls"

  # ssh-ng:// builders run nix-daemon, which is told the trace in-band.
  nix build --store "$STORE" --file "$DAG" a \
    --arg nixpkgs "$NP" --argstr system "$BUILD_SYSTEM" --argstr marker "${MARKER}-ng" \
    --argstr sleepSeconds 1 --max-jobs 0 --no-link -L --builders-use-substitutes \
    --builders "ssh-ng://nixbld@nrbc-proxy ${BUILD_SYSTEM} - 100 1" 2> "${WORK}/ssh-ng.log" || {
    cat "${WORK}/ssh-ng.log"
    fail "build through an ssh-ng:// builder failed"
  }
  find_trace 'nix nix build' 'nix-remote-build-proxy session' 'nix-daemon daemon connection' >/dev/null ||
    fail "no trace covers the client, proxy and daemon of an ssh-ng:// build"
  info "PASS: ssh-ng:// builds are traced from the client to the builder's daemon"

  for metric in nix_remote_build_proxy.sessions nix_remote_build_controller.reconcile; do
    for i in $(seq 1 30); do
      collector_log
      grep -q "Name: ${metric}\$" "${WORK}/collector.log" && break
      [ "$i" = 30 ] && fail "the collector received no ${metric} metric"
      sleep 2
    done
  done
  info "PASS: the collector received OTLP metrics"

  grep -F 'Build session opened' "${WORK}/collector.log" >/dev/null ||
    fail "the collector received no OTLP logs"
  grep -F -A12 'Build session opened' "${WORK}/collector.log" |
    grep -E 'Trace ID: [0-9a-f]{32}' >/dev/null || fail "OTLP logs are not tied to their trace"
  info "PASS: the collector received OTLP logs tied to their traces"

  kubectl -n "$NS" logs deploy/proxy > "${WORK}/proxy.log"
  grep -F 'Build session opened' "${WORK}/proxy.log" | grep -F '"trace_id":' >/dev/null ||
    fail "stderr logs carry no trace IDs"
  info "PASS: stderr logs carry trace IDs"
fi

# Leader failover: delete the leader; the standby must take the lease and keep
# reconciling (the failing build below needs a working controller).
if [ "$HA" = "1" ]; then
  kubectl -n "$NS" delete pod "$LEADER" --wait=false >/dev/null
  for i in $(seq 1 90); do
    NEW_LEADER="$(leader_pod)"
    [ -n "$NEW_LEADER" ] && [ "$NEW_LEADER" != "$LEADER" ] && break
    [ "$i" = 90 ] && fail "standby controller did not take over the leader lease"
    sleep 1
  done
  info "PASS: leadership moved from ${LEADER} to ${NEW_LEADER}"
fi

# Failure behavior: a failing derivation fails the client build.
remote_build failing "${WORK}/fail.log" && fail "failing derivation unexpectedly succeeded"
info "PASS: failing build failed as expected"

########################################################################
log "8/8 Verifying cleanup"
kill "$PF_PID" 2>/dev/null || true
PF_PID=""
deadline=$(( $(date +%s) + 120 ))
while :; do
  pods=$(count pods -l app=nix-builder); reqs=$(count nixbuildrequests)
  [ "$pods" = "0" ] && [ "$reqs" = "0" ] && break
  [ "$(date +%s)" -gt "$deadline" ] && fail "not cleaned up (pods=$pods requests=$reqs)"
  sleep 3
done
info "PASS: all builder pods and NixBuildRequests cleaned up"

log "ALL END-TO-END CHECKS PASSED"
cat <<SUMMARY

Summary:
  dep-a and dep-b overlapped for ${overlap}s in separate pods
  peak simultaneous builder pods: ${max_pods}
  distinct builder sessions: ${sessions}
  top result transferred back with both dependencies
  failing derivation failed cleanly
  builder pods and NixBuildRequests cleaned up
SUMMARY
