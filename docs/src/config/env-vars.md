# Environment Variables

| Variable                          | Equivalent flag        | Purpose                                                     |
|-----------------------------------|------------------------|-------------------------------------------------------------|
| `MCP_FEDERATION_CONFIG`           | `--config`             | Path to config file.                                        |
| `MCP_FEDERATION_LISTEN`           | `--listen`             | HTTP listen address override.                               |
| `MCP_FEDERATION_AUTH_TOKEN`       | `--auth-token`         | Bearer token for client auth.                               |
| `MCP_FEDERATION_AUTH_TOKEN_FILE`  | `--auth-token-file`    | Path to bearer token file (rotation-friendly).              |
| `TLS_CERT`                        | `--tls-cert`           | Path to TLS cert PEM (with `TLS_KEY` enables HTTPS).        |
| `TLS_KEY`                         | `--tls-key`            | Path to TLS key PEM.                                        |
| `LOG_FORMAT`                      | `--log-format`         | `text` (default) or `json`.                                 |
| `RUST_LOG`                        | --                     | Standard `tracing-subscriber` env filter, e.g. `debug`.     |

## Config file interpolation

Any `${ENV_VAR}` in the config file itself is interpolated too, so you can
push leaf auth tokens in via the environment:

```yaml
servers:
  - alias: "prod-k8s"
    url: "http://mcp-k8s:8080/mcp"
    auth_token: "${PROD_K8S_TOKEN}"
```
