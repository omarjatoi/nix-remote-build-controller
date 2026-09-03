# nix-remote-build-controller

A Kubernetes controller and SSH proxy that turn a cluster into an elastic pool of
Nix remote builders. Every remote build session gets its own short-lived builder
pod, so independent derivations build **concurrently** and the pool scales to zero
when idle.

```
        nix build --builders 'ssh://nixbld@<PROXY_IP> x86_64-linux - 100 1'
                                    │
                                    │  one SSH session per remote build slot
                                    ▼
┌─────────────┐   SSH    ┌───────────────────┐   creates   ┌──────────────────┐
│  Nix client │ ───────▶ │     SSH proxy     │ ──────────▶ │  NixBuildRequest │
│ (build hook)│          │    (Deployment)   │             │       (CRD)      │
└─────────────┘          └─────────┬─────────┘             └────────┬─────────┘
                                   │  tunnels the session           │ watches
                                   │  to the pod                    ▼
                                   │                        ┌──────────────────┐
                                   └──────────SSH──────────▶│   Builder pod    │
                                                            │    (dynamic)     │◀── Controller
                                                            └──────────────────┘    creates/reaps
```

## How concurrency works (and the four "jobs" knobs)

Nix already parallelises a build DAG: when several derivations are ready and
independent, it dispatches them at the same time. The number it dispatches to a
remote machine is that machine's advertised **`maxJobs`** — the 4th field of the
builder specification. This is the knob that makes remote builds parallel, and it
is easy to get wrong because four different settings all sound like "jobs":

| Setting | Where | What it controls |
|---|---|---|
| remote **`maxJobs`** | the `--builders` line, field 4 | How many builds the client sends to **this proxy at once**. Each one becomes its own SSH session → NixBuildRequest → builder pod. **This is what you raise to get parallel remote builds.** |
| local **`max-jobs`** | client `nix.conf` / `--max-jobs` | How many builds run **on the client machine** itself. `-j0` disables local builds entirely; it does **not** set remote capacity. |
| **`cores`** | client or builder `nix.conf` | How many CPU cores a **single** derivation's build may use (`make -j`, etc.). Parallelism *inside* one build, not across builds. |
| controller **`--max-concurrent-reconciles`** | controller flag | How many NixBuildRequests the controller reconciles in parallel. An internal control-plane worker count, unrelated to build parallelism. |

### The builder specification

```
ssh://nixbld@<PROXY_IP> x86_64-linux - 100 1
└──────┬───────────────┘ └────┬────┘ │ └┬┘ │
       │                      │       │  │  └ speed factor
       │                      │       │  └─── maxJobs: advertised concurrent capacity  ← parallelism
       │                      │       └────── SSH key path ("-" = use the default/agent)
       │                      └────────────── system(s) this builder can build
       └───────────────────────────────────── ssh://<remote-user>@<proxy-host>
```

- **`-j0` disables local builds but does not set remote capacity.** Local
  `max-jobs` and a remote builder's `maxJobs` are independent numbers. If you pass
  `-j0` and leave the builder's `maxJobs` at its default of `1`, the client sends
  exactly one build at a time and everything serialises — the classic
  "why is my remote build not parallel?" trap.
- **The `100` is the remote builder's advertised concurrent capacity.** With it,
  the client will open up to 100 simultaneous SSH sessions to the proxy, and the
  proxy creates up to 100 independent builder pods. Set it to the most concurrent
  builds you want the cluster to run.

For a DAG where `top` depends on uncached `A` and `B`, a `maxJobs` of at least 2
makes `A` and `B` build at the same time in two separate pods; `top` starts only
after both finish. With `maxJobs = 1` the same DAG builds `A`, then `B`, then
`top`, one pod at a time.

## Quick start

### 1. Prerequisites

- A Kubernetes cluster and `kubectl` context pointing at it.
- A Nix client (the machine you run `nix build` on).
- Images for `controller`, `proxy`, and `builder`. Build them with the flake
  (`nix build .#controller-image .#proxy-image .#builder-image`) and push to a
  registry your cluster can pull, or use the published
  `ghcr.io/omarjatoi/nix-remote-build-controller/*` images.
- **The user who runs `nix build` must be a Nix trusted user.** Nix silently
  ignores a client-specified `builders` setting for untrusted users, so remote
  builds would no-op. Check with `nix store info --json | jq .trusted`; if it is
  not `true`, add yourself once and restart the daemon:

  ```sh
  # Determinate Nix (macOS):
  echo 'extra-trusted-users = @admin' | sudo tee -a /etc/nix/nix.custom.conf
  sudo launchctl kickstart -k system/systems.determinate.nix-daemon

  # Upstream Nix: add "trusted-users = root <you>" to /etc/nix/nix.conf, then
  sudo systemctl restart nix-daemon                          # Linux
  sudo launchctl kickstart -k system/org.nixos.nix-daemon    # macOS
  ```

### 2. Generate SSH keys and create the secret

The system uses SSH throughout. Generate the keys:

```sh
ssh-keygen -t ed25519 -f nixbld       -N "" -C nixbld        # proxy → builder auth
ssh-keygen -t ed25519 -f builder-host -N "" -C builder-host  # builder host key (verified by proxy)
ssh-keygen -t ed25519 -f proxy-host   -N "" -C proxy-host    # proxy host key (stable across restarts)
ssh-keygen -t ed25519 -f client       -N "" -C nix-client    # a client allowed to use the proxy
```

Create one secret holding all of it:

```sh
kubectl create secret generic nix-builder-ssh-keys \
  --from-file=private=nixbld \
  --from-file=public=nixbld.pub \
  --from-file=builder-host-key=builder-host \
  --from-file=host-key=proxy-host \
  --from-file=client-authorized-keys=client.pub
```

Only `private` and `public` are strictly required. `builder-host-key`,
`host-key`, and `client-authorized-keys` are recommended; see
[Security](#security). If `client-authorized-keys` is omitted, **anyone who can
reach the proxy can start builder pods** and the proxy logs a warning at startup.

### 3. Deploy the controller and proxy

```sh
kubectl apply -k deploy
kubectl -n default rollout status deploy/controller deploy/proxy
```

### 4. Find the proxy address

The proxy is exposed as a `LoadBalancer` Service:

```sh
kubectl get svc proxy -w
PROXY_IP=$(kubectl get svc proxy -o jsonpath='{.status.loadBalancer.ingress[0].ip}')
```

On a cluster without a LoadBalancer (e.g. Kind), reach it with a port-forward
instead:

```sh
kubectl -n default port-forward svc/proxy 2222:22
# then use ssh://nixbld@127.0.0.1 with an SSH config that sets Port 2222
```

### 5. Configure your Nix client

Point your client at your client key and (if you verify the proxy host key) tell
SSH about it. The simplest robust setup is an SSH config alias:

```
# ~/.ssh/config
Host nrbc-proxy
  HostName <PROXY_IP>
  User nixbld
  IdentityFile ~/.ssh/client
  IdentitiesOnly yes
```

Then advertise the builder with **multiple remote slots**:

```sh
nix build nixpkgs#hello \
  --max-jobs 0 \
  --builders 'ssh://nixbld@nrbc-proxy x86_64-linux - 100 1'
```

Or persist it in `~/.config/nix/nix.conf`:

```ini
# 100 = remote concurrent capacity; without it, remote builds serialise.
builders = ssh://nixbld@nrbc-proxy x86_64-linux - 100 1
builders-use-substitutes = true
max-jobs = 0        # do not build locally; offload everything to the cluster
```

> Match the builder's `system` field to your cluster nodes' architecture
> (`x86_64-linux` or `aarch64-linux`). To offload from macOS, list the Linux
> system your nodes provide.

### 6. Watch it work

```sh
kubectl get nixbuildrequests -w   # nbr for short
kubectl get pods -l app=nix-builder -w
```

Build a DAG with independent dependencies and you will see two builder pods
appear at once.

## End-to-end test

`test/e2e/run.sh` is a self-contained, reproducible proof. It spins up a Kind
cluster, builds and loads the images, deploys everything, and runs a real
`nix build` of a synthetic uncached DAG (`top → {dep-a, dep-b}`) through the
proxy. It then asserts:

- **Concurrency:** `dep-a` and `dep-b` execution intervals overlap, and at least
  two builder pods exist simultaneously across distinct sessions.
- **Ordering:** `top` starts only after both dependencies finish.
- **Result transfer:** `top`'s output comes back and contains both dependencies.
- **Failure behavior:** an intentionally failing derivation fails the build cleanly.
- **Cleanup:** all builder pods and NixBuildRequests are removed afterward.

Run it from the dev shell (which provides `kind`, `kubectl`, `kustomize`, `jq`,
and the rest):

```sh
nix develop -c make e2e
# or, with tools already on PATH:
bash test/e2e/run.sh
```

Useful knobs: `KEEP=1` leaves the cluster up for inspection, `SLEEP_SECONDS=45`
lengthens the dependency builds, `PROXY_LOCAL_PORT=2223` changes the forwarded port.

## Validation commands

From the dev shell (`nix develop`) or with the equivalent tools on `PATH`:

```sh
make fmt-check      # gofmt is clean
make vet            # go vet ./...
make lint           # golangci-lint run ./...
make race           # go test -race -count=1 ./...
make build          # go build ./...
make validate       # kustomize build | kubeconform (base + e2e overlay)
make images         # nix build the three container images
make e2e            # full Kind end-to-end test
```

## Architecture

### Request lifecycle

1. The Nix client opens an SSH session to the proxy for each remote build slot it
   wants to use (up to the builder's `maxJobs`).
2. The proxy creates a `NixBuildRequest` named after the session and waits for it
   to report a ready builder pod.
3. The controller reconciles the request: it creates a builder pod
   (`Pending → Creating`), watches it come up, records the pod IP, and marks the
   request `Running`.
4. The proxy dials the pod over SSH and splices the client's session onto it,
   forwarding stdin/stdout/stderr, `exec`/`env` requests, and the final
   `exit-status` so Nix sees the true build result.
5. When the session ends, the proxy records the outcome on the request and deletes
   it. Deleting the request triggers the controller's finalizer, which removes the
   builder pod. A background TTL reaps anything the proxy could not delete (for
   example if it crashed).

### Components

- **SSH proxy (`cmd/proxy`, `pkg/proxy`)** — Authenticates clients by public key,
  turns every session channel into an independent build session, provisions and
  waits for a pod, and runs a cancellation-aware bidirectional tunnel with
  keepalives. Sessions never block on one another; a `--max-sessions` cap is
  available but off by default.
- **Controller (`cmd/controller`, `pkg/controller`)** — A controller-runtime
  reconciler that renders builder pods (SSH `authorized_keys` from the secret,
  optional shared host key, optional `nix.conf` ConfigMap, resource
  requests/limits, `activeDeadlineSeconds`), owns them via owner references, and
  reconciles many requests in parallel.
- **Builder image (`flake.nix`)** — A minimal Nix-built image running single-user
  Nix under `sshd` as the `nixbld` user. It prefers a mounted host key so the
  proxy can verify it.

### Custom resource: NixBuildRequest

```yaml
apiVersion: nix.io/v1alpha1
kind: NixBuildRequest
metadata:
  name: build-<session-id>
spec:
  sessionId: "<session-id>"
  resources:            # optional; falls back to the controller's defaults
    requests: { cpu: "1", memory: "2Gi" }
    limits:   { cpu: "4", memory: "8Gi" }
  timeoutSeconds: 3600  # optional; becomes the pod's activeDeadlineSeconds
  image: ""             # optional; overrides the controller's --builder-image
  nodeSelector: {}      # optional
status:
  phase: Running        # Pending → Creating → Running → Completed/Failed
  podName: nix-builder-<session-id>
  podIP: 10.0.0.42
```

## Configuration

### Proxy flags

| Flag | Default | Description |
|---|---|---|
| `--port` | `2222` | SSH listen port |
| `--health-port` | `8080` | Health/readiness port |
| `--host-key` | (secret/ephemeral) | Path to the proxy host key; otherwise the secret's `host-key`, else ephemeral |
| `--client-authorized-keys` | (secret) | authorized_keys of allowed clients; otherwise the secret's `client-authorized-keys`; if neither, auth is disabled |
| `--namespace` | `default` | Namespace for build requests |
| `--remote-user` | `nixbld` | SSH user on builder pods |
| `--remote-port` | `22` | SSH port on builder pods |
| `--ssh-key-secret` | `nix-builder-ssh-keys` | Secret with the SSH key material |
| `--pod-ready-timeout` | `5m` | How long a session waits for its builder pod |
| `--build-timeout` | `1h` | Builder pod lifetime (`spec.timeoutSeconds`) |
| `--shutdown-timeout` | `30m` | Drain window for in-flight sessions on SIGTERM |
| `--keepalive-interval` | `30s` | SSH keepalives to client and builder (`0` disables) |
| `--max-sessions` | `0` | Cap on concurrent sessions (`0` = unlimited) |
| `--log-level` | `info` | `trace`/`debug`/`info`/`warn`/`error` |

### Controller flags

| Flag | Default | Description |
|---|---|---|
| `--builder-image` | (required) | Default builder image |
| `--builder-image-pull-policy` | (k8s default) | `Always`/`IfNotPresent`/`Never` |
| `--builder-requests` / `--builder-limits` | (none) | Default pod resources, e.g. `cpu=1,memory=2Gi` |
| `--builder-service-account` | (namespace default) | Service account for builder pods |
| `--remote-port` | `22` | SSH port on builder pods |
| `--nix-config` | (none) | ConfigMap with `nix.conf`, mounted at `/etc/nix` |
| `--ssh-key-secret` | `nix-builder-ssh-keys` | Secret with the SSH key material |
| `--namespace` | `default` | Namespace of the secret and leader lease |
| `--completed-ttl` | `10m` | How long finished requests linger before the controller reaps them |
| `--max-concurrent-reconciles` | `10` | Reconcile worker count (control-plane concurrency) |
| `--metrics-addr` | `0` | Prometheus metrics address (`0` disables) |
| `--leader-elect` | `false` | Enable when running more than one replica |
| `--shutdown-timeout` | `30s` | Grace period for in-flight reconciles |
| `--log-level` | `info` | Log level |

### Builder Nix configuration

Edit `deploy/nix-config.yaml`. Because each pod builds one derivation for this
proxy, in-pod parallelism comes from `cores`, not `max-jobs`:

```yaml
data:
  nix.conf: |
    experimental-features = nix-command flakes
    sandbox = false
    trusted-users = root nixbld
    max-jobs = 1                 # one build per pod
    cores = 0                    # use all cores the pod is scheduled on
    substituters = https://cache.nixos.org
    builders-use-substitutes = true
```

## Security

- **Client authentication** is by SSH public key. Populate
  `client-authorized-keys` in the secret (or pass `--client-authorized-keys`).
  Without it the proxy accepts anyone who can reach it and warns loudly at startup.
- **Builder host keys** are verified when the secret contains `builder-host-key`
  (mounted into every pod). Without it the proxy cannot authenticate builders and
  logs a warning; pod-to-pod traffic then relies on network policy.
- **Proxy host key**: set `host-key` so clients see a stable key across restarts.
- The controller and proxy pods run as non-root with a read-only root filesystem
  and all capabilities dropped. Builder pods run `sshd`, which needs root inside
  the pod; constrain them with resource limits, network policy, and (optionally) a
  dedicated node pool via `nodeSelector`.

## Limitations

- Builder pods build one derivation at a time; per-pod throughput scales with
  `cores`, cluster throughput with the client's remote `maxJobs`.
- The proxy is stateful in the sense that it owns the requests it created; run one
  replica (the Deployment uses the `Recreate` strategy). The controller can be run
  with `--leader-elect` for HA.
- Builder host-key verification uses a single shared key for all pods. That is a
  deliberate trade-off for a fleet of interchangeable, ephemeral builders.

## License

Copyright © 2026 Omar Jatoi. Licensed under the Apache License, Version 2.0.
See [LICENSE](LICENSE).
