# Installation

## From source

```bash
cargo build --release
```

The binary is at `target/release/mcp-federation`.

## Docker

Multi-arch images (linux/amd64, linux/arm64) are published to GHCR:

```bash
docker pull ghcr.io/alexconrey/mcp-federation:latest
```

## Helm

A first-class Helm chart lives at `helm/mcp-federation/`. See the
[Helm deployment](../deployment/helm.md) chapter for install instructions.

## Feature flags

- `crd` -- enable the Kubernetes CRD watcher. Pulls in the `kube`,
  `k8s-openapi`, and `schemars` crates. Off by default to keep the base
  binary small.

```bash
cargo build --release --features crd
```
