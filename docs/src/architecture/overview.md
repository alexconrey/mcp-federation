# Architecture Overview

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

The `__` (double underscore) separator is reserved -- leaf aliases cannot
contain it.

The federation itself advertises five built-in tools under the `federation`
namespace (`federation__list_servers`, `federation__topology`, etc.) that let
clients inspect and manage the federation. See
[Federation-native tools](../features/native-tools.md).

## Source layout

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
