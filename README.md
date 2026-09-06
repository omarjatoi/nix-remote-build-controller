# nix-remote-build-controller

A Kubernetes controller and SSH proxy that turn a cluster into an elastic pool of
Nix remote builders. Each remote build session gets its own short-lived builder
pod, so independent derivations build concurrently and the pool scales to zero
when idle.

```
  nix build --builders 'ssh://nixbld@<PROXY_IP> x86_64-linux - 100 1'
        │
        │  one SSH session per remote build slot
        ▼
  ┌─────────────┐   SSH   ┌───────────────┐  creates  ┌──────────────────┐
  │  Nix client │ ──────▶ │   SSH proxy   │ ────────▶ │ NixBuildRequest  │
  └─────────────┘         └───────┬───────┘           └────────┬─────────┘
                                  │                            │ watches
                                  │  tunnels the session       ▼
                                  │                    ┌──────────────────┐
                                  └──────── SSH ──────▶│   Builder pod    │◀── Controller
                                                       └──────────────────┘   creates / reaps
```

## Concurrency and the "jobs" settings

Nix parallelizes a build DAG on its own: when several derivations are ready and
independent, it dispatches them together. The number it sends to a remote machine
is that machine's advertised `maxJobs`, the fourth field of the builder
specification. This is the setting that makes remote builds parallel, and it is
easy to confuse with three others:

- Remote `maxJobs` (the `--builders` line, field 4): how many builds the client
  sends to this proxy at once. Each becomes its own SSH session, NixBuildRequest,
  and builder pod. Raise this to get parallel remote builds.
- Local `max-jobs` (client `nix.conf` or `--max-jobs`): how many builds run on the
  client machine. `-j0` disables local builds; it does not set remote capacity.
- `cores` (client or builder `nix.conf`): CPU cores a single build may use.
  Parallelism inside one build, not across builds.
- `--max-concurrent-reconciles` (controller flag): how many NixBuildRequests the
  controller reconciles at once. A control-plane worker count, unrelated to build
  parallelism.

The builder specification fields are:

```
ssh://nixbld@<PROXY_IP>  x86_64-linux  -   100  1
    ssh://user@host        system     key  jobs speed
```

`-j0` disables local builds but does not set remote capacity: local `max-jobs` and
a remote builder's `maxJobs` are independent. If you pass `-j0` and leave `maxJobs`
at its default of 1, the client sends one build at a time and everything
serializes. The `100` above is the remote builder's advertised concurrent
capacity: the client opens up to 100 simultaneous sessions and the proxy creates up
to 100 builder pods.

For a DAG where `top` depends on uncached `A` and `B`, a `maxJobs` of at least 2
builds `A` and `B` at the same time in two pods; `top` starts only after both
finish. With `maxJobs` of 1 the same DAG builds `A`, then `B`, then `top`.

## Quick start

### Prerequisites

- A Kubernetes cluster and a `kubectl` context.
- A Nix client.
- Images for `controller`, `proxy`, and `builder`. Build them with
  `nix build .#controller-image .#proxy-image .#builder-image` and push to a
  registry the cluster can reach, or use the published
  `ghcr.io/omarjatoi/nix-remote-build-controller/*` images.
- The user who runs `nix build` must be a Nix trusted user. Nix ignores a
  client-specified `builders` setting for untrusted users, so remote builds would
  no-op. Check with `nix store info --json | jq .trusted`. If it is not `true`, add
  the user to `trusted-users` in `/etc/nix/nix.conf` (or `nix.custom.conf` on
  Determinate Nix) and restart the daemon.

### Generate keys and create the secret

```sh
ssh-keygen -t ed25519 -f nixbld       -N "" -C nixbld        # proxy to builder
ssh-keygen -t ed25519 -f builder-host -N "" -C builder-host  # builder host key
ssh-keygen -t ed25519 -f proxy-host   -N "" -C proxy-host    # proxy host key
ssh-keygen -t ed25519 -f client       -N "" -C nix-client    # a permitted client

kubectl create secret generic nix-builder-ssh-keys \
  --from-file=private=nixbld \
  --from-file=public=nixbld.pub \
  --from-file=builder-host-key=builder-host \
  --from-file=host-key=proxy-host \
  --from-file=client-authorized-keys=client.pub
```

Only `private` and `public` are required. The rest are recommended; see
[Security](#security). If `client-authorized-keys` is omitted, any client that can
reach the proxy may start builder pods, and the proxy warns at startup.

### Deploy

```sh
kubectl apply -k deploy
kubectl -n default rollout status deploy/controller deploy/proxy
```

### Find the proxy address

The proxy is a `LoadBalancer` Service:

```sh
PROXY_IP=$(kubectl get svc proxy -o jsonpath='{.status.loadBalancer.ingress[0].ip}')
```

Without a LoadBalancer (for example on Kind), use a port-forward and address the
proxy as `127.0.0.1` on the forwarded port.

### Configure the client

An SSH config alias keeps the key and host settings in one place:

```
# ~/.ssh/config
Host nrbc-proxy
  HostName <PROXY_IP>
  User nixbld
  IdentityFile ~/.ssh/client
  IdentitiesOnly yes
```

Advertise the builder with multiple remote slots:

```sh
nix build nixpkgs#hello \
  --max-jobs 0 \
  --builders 'ssh://nixbld@nrbc-proxy x86_64-linux - 100 1'
```

Or persist it in `~/.config/nix/nix.conf`:

```ini
builders = ssh://nixbld@nrbc-proxy x86_64-linux - 100 1
builders-use-substitutes = true
max-jobs = 0
```

Match the builder's `system` field to the cluster nodes' architecture
(`x86_64-linux` or `aarch64-linux`). Watch progress with
`kubectl get nixbuildrequests -w` and `kubectl get pods -l app=nix-builder -w`.

## End-to-end test

`test/e2e/run.sh` is a self-contained proof. It creates a Kind cluster, builds and
loads the images, deploys the components, and runs a real `nix build` of an
uncached DAG (`top` depending on `dep-a` and `dep-b`) through the proxy. It asserts
that the two dependencies overlap in separate pods, that `top` starts only after
both finish, that the result transfers back, that a failing derivation fails
cleanly, and that all pods and requests are cleaned up.

```sh
nix develop -c make e2e   # or: bash test/e2e/run.sh
```

Environment overrides: `KEEP=1` leaves the cluster running, `SLEEP_SECONDS`
changes the dependency build time, `PROXY_LOCAL_PORT` changes the forwarded port.

## Validation

```sh
make fmt-check vet lint race build   # Go checks
make validate                        # kustomize build | kubeconform
make images                          # build the three container images
make e2e                             # full Kind end-to-end test
```

## Architecture

Each remote build slot is one SSH session to the proxy. For each session the proxy
creates a NixBuildRequest and waits for a pod IP. The controller reconciles the
request through `Pending`, `Creating`, and `Running`, creating a dedicated builder
pod and recording its IP as soon as the pod has one. The proxy then dials the pod,
retrying while the builder's `sshd` is still starting, and splices the client's
session onto it, forwarding stdin, stdout, stderr, the `exec` and `env` requests,
and the final `exit-status`. When the session ends the proxy records the outcome
and deletes the request, whose finalizer removes the pod; a TTL reaper handles
anything the proxy could not delete.

Nothing waits on the builder pod's readiness probe: the SSH dial is itself the
readiness check, so a session starts as soon as the builder answers rather than
after a probe period plus a kubelet status round-trip. The probe is still set on
the pod, but only so `kubectl get pods` reports something meaningful.

Components:

- SSH proxy (`cmd/proxy`, `pkg/proxy`): authenticates clients by public key, turns
  every session channel into an independent build session, and runs a
  cancellation-aware bidirectional tunnel with keepalives.
- Controller (`cmd/controller`, `pkg/controller`): a controller-runtime reconciler
  that renders builder pods, owns them by owner reference, and reconciles many
  requests in parallel.
- Builder image (`flake.nix`): a minimal image running single-user Nix under
  `sshd` as the `nixbld` user, preferring a mounted host key. Its Nix database is
  populated at image build time, so the paths baked into the image count as valid
  and are neither re-fetched nor rewritten at run time.

### Custom resource: NixBuildRequest

```yaml
apiVersion: nix.io/v1alpha1
kind: NixBuildRequest
metadata:
  name: build-<session-id>
spec:
  sessionId: "<session-id>"
  resources:            # optional; defaults come from the controller
    requests: { cpu: "1", memory: "2Gi" }
    limits:   { cpu: "4", memory: "8Gi" }
  timeoutSeconds: 3600  # optional; becomes the pod's activeDeadlineSeconds
  image: ""             # optional; overrides --builder-image
  nodeSelector: {}      # optional
status:
  phase: Running        # Pending, Creating, Running, Completed, Failed
  podName: nix-builder-<session-id>
  podIP: 10.0.0.42
```

## Configuration

### Proxy flags

| Flag | Default | Description |
|---|---|---|
| `--port` | `2222` | SSH listen port |
| `--health-port` | `8080` | Health and readiness port |
| `--host-key` | secret or ephemeral | Proxy host key; otherwise the secret's `host-key`, else ephemeral |
| `--client-authorized-keys` | secret | Allowed client keys; otherwise the secret's `client-authorized-keys`; if neither, auth is disabled |
| `--namespace` | `default` | Namespace for build requests |
| `--remote-user` | `nixbld` | SSH user on builder pods |
| `--remote-port` | `22` | SSH port on builder pods |
| `--ssh-key-secret` | `nix-builder-ssh-keys` | Secret with the SSH key material |
| `--pod-ready-timeout` | `5m` | Wait for a builder pod before failing the session |
| `--build-timeout` | `1h` | Builder pod lifetime (`spec.timeoutSeconds`) |
| `--shutdown-timeout` | `30m` | Drain window for in-flight sessions on SIGTERM |
| `--keepalive-interval` | `30s` | SSH keepalives (`0` disables) |
| `--max-sessions` | `0` | Concurrent session cap (`0` is unlimited) |
| `--log-level` | `info` | Log level |

### Controller flags

| Flag | Default | Description |
|---|---|---|
| `--builder-image` | required | Default builder image |
| `--builder-image-pull-policy` | k8s default | `Always`, `IfNotPresent`, or `Never` |
| `--builder-requests` / `--builder-limits` | none | Default pod resources, e.g. `cpu=1,memory=2Gi` |
| `--builder-service-account` | namespace default | Service account for builder pods |
| `--remote-port` | `22` | SSH port on builder pods |
| `--nix-config` | none | ConfigMap with `nix.conf`, mounted at `/etc/nix` |
| `--ssh-key-secret` | `nix-builder-ssh-keys` | Secret with the SSH key material |
| `--namespace` | `default` | Namespace of the secret and leader lease |
| `--completed-ttl` | `10m` | How long finished requests linger before deletion |
| `--max-concurrent-reconciles` | `10` | Reconcile worker count |
| `--metrics-addr` | `0` | Prometheus metrics address (`0` disables) |
| `--leader-elect` | `false` | Enable when running more than one replica |
| `--shutdown-timeout` | `30s` | Grace period for in-flight reconciles |
| `--log-level` | `info` | Log level |

### Builder Nix configuration

Each pod builds one derivation, so in-pod parallelism comes from `cores`, not
`max-jobs`. Edit `deploy/nix-config.yaml`:

```yaml
data:
  nix.conf: |
    experimental-features = nix-command flakes
    sandbox = false
    trusted-users = root nixbld
    max-jobs = 1
    cores = 0
    substituters = https://cache.nixos.org
    builders-use-substitutes = true
```

## Security

- Client authentication uses SSH public keys from `client-authorized-keys`. Without
  it the proxy accepts any client that can reach it and warns at startup.
- Builder host keys are verified when the secret contains `builder-host-key`.
  Without it the proxy cannot authenticate builders and relies on network policy.
- Set `host-key` so clients see a stable proxy host key across restarts.
- The controller and proxy run as non-root with a read-only root filesystem and no
  capabilities. Builder pods run `sshd`, which needs root inside the pod; constrain
  them with resource limits, network policy, and a dedicated node pool if needed.

## High availability

The two components scale differently:

- The proxy is stateless across connections: each SSH connection is handled by one
  replica, and each session creates its own uniquely named NixBuildRequest, so
  there is no shared state to coordinate. It can run active-active with several
  replicas behind the Service. Two requirements: set a shared `host-key` in the
  secret so every replica presents the same host key to clients, and switch the
  Deployment to `replicas: N` with a `RollingUpdate` strategy. The default manifest
  ships a single replica with the `Recreate` strategy for simplicity. An
  individual in-flight session is still tied to the replica handling it; if that
  pod dies, that one build fails and the client retries.
- The controller is active-passive. Run several replicas with `--leader-elect` so
  exactly one reconciles at a time; the others stand by and take over on failure.

## Limitations

- Builder pods build one derivation at a time. Per-pod throughput scales with
  `cores`, cluster throughput with the client's remote `maxJobs`.
- Builder host-key verification uses a single shared key for all pods, a deliberate
  trade-off for interchangeable ephemeral builders.

## License

Copyright © 2026 Omar Jatoi

Licensed under the Apache License, Version 2.0 (the "License");
you may not use this file except in compliance with the License.
You may obtain a copy of the License at

    http://www.apache.org/licenses/LICENSE-2.0

Unless required by applicable law or agreed to in writing, software
distributed under the License is distributed on an "AS IS" BASIS,
WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
See the License for the specific language governing permissions and
limitations under the License.
