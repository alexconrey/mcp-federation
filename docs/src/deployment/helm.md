# Helm Chart

A first-class Helm chart lives at `helm/mcp-federation/`.

## What the chart deploys

- `Deployment` -- one or more `mcp-federation --http` pods.
- `ConfigMap` -- holds the rendered `federation.yaml`, mounted at
  `/etc/mcp-federation/federation.yaml`.
- `Service` (ClusterIP) -- exposes port 8080 in-cluster.
- `ServiceAccount` -- created by default; opt out with `serviceAccount.create=false`.
- `Ingress` -- optional (`ingress.enabled=true`).
- `HorizontalPodAutoscaler` -- optional (`autoscaling.enabled=true`).
- `PodDisruptionBudget` -- optional (`pdb.enabled=true`).
- `NetworkPolicy` -- optional (`networkPolicy.enabled=true`).

## Install

```bash
helm install mcp-federation ./helm/mcp-federation \
  --namespace mcp-system --create-namespace \
  --set-file config=federation.yaml \
  --set authToken=$MCP_AUTH_TOKEN
```

## Upgrade

```bash
helm upgrade mcp-federation ./helm/mcp-federation \
  --namespace mcp-system \
  --set-file config=federation.yaml
```

The `Deployment` has a `checksum/config` annotation on the pod template so
config changes trigger a rolling restart automatically.

## Key values

| Value                                        | Default                                     | Notes                                                                   |
|----------------------------------------------|---------------------------------------------|-------------------------------------------------------------------------|
| `image.repository`                           | `ghcr.io/alexconrey/mcp-federation`         | Multi-arch (amd64/arm64) images published by CI.                        |
| `image.tag`                                  | `latest`                                    | Pin to a commit SHA for reproducible deploys.                           |
| `replicaCount`                               | `1`                                         | Bump for HA.                                                            |
| `config`                                     | minimal skeleton                            | The full `federation.yaml`. Prefer `--set-file config=path`.            |
| `authToken`                                  | `""`                                        | Bearer token for `/mcp`. Exported as `MCP_FEDERATION_AUTH_TOKEN`.       |
| `logFormat`                                  | `text`                                      | Set to `json` for structured logs.                                      |
| `service.type` / `service.port`              | `ClusterIP` / `8080`                        | The `Service` name is `<release>-mcp-federation`.                       |
| `ingress.enabled`                            | `false`                                     | Renders an `Ingress` on `ingress.host` with `ingress.className`.        |
| `autoscaling.enabled`                        | `false`                                     | Renders an `HPA`.                                                       |
| `pdb.enabled`                                | `false`                                     | Renders a `PodDisruptionBudget`.                                        |
| `networkPolicy.enabled`                      | `false`                                     | Restricts ingress to `allowedCIDRs` / `allowedNamespaces`.              |

See `values.yaml` for the complete list and inline comments.

## Uninstall

```bash
helm uninstall mcp-federation -n mcp-system
```

Helm does not delete CRDs installed from `crds/`. Remove them manually if
you no longer need them:

```bash
kubectl delete crd mcpservers.federation.mcp.io
```
