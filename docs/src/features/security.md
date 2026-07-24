# Security

## Bearer token auth

Set `federation.auth_token` (or `--auth-token`) and every request to
`/mcp`, `/api/v1/*` must present `Authorization: Bearer <token>`.

`/healthz`, `/metrics`, `/status`, `/swagger-ui`, and `/openapi.json` are
always unauthenticated so ops tooling and load balancers can reach them.

## Rotating tokens without restart

Point `auth_token_file` (or `--auth-token-file`) at a file. The file is
re-read on every request -- write a new token atomically (write-to-temp +
rename) and the next request will see it.

The same pattern applies per leaf: `servers[].auth_token_file` rotates
outbound tokens the federation presents to leaves.

## Per-client RBAC

See [Federation Settings](../config/federation.md) for the `clients[]`
schema. Each client token has its own visible tool subset. `tools/list` is
filtered; `tools/call` returns `access denied` for disallowed leaves.
Federation-native tools always pass.

Every request is bound to a `RequestContext` with a `client_label`
(`admin`, `client-N`, or `anonymous`) that flows into audit logs and the
rate limiter's bucket key.

## TLS

Client-facing HTTPS is enabled by passing both `--tls-cert` and
`--tls-key` (or the `TLS_CERT` / `TLS_KEY` env vars). HTTPS mode also
honors `shutdown_timeout_seconds` for graceful drain.

For **leaf** TLS, `servers[].tls` controls verification:

- `verify: false` accepts self-signed certs (dev only).
- `ca_cert_path` adds a trust anchor for private CAs.

## Origin allowlist

For browser-based clients, populate `federation.allowed_origins` with the
exact `Origin` header values you want to accept. Requests with an `Origin`
outside the list get HTTP 403. This defends against DNS-rebinding attacks.

## Rate limiting

See [Federation Settings](../config/federation.md) for the `rate_limit`
schema. Per-client token-bucket, keyed by the authenticated client label.
Exceeded requests return HTTP 429 with JSON-RPC error `-32000`.
