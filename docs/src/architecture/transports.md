# Transports

Each leaf declares which MCP transport it speaks in its `transport:` field.
The federation itself always accepts both plain HTTP JSON-RPC and the full
Streamable HTTP transport from clients.

## `http-post`

Simple JSON-RPC over HTTP POST. Every request is stateless. This is the
default and what `mcp-k8s` currently speaks.

## `streamable-http`

The full [MCP Streamable HTTP transport](https://spec.modelcontextprotocol.io/specification/basic/transports/):

- The leaf issues an `Mcp-Session-Id` header on `initialize`. Subsequent
  requests must echo it back -- the federation tracks the session id per leaf.
- POST responses may be `application/json` or an inline `text/event-stream`
  frame carrying one `data:` event with the JSON-RPC reply. Both are handled.
- On HTTP 404 (session expired) or HTTP 400 (session missing) the federation
  transparently re-initializes and retries the original request once.
- A long-lived `GET {leaf_url}` SSE stream is opened at startup to receive
  leaf-originated notifications (`notifications/tools/list_changed`, etc.),
  which are then re-broadcast to subscribed clients.

## Client-facing session handling

The federation's own `/mcp` endpoint speaks Streamable HTTP:

- `POST /mcp initialize` returns an `Mcp-Session-Id` header. Subsequent POSTs
  should echo it. Sessions expire after `session_ttl_seconds` of inactivity.
- `POST /mcp` with `Accept: text/event-stream` returns the JSON-RPC reply as
  a single `data:` event.
- `GET /mcp` (with a valid session id) opens an SSE stream carrying every
  server-initiated notification.
- `DELETE /mcp` explicitly terminates the session.
- Plain HTTP POST clients that never call `initialize` and never send a
  session header still work -- the federation stays backward-compatible with
  the simple JSON-RPC transport.

Session ids are HMAC-signed JWTs. Set `session_secret` (base64-encoded) to
keep them valid across restarts.
