# Observability

## Prometheus metrics

Scrape `GET /metrics` (no auth required). Key metrics:

| Metric                                       | Type      | Labels             | Meaning                                              |
|----------------------------------------------|-----------|--------------------|------------------------------------------------------|
| `federation_requests_total`                  | counter   | `method`           | Every dispatched JSON-RPC request.                   |
| `federation_tool_calls_total`                | counter   | `tool`, `alias`    | Tool call invocations.                               |
| `federation_tool_call_errors_total`          | counter   | `tool`, `alias`    | Tool call failures (leaf error, RBAC, unhealthy).    |
| `federation_tool_call_duration_seconds`      | histogram | `tool`, `alias`    | End-to-end tool call latency.                        |
| `federation_leaf_health`                     | gauge     | `alias`            | `1.0` healthy, `0.0` unhealthy.                      |
| `federation_leaf_tool_count`                 | gauge     | `alias`            | Cached tool count per leaf.                          |

## Structured logging

Every request opens a `tracing` span with `trace_id`, `method`, `client`, and
(for tool calls) `tool` + `alias`. Audit events use structured fields:

```json
{
  "event": "tool_call",
  "client": "client-2",
  "alias": "prod-k8s",
  "tool": "prod-k8s__list_pods",
  "status": "success",
  "duration_ms": 42
}
```

Pass `--log-format json` (or `LOG_FORMAT=json`) to emit JSON; ideal for
Loki, CloudWatch, or Datadog ingestion. Set `RUST_LOG=debug` to raise the
verbosity.

Audit events emitted: `session_init`, `logging_set_level`, `tools_list`,
`tool_call` (both success and RBAC-denied).

## OpenAPI / Swagger

`GET /swagger-ui` renders an interactive OpenAPI 3 explorer;
`GET /openapi.json` returns the raw spec. Currently documents the MCP
JSON-RPC endpoint, session lifecycle, and `/healthz`.

## Status page

`GET /status` returns a plain HTML dashboard listing every leaf with its
URL, health, cached tool count, tags, and time since last health check. No
JavaScript, no auth -- safe to expose over a firewall for humans.

## Health check

`GET /healthz` returns `200 ok`. Cheap; use for liveness/readiness probes.
