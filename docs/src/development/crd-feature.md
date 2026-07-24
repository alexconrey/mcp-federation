# CRD Feature

The `crd` feature flag enables the Kubernetes CRD-based discovery controller.
It is off by default to keep the base binary small.

## Building with CRD support

```bash
cargo build --release --features crd
```

## How it works

When enabled, the federation watches for `MCPServer` custom resources in the
configured namespace (or cluster-wide if the namespace is empty). Each CR is
translated into a leaf server entry in the federation registry.

## CRD installation

The CRD manifest ships with the Helm chart:

```bash
kubectl apply -f helm/mcp-federation/crds/mcpserver.yaml
```

## Example MCPServer CR

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

## Configuration

Enable in the federation config:

```yaml
federation:
  crd_discovery:
    enabled: true
    namespace: mcp-system     # empty = all namespaces
```

## RBAC requirements

The federation's `ServiceAccount` needs `get`, `list`, and `watch`
permissions on `mcpservers.federation.mcp.io`. The default Helm chart does
not ship this RBAC -- add a `ClusterRole` / `RoleBinding` yourself.

## Alias namespacing

CRD-discovered aliases are prefixed with `crd-<namespace>-<name>` so they
don't collide with static or DNS-discovered leaves. CR deletion removes the
leaf from the registry.

If the CRD is not installed in the cluster, the controller logs a warning
and exits cleanly -- the rest of the federation keeps running.
