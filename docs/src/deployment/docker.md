# Docker

Multi-arch images (linux/amd64, linux/arm64) are built by the CI workflow and
pushed to `ghcr.io/alexconrey/mcp-federation:latest`. The `Dockerfile` uses
a musl static build on top of a distroless base -- the runtime image has no
shell, no package manager, and runs as `nonroot`.

```bash
docker run --rm -p 8080:8080 \
  -v $(pwd)/federation.yaml:/etc/mcp-federation/federation.yaml:ro \
  -e MCP_FEDERATION_AUTH_TOKEN=$TOKEN \
  ghcr.io/alexconrey/mcp-federation:latest \
  --config /etc/mcp-federation/federation.yaml
```

## Tags

- `latest` -- built from the latest tagged release
- `v*` -- specific version tags (e.g. `v0.1.0`)
- `<commit-sha>` -- built from pushes to `main`
