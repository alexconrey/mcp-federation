# Multi-Level Federation

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
`federation-east` reachable through `parent -> east -> prod-k8s` becomes:

```
east__prod-k8s__list_pods
```

Each federation captures the child's `serverInfo` on initialize and flags
it via `is_federation: true` in `federation__list_servers` and
`federation__topology`, so operators can walk the tree.
