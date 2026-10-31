# Configuration

## Proxy

| Flag | Default | Description |
|---|---|---|
| `--port` | `2222` | SSH port |
| `--health-port` | `8080` | Port for `/healthz` and `/readyz` |
| `--metrics-addr` | `0` | Address for Prometheus metrics (`0` turns metrics off) |
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

## Controller

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
`--log-level`. See [Observability](observability.md) for metrics and traces.

## Builder Nix settings

`deploy/nix-config.yaml` holds the `nix.conf` for builder pods. With the Helm
chart, use `builder.nixConf`. Each pod builds one derivation, so use `cores` (not
`max-jobs`) to make one build faster.

## NixBuildRequest

The proxy creates these objects. You do not usually write them yourself.

```yaml
apiVersion: nix.io/v1alpha1
kind: NixBuildRequest
metadata:
  name: build-<session-id>
spec:
  sessionId: "<session-id>"  # lowercase letters, digits and "-"; at most 51 characters
  resources: {}         # optional; the controller defaults apply if empty
  timeoutSeconds: 3600  # optional; maximum pod life (0 means no limit)
  image: ""             # optional; replaces --builder-image
  nodeSelector: {}      # optional
status:
  phase: Running        # Pending, Creating, Running, Completed or Failed
  podName: nix-builder-<session-id>
  podIP: 10.0.0.42
```
