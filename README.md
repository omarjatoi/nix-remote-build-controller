> [!WARNING]  
> This project is incomplete and not currently in working condition.

# nix-remote-build-controller

Kubernetes controller enabling dynamically scaling Nix remote builders with an SSH proxy.

## Quick Start

### Generating SSH Keys

The proxy and builder pods communicate over SSH using a shared keypair, which you can generate with:

```sh
ssh-keygen -t ed25519 -f nix-builder-key -N "" -C "nix-builder"
```

### Creating the SSH Keys Secret

Create a Kubernetes secret containing the keypair:

```sh
kubectl create secret generic nix-builder-ssh-keys \
  --from-file=private=nix-builder-key \
  --from-file=public=nix-builder-key.pub
```

### Deploying the Controller and Proxy

Deploy components to the cluster using Kustomize:

```sh
kubectl apply -k deploy
```

### Configuring Your Nix Client

Get the IP address of the proxy service:

```sh
kubectl get svc proxy -w
```

Add the remote builder to your Nix configuration in `~/.config/nix/nix.conf` or `/etc/nix/nix.conf`:

```ini
builders = ssh://nixbld@<PROXY_IP> x86_64-linux
```

Or use the `--builders` flag:

```sh
nix build --builders 'ssh://nixbld@<PROXY_IP> x86_64-linux'
```

### Testing a Remote Build

Try building something:

```sh
nix build nixpkgs#hello --builders 'ssh://nixbld@<PROXY_IP> x86_64-linux'
```

You can watch the builder pods being created:

```sh
kubectl get pods -w
kubectl get nixbuildrequests -w
```

## Architecture

```
┌─────────────────┐         ┌─────────────────┐         ┌─────────────────┐
│   Nix Client    │──SSH───▶│   SSH Proxy     │──SSH───▶│  Builder Pod    │
│                 │         │   (Deployment)  │         │  (Dynamic)      │
└─────────────────┘         └────────┬────────┘         └─────────────────┘
                                     │                           ▲
                                     │ creates                   │ creates
                                     ▼                           │
                            ┌─────────────────┐         ┌────────┴────────┐
                            │ NixBuildRequest │◀────────│   Controller    │
                            │   (CRD)         │ watches │   (Deployment)  │
                            └─────────────────┘         └─────────────────┘
```

### Components

#### SSH Proxy (`cmd/proxy`)

- Listens for incoming SSH connections from Nix clients
- Creates a `NixBuildRequest` CR for each session
- Waits for the controller to provision a builder pod
- Forwards the SSH session to the builder pod
- Updates the CR status when the build completes
- Cleans up the CR on session end

#### Controller (`cmd/controller`)

- Watches `NixBuildRequest` resources
- Creates builder pods with appropriate configuration
- Mounts the SSH public key as `authorized_keys`
- Mounts the Nix configuration ConfigMap
- Updates CR status with pod information
- Handles pod lifecycle and failure conditions

#### Builder Image

- Based on `nixos/nix` with SSH server enabled
- Runs `nix-daemon` for multi-user builds
- Accepts SSH connections from the proxy
- Configured via mounted ConfigMap for Nix settings

### Custom Resource: NixBuildRequest

```yaml
apiVersion: nix.io/v1alpha1
kind: NixBuildRequest
metadata:
  name: build-abc123
spec:
  sessionId: "abc123"
  resources:
    requests:
      cpu: "2"
      memory: "4Gi"
    limits:
      cpu: "4"
      memory: "8Gi"
  timeoutSeconds: 3600
  nodeSelector:
    kubernetes.io/arch: amd64
status:
  phase: Running
  podName: nix-builder-abc123
  podIP: 10.0.0.42
  startTime: "2025-01-15T10:30:00Z"
```

Phases: `Pending` → `Creating` → `Running` → `Completed`/`Failed`

## Configuration

### Proxy Flags

| Flag | Default | Description |
|------|---------|-------------|
| `--port` | `2222` | SSH listen port |
| `--health-port` | `8080` | Health check port |
| `--namespace` | `default` | Namespace for build requests |
| `--remote-user` | `nixbld` | SSH user on builder pods |
| `--remote-port` | `22` | SSH port on builder pods |
| `--ssh-key-secret` | (required) | Secret containing SSH keypair |

### Controller Flags

| Flag | Default | Description |
|------|---------|-------------|
| `--builder-image` | (required) | Container image for builder pods |
| `--remote-port` | `22` | SSH port on builder pods |
| `--nix-config` | (required) | ConfigMap name with nix.conf |
| `--ssh-key-secret` | (required) | Secret containing SSH keypair |
| `--health-port` | `8081` | Health check port |
| `--shutdown-timeout` | `30s` | Graceful shutdown timeout |

### Customizing Builder Resources

Edit `deploy/controller-deployment.yaml` to set default resource requests/limits, or configure them per-build through the CRD spec.

### Customizing Nix Configuration

Edit `deploy/nix-config.yaml` to modify the `nix.conf` mounted in builder pods:

```yaml
data:
  nix.conf: |
    sandbox = false
    trusted-users = root nixbld
    experimental-features = nix-command flakes
    max-jobs = auto
    cores = 0
```

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
