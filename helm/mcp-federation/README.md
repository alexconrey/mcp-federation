# mcp-federation Helm chart

Deploys an [`mcp-federation`](../../README.md) server on Kubernetes. The
federation config is mounted into the pod from a `ConfigMap` that the chart
generates from the `config` value.

## What this chart deploys

- `Deployment` — one or more `mcp-federation --http` pods.
- `ConfigMap` — holds the rendered `federation.yaml`, mounted at
  `/etc/mcp-federation/federation.yaml`.
- `Service` (ClusterIP) — exposes port 8080 in-cluster.
- `ServiceAccount` — created by default; opt out with `serviceAccount.create=false`.
- `Ingress` — optional (`ingress.enabled=true`).
- `HorizontalPodAutoscaler` — optional (`autoscaling.enabled=true`).
- `PodDisruptionBudget` — optional (`pdb.enabled=true`).
- `NetworkPolicy` — optional (`networkPolicy.enabled=true`).

The `MCPServer` CRD is shipped separately under `crds/mcpserver.yaml` and
Helm installs it automatically the first time the chart is applied. It is
only consumed by the federation when the binary is built with
`--features crd` and the config sets `federation.crd_discovery.enabled: true`.

## Prerequisites

- Kubernetes 1.24+
- Helm 3+
- A `federation.yaml` describing the leaf servers you want to aggregate
  (see the top-level [README](../../README.md) for the schema).

## Install

Simplest install — inline config via `--set-file`:

```bash
helm install mcp-federation ./helm/mcp-federation \
  --namespace mcp-system --create-namespace \
  --set-file config=federation.yaml \
  --set authToken=$MCP_AUTH_TOKEN
```

Upgrade (config file changed):

```bash
helm upgrade mcp-federation ./helm/mcp-federation \
  --namespace mcp-system \
  --set-file config=federation.yaml
```

The `Deployment` has a `checksum/config` annotation on the pod template so
config changes trigger a rolling restart automatically.

## Key values

| Value                                        | Default                                     | Notes                                                                                                                    |
|----------------------------------------------|---------------------------------------------|--------------------------------------------------------------------------------------------------------------------------|
| `image.repository`                           | `ghcr.io/alexconrey/mcp-federation`         | Multi-arch (amd64/arm64) images published by CI.                                                                         |
| `image.tag`                                  | `latest`                                    | Pin to a commit SHA for reproducible deploys.                                                                            |
| `replicaCount`                               | `1`                                         | Bump for HA. The federation is largely stateless — sessions live in-memory only.                                         |
| `config`                                     | minimal skeleton                            | The full `federation.yaml`. Prefer `--set-file config=path/to/federation.yaml`.                                          |
| `authToken`                                  | `""`                                        | Bearer token for `/mcp`. Exported as `MCP_FEDERATION_AUTH_TOKEN`. Consider mounting a `Secret` instead of inline values. |
| `logFormat`                                  | `text`                                      | Set to `json` for structured logs.                                                                                       |
| `service.type` / `service.port`              | `ClusterIP` / `8080`                        | The `Service` name is `<release>-mcp-federation`.                                                                        |
| `ingress.enabled`                            | `false`                                     | Renders an `Ingress` on `ingress.host` with `ingress.className`.                                                         |
| `autoscaling.enabled`                        | `false`                                     | Renders an `HPA` (min/max/target CPU or memory).                                                                         |
| `pdb.enabled`                                | `false`                                     | Renders a `PodDisruptionBudget` (`minAvailable` or `maxUnavailable`).                                                    |
| `networkPolicy.enabled`                      | `false`                                     | Restricts ingress to `allowedCIDRs` / `allowedNamespaces`.                                                               |
| `serviceAccount.create` / `serviceAccount.name` | `true` / auto                            | Set `create: false` to bind an existing SA.                                                                              |
| `resources`                                  | 50m/64Mi requests, 200m/256Mi limits        | Very lightweight; tune for tool-cache size and leaf fan-out.                                                             |

See `values.yaml` for the complete list and inline comments.

## Overriding the federation config

Two patterns work well:

**Inline in `values.yaml`** — good for GitOps repos where the leaf catalog
lives beside the chart values:

```yaml
config: |
  federation:
    listen: "0.0.0.0:8080"
    endpoint: "/mcp"
    session_secret: "${SESSION_SECRET}"
  servers:
    - alias: prod-k8s
      url: http://mcp-k8s.mcp-system.svc:8080/mcp
      transport: http-post
```

**File on disk** — good for local development:

```bash
helm upgrade --install mcp-federation ./helm/mcp-federation \
  --set-file config=./federation.yaml
```

## Secrets

The chart doesn't mount `Secret`s for you today. For production, prefer:

- Set `authToken` to a placeholder in values and use a Kubernetes `Secret`
  with `envFrom`, patching the `Deployment` (or override the chart).
- Mount leaf credentials as files and reference them from `federation.yaml`:

  ```yaml
  servers:
    - alias: prod-k8s
      auth_token_file: /var/run/secrets/leaf/token
  ```

  The federation re-reads the file on every request, so `Secret` rotation
  is picked up without a pod restart.

## CRD-based leaf discovery

The `MCPServer` CRD (`crds/mcpserver.yaml`) is installed automatically by
Helm. To enable it in the federation:

1. Build/deploy an image with `--features crd`.
2. Set `federation.crd_discovery.enabled: true` in your config.
3. Grant the federation's `ServiceAccount` permission to `get/list/watch`
   `mcpservers.federation.mcp.io` (RBAC not shipped by the default chart —
   add a `ClusterRole` / `RoleBinding` yourself).

## Uninstall

```bash
helm uninstall mcp-federation -n mcp-system
```

Helm does not delete CRDs installed from `crds/`. Remove them manually if
you no longer need them:

```bash
kubectl delete crd mcpservers.federation.mcp.io
```
