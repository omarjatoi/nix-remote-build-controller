# nix-remote-build-controller

Run Nix remote builds on Kubernetes. Each remote build gets its own short-lived
builder pod. Independent builds run at the same time, and no builder pods run
when there is no work.

```
  nix build --builders 'ssh://nixbld@<PROXY_IP> x86_64-linux - 100 1'
        │
        │  one SSH session per remote build
        ▼
  ┌─────────────┐   SSH   ┌───────────────┐  creates  ┌──────────────────┐
  │  Nix client │ ──────▶ │   SSH proxy   │ ────────▶ │ NixBuildRequest  │
  └─────────────┘         └───────┬───────┘           └────────┬─────────┘
                                  │                            │ watches
                                  │  forwards the session      ▼
                                  │                    ┌──────────────────┐
                                  └──────── SSH ──────▶│   Builder pod    │◀── Controller
                                                       └──────────────────┘   creates, deletes
```

## How it works

1. The Nix client opens one SSH session to the proxy for each remote build.
2. The proxy creates a `NixBuildRequest` for the session.
3. The controller creates a builder pod for the request and writes the pod's IP
   address to the request status.
4. The proxy connects to the pod over SSH. It retries until `sshd` in the pod
   accepts the connection.
5. The proxy forwards the session to the pod: input, output, errors and the
   exit status.
6. When the session ends, the proxy deletes the request. The controller then
   deletes the pod. If the proxy cannot delete the request, the controller
   deletes it after `--completed-ttl`.

The repository has two programs, built from one Rust crate (`src/`):

- `proxy` (`src/proxy/`): the SSH server that clients connect to.
- `controller` (`src/controller/`): creates and deletes builder pods.

The builder image (`flake.nix`) runs single-user Nix and `sshd` as the user
`nixbld`.

## Parallel builds

Nix sends at most `maxJobs` builds to a remote builder at the same time.
`maxJobs` is the fourth field of the builder line:

```
ssh://nixbld@<PROXY_IP>  x86_64-linux  -    100   1
ssh://user@host          system        key  jobs  speed
```

Each of these builds gets its own pod. Set `maxJobs` higher than 1 to build in
parallel. With `maxJobs` set to 100, the client sends up to 100 builds at once,
and the cluster runs up to 100 builder pods.

Do not confuse `maxJobs` with these settings:

- `--max-jobs` / `max-jobs` (client): builds that run on the client machine.
  `--max-jobs 0` stops local builds. It does not change remote capacity.
- `cores` (`nix.conf`): CPU cores that one build can use.
- `--max-concurrent-reconciles` (controller): how many requests the controller
  processes at once. It does not limit builds.

## Quick start

### Requirements

- A Kubernetes cluster and `kubectl`.
- Nix on the client machine.
- The `controller`, `proxy` and `builder` images. Use the published
  `ghcr.io/omarjatoi/nix-remote-build-controller/*` images, or build them with
  `make images` and push them to a registry that the cluster can use.
- The user who runs `nix build` must be a trusted Nix user. Nix ignores the
  `builders` setting of other users. To check, run
  `nix store info --json | jq .trusted`. If the result is not `true`, add the user
  to `trusted-users` in `/etc/nix/nix.conf` and restart the Nix daemon.

### 1. Create the SSH keys

```sh
ssh-keygen -t ed25519 -f nixbld       -N "" -C nixbld        # proxy to builder
ssh-keygen -t ed25519 -f builder-host -N "" -C builder-host  # builder host key
ssh-keygen -t ed25519 -f proxy-host   -N "" -C proxy-host    # proxy host key
ssh-keygen -t ed25519 -f client       -N "" -C nix-client    # a client key

kubectl create secret generic nix-builder-ssh-keys \
  --from-file=private=nixbld \
  --from-file=public=nixbld.pub \
  --from-file=builder-host-key=builder-host \
  --from-file=host-key=proxy-host \
  --from-file=client-authorized-keys=client.pub
```

Only `private` and `public` are necessary. Use the other keys too; see
[Security](#security).

### 2. Deploy

```sh
kubectl apply -k deploy
kubectl -n default rollout status deploy/controller deploy/proxy
```

### 3. Get the proxy address

The proxy has a `LoadBalancer` service:

```sh
kubectl get svc proxy -o jsonpath='{.status.loadBalancer.ingress[0].ip}'
```

If the cluster has no load balancer (for example Kind), use
`kubectl port-forward svc/proxy 2222:22` and connect to `127.0.0.1` port 2222.

### 4. Configure the client

Add the proxy to `~/.ssh/config`:

```
Host nrbc-proxy
  HostName <PROXY_IP>
  User nixbld
  IdentityFile ~/.ssh/client
  IdentitiesOnly yes
```

Then build:

```sh
nix build nixpkgs#hello --max-jobs 0 \
  --builders 'ssh://nixbld@nrbc-proxy x86_64-linux - 100 1'
```

Or put the settings in `~/.config/nix/nix.conf`:

```ini
builders = ssh://nixbld@nrbc-proxy x86_64-linux - 100 1
builders-use-substitutes = true
max-jobs = 0
```

Set the system (`x86_64-linux` or `aarch64-linux`) to match the cluster nodes.
To see the builds, run `kubectl get nixbuildrequests -w` and
`kubectl get pods -l app=nix-builder -w`.

## Configuration

### Proxy

| Flag | Default | Description |
|---|---|---|
| `--port` | `2222` | SSH port |
| `--health-port` | `8080` | Port for `/healthz` and `/readyz` |
| `--host-key` | from secret | File with the proxy host key. If not set, the secret's `host-key` is used. If neither exists, the proxy makes a new key at each start. |
| `--client-authorized-keys` | from secret | File with the client keys that can connect. If not set, the secret's `client-authorized-keys` is used. If neither exists, all clients can connect. |
| `--namespace` | `default` | Namespace for build requests and the secret |
| `--remote-user` | `nixbld` | SSH user in builder pods |
| `--remote-port` | `22` | SSH port in builder pods |
| `--ssh-key-secret` | `nix-builder-ssh-keys` | Secret with the SSH keys |
| `--pod-ready-timeout` | `5m` | Time to wait for a builder pod before the build fails |
| `--build-timeout` | `1h` | Maximum life of a builder pod |
| `--shutdown-timeout` | `30m` | After SIGTERM, time that running builds can continue |
| `--keepalive-interval` | `30s` | Keepalive interval for client and builder connections (`0` turns keepalives off) |
| `--max-sessions` | `0` | Maximum number of builds at the same time (`0` means no limit) |
| `--log-level` | `info` | `trace`, `debug`, `info`, `warn`, `error` or `off` |

### Controller

| Flag | Default | Description |
|---|---|---|
| `--builder-image` | (required) | Builder image |
| `--builder-image-pull-policy` | Kubernetes default | `Always`, `IfNotPresent` or `Never` |
| `--builder-requests`, `--builder-limits` | none | Pod resources, for example `cpu=1,memory=2Gi` |
| `--builder-service-account` | namespace default | Service account for builder pods |
| `--remote-port` | `22` | SSH port in builder pods |
| `--nix-config` | none | ConfigMap with `nix.conf`, mounted at `/etc/nix` |
| `--ssh-key-secret` | `nix-builder-ssh-keys` | Secret with the SSH keys |
| `--namespace` | `default` | Namespace of the secret and the leader lease |
| `--completed-ttl` | `10m` | Time before the controller deletes a finished request |
| `--max-concurrent-reconciles` | `10` | Requests processed at the same time |
| `--health-port` | `8081` | Port for `/healthz` and `/readyz` |
| `--metrics-addr` | `0` | Address for Prometheus metrics (`0` turns metrics off) |
| `--leader-elect` | `false` | Use leader election. Necessary with more than one replica. |
| `--shutdown-timeout` | `30s` | After SIGTERM, time to finish the current work |
| `--log-level` | `info` | `trace`, `debug`, `info`, `warn`, `error` or `off` |

Both programs write JSON logs to stderr. If `RUST_LOG` is set, it replaces
`--log-level`.

### Builder Nix settings

`deploy/nix-config.yaml` holds the `nix.conf` for builder pods. Each pod builds
one derivation, so use `cores` (not `max-jobs`) to make one build faster.

### NixBuildRequest

The proxy creates these objects. You do not usually write them yourself.

```yaml
apiVersion: nix.io/v1alpha1
kind: NixBuildRequest
metadata:
  name: build-<session-id>
spec:
  sessionId: "<session-id>"
  resources: {}         # optional; the controller defaults apply if empty
  timeoutSeconds: 3600  # optional; maximum pod life (0 means no limit)
  image: ""             # optional; replaces --builder-image
  nodeSelector: {}      # optional
status:
  phase: Running        # Pending, Creating, Running, Completed or Failed
  podName: nix-builder-<session-id>
  podIP: 10.0.0.42
```

## Security

- Set `client-authorized-keys`. If you do not, every client that can reach the
  proxy can start builder pods and run commands in them. The proxy logs a
  warning.
- Set `builder-host-key`. The proxy then checks the identity of each builder pod.
  If you do not, the proxy accepts any builder, and only network policy protects
  the connection.
- Set `host-key`, so that clients see the same proxy host key after a restart.
- The controller and proxy run as non-root users with a read-only file system.
  Builder pods run `sshd`, which needs root in the pod. Limit builder pods with
  resource limits, network policy and, if necessary, a separate node pool.

## High availability

- Proxy: you can run more than one replica. Give all replicas the same
  `host-key`, and set the deployment to `replicas: N` with a `RollingUpdate`
  strategy. If a proxy pod stops, only the builds on that pod fail.
- Controller: run more than one replica with `--leader-elect`. One replica does
  the work. If it stops, another replica takes over in about 15 seconds.

## Development

```sh
nix develop          # shell with all tools
make test            # unit and integration tests
make check           # cargo fmt, clippy and tests in the Nix sandbox
make validate        # check the Kubernetes manifests
make images          # build the container images for the host's Linux system
make e2e             # end-to-end test on a Kind cluster
```

The end-to-end test (`test/e2e/run.sh`) makes a Kind cluster, deploys the
images and runs real `nix build` commands through the proxy. It checks that:

- two independent builds run at the same time in different pods,
- a build starts only after its dependencies finish,
- the result gets back to the client,
- a large data stream works in both directions at the same time,
- a build that hangs is cleaned up after the client leaves,
- a failed build fails on the client,
- all pods and requests are deleted at the end.

Options: `HA=1` uses three nodes and two controller replicas, and checks that a
standby controller takes over when the leader stops. `KEEP=1` keeps the
cluster. `SLEEP_SECONDS` sets the build time of the test builds.
`PROXY_LOCAL_PORT` sets the local port for the proxy.

## Limitations

- A builder pod builds one derivation at a time.
- All builder pods share one host key.

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
