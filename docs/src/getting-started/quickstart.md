# Quick Start

## 1. Write a `federation.yaml`

```yaml
federation:
  listen: "0.0.0.0:8080"
  endpoint: "/mcp"

servers:
  - alias: "prod-k8s"
    url: "http://mcp-k8s-prod.internal:8080/mcp"
    transport: http-post
    tags: ["kubernetes", "production"]

  - alias: "staging-k8s"
    url: "http://mcp-k8s-staging.internal:8080/mcp"
    transport: http-post
    tags: ["kubernetes", "staging"]
```

## 2. Run it

**Stdio mode** (embeds directly into a Claude-style client):

```bash
cargo run -- --config federation.yaml
```

**HTTP mode** (single shared endpoint for many clients):

```bash
cargo run -- --http --config federation.yaml
```

**Docker**:

```bash
docker run --rm -p 8080:8080 \
  -v $(pwd)/federation.yaml:/etc/mcp-federation/federation.yaml \
  ghcr.io/alexconrey/mcp-federation:latest \
  --config /etc/mcp-federation/federation.yaml
```

## 3. Point Claude Code (or any MCP client) at it

For **HTTP mode**, add to your Claude Code `~/.claude.json`:

```json
{
  "mcpServers": {
    "federation": {
      "type": "http",
      "url": "http://localhost:8080/mcp"
    }
  }
}
```

For **stdio mode**:

```json
{
  "mcpServers": {
    "federation": {
      "command": "mcp-federation",
      "args": ["--config", "/etc/mcp-federation/federation.yaml"]
    }
  }
}
```

## 4. Verify

```bash
curl -s http://localhost:8080/healthz            # -> "ok"
curl -s http://localhost:8080/status | less      # HTML status page
curl -s http://localhost:8080/metrics | head     # Prometheus metrics
open http://localhost:8080/swagger-ui            # OpenAPI docs

# Dry-run: dump the aggregated tool list and exit
cargo run -- --config federation.yaml --list-tools
```
