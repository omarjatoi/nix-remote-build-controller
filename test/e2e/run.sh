#!/usr/bin/env bash
# End-to-end test for nix-remote-build-controller.
#
# Creates a Kind cluster, deploys the controller and proxy, and runs a real
# `nix build` of a synthetic uncached DAG (top -> {dep-a, dep-b}) through the
# proxy. Asserts that the two dependencies build concurrently in separate pods,
# that top waits for them, that the result transfers back, that a failing
# derivation fails cleanly, and that all pods and requests are cleaned up.
#
# Requires on PATH: kind, kubectl, docker, nix, jq, ssh, ssh-keygen.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
CLUSTER="nix-remote-build-e2e"
NS="default"
KEEP="${KEEP:-0}"
SLEEP_SECONDS="${SLEEP_SECONDS:-30}"
PROXY_PORT="${PROXY_LOCAL_PORT:-2222}"
MARKER="e2e-$(date +%s)-$$"
DAG="${REPO_ROOT}/test/e2e/dag.nix"
WORK="$(mktemp -d "${TMPDIR:-/tmp}/nrbc-e2e.XXXXXX")"
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

# Nix ignores a client-specified `builders` setting for untrusted users. Rather
# than edit /etc/nix and restart the daemon, build into a private chroot store:
# the invoking user is always trusted for a store it created. The store must have
# no symlink ancestors, so `pwd -P` resolves macOS's /var and /tmp symlinks.
STORE="$(cd "$(mktemp -d "${TMPDIR:-/tmp}/nrbc-store.XXXXXX")" && pwd -P)"
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

########################################################################
log "1/8 Building and loading images"
NP="$(nix eval --raw --impure --expr "builtins.toString (builtins.getFlake (toString $REPO_ROOT)).inputs.nixpkgs")"
for img in controller proxy builder; do
  nix build "${REPO_ROOT}#packages.${BUILD_SYSTEM}.${img}-image" -o "${WORK}/${img}" >/dev/null
done

log "2/8 Creating Kind cluster '$CLUSTER'"
kind delete cluster --name "$CLUSTER" >/dev/null 2>&1 || true
kind create cluster --name "$CLUSTER" --config "${REPO_ROOT}/test/e2e/kind-config.yaml" >/dev/null
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
kubectl create secret generic nix-builder-ssh-keys -n "$NS" \
  --from-file=private="${WORK}/nixbld" \
  --from-file=public="${WORK}/nixbld.pub" \
  --from-file=builder-host-key="${WORK}/builder-host" \
  --from-file=client-authorized-keys="${WORK}/client.pub" \
  --dry-run=client -o yaml | kubectl apply -f - >/dev/null

log "4/8 Deploying controller and proxy"
kubectl apply -k "${REPO_ROOT}/test/e2e/manifests" >/dev/null
kubectl -n "$NS" rollout status deploy/controller --timeout=120s
kubectl -n "$NS" rollout status deploy/proxy --timeout=120s

log "5/8 Port-forwarding proxy to localhost:${PROXY_PORT}"
kubectl -n "$NS" port-forward svc/proxy "${PROXY_PORT}:22" >/dev/null 2>&1 &
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
SSH
export NIX_SSHOPTS="-F ${WORK}/ssh_config"

# Sanity: one plain SSH session reaches a builder pod and can run nix.
ssh -F "${WORK}/ssh_config" nrbc-proxy "nix-store --version" >/dev/null 2>&1 ||
  fail "could not run nix-store on a builder pod through the proxy"

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

# Failure behavior: a failing derivation fails the client build.
remote_build failing "${WORK}/fail.log" && fail "failing derivation unexpectedly succeeded"
info "PASS: failing build failed as expected"

########################################################################
log "8/8 Verifying cleanup"
kill "$PF_PID" 2>/dev/null || true
PF_PID=""
deadline=$(( $(date +%s) + 120 ))
count() { kubectl -n "$NS" get "$@" --no-headers 2>/dev/null | wc -l | tr -d ' '; }
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
