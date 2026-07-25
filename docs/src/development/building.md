# Building

## Default build

```bash
cargo build
cargo build --release
```

## With CRD support

The `crd` feature flag enables the Kubernetes CRD watcher, pulling in
`kube`, `k8s-openapi`, and `schemars`:

```bash
cargo build --release --features crd
```

## Linting and formatting

```bash
cargo clippy --all-targets -- -D warnings
cargo fmt --check
```
