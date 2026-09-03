#!/usr/bin/env bash
# End-to-end test for nix-remote-build-controller.
#
# It stands up a Kind cluster, deploys the controller and proxy, and drives a
# real `nix build` of a synthetic uncached DAG (top -> {a, b}) through the proxy.
# It then proves:
#   * a and b build concurrently in two separate builder pods (overlapping
#     execution intervals + >=2 builder pods alive at once),
#   * top starts only after a and b finish (dependency ordering),
#   * the build result transfers back and is correct,
#   * a failing derivation fails cleanly, and
#   * builder pods and NixBuildRequests are cleaned up afterwards.
#
# Requirements on PATH: kind, kubectl, docker, nix, jq, ssh, ssh-keygen.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
CLUSTER="nix-remote-build-e2e"
NS="default"
WORK="$(mktemp -d "${TMPDIR:-/tmp}/nrbc-e2e.XXXXXX")"
KEEP="${KEEP:-0}"
SLEEP_SECONDS="${SLEEP_SECONDS:-30}"
PROXY_LOCAL_PORT="${PROXY_LOCAL_PORT:-2222}"
MARKER="e2e-$(date +%s)-$$"
PF_PID=""
WATCH_PID=""

log()  { printf '\n\033[1;34m==> %s\033[0m\n' "$*"; }
info() { printf '    %s\n' "$*"; }
fail() { printf '\n\033[1;31mFAIL: %s\033[0m\n' "$*" >&2; exit 1; }

cleanup() {
  local ec=$?
  [ -n "$PF_PID" ] && kill "$PF_PID" 2>/dev/null || true
  [ -n "$WATCH_PID" ] && kill "$WATCH_PID" 2>/dev/null || true
  if [ "$KEEP" = "1" ]; then
    info "KEEP=1 set; leaving cluster '$CLUSTER' and workdir $WORK"
  else
    log "Tearing down Kind cluster"
    kind delete cluster --name "$CLUSTER" >/dev/null 2>&1 || true
    rm -rf "$WORK"
    [ -n "${STORE:-}" ] && chmod -R u+w "$STORE" 2>/dev/null && rm -rf "$STORE" || true
  fi
  exit $ec
}
trap cleanup EXIT

need() { command -v "$1" >/dev/null 2>&1 || fail "missing required tool: $1"; }
for t in kind kubectl docker nix jq ssh ssh-keygen; do need "$t"; done

# Nix ignores a client-specified `builders` setting for UNTRUSTED users, so a
# remote build against the default daemon store would silently no-op. Rather than
# require the tester to edit /etc/nix and restart the daemon, the test builds into
# a private chroot Nix store: the invoking user always owns and is trusted for a
# store it created, so --builders is honored without any system change.
#
# STORE must have no symlink ancestors (a Nix store requirement). `pwd -P`
# resolves macOS's /var -> /private/var and /tmp -> /private/tmp symlinks.
STORE="$(cd "$(mktemp -d "${TMPDIR:-/tmp}/nrbc-store.XXXXXX")" && pwd -P)"
info "private chroot store: $STORE"
if [ "$(nix store info --store "$STORE" --json 2>/dev/null | jq -r '.trusted // false')" != "true" ]; then
  fail "not trusted even in a private store; cannot honor --builders (nix store info --store $STORE)"
fi

# Arch of the Kind node determines the builder system.
HOST_ARCH="$(uname -m)"
case "$HOST_ARCH" in
  arm64|aarch64) BUILD_SYSTEM="aarch64-linux" ;;
  x86_64|amd64)  BUILD_SYSTEM="x86_64-linux" ;;
  *) fail "unsupported host arch $HOST_ARCH" ;;
esac
info "workdir: $WORK"
info "builder system: $BUILD_SYSTEM  marker: $MARKER"

########################################################################
log "1/9 Building images with Nix"
NP="$(nix eval --raw --impure --expr "builtins.toString (builtins.getFlake (toString $REPO_ROOT)).inputs.nixpkgs")"
info "nixpkgs: $NP"
for img in controller proxy builder; do
  info "nix build .#packages.${BUILD_SYSTEM}.${img}-image"
  nix build "${REPO_ROOT}#packages.${BUILD_SYSTEM}.${img}-image" -o "${WORK}/${img}-image" >/dev/null
done

########################################################################
log "2/9 Creating Kind cluster '$CLUSTER'"
if kind get clusters 2>/dev/null | grep -qx "$CLUSTER"; then
  info "cluster already exists, deleting it first"
  kind delete cluster --name "$CLUSTER" >/dev/null
fi
kind create cluster --name "$CLUSTER" --config "${REPO_ROOT}/test/e2e/kind-config.yaml" >/dev/null
KUBECONFIG_FILE="${WORK}/kubeconfig"
kind get kubeconfig --name "$CLUSTER" > "$KUBECONFIG_FILE"
export KUBECONFIG="$KUBECONFIG_FILE"
kubectl wait --for=condition=Ready nodes --all --timeout=120s >/dev/null

log "3/9 Loading images into Kind"
for img in controller proxy builder; do
  tag="ghcr.io/omarjatoi/nix-remote-build-controller/${img}:e2e"
  info "docker load ${img}"
  loaded="$(docker load < "${WORK}/${img}-image" | sed -n 's/^Loaded image: //p' | head -1)"
  docker tag "$loaded" "$tag"
  kind load docker-image "$tag" --name "$CLUSTER" >/dev/null
done

########################################################################
log "4/9 Generating SSH key material and secret"
ssh-keygen -t ed25519 -f "${WORK}/nixbld"       -N "" -C nixbld       -q  # proxy -> builder
ssh-keygen -t ed25519 -f "${WORK}/client"       -N "" -C nix-client   -q  # client -> proxy
ssh-keygen -t ed25519 -f "${WORK}/builder-host" -N "" -C builder-host -q  # builder host key
kubectl create secret generic nix-builder-ssh-keys -n "$NS" \
  --from-file=private="${WORK}/nixbld" \
  --from-file=public="${WORK}/nixbld.pub" \
  --from-file=builder-host-key="${WORK}/builder-host" \
  --from-file=client-authorized-keys="${WORK}/client.pub" \
  --dry-run=client -o yaml | kubectl apply -f - >/dev/null

########################################################################
log "5/9 Deploying controller and proxy (e2e overlay)"
kubectl apply -k "${REPO_ROOT}/test/e2e/manifests" >/dev/null
kubectl -n "$NS" rollout status deploy/controller --timeout=120s
kubectl -n "$NS" rollout status deploy/proxy --timeout=120s

log "6/9 Port-forwarding proxy to localhost:${PROXY_LOCAL_PORT}"
( exec kubectl -n "$NS" port-forward svc/proxy "${PROXY_LOCAL_PORT}:22" >/dev/null 2>&1 ) &
PF_PID=$!
for i in $(seq 1 30); do
  if (exec 3<>"/dev/tcp/127.0.0.1/${PROXY_LOCAL_PORT}") 2>/dev/null; then exec 3>&- 3<&-; break; fi
  sleep 1
  [ "$i" = 30 ] && fail "port-forward to proxy never came up"
done

# SSH config so `ssh://nixbld@nrbc-proxy` reaches the forwarded port with our key.
cat > "${WORK}/ssh_config" <<SSH
Host nrbc-proxy
  HostName 127.0.0.1
  Port ${PROXY_LOCAL_PORT}
  User nixbld
  IdentityFile ${WORK}/client
  IdentitiesOnly yes
  StrictHostKeyChecking no
  UserKnownHostsFile /dev/null
  LogLevel ERROR
SSH
export NIX_SSHOPTS="-F ${WORK}/ssh_config"
BUILDER_SPEC="ssh://nixbld@nrbc-proxy ${BUILD_SYSTEM} - 100 1"
info "builders: ${BUILDER_SPEC}"

# Sanity: the client can open a plain SSH session (=> one builder pod) and run nix.
info "SSH connectivity check through the proxy"
if ! ssh -F "${WORK}/ssh_config" nrbc-proxy "nix-store --version" >"${WORK}/ssh_check.log" 2>&1; then
  cat "${WORK}/ssh_check.log" || true
  fail "could not run nix-store on a builder pod through the proxy"
fi
info "remote nix-store: $(cat "${WORK}/ssh_check.log")"

########################################################################
log "7/9 Watching builder pods while building the DAG"
POD_EVENTS="${WORK}/pod_events.jsonl"
: > "$POD_EVENTS"
(
  while true; do
    ts="$(date -u +%Y-%m-%dT%H:%M:%S.%3NZ 2>/dev/null || date -u +%Y-%m-%dT%H:%M:%SZ)"
    kubectl -n "$NS" get pods -l app=nix-builder \
      -o json 2>/dev/null | jq -c --arg ts "$ts" \
      '{ts:$ts, pods:[.items[]|{name:.metadata.name, phase:.status.phase, session:.metadata.labels["nix.io/session-id"], start:.status.startTime}]}' \
      >> "$POD_EVENTS" 2>/dev/null || true
    sleep 0.5
  done
) &
WATCH_PID=$!

BUILD_LOG="${WORK}/build.log"
DAG="${REPO_ROOT}/test/e2e/dag.nix"
set +e
nix build --store "$STORE" --file "$DAG" top \
  --arg nixpkgs "$NP" --argstr system "$BUILD_SYSTEM" \
  --argstr marker "$MARKER" --argstr sleepSeconds "$SLEEP_SECONDS" \
  --max-jobs 0 --builders "$BUILDER_SPEC" --builders-use-substitutes \
  --no-link --print-out-paths -L > "${WORK}/top_out.txt" 2> "$BUILD_LOG"
BUILD_RC=$?
set -e
kill "$WATCH_PID" 2>/dev/null || true; WATCH_PID=""

if [ "$BUILD_RC" -ne 0 ]; then
  echo "----- build log -----"; cat "$BUILD_LOG"; echo "---------------------"
  fail "nix build of top failed (rc=$BUILD_RC)"
fi
TOP_OUT="$(cat "${WORK}/top_out.txt")"
info "top output path (logical): $TOP_OUT"

########################################################################
log "8/9 Verifying concurrency, ordering, and result transfer"

# (a) Result transfer + correctness. The physical path lives inside the chroot
#     store, so the remote build's output really came back to this machine.
combined="$(cat "${STORE}${TOP_OUT}/combined" 2>/dev/null || true)"
info "top/combined: ${combined//$'\n'/ | }"
echo "$combined" | grep -q "dep-a:${MARKER}" || fail "top result missing dep-a"
echo "$combined" | grep -q "dep-b:${MARKER}" || fail "top result missing dep-b"
info "PASS: result transferred back and contains both dependencies"

# (b) Concurrency from the DAG build logs: a and b execution intervals overlap.
#     The @@ markers are emitted on the builder and streamed back via -L.
a_start=$(grep -aE '@@ dep-a START' "$BUILD_LOG" | head -1 | grep -oE '[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9:]+Z' | head -1)
a_end=$(grep -aE '@@ dep-a END'   "$BUILD_LOG" | head -1 | grep -oE '[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9:]+Z' | head -1)
b_start=$(grep -aE '@@ dep-b START' "$BUILD_LOG" | head -1 | grep -oE '[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9:]+Z' | head -1)
b_end=$(grep -aE '@@ dep-b END'   "$BUILD_LOG" | head -1 | grep -oE '[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9:]+Z' | head -1)
top_start=$(grep -aE '@@ top START' "$BUILD_LOG" | head -1 | grep -oE '[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9:]+Z' | head -1)
[ -n "$a_start" ] && [ -n "$a_end" ] && [ -n "$b_start" ] && [ -n "$b_end" ] || fail "could not extract dep-a/dep-b timestamps from build log"

epoch() { date -u -j -f "%Y-%m-%dT%H:%M:%SZ" "$1" +%s 2>/dev/null || date -u -d "$1" +%s; }
as=$(epoch "$a_start"); ae=$(epoch "$a_end"); bs=$(epoch "$b_start"); be=$(epoch "$b_end")
info "dep-a: $a_start .. $a_end"
info "dep-b: $b_start .. $b_end"
overlap_start=$(( as > bs ? as : bs ))
overlap_end=$(( ae < be ? ae : be ))
overlap=$(( overlap_end - overlap_start ))
info "overlap between dep-a and dep-b: ${overlap}s"
[ "$overlap" -gt 0 ] || fail "dep-a and dep-b did NOT overlap (serialized): overlap=${overlap}s"
info "PASS: dep-a and dep-b executed concurrently (${overlap}s overlap)"

# (c) Dependency ordering: top started only after both a and b ended.
if [ -n "$top_start" ]; then
  ts=$(epoch "$top_start")
  [ "$ts" -ge "$ae" ] && [ "$ts" -ge "$be" ] || fail "top started before its dependencies finished"
  info "PASS: top started at $top_start, after dep-a ($a_end) and dep-b ($b_end)"
fi

# (d) At least two builder pods existed simultaneously (from the pod watcher).
max_pods=$(jq -s 'map(.pods|length)|max' "$POD_EVENTS")
info "max simultaneous builder pods observed: ${max_pods:-0}"
[ "${max_pods:-0}" -ge 2 ] || fail "expected >=2 concurrent builder pods, saw ${max_pods:-0}"
distinct_sessions=$(jq -rs '[.[].pods[].session]|unique|length' "$POD_EVENTS")
info "distinct builder sessions observed: $distinct_sessions"
[ "${distinct_sessions:-0}" -ge 3 ] || fail "expected >=3 distinct builder sessions (a, b, top), saw ${distinct_sessions:-0}"
info "PASS: >=2 builder pods ran concurrently across $distinct_sessions distinct sessions"

# (e) Failure behavior: a failing derivation fails the client build cleanly.
log "Verifying failure behavior with an intentionally failing derivation"
set +e
nix build --store "$STORE" --file "$DAG" failing \
  --arg nixpkgs "$NP" --argstr system "$BUILD_SYSTEM" --argstr marker "$MARKER" \
  --max-jobs 0 --builders "$BUILDER_SPEC" --builders-use-substitutes \
  --no-link -L > "${WORK}/fail.log" 2>&1
FAIL_RC=$?
set -e
[ "$FAIL_RC" -ne 0 ] || fail "failing derivation unexpectedly succeeded"
info "PASS: failing build returned rc=$FAIL_RC as expected"

########################################################################
log "9/9 Verifying cleanup"
# Kill the port-forward so the SSH sanity session's request is finalized too.
[ -n "$PF_PID" ] && kill "$PF_PID" 2>/dev/null || true; PF_PID=""
deadline=$(( $(date +%s) + 120 ))
while :; do
  pods=$(kubectl -n "$NS" get pods -l app=nix-builder --no-headers 2>/dev/null | wc -l | tr -d ' ')
  nbrs=$(kubectl -n "$NS" get nixbuildrequests --no-headers 2>/dev/null | wc -l | tr -d ' ')
  [ "$pods" = "0" ] && [ "$nbrs" = "0" ] && break
  [ "$(date +%s)" -gt "$deadline" ] && { kubectl -n "$NS" get pods,nixbuildrequests; fail "builder pods/requests were not cleaned up (pods=$pods nbrs=$nbrs)"; }
  sleep 3
done
info "PASS: all builder pods and NixBuildRequests cleaned up"

log "ALL END-TO-END CHECKS PASSED"
echo
echo "Summary:"
echo "  * dep-a and dep-b overlapped for ${overlap}s in separate pods"
echo "  * peak simultaneous builder pods: ${max_pods}"
echo "  * distinct builder sessions: ${distinct_sessions}"
echo "  * top result transferred back with both dependencies"
echo "  * failing derivation failed cleanly (rc=${FAIL_RC})"
echo "  * builder pods and NixBuildRequests cleaned up"
