# mcp-federation

An aggregating [Model Context Protocol](https://modelcontextprotocol.io) (MCP)
server. `mcp-federation` sits in front of N downstream ("leaf") MCP servers
and exposes them to clients as a **single MCP endpoint** with a unified tool
catalog. Every leaf tool is namespaced with the leaf's alias (`alias__tool`),
health is monitored, tools are cached, and requests are routed to the
appropriate leaf.

## Contents

- [Overview](#overview)
- [Architecture](#architecture)
- [Quick start](#quick-start)
- [Configuration reference](#configuration-reference)
- [CLI reference](#cli-reference)
- [Transports](#transports)
- [Federation-native tools](#federation-native-tools)
- [Discovery](#discovery)
- [Security](#security)
- [Observability](#observability)
- [Deployment](#deployment)
- [Multi-level federation](#multi-level-federation)
- [Development](#development)

---

## Overview

You have several MCP servers — one per Kubernetes cluster, one per database,
one for your ticketing system, one on every CI runner. Clients want a **single
endpoint** rather than a config file with N entries that each rotate
independently.

`mcp-federation` is that single endpoint. It:

- **Aggregates** — fetches every leaf's tool list at startup and on refresh.
- **Namespaces** — rewrites each tool name as `alias__tool_name` so the same
  logical tool can exist on several leaves without collision.
- **Routes** — strips the alias off incoming `tools/call` requests and forwards
  the call to the right leaf.
- **Monitors** — health-checks each leaf, drops unhealthy ones from the
  advertised tool list, and applies exponential backoff to failing leaves.
- **Caches** — tool lists are cached with a TTL and refreshed in the background
  on demand.
- **Secures** — bearer-token auth, per-client RBAC, TLS, rate limiting,
  origin allowlist.
- **Observes** — Prometheus metrics, structured JSON logs with per-request
  trace IDs, an admin status page, and OpenAPI docs.

## Architecture

```
                                                    +----------------+
                                                    | mcp-k8s (prod) |
                                                    +----------------+
                              +------------------+ /
+----------+  MCP JSON-RPC   |                  |/     +-----------------+
| clients  |---------------->|  mcp-federation  |----->| mcp-k8s (stage) |
+----------+  (namespaced)   |                  |\     +-----------------+
                              +------------------+ \
                                                    +----------------+
                                                    | mcp-postgres   |
                                                    +----------------+
```

Every tool the leaves advertise is re-exported by the federation with a
`<alias>__` prefix. So if `prod-k8s` advertises `list_pods` and `staging-k8s`
also advertises `list_pods`, clients see both:

- `prod-k8s__list_pods`
- `staging-k8s__list_pods`

The `__` (double underscore) separator is reserved — leaf aliases cannot
contain it.

The federation itself advertises five built-in tools under the `federation`
namespace (`federation__list_servers`, `federation__topology`, etc.) that let
clients inspect and manage the federation. See
[Federation-native tools](#federation-native-tools).

## Quick start

### 1. Write a `federation.yaml`

```yaml
federation:
  listen: "0.0.0.0:8080"
  endpoint: "/mcp"

servers:
  - alias: "prod-k8s"
    url: "http://mcp-k8s-prod.internal:8080/mcp"
    transport: http-post
    tags: ["kubernetes", "production"]

  - alias: "staging-k8s"
    url: "http://mcp-k8s-staging.internal:8080/mcp"
    transport: http-post
    tags: ["kubernetes", "staging"]
```

### 2. Run it

**Stdio mode** (embeds directly into a Claude-style client):

```bash
cargo run -- --config federation.yaml
```

**HTTP mode** (single shared endpoint for many clients):

```bash
cargo run -- --http --config federation.yaml
```

**Docker**:

```bash
docker run --rm -p 8080:8080 \
  -v $(pwd)/federation.yaml:/etc/mcp-federation/federation.yaml \
  ghcr.io/alexconrey/mcp-federation:latest \
  --config /etc/mcp-federation/federation.yaml
```

### 3. Point Claude Code (or any MCP client) at it

For **HTTP mode**, add to your Claude Code `~/.claude.json`:

```json
{
  "mcpServers": {
    "federation": {
      "type": "http",
      "url": "http://localhost:8080/mcp"
    }
  }
}
```

For **stdio mode**:

```json
{
  "mcpServers": {
    "federation": {
      "command": "mcp-federation",
      "args": ["--config", "/etc/mcp-federation/federation.yaml"]
    }
  }
}
```

### 4. Verify

```bash
curl -s http://localhost:8080/healthz            # → "ok"
curl -s http://localhost:8080/status | less      # HTML status page
curl -s http://localhost:8080/metrics | head     # Prometheus metrics
open http://localhost:8080/swagger-ui            # OpenAPI docs

# Dry-run: dump the aggregated tool list and exit
cargo run -- --config federation.yaml --list-tools
```

## Configuration reference

The config file is YAML. Every string value supports `${ENV_VAR}` interpolation
against the process environment (unset vars expand to empty strings).

### `federation` — top-level settings

| Key                        | Type          | Default          | Description                                                                                               |
|----------------------------|---------------|------------------|-----------------------------------------------------------------------------------------------------------|
| `listen`                   | string        | `"0.0.0.0:8080"` | Bind address for HTTP/HTTPS mode.                                                                         |
| `endpoint`                 | string        | `"/mcp"`         | Path for the MCP JSON-RPC endpoint.                                                                       |
| `auth_token`               | string        | unset            | Bearer token clients must present. If unset, `/mcp` is unauthenticated.                                   |
| `auth_token_file`          | string (path) | unset            | Path to a file containing the bearer token. Re-read on every request so tokens can rotate without restart.|
| `session_ttl_seconds`      | u64           | `1800`           | Streamable HTTP session idle timeout.                                                                     |
| `session_secret`           | string (b64)  | unset            | Base64-encoded HMAC key used to sign session JWTs. If unset a random key is generated on startup, meaning sessions do not survive a restart. Set this to keep sessions valid across deploys. |
| `tool_cache_ttl_seconds`   | u64           | `300`            | How long an aggregated tool list is considered fresh before a background refresh is triggered.            |
| `shutdown_timeout_seconds` | u64           | `15`             | Grace period for in-flight connections at shutdown (HTTPS only today).                                    |
| `allowed_origins`          | list<string>  | `[]`             | DNS-rebinding protection: if non-empty, `/mcp` rejects requests whose `Origin` header is not in the list. |
| `connection_pool`          | object        | see below        | HTTP client connection pool tuning for outbound leaf calls.                                               |
| `clients`                  | list          | `[]`             | Per-client bearer tokens with RBAC. See [RBAC](#per-client-rbac).                                         |
| `rate_limit`               | object        | disabled         | Per-client token-bucket rate limiter. See [rate limiting](#rate-limiting).                                |
| `dns_discovery`            | object        | disabled         | DNS SRV-based leaf discovery. See [Discovery](#discovery).                                                |
| `crd_discovery`            | object        | disabled         | Kubernetes CRD-based leaf discovery. See [Discovery](#discovery).                                         |

#### `federation.connection_pool`

Applied to the reqwest client used to reach each leaf.

| Key                    | Type | Default | Description                                    |
|------------------------|------|---------|------------------------------------------------|
| `max_idle_per_host`    | usize| `10`    | Max idle connections kept per leaf host.       |
| `idle_timeout_seconds` | u64  | `90`    | How long an idle connection is retained.       |

#### `federation.rate_limit`

Classic token-bucket. Keyed by client identity (admin token, per-client token,
or `anonymous`).

| Key                   | Type | Default | Description                                          |
|-----------------------|------|---------|------------------------------------------------------|
| `enabled`             | bool | `false` | Turn the limiter on.                                 |
| `requests_per_second` | u32  | `10`    | Refill rate.                                         |
| `burst_size`          | u32  | `20`    | Bucket capacity.                                     |

When a caller is over budget the federation responds with HTTP 429 and JSON-RPC
error `-32000` "rate limit exceeded".

#### Per-client RBAC — `federation.clients[]`

```yaml
federation:
  auth_token: "admin-super-secret"
  clients:
    - token: "team-a"
      allowed_servers: ["prod-k8s"]
    - token: "team-b"
      allowed_servers: ["prod-k8s", "staging-k8s"]
    - token: "readonly"
      allowed_servers: ["*"]   # wildcard = same visibility as admin
```

| Key               | Type         | Description                                                                                                   |
|-------------------|--------------|---------------------------------------------------------------------------------------------------------------|
| `token`           | string       | Bearer token the client presents.                                                                             |
| `allowed_servers` | list<string> | Leaf aliases this client may see and invoke. `["*"]` grants full access. Federation-native tools always pass. |

Behavior:

- `tools/list` responses are filtered — clients only see tools from leaves in
  their `allowed_servers`.
- `tools/call` on a disallowed leaf returns JSON-RPC error `-32000` "access denied".
- The admin `auth_token` always shadows a matching client entry (admin wins).
- When `auth_token` is unset the auth middleware treats every request as
  anonymous with full visibility — clients are ignored.

#### `federation.dns_discovery`

See [Discovery — DNS SRV](#dns-srv). Fields:

| Key                     | Type   | Default             | Description                                                     |
|-------------------------|--------|---------------------|-----------------------------------------------------------------|
| `enabled`               | bool   | `false`             | Turn discovery on.                                              |
| `srv_name`              | string | `""`                | Fully-qualified SRV record, e.g. `_mcp._tcp.example.com`.       |
| `poll_interval_seconds` | u64    | `60`                | How often to re-query the SRV record.                           |
| `default_transport`     | enum   | `http-post`         | Transport to use for discovered leaves.                         |
| `url_path`              | string | `"/mcp"`            | Path appended to each `scheme://target:port`.                   |
| `scheme`                | string | `"http"`            | URL scheme.                                                     |

#### `federation.crd_discovery`

See [Discovery — Kubernetes CRD](#kubernetes-crd). Requires `--features crd`.

| Key         | Type   | Default | Description                                                    |
|-------------|--------|---------|----------------------------------------------------------------|
| `enabled`   | bool   | `false` | Start the controller.                                          |
| `namespace` | string | `""`    | Namespace to watch. Empty means all namespaces (cluster-wide). |

### `servers[]` — leaf definitions

Every entry is a leaf MCP server the federation aggregates.

| Key                | Type          | Default     | Description                                                                                              |
|--------------------|---------------|-------------|----------------------------------------------------------------------------------------------------------|
| `alias`            | string        | required    | Namespace prefix for the leaf's tools. Must NOT contain `__`. Must be unique across the federation.      |
| `url`              | string        | required    | Full URL to the leaf's MCP endpoint (e.g. `http://svc:8080/mcp`).                                        |
| `transport`        | enum          | `http-post` | `http-post` or `streamable-http`. See [Transports](#transports).                                         |
| `tags`             | list<string>  | `[]`        | Free-form tags surfaced via `federation__list_servers` — purely informational.                           |
| `timeout_seconds`  | u64           | `30`        | Per-request timeout for calls to this leaf.                                                              |
| `auth_token`       | string        | unset       | Bearer token the federation presents when talking to this leaf.                                          |
| `auth_token_file`  | string (path) | unset       | Path to a file containing the leaf's bearer token. Re-read on every request so tokens can rotate.        |
| `health_check`     | object        | see below   | Per-leaf health-check tuning.                                                                            |
| `tls`              | object        | see below   | TLS verification settings for HTTPS leaves.                                                              |

#### `servers[].health_check`

| Key                  | Type | Default | Description                                                                                                          |
|----------------------|------|---------|----------------------------------------------------------------------------------------------------------------------|
| `enabled`            | bool | `true`  | Whether to run the background health monitor at all.                                                                 |
| `interval_seconds`   | u64  | `30`    | Base polling interval.                                                                                               |
| `timeout_seconds`    | u64  | `5`     | Also used as the TCP connect timeout for regular leaf calls.                                                         |
| `failure_threshold`  | u32  | `3`     | Consecutive failures before the leaf is flipped to Unhealthy.                                                        |
| `backoff_multiplier` | f64  | `2.0`   | Once past the threshold, each additional failure multiplies the interval by this factor (capped at 300 s).           |

Leaves start in `Unknown` state (treated as healthy for aggregation purposes)
and flip to `Healthy` on first successful initialize. On recovery the tool
list is automatically refreshed.

#### `servers[].tls`

| Key            | Type          | Default | Description                                                                    |
|----------------|---------------|---------|--------------------------------------------------------------------------------|
| `verify`       | bool          | `true`  | Verify the leaf's TLS certificate. Set to `false` for self-signed dev leaves.  |
| `ca_cert_path` | string (path) | unset   | Additional CA certificate (PEM) to trust for this leaf (private CA scenarios). |

### Environment variables

| Variable                          | Equivalent flag        | Purpose                                                     |
|-----------------------------------|------------------------|-------------------------------------------------------------|
| `MCP_FEDERATION_CONFIG`           | `--config`             | Path to config file.                                        |
| `MCP_FEDERATION_LISTEN`           | `--listen`             | HTTP listen address override.                               |
| `MCP_FEDERATION_AUTH_TOKEN`       | `--auth-token`         | Bearer token for client auth.                               |
| `MCP_FEDERATION_AUTH_TOKEN_FILE`  | `--auth-token-file`    | Path to bearer token file (rotation-friendly).              |
| `TLS_CERT`                        | `--tls-cert`           | Path to TLS cert PEM (with `TLS_KEY` enables HTTPS).        |
| `TLS_KEY`                         | `--tls-key`            | Path to TLS key PEM.                                        |
| `LOG_FORMAT`                      | `--log-format`         | `text` (default) or `json`.                                 |
| `RUST_LOG`                        | —                      | Standard `tracing-subscriber` env filter, e.g. `debug`.     |

Any `${ENV_VAR}` in the config file itself is interpolated too, so you can
push leaf auth tokens in via the environment:

```yaml
servers:
  - alias: "prod-k8s"
    url: "http://mcp-k8s:8080/mcp"
    auth_token: "${PROD_K8S_TOKEN}"
```

## CLI reference

```
mcp-federation [FLAGS]
```

| Flag                          | Env                                | Description                                                                                                                    |
|-------------------------------|------------------------------------|--------------------------------------------------------------------------------------------------------------------------------|
| `--config <PATH>`             | `MCP_FEDERATION_CONFIG`            | Config file path. Defaults to `federation.yaml`.                                                                               |
| `--http`                      | —                                  | Run in HTTP server mode. Without this flag the process runs in stdio mode.                                                     |
| `--listen <ADDR>`             | `MCP_FEDERATION_LISTEN`            | Override the `federation.listen` from config.                                                                                  |
| `--auth-token <TOKEN>`        | `MCP_FEDERATION_AUTH_TOKEN`        | Override the config file's `auth_token`.                                                                                       |
| `--auth-token-file <PATH>`    | `MCP_FEDERATION_AUTH_TOKEN_FILE`   | Path to file containing the bearer token. Re-read per request. Takes effect only when `--auth-token` is unset.                 |
| `--tls-cert <PATH>`           | `TLS_CERT`                         | TLS certificate PEM. Combined with `--tls-key` switches HTTP mode to HTTPS.                                                    |
| `--tls-key <PATH>`            | `TLS_KEY`                          | TLS private key PEM.                                                                                                           |
| `--log-format <text\|json>`   | `LOG_FORMAT`                       | Log output format. `json` emits structured logs for shipping to Loki/CloudWatch.                                               |
| `--list-tools`                | —                                  | Load config, initialize leaves, print the full aggregated tool table, and exit. Handy for smoke testing.                       |
| `--validate`                  | —                                  | Parse and validate the config file, then exit `0` on success (`1` on error). Does NOT start the server or connect to leaves.   |

## Transports

Each leaf declares which MCP transport it speaks in its `transport:` field.
The federation itself always accepts both plain HTTP JSON-RPC and the full
Streamable HTTP transport from clients.

### `http-post`

Simple JSON-RPC over HTTP POST. Every request is stateless. This is the
default and what `mcp-k8s` currently speaks.

### `streamable-http`

The full [MCP Streamable HTTP transport](https://spec.modelcontextprotocol.io/specification/basic/transports/):

- The leaf issues an `Mcp-Session-Id` header on `initialize`. Subsequent
  requests must echo it back — the federation tracks the session id per leaf.
- POST responses may be `application/json` or an inline `text/event-stream`
  frame carrying one `data:` event with the JSON-RPC reply. Both are handled.
- On HTTP 404 (session expired) or HTTP 400 (session missing) the federation
  transparently re-initializes and retries the original request once.
- A long-lived `GET {leaf_url}` SSE stream is opened at startup to receive
  leaf-originated notifications (`notifications/tools/list_changed`, etc.),
  which are then re-broadcast to subscribed clients.

### Client-facing session handling

The federation's own `/mcp` endpoint speaks Streamable HTTP:

- `POST /mcp initialize` returns an `Mcp-Session-Id` header. Subsequent POSTs
  should echo it. Sessions expire after `session_ttl_seconds` of inactivity.
- `POST /mcp` with `Accept: text/event-stream` returns the JSON-RPC reply as
  a single `data:` event.
- `GET /mcp` (with a valid session id) opens an SSE stream carrying every
  server-initiated notification.
- `DELETE /mcp` explicitly terminates the session.
- Plain HTTP POST clients that never call `initialize` and never send a
  session header still work — the federation stays backward-compatible with
  the simple JSON-RPC transport.

Session ids are HMAC-signed JWTs. Set `session_secret` (base64-encoded) to
keep them valid across restarts.

## Federation-native tools

The federation always exposes five built-in tools under the `federation`
namespace, independent of any leaf state. They are visible to every client
(including per-client RBAC entries) since they operate on federation state.

| Tool                          | Arguments                                    | Purpose                                                                                             |
|-------------------------------|----------------------------------------------|-----------------------------------------------------------------------------------------------------|
| `federation__list_servers`    | none                                         | List every configured leaf with its URL, transport, tags, health, cached tool count, and `is_federation`. |
| `federation__server_info`     | `alias` (required)                           | Details for one leaf: health, consecutive failures, cached tool names.                              |
| `federation__refresh`         | `alias` (optional)                           | Force a tool-list refresh for one leaf, or all leaves when `alias` is omitted.                      |
| `federation__search_tools`    | `query` (required), `alias` (optional)       | Case-insensitive substring match against tool names and descriptions across leaves.                 |
| `federation__topology`        | none                                         | Full federation tree — this node plus each leaf, including an `is_federation` flag to walk nested federations. |

Under the hood these are just normal MCP tools; a client sees them in
`tools/list` alongside every leaf tool.

## Discovery

Leaves can be added three ways. All three coexist — you can have a mix of
static, DNS-discovered, and CRD-discovered leaves in the same federation,
and each source manages its own aliases (`dns-`, `crd-`, or config-defined).

### Static config

The default. Every entry in `servers[]` is registered on startup.

**Hot reload**: on **SIGHUP** the config file is re-read. New leaves are
added, existing leaves whose `url`/`auth_token`/`transport`/`timeout_seconds`
changed are replaced, and leaves removed from the file are dropped. Reload
failures keep the current registry in place.

```bash
kill -HUP $(pgrep mcp-federation)
```

### Dynamic registration REST API

REST endpoints for adding and removing leaves at runtime. Guarded by the
same bearer-token auth as `/mcp`.

```
POST   /api/v1/servers            Register a new leaf
GET    /api/v1/servers            List all leaves
GET    /api/v1/servers/{alias}    Get one leaf
DELETE /api/v1/servers/{alias}    Deregister a leaf
```

Example:

```bash
curl -s -X POST http://localhost:8080/api/v1/servers \
  -H "Authorization: Bearer $ADMIN_TOKEN" \
  -H "Content-Type: application/json" \
  -d '{
    "alias": "ephemeral-k8s",
    "url": "http://mcp-k8s-review-app:8080/mcp",
    "transport": "http-post",
    "tags": ["review-app"]
  }'
```

Body fields for `POST /api/v1/servers`: `alias`, `url`, `transport`
(defaults to `http-post`), `tags`, `auth_token`, `timeout_seconds`.
The MCP handshake and tool warm-up happen in the background, so the REST
call returns as soon as the entry is inserted (HTTP 201).

### DNS SRV

Set `federation.dns_discovery.enabled: true` and point `srv_name` at a
SRV record. A background task polls the record every
`poll_interval_seconds` (default 60s) and reconciles:

```yaml
federation:
  dns_discovery:
    enabled: true
    srv_name: "_mcp._tcp.example.com"
    poll_interval_seconds: 30
    default_transport: http-post
    scheme: http
    url_path: /mcp
```

- Each `priority weight port target` from the SRV record becomes a leaf.
- Alias is derived from the sanitized target hostname with a `dns-` prefix
  (e.g. `leaf-1.example.com.` → `dns-leaf-1-example-com`).
- Targets that disappear from the record are removed on the next poll.
- Only `dns-`-prefixed aliases are touched — static and CRD leaves are safe.

Implementation note: shells out to `dig SRV +short`, so the `dig` binary
must be on `PATH` in the container. (The distroless image published in the
Docker workflow does not include `dig` — deploy DNS discovery with a slim
Debian/Alpine base if you need it.)

### Kubernetes CRD

Feature-gated behind `--features crd`. Watches the `MCPServer` custom
resource and registers a leaf per CR.

Install the CRD:

```bash
kubectl apply -f helm/mcp-federation/crds/mcpserver.yaml
```

Create leaves as ordinary K8s resources:

```yaml
apiVersion: federation.mcp.io/v1alpha1
kind: MCPServer
metadata:
  name: prod-k8s
  namespace: mcp-system
spec:
  alias: prod-k8s
  url: http://mcp-k8s.mcp-system.svc:8080/mcp
  transport: http-post
  tags:
    - kubernetes
    - production
  # authToken: "shhh"                          # rarely — prefer authTokenFile
  # authTokenFile: /var/run/secrets/leaf/token
  # timeoutSeconds: 60
```

Enable in the federation config:

```yaml
federation:
  crd_discovery:
    enabled: true
    namespace: mcp-system     # empty = all namespaces
```

Aliases are namespaced (`crd-<namespace>-<name>`) so CRD leaves don't
collide with static or DNS leaves. CR deletion (deletion timestamp) drops
the leaf from the registry.

If the CRD is not installed the controller logs a warning and exits
cleanly — the rest of the federation keeps running.

## Security

### Bearer token auth

Set `federation.auth_token` (or `--auth-token`) and every request to
`/mcp`, `/api/v1/*` must present `Authorization: Bearer <token>`.

`/healthz`, `/metrics`, `/status`, `/swagger-ui`, and `/openapi.json` are
always unauthenticated so ops tooling and load balancers can reach them.

### Rotating tokens without restart

Point `auth_token_file` (or `--auth-token-file`) at a file. The file is
re-read on every request — write a new token atomically (write-to-temp +
rename) and the next request will see it.

The same pattern applies per leaf: `servers[].auth_token_file` rotates
outbound tokens the federation presents to leaves.

### Per-client RBAC

See [`federation.clients[]`](#per-client-rbac--federationclients). Each
client token has its own visible tool subset. `tools/list` is filtered;
`tools/call` returns `access denied` for disallowed leaves. Federation-native
tools always pass.

Every request is bound to a `RequestContext` with a `client_label`
(`admin`, `client-N`, or `anonymous`) that flows into audit logs and the
rate limiter's bucket key.

### TLS

Client-facing HTTPS is enabled by passing both `--tls-cert` and
`--tls-key` (or the `TLS_CERT` / `TLS_KEY` env vars). HTTPS mode also
honors `shutdown_timeout_seconds` for graceful drain.

For **leaf** TLS, `servers[].tls` controls verification:

- `verify: false` accepts self-signed certs (dev only).
- `ca_cert_path` adds a trust anchor for private CAs.

### Origin allowlist

For browser-based clients, populate `federation.allowed_origins` with the
exact `Origin` header values you want to accept. Requests with an `Origin`
outside the list get HTTP 403. This defends against DNS-rebinding attacks.

### Rate limiting

See [`federation.rate_limit`](#federationrate_limit). Per-client
token-bucket, keyed by the authenticated client label. Exceeded requests
return HTTP 429 with JSON-RPC error `-32000`.

## Observability

### Prometheus metrics

Scrape `GET /metrics` (no auth required). Key metrics:

| Metric                                       | Type      | Labels             | Meaning                                              |
|----------------------------------------------|-----------|--------------------|------------------------------------------------------|
| `federation_requests_total`                  | counter   | `method`           | Every dispatched JSON-RPC request.                   |
| `federation_tool_calls_total`                | counter   | `tool`, `alias`    | Tool call invocations.                               |
| `federation_tool_call_errors_total`          | counter   | `tool`, `alias`    | Tool call failures (leaf error, RBAC, unhealthy).    |
| `federation_tool_call_duration_seconds`      | histogram | `tool`, `alias`    | End-to-end tool call latency.                        |
| `federation_leaf_health`                     | gauge     | `alias`            | `1.0` healthy, `0.0` unhealthy.                      |
| `federation_leaf_tool_count`                 | gauge     | `alias`            | Cached tool count per leaf.                          |

### Structured logging

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

### OpenAPI / Swagger

`GET /swagger-ui` renders an interactive OpenAPI 3 explorer;
`GET /openapi.json` returns the raw spec. Currently documents the MCP
JSON-RPC endpoint, session lifecycle, and `/healthz`.

### Status page

`GET /status` returns a plain HTML dashboard listing every leaf with its
URL, health, cached tool count, tags, and time since last health check. No
JavaScript, no auth — safe to expose over a firewall for humans.

### Health check

`GET /healthz` returns `200 ok`. Cheap; use for liveness/readiness probes.

## Deployment

### Helm

A first-class Helm chart lives at `helm/mcp-federation/`. Basic install:

```bash
helm install mcp-federation ./helm/mcp-federation \
  --namespace mcp-system --create-namespace \
  --set-file config=federation.yaml \
  --set authToken=$MCP_AUTH_TOKEN
```

The chart ships templates for `Deployment`, `Service`, `Ingress`,
`ServiceAccount`, `HorizontalPodAutoscaler`, `PodDisruptionBudget`, and
`NetworkPolicy`. See `helm/mcp-federation/README.md` for the values you can
override.

### Docker

Multi-arch images (linux/amd64, linux/arm64) built by the CI workflow and
pushed to `ghcr.io/alexconrey/mcp-federation:latest`. The `Dockerfile` uses
a musl static build on top of a distroless base — the runtime image has no
shell, no package manager, and runs as `nonroot`.

```bash
docker run --rm -p 8080:8080 \
  -v $(pwd)/federation.yaml:/etc/mcp-federation/federation.yaml:ro \
  -e MCP_FEDERATION_AUTH_TOKEN=$TOKEN \
  ghcr.io/alexconrey/mcp-federation:latest \
  --config /etc/mcp-federation/federation.yaml
```

### CI / CD

`.github/workflows/ci.yaml` runs `cargo fmt --check`, `cargo clippy
--all-targets -D warnings`, `cargo build --release`, and `cargo test` on
every PR. Pushes to `main` additionally publish a multi-arch Docker image
to GHCR tagged `:latest` and `:$COMMIT_SHA`.

## Multi-level federation

Because a federation is itself an MCP server, one federation can be a leaf
of another federation. Nesting works transparently:

```yaml
# parent-federation.yaml
servers:
  - alias: "east"
    url: "http://federation-east:8080/mcp"
    transport: streamable-http
  - alias: "west"
    url: "http://federation-west:8080/mcp"
    transport: streamable-http
```

Tools are namespaced at every hop. A `list_pods` tool on a leaf in
`federation-east` reachable through `parent → east → prod-k8s` becomes:

```
east__prod-k8s__list_pods
```

Each federation captures the child's `serverInfo` on initialize and flags
it via `is_federation: true` in `federation__list_servers` and
`federation__topology`, so operators can walk the tree.

## Development

### Build and test

```bash
cargo build
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt --check
```

### Feature flags

- `crd` — enable the Kubernetes CRD watcher. Pulls in the `kube`,
  `k8s-openapi`, and `schemars` crates. Off by default to keep the base
  binary small.

```bash
cargo build --release --features crd
```

### Layout

```
src/
├── main.rs              # CLI, startup, auth middleware, SIGHUP reload
├── server.rs            # /mcp handlers, session lifecycle, dispatch, audit logs
├── config.rs            # YAML config schema + env interpolation
├── registry.rs          # Leaf entries: cache, health, dynamic add/remove
├── leaf_client.rs       # HTTP/JSON-RPC + Streamable HTTP + SSE parsing
├── aggregator.rs        # Tool namespacing + TTL cache + native tool list
├── router.rs            # Dispatch tool/resource/prompt calls to leaves
├── health.rs            # Circuit breaker + exponential backoff
├── session.rs           # JWT session ids + prune task
├── rbac.rs              # RequestContext + per-client authorization
├── rate_limit.rs        # Token-bucket limiter
├── notifications.rs     # SSE fanout broker
├── api.rs               # /api/v1/servers dynamic registration REST API
├── dns_discovery.rs     # DNS SRV polling + reconciler
├── crd_discovery.rs     # Kubernetes CRD controller (feature-gated)
└── mcp/                 # JSON-RPC 2.0 types
```

### License

MIT.
