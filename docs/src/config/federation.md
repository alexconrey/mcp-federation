# Federation Settings

Top-level `federation` settings in the config file.

| Key                        | Type          | Default          | Description                                                                                               |
|----------------------------|---------------|------------------|-----------------------------------------------------------------------------------------------------------|
| `listen`                   | string        | `"0.0.0.0:8080"` | Bind address for HTTP/HTTPS mode.                                                                         |
| `endpoint`                 | string        | `"/mcp"`         | Path for the MCP JSON-RPC endpoint.                                                                       |
| `auth_token`               | string        | unset            | Bearer token clients must present. If unset, `/mcp` is unauthenticated.                                   |
| `auth_token_file`          | string (path) | unset            | Path to a file containing the bearer token. Re-read on every request so tokens can rotate without restart.|
| `session_ttl_seconds`      | u64           | `1800`           | Streamable HTTP session idle timeout.                                                                     |
| `session_secret`           | string (b64)  | unset            | Base64-encoded HMAC key used to sign session JWTs. If unset a random key is generated on startup, meaning sessions do not survive a restart. |
| `tool_cache_ttl_seconds`   | u64           | `300`            | How long an aggregated tool list is considered fresh before a background refresh is triggered.            |
| `shutdown_timeout_seconds` | u64           | `15`             | Grace period for in-flight connections at shutdown.                                                       |
| `allowed_origins`          | list\<string\>| `[]`             | DNS-rebinding protection: if non-empty, `/mcp` rejects requests whose `Origin` header is not in the list. |
| `connection_pool`          | object        | see below        | HTTP client connection pool tuning for outbound leaf calls.                                               |
| `clients`                  | list          | `[]`             | Per-client bearer tokens with RBAC.                                                                       |
| `rate_limit`               | object        | disabled         | Per-client token-bucket rate limiter.                                                                    |
| `dns_discovery`            | object        | disabled         | DNS SRV-based leaf discovery.                                                                             |
| `crd_discovery`            | object        | disabled         | Kubernetes CRD-based leaf discovery.                                                                      |

## `connection_pool`

Applied to the reqwest client used to reach each leaf.

| Key                    | Type | Default | Description                                    |
|------------------------|------|---------|------------------------------------------------|
| `max_idle_per_host`    | usize| `10`    | Max idle connections kept per leaf host.       |
| `idle_timeout_seconds` | u64  | `90`    | How long an idle connection is retained.       |

## `rate_limit`

Classic token-bucket. Keyed by client identity (admin token, per-client token,
or `anonymous`).

| Key                   | Type | Default | Description                                          |
|-----------------------|------|---------|------------------------------------------------------|
| `enabled`             | bool | `false` | Turn the limiter on.                                 |
| `requests_per_second` | u32  | `10`    | Refill rate.                                         |
| `burst_size`          | u32  | `20`    | Bucket capacity.                                     |

When a caller is over budget the federation responds with HTTP 429 and JSON-RPC
error `-32000` "rate limit exceeded".

## `clients[]` -- Per-client RBAC

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
| `allowed_servers` | list\<string\>| Leaf aliases this client may see and invoke. `["*"]` grants full access. Federation-native tools always pass. |

## `dns_discovery`

| Key                     | Type   | Default             | Description                                                     |
|-------------------------|--------|---------------------|-----------------------------------------------------------------|
| `enabled`               | bool   | `false`             | Turn discovery on.                                              |
| `srv_name`              | string | `""`                | Fully-qualified SRV record, e.g. `_mcp._tcp.example.com`.       |
| `poll_interval_seconds` | u64    | `60`                | How often to re-query the SRV record.                           |
| `default_transport`     | enum   | `http-post`         | Transport to use for discovered leaves.                         |
| `url_path`              | string | `"/mcp"`            | Path appended to each `scheme://target:port`.                   |
| `scheme`                | string | `"http"`            | URL scheme.                                                     |

## `crd_discovery`

Requires `--features crd`.

| Key         | Type   | Default | Description                                                    |
|-------------|--------|---------|----------------------------------------------------------------|
| `enabled`   | bool   | `false` | Start the controller.                                          |
| `namespace` | string | `""`    | Namespace to watch. Empty means all namespaces (cluster-wide). |
