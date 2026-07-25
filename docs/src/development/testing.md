# Testing

## Run all tests

```bash
cargo test
```

## CI pipeline

`.github/workflows/ci.yaml` runs on every PR and push to `main`:

1. `cargo fmt --check` -- formatting
2. `cargo clippy --all-targets -D warnings` -- linting
3. `cargo build --release` -- release build
4. `cargo test` -- unit tests

Pushes to `main` also build and publish a Docker image to GHCR.
