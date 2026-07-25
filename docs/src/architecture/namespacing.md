# Tool Namespacing

Every tool from a leaf MCP server is re-exported by the federation with an
`<alias>__` prefix. This prevents name collisions when multiple leaves expose
tools with the same name.

## How it works

If `prod-k8s` advertises `list_pods` and `staging-k8s` also advertises
`list_pods`, clients see:

- `prod-k8s__list_pods`
- `staging-k8s__list_pods`

When a client calls `prod-k8s__list_pods`, the federation:

1. Strips the `prod-k8s__` prefix to recover the original tool name `list_pods`.
2. Routes the call to the `prod-k8s` leaf.
3. Returns the result to the client.

## Rules

- The `__` (double underscore) separator is **reserved**. Leaf aliases cannot
  contain `__`.
- Each alias must be unique across the federation.
- Federation-native tools use the `federation` alias
  (e.g. `federation__list_servers`).

## Multi-level namespacing

When federations are nested, namespacing stacks at each hop. A `list_pods`
tool on a leaf in `federation-east`, reachable through
`parent -> east -> prod-k8s`, becomes:

```
east__prod-k8s__list_pods
```
