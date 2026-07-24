# Discovery

Leaves can be added three ways. All three coexist -- you can have a mix of
static, DNS-discovered, and CRD-discovered leaves in the same federation,
and each source manages its own aliases (`dns-`, `crd-`, or config-defined).

## Static config

The default. Every entry in `servers[]` is registered on startup.

**Hot reload**: on **SIGHUP** the config file is re-read. New leaves are
added, existing leaves whose `url`/`auth_token`/`transport`/`timeout_seconds`
changed are replaced, and leaves removed from the file are dropped. Reload
failures keep the current registry in place.

```bash
kill -HUP $(pgrep mcp-federation)
```

## Dynamic registration REST API

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

## DNS SRV

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
  (e.g. `leaf-1.example.com.` -> `dns-leaf-1-example-com`).
- Targets that disappear from the record are removed on the next poll.
- Only `dns-`-prefixed aliases are touched -- static and CRD leaves are safe.

Implementation note: shells out to `dig SRV +short`, so the `dig` binary
must be on `PATH` in the container.

## Kubernetes CRD

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
```

Enable in the federation config:

```yaml
federation:
  crd_discovery:
    enabled: true
    namespace: mcp-system     # empty = all namespaces
```

Aliases are namespaced (`crd-<namespace>-<name>`) so CRD leaves don't
collide with static or DNS leaves. CR deletion drops the leaf from the
registry.
