# Deployment

How to deploy the controller and proxy and connect a Nix client, and how to
secure and scale the deployment.

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
