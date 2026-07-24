# CLI Reference

```
mcp-federation [FLAGS]
```

| Flag                          | Env                                | Description                                                                                                                    |
|-------------------------------|------------------------------------|--------------------------------------------------------------------------------------------------------------------------------|
| `--config <PATH>`             | `MCP_FEDERATION_CONFIG`            | Config file path. Defaults to `federation.yaml`.                                                                               |
| `--http`                      | --                                 | Run in HTTP server mode. Without this flag the process runs in stdio mode.                                                     |
| `--listen <ADDR>`             | `MCP_FEDERATION_LISTEN`            | Override the `federation.listen` from config.                                                                                  |
| `--auth-token <TOKEN>`        | `MCP_FEDERATION_AUTH_TOKEN`        | Override the config file's `auth_token`.                                                                                       |
| `--auth-token-file <PATH>`    | `MCP_FEDERATION_AUTH_TOKEN_FILE`   | Path to file containing the bearer token. Re-read per request. Takes effect only when `--auth-token` is unset.                 |
| `--tls-cert <PATH>`           | `TLS_CERT`                         | TLS certificate PEM. Combined with `--tls-key` switches HTTP mode to HTTPS.                                                    |
| `--tls-key <PATH>`            | `TLS_KEY`                          | TLS private key PEM.                                                                                                           |
| `--log-format <text\|json>`   | `LOG_FORMAT`                       | Log output format. `json` emits structured logs for shipping to Loki/CloudWatch.                                               |
| `--list-tools`                | --                                 | Load config, initialize leaves, print the full aggregated tool table, and exit. Handy for smoke testing.                       |
| `--validate`                  | --                                 | Parse and validate the config file, then exit `0` on success (`1` on error). Does NOT start the server or connect to leaves.   |
