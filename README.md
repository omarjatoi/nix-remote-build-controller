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

The repository is a Cargo workspace with one crate per part:

- `crates/proxy`: the SSH server that clients connect to.
- `crates/controller`: creates and deletes builder pods.
- `crates/api`: the `NixBuildRequest` resource and the Kubernetes calls both
  programs make. Its `testing` feature adds an in-memory Kubernetes API for
  tests.
- `crates/common`: telemetry, HTTP servers, shutdown and flag parsing.

The builder image (`flake.nix`) runs single-user Determinate Nix and `sshd` as
the user `nixbld`.

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

## Documentation

- [Deployment](docs/deployment.md): quick start, security and high availability.
- [Helm](docs/helm.md): installing and configuring the Helm chart.
- [Configuration](docs/configuration.md): flags, builder Nix settings and the
  `NixBuildRequest` resource.
- [Observability](docs/observability.md): traces, logs and metrics with
  OpenTelemetry and Prometheus.
- [Development](docs/development.md): building, testing and the end-to-end test.

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
