# Helm

The chart in `charts/nix-remote-build-controller` installs the same controller
and proxy as `deploy/`, and is the easier way to change their settings. It works
with Helm 3 and 4. The chart is not in a registry, so install it from a checkout
of this repository.

Use the chart or `deploy/`, not both, and install only one release in a cluster.
The controller handles the build requests of all namespaces, so two controllers
would both act on each request.

## Install

Create the namespace and the SSH key secret (see [SSH keys](#ssh-keys)), then
install the chart:

```sh
helm install nix-remote-build-controller charts/nix-remote-build-controller \
  --namespace nix-builds
kubectl -n nix-builds rollout status \
  deploy/nix-remote-build-controller-controller \
  deploy/nix-remote-build-controller-proxy
```

Get the address of the proxy:

```sh
kubectl -n nix-builds get svc nix-remote-build-controller-proxy \
  -o jsonpath='{.status.loadBalancer.ingress[0]}'
```

Then [configure the client](deployment.md#4-configure-the-client). If the cluster
has no load balancer, set `proxy.service.type=ClusterIP` and use
`kubectl port-forward`. Do not use `helm install --wait` with a load balancer
service in such a cluster, because the service never gets an address.

To use other settings, put them in a file and add `--values my-values.yaml`:

```yaml
builder:
  resources:
    requests: { cpu: 2, memory: 4Gi }
    limits: { cpu: 8, memory: 16Gi }
proxy:
  buildTimeout: 2h
  service:
    loadBalancerSourceRanges:
      - 203.0.113.0/24
```

`values.yaml` lists all settings. The flags that they set are described in
[Configuration](configuration.md). For a flag or environment variable with no
setting, use `controller.extraArgs`, `proxy.extraArgs`, `controller.extraEnv` or
`proxy.extraEnv`.

## SSH keys

Both programs read the keys when they start, and do not start if the secret is
missing or does not have the `private` and `public` keys.

The recommended way is to create the secret yourself, so that the keys are not
stored in the Helm release. Make the keys as in
[Create the SSH keys](deployment.md#1-create-the-ssh-keys), then create the
secret in the namespace of the release:

```sh
kubectl create namespace nix-builds
kubectl -n nix-builds create secret generic nix-builder-ssh-keys \
  --from-file=private=nixbld \
  --from-file=public=nixbld.pub \
  --from-file=builder-host-key=builder-host \
  --from-file=host-key=proxy-host \
  --from-file=client-authorized-keys=client.pub
```

For another name, set `sshKeys.secretName`. After you change this secret, restart
the controller and the proxy with `kubectl rollout restart`.

The chart can also create the secret. Anyone who can read the release's values
can then read the private keys.

```sh
helm install nix-remote-build-controller charts/nix-remote-build-controller \
  --namespace nix-builds --create-namespace \
  --set sshKeys.create=true \
  --set-file sshKeys.private=nixbld \
  --set-file sshKeys.public=nixbld.pub \
  --set-file sshKeys.builderHostKey=builder-host \
  --set-file sshKeys.hostKey=proxy-host \
  --set-file sshKeys.clientAuthorizedKeys=client.pub
```

If a key in this secret changes, `helm upgrade` restarts the controller and the
proxy. See [Security](deployment.md#security) for what each key does.

## Images

The chart uses the `latest` tag of the published images, which is the only tag
that CI publishes, and pulls it each time a pod starts. To stay on one build, set
`builder.image.tag`, `controller.image.tag` and `proxy.image.tag` to a digest, for
example `latest@sha256:<DIGEST>`, or use your own registry with
`*.image.repository`.

For a private registry, set `imagePullSecrets` for the controller and proxy. For
builder pods, add the pull secret to a service account and set
`builder.serviceAccountName`.

## High availability

```yaml
controller:
  replicaCount: 2 # also turns on leader election
proxy:
  replicaCount: 2
  strategy:
    type: RollingUpdate
```

All proxy replicas must have the same host key. If the chart creates the secret,
it stops you from leaving out `sshKeys.hostKey`. If you create the secret, this is
for you to check. See [High availability](deployment.md#high-availability).

## Telemetry

Set `telemetry.otlpEndpoint` to the base URL of an OTLP/HTTP collector, for example
`http://otel-collector:4318`. The controller, the proxy and Nix on builder pods
then all send to it. The chart does not install a collector. Use `extraEnv` for
the other `OTEL_*` variables. See [Observability](observability.md).

## Timeouts

`proxy.shutdownTimeoutSeconds` and `controller.shutdownTimeoutSeconds` are numbers
of seconds, not durations such as `30m`. The chart sets each pod's termination
grace period 30 seconds longer, so that Kubernetes does not stop a program while
it is still finishing its work.

## Upgrade

```sh
helm upgrade nix-remote-build-controller charts/nix-remote-build-controller \
  --namespace nix-builds --values my-values.yaml
```

Helm installs the CRD but does not upgrade it. If `deploy/crd.yaml` changed, apply
it first:

```sh
kubectl apply -f deploy/crd.yaml
```

A proxy with the default `Recreate` strategy stops when you upgrade, and running
builds can continue for `proxy.shutdownTimeoutSeconds`. New builds fail until the
new pod is ready.

## Uninstall

Stop the builds first. Only the controller can finish deleting a build request, so
if you remove it while builds run, their requests stay in `Terminating` and their
builder pods keep running.

```sh
kubectl -n nix-builds scale deploy/nix-remote-build-controller-proxy --replicas=0
kubectl -n nix-builds get nixbuildrequests  # wait until there are none
helm uninstall nix-remote-build-controller --namespace nix-builds
```

If a request is stuck, remove its finalizer. Kubernetes then deletes the request
and its builder pod:

```sh
kubectl -n nix-builds patch nixbuildrequest <NAME> --type=merge \
  -p '{"metadata":{"finalizers":null}}'
```

Helm leaves the CRD and any secret that you created. To remove the CRD, run
`kubectl delete crd nixbuildrequests.nix.io`.
