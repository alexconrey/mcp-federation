# mcp-federation

An aggregating [Model Context Protocol](https://modelcontextprotocol.io) (MCP)
server. `mcp-federation` sits in front of N downstream ("leaf") MCP servers
and exposes them to clients as a **single MCP endpoint** with a unified tool
catalog.

Every leaf tool is namespaced with the leaf's alias (`alias__tool`),
health is monitored, tools are cached, and requests are routed to the
appropriate leaf.

## Why federation?

You have several MCP servers -- one per Kubernetes cluster, one per database,
one for your ticketing system, one on every CI runner. Clients want a **single
endpoint** rather than a config file with N entries that each rotate
independently.

`mcp-federation` is that single endpoint. It:

- **Aggregates** -- fetches every leaf's tool list at startup and on refresh.
- **Namespaces** -- rewrites each tool name as `alias__tool_name` so the same
  logical tool can exist on several leaves without collision.
- **Routes** -- strips the alias off incoming `tools/call` requests and forwards
  the call to the right leaf.
- **Monitors** -- health-checks each leaf, drops unhealthy ones from the
  advertised tool list, and applies exponential backoff to failing leaves.
- **Caches** -- tool lists are cached with a TTL and refreshed in the background
  on demand.
- **Secures** -- bearer-token auth, per-client RBAC, TLS, rate limiting,
  origin allowlist.
- **Observes** -- Prometheus metrics, structured JSON logs with per-request
  trace IDs, an admin status page, and OpenAPI docs.
