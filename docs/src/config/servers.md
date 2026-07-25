# Server Entries

Every entry in the `servers[]` list is a leaf MCP server the federation
aggregates.

| Key                | Type          | Default     | Description                                                                                              |
|--------------------|---------------|-------------|----------------------------------------------------------------------------------------------------------|
| `alias`            | string        | required    | Namespace prefix for the leaf's tools. Must NOT contain `__`. Must be unique across the federation.      |
| `url`              | string        | required    | Full URL to the leaf's MCP endpoint (e.g. `http://svc:8080/mcp`).                                        |
| `transport`        | enum          | `http-post` | `http-post` or `streamable-http`.                                                                        |
| `tags`             | list\<string\>| `[]`        | Free-form tags surfaced via `federation__list_servers` -- purely informational.                           |
| `timeout_seconds`  | u64           | `30`        | Per-request timeout for calls to this leaf.                                                              |
| `auth_token`       | string        | unset       | Bearer token the federation presents when talking to this leaf.                                          |
| `auth_token_file`  | string (path) | unset       | Path to a file containing the leaf's bearer token. Re-read on every request so tokens can rotate.        |
| `health_check`     | object        | see below   | Per-leaf health-check tuning.                                                                            |
| `tls`              | object        | see below   | TLS verification settings for HTTPS leaves.                                                              |

## `health_check`

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

## `tls`

| Key            | Type          | Default | Description                                                                    |
|----------------|---------------|---------|--------------------------------------------------------------------------------|
| `verify`       | bool          | `true`  | Verify the leaf's TLS certificate. Set to `false` for self-signed dev leaves.  |
| `ca_cert_path` | string (path) | unset   | Additional CA certificate (PEM) to trust for this leaf (private CA scenarios). |
