# Configuration

The config file is YAML. Every string value supports `${ENV_VAR}` interpolation
against the process environment (unset vars expand to empty strings).

## Minimal example

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

## Environment variable interpolation

Any `${ENV_VAR}` in the config file itself is interpolated, so you can
push leaf auth tokens in via the environment:

```yaml
servers:
  - alias: "prod-k8s"
    url: "http://mcp-k8s:8080/mcp"
    auth_token: "${PROD_K8S_TOKEN}"
```

See the [Configuration Reference](../config/federation.md) section for the
full schema.
