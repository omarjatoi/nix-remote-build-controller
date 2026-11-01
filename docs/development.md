# Development

```sh
nix develop          # shell with all tools
make test            # unit and integration tests
make check           # cargo fmt, clippy and tests in the Nix sandbox
make validate        # check the Kubernetes manifests and the Helm chart
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
- all pods and requests are deleted at the end,
- the proxy and controller report the builds in their Prometheus metrics.

Options: `HA=1` uses three nodes and two controller replicas, and checks that a
standby controller takes over when the leader stops. `OTEL=1` adds a collector,
and checks that the client, proxy, controller and builder are in one trace, and
that logs and metrics arrive. `HELM=1` deploys with the Helm chart, in a namespace
of its own, and works with `HA=1` but not `OTEL=1`. `KEEP=1` keeps the cluster. `SLEEP_SECONDS` sets the build time of the test builds.
`PROXY_LOCAL_PORT` sets the local port for the proxy.
