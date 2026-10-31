# Observability

The proxy, the controller and Nix on the builder pods all support
[OpenTelemetry](https://opentelemetry.io/). `deploy/components/otel` adds a
collector and connects everything to it:

```yaml
# kustomization.yaml of your overlay
resources:
  - ../deploy
components:
  - ../deploy/components/otel
```

The collector only logs a summary of what it receives. Replace its `debug`
exporter with your backend.

## Traces

Each build session is a trace. The proxy's `session` span contains its steps:
`create_build_request`, `wait_for_builder_pod`, `dial_builder`, `tunnel` and
`finalize_build_request`. The controller's `reconcile` spans, with their
Kubernetes API calls, join the same trace through the `nix.io/traceparent`
annotation on the `NixBuildRequest`. Nix on the builder pod also joins it, and
adds spans for the build itself.

To put a whole `nix build` in one trace, use Determinate Nix 3.23 or later on the
client, export its traces, and pass the trace on over SSH:

```
Host nrbc-proxy
  SendEnv TRACEPARENT
```

```sh
OTEL_EXPORTER_OTLP_ENDPOINT=http://<COLLECTOR>:4318 nix build ...
```

If a Nix daemon runs your builds, this SSH configuration and the `otlp` settings
in `nix.custom.conf` belong to the daemon, not to your user. Without `SendEnv`,
an `ssh://` build is a separate trace that starts at the proxy. An `ssh-ng://`
build then still links the client and the builder pod's daemon, but not the proxy
or the controller.

## Logs

Log lines from inside a span have `trace_id` and `span_id` fields, so you can go
from a log line to its trace.

## Metrics

With `--metrics-addr`, each program serves Prometheus metrics at `/metrics`. Its
pods have `prometheus.io/*` annotations for scraping.

| Proxy metric | Description |
|---|---|
| `nix_remote_build_proxy_connections_total` | SSH connections accepted |
| `nix_remote_build_proxy_auth_rejections_total` | Client keys that were not authorized |
| `nix_remote_build_proxy_sessions_total{outcome}` | Ended sessions: `success`, `command_failed` or `error` |
| `nix_remote_build_proxy_sessions_rejected_total{reason}` | Refused sessions: `session_limit` or `shutting_down` |
| `nix_remote_build_proxy_active_sessions` | Sessions running now |
| `nix_remote_build_proxy_session_duration_seconds{outcome}` | Length of sessions |
| `nix_remote_build_proxy_builder_wait_duration_seconds` | Time until the builder pod has an address |
| `nix_remote_build_proxy_builder_dial_duration_seconds` | Time to connect to the builder pod |
| `nix_remote_build_proxy_tunnel_transferred_bytes_total{direction}` | Data forwarded |

| Controller metric | Description |
|---|---|
| `nix_remote_build_controller_reconcile_total{result}` | Reconciles, by result |
| `nix_remote_build_controller_reconcile_errors_total` | Reconciles that failed and will be retried |
| `nix_remote_build_controller_reconcile_duration_seconds` | Time taken by each reconcile |
| `nix_remote_build_controller_active_reconciles` | Reconciles running now |
| `nix_remote_build_controller_max_concurrent_reconciles` | Reconciles allowed at the same time |
| `nix_remote_build_controller_builder_pods_created_total` | Builder pods created |
| `nix_remote_build_controller_builder_pod_startup_duration_seconds` | Time until a builder pod has an IP address |
| `nix_remote_build_controller_build_requests_failed_total{reason}` | Requests marked as failed |
| `nix_remote_build_controller_leader` | `1` if this replica reconciles, `0` if it is a standby |

## Configuration

The proxy and controller send logs, traces and metrics over OTLP when
`OTEL_EXPORTER_OTLP_ENDPOINT` is set. They use HTTP, not gRPC, and the standard
`OTEL_*` variables:

| Variable | Description |
|---|---|
| `OTEL_EXPORTER_OTLP_ENDPOINT` | Base URL of the collector, for example `http://otel-collector:4318` |
| `OTEL_EXPORTER_OTLP_{TRACES,METRICS,LOGS}_ENDPOINT` | Full URL for one signal, which also turns on only that signal |
| `OTEL_EXPORTER_OTLP_PROTOCOL` | `http/protobuf` (default) or `http/json` |
| `OTEL_EXPORTER_OTLP_HEADERS` | Headers, for example for credentials |
| `OTEL_EXPORTER_OTLP_COMPRESSION` | `gzip` to compress |
| `OTEL_{TRACES,METRICS,LOGS}_EXPORTER` | `none` turns one signal off. Use `OTEL_LOGS_EXPORTER=none` if something already collects the pods' logs. |
| `OTEL_TRACES_SAMPLER`, `OTEL_TRACES_SAMPLER_ARG` | Sampling; the default keeps every trace |
| `OTEL_RESOURCE_ATTRIBUTES`, `OTEL_SERVICE_NAME` | How the program is described |
| `OTEL_METRIC_EXPORT_INTERVAL` | Milliseconds between metric exports |
| `OTEL_SDK_DISABLED` | `true` turns all OTLP export off |

A problem with OTLP export never stops the proxy or the controller: if a signal
cannot be exported, they log an error and run without it. The exporter needs a CA
bundle, even for `http://` endpoints. The images have one; elsewhere, set
`SSL_CERT_FILE` if the system has none.

Builder pods do not see these variables, because `sshd` does not pass them on.
Nix there reads its settings from `otlp.conf` next to `nix.conf`, which the
component adds to `deploy/nix-config.yaml`:

```ini
otlp = true
otlp-endpoint = http://otel-collector:4318
```

Traces contain store paths, and failed builds can include the end of the build
log. Send them only to a collector that you trust.
