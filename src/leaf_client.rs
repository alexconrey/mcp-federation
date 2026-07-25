use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;

use crate::config::{ConnectionPoolConfig, ServerConfig, TransportType};
use crate::error::FederationError;
use crate::notifications::NotificationBroker;

pub struct LeafClient {
    alias: String,
    url: String,
    transport: TransportType,
    http: reqwest::Client,
    auth_token: Option<String>,
    auth_token_file: Option<PathBuf>,
    session_id: tokio::sync::RwLock<Option<String>>,
    next_id: AtomicU64,
}

impl LeafClient {
    pub fn new(config: &ServerConfig, pool_config: &ConnectionPoolConfig) -> Self {
        let timeout = config.timeout_seconds.unwrap_or(30);
        let mut builder = reqwest::Client::builder()
            .timeout(Duration::from_secs(timeout))
            .connect_timeout(Duration::from_secs(config.health_check.timeout_seconds))
            .pool_max_idle_per_host(pool_config.max_idle_per_host)
            .pool_idle_timeout(Duration::from_secs(pool_config.idle_timeout_seconds));

        if !config.tls.verify {
            builder = builder.danger_accept_invalid_certs(true);
        }

        if let Some(ref ca_path) = config.tls.ca_cert_path {
            match std::fs::read(ca_path) {
                Ok(bytes) => match reqwest::Certificate::from_pem(&bytes) {
                    Ok(cert) => {
                        builder = builder.add_root_certificate(cert);
                    }
                    Err(e) => {
                        tracing::warn!(
                            alias = %config.alias,
                            path = %ca_path.display(),
                            error = %e,
                            "failed to parse CA cert PEM; continuing without custom CA"
                        );
                    }
                },
                Err(e) => {
                    tracing::warn!(
                        alias = %config.alias,
                        path = %ca_path.display(),
                        error = %e,
                        "failed to read CA cert file; continuing without custom CA"
                    );
                }
            }
        }

        let http = builder.build().expect("failed to build HTTP client");

        Self {
            alias: config.alias.clone(),
            url: config.url.clone(),
            transport: config.transport.clone(),
            http,
            auth_token: config.auth_token.clone(),
            auth_token_file: config.auth_token_file.clone(),
            session_id: tokio::sync::RwLock::new(None),
            next_id: AtomicU64::new(1),
        }
    }

    fn resolve_auth_token(&self) -> Option<String> {
        if let Some(ref token) = self.auth_token {
            return Some(token.clone());
        }
        if let Some(ref path) = self.auth_token_file {
            match std::fs::read_to_string(path) {
                Ok(contents) => return Some(contents.trim().to_string()),
                Err(e) => {
                    tracing::warn!(
                        alias = %self.alias,
                        path = %path.display(),
                        error = %e,
                        "failed to read auth_token_file"
                    );
                }
            }
        }
        None
    }

    pub fn alias(&self) -> &str {
        &self.alias
    }

    fn next_request_id(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }

    pub async fn initialize(&self) -> Result<serde_json::Value, FederationError> {
        // Clear any stale session id first — initialize always starts fresh.
        if self.transport == TransportType::StreamableHttp {
            *self.session_id.write().await = None;
        }

        let request = serde_json::json!({
            "jsonrpc": "2.0",
            "id": self.next_request_id(),
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-11-25",
                "capabilities": {},
                "clientInfo": {
                    "name": "mcp-federation",
                    "version": env!("CARGO_PKG_VERSION")
                }
            }
        });

        // Use send_once (no retry) — a fresh initialize cannot recover from
        // 400/404 by re-initializing again.
        let (status, response) = self.send_once(&request).await?;
        if !status.is_success() {
            return Err(FederationError::LeafError {
                alias: self.alias.clone(),
                message: format!("initialize returned HTTP {status}"),
            });
        }

        // Send initialized notification (best-effort)
        let notification = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized",
            "params": {}
        });
        let _ = self.send_once(&notification).await;

        response
            .get("result")
            .cloned()
            .ok_or_else(|| FederationError::LeafError {
                alias: self.alias.clone(),
                message: "initialize response missing 'result'".to_string(),
            })
    }

    pub async fn list_tools(&self) -> Result<Vec<serde_json::Value>, FederationError> {
        let request = serde_json::json!({
            "jsonrpc": "2.0",
            "id": self.next_request_id(),
            "method": "tools/list",
            "params": {}
        });

        let response = self.send_request(&request).await?;

        let tools = response
            .get("result")
            .and_then(|r| r.get("tools"))
            .and_then(|t| t.as_array())
            .cloned()
            .unwrap_or_default();

        Ok(tools)
    }

    pub async fn call_tool(
        &self,
        tool_name: &str,
        arguments: &serde_json::Value,
    ) -> Result<serde_json::Value, FederationError> {
        let request = serde_json::json!({
            "jsonrpc": "2.0",
            "id": self.next_request_id(),
            "method": "tools/call",
            "params": {
                "name": tool_name,
                "arguments": arguments
            }
        });

        let response = self.send_request(&request).await?;

        if let Some(error) = response.get("error") {
            return Err(FederationError::LeafError {
                alias: self.alias.clone(),
                message: error
                    .get("message")
                    .and_then(|m| m.as_str())
                    .unwrap_or("unknown error")
                    .to_string(),
            });
        }

        response
            .get("result")
            .cloned()
            .ok_or_else(|| FederationError::LeafError {
                alias: self.alias.clone(),
                message: "tool call response missing 'result'".to_string(),
            })
    }

    pub async fn list_resources(&self) -> Result<Vec<serde_json::Value>, FederationError> {
        let request = serde_json::json!({
            "jsonrpc": "2.0",
            "id": self.next_request_id(),
            "method": "resources/list",
            "params": {}
        });

        let response = self.send_request(&request).await?;

        let resources = response
            .get("result")
            .and_then(|r| r.get("resources"))
            .and_then(|t| t.as_array())
            .cloned()
            .unwrap_or_default();

        Ok(resources)
    }

    pub async fn list_prompts(&self) -> Result<Vec<serde_json::Value>, FederationError> {
        let request = serde_json::json!({
            "jsonrpc": "2.0",
            "id": self.next_request_id(),
            "method": "prompts/list",
            "params": {}
        });

        let response = self.send_request(&request).await?;

        let prompts = response
            .get("result")
            .and_then(|r| r.get("prompts"))
            .and_then(|t| t.as_array())
            .cloned()
            .unwrap_or_default();

        Ok(prompts)
    }

    pub async fn get_prompt(
        &self,
        name: &str,
        arguments: &serde_json::Value,
    ) -> Result<serde_json::Value, FederationError> {
        let request = serde_json::json!({
            "jsonrpc": "2.0",
            "id": self.next_request_id(),
            "method": "prompts/get",
            "params": {
                "name": name,
                "arguments": arguments
            }
        });

        let response = self.send_request(&request).await?;

        if let Some(error) = response.get("error") {
            return Err(FederationError::LeafError {
                alias: self.alias.clone(),
                message: error
                    .get("message")
                    .and_then(|m| m.as_str())
                    .unwrap_or("unknown error")
                    .to_string(),
            });
        }

        response
            .get("result")
            .cloned()
            .ok_or_else(|| FederationError::LeafError {
                alias: self.alias.clone(),
                message: "prompts/get response missing 'result'".to_string(),
            })
    }

    pub async fn read_resource(&self, uri: &str) -> Result<serde_json::Value, FederationError> {
        let request = serde_json::json!({
            "jsonrpc": "2.0",
            "id": self.next_request_id(),
            "method": "resources/read",
            "params": {
                "uri": uri
            }
        });

        let response = self.send_request(&request).await?;

        if let Some(error) = response.get("error") {
            return Err(FederationError::LeafError {
                alias: self.alias.clone(),
                message: error
                    .get("message")
                    .and_then(|m| m.as_str())
                    .unwrap_or("unknown error")
                    .to_string(),
            });
        }

        response
            .get("result")
            .cloned()
            .ok_or_else(|| FederationError::LeafError {
                alias: self.alias.clone(),
                message: "resources/read response missing 'result'".to_string(),
            })
    }

    /// Open a long-lived SSE connection to the leaf's `GET {url}` endpoint and
    /// forward each `notifications/*` payload to `broker`. Every notification
    /// gets a `_leaf: <alias>` marker on its `params` object so downstream
    /// subscribers can identify the source.
    ///
    /// The returned `JoinHandle` runs until the stream closes (leaf disconnect,
    /// error) — callers may re-invoke to restart. Only meaningful for leaves
    /// using `transport: streamable-http`; other transports return an error
    /// without opening the connection.
    pub async fn listen_notifications(
        self: Arc<Self>,
        broker: Arc<NotificationBroker>,
    ) -> Result<tokio::task::JoinHandle<()>, FederationError> {
        if self.transport != TransportType::StreamableHttp {
            return Err(FederationError::LeafError {
                alias: self.alias.clone(),
                message: "listen_notifications requires streamable-http transport".to_string(),
            });
        }

        let handle = tokio::spawn(async move {
            let alias = self.alias.clone();
            if let Err(e) = self.run_notification_stream(broker).await {
                tracing::warn!(
                    alias = %alias,
                    error = %e,
                    "leaf notification stream terminated"
                );
            } else {
                tracing::info!(
                    alias = %alias,
                    "leaf notification stream closed cleanly"
                );
            }
        });

        Ok(handle)
    }

    async fn run_notification_stream(
        &self,
        broker: Arc<NotificationBroker>,
    ) -> Result<(), FederationError> {
        let mut req = self
            .http
            .get(&self.url)
            .header("Accept", "text/event-stream");

        if let Some(token) = self.resolve_auth_token() {
            req = req.bearer_auth(token);
        }

        if let Some(ref session_id) = *self.session_id.read().await {
            req = req.header("Mcp-Session-Id", session_id.as_str());
        }

        let response = req
            .send()
            .await
            .map_err(|e| FederationError::LeafUnreachable {
                alias: self.alias.clone(),
                source: e,
            })?;

        let status = response.status();
        if !status.is_success() {
            return Err(FederationError::LeafError {
                alias: self.alias.clone(),
                message: format!("SSE GET returned HTTP {status}"),
            });
        }

        let mut stream = response.bytes_stream();
        // SSE parser buffer: accumulate bytes until a complete event (delimited
        // by a blank line). Events span multiple `data:` lines per spec, so we
        // gather them into `data_buf` and flush on the blank-line boundary.
        let mut line_buf: Vec<u8> = Vec::new();
        let mut data_buf: String = String::new();

        while let Some(chunk) = stream.next().await {
            let bytes = chunk.map_err(|e| FederationError::LeafUnreachable {
                alias: self.alias.clone(),
                source: e,
            })?;

            for &b in bytes.iter() {
                if b == b'\n' {
                    // Complete line — strip an optional trailing CR from CRLF endings.
                    if line_buf.last() == Some(&b'\r') {
                        line_buf.pop();
                    }
                    let line = std::str::from_utf8(&line_buf).unwrap_or("");
                    self.handle_sse_line(line, &mut data_buf, &broker).await;
                    line_buf.clear();
                } else {
                    line_buf.push(b);
                }
            }
        }

        // Stream ended — flush any trailing partial event so a final
        // notification without a blank-line terminator isn't silently dropped.
        if !line_buf.is_empty() {
            let line = std::str::from_utf8(&line_buf).unwrap_or("").to_string();
            self.handle_sse_line(&line, &mut data_buf, &broker).await;
        }
        if !data_buf.is_empty() {
            self.dispatch_event(&data_buf, &broker).await;
        }

        Ok(())
    }

    async fn handle_sse_line(
        &self,
        line: &str,
        data_buf: &mut String,
        broker: &Arc<NotificationBroker>,
    ) {
        if line.is_empty() {
            // Event boundary — flush accumulated `data:` payload if any.
            if !data_buf.is_empty() {
                self.dispatch_event(data_buf, broker).await;
                data_buf.clear();
            }
            return;
        }
        if let Some(rest) = line.strip_prefix(':') {
            // Comment line — ignore per SSE spec.
            let _ = rest;
            return;
        }
        if let Some(payload) = line.strip_prefix("data:") {
            // Per SSE spec, exactly one leading space (if present) is stripped.
            let payload = payload.strip_prefix(' ').unwrap_or(payload);
            if !data_buf.is_empty() {
                data_buf.push('\n');
            }
            data_buf.push_str(payload);
        }
        // Silently ignore other field types (event:, id:, retry:) — we don't
        // consume them.
    }

    async fn dispatch_event(&self, payload: &str, broker: &Arc<NotificationBroker>) {
        let mut value: serde_json::Value = match serde_json::from_str(payload) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(
                    alias = %self.alias,
                    error = %e,
                    "failed to parse SSE event payload as JSON"
                );
                return;
            }
        };

        // Annotate the source leaf so downstream subscribers can tell payloads
        // apart. Insert `_leaf` into `params` (creating params as an object if
        // absent or non-object).
        if let Some(obj) = value.as_object_mut() {
            let params = obj
                .entry("params".to_string())
                .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
            if !params.is_object() {
                *params = serde_json::Value::Object(serde_json::Map::new());
            }
            if let Some(params_obj) = params.as_object_mut() {
                params_obj.insert(
                    "_leaf".to_string(),
                    serde_json::Value::String(self.alias.clone()),
                );
            }
        }

        broker.publish(value).await;
    }

    pub async fn health_check(&self) -> Result<(), FederationError> {
        let url = self.url.trim_end_matches("/mcp").trim_end_matches('/');
        let health_url = format!("{url}/healthz");

        self.http
            .get(&health_url)
            .send()
            .await
            .map_err(|e| FederationError::LeafUnreachable {
                alias: self.alias.clone(),
                source: e,
            })?
            .error_for_status()
            .map_err(|e| FederationError::LeafUnreachable {
                alias: self.alias.clone(),
                source: e,
            })?;

        Ok(())
    }

    async fn send_request(
        &self,
        body: &serde_json::Value,
    ) -> Result<serde_json::Value, FederationError> {
        let (status, value) = self.send_once(body).await?;

        // Streamable HTTP retry semantics: on 404 the session expired; on 400
        // with no session id present, we may need to initialize first. In both
        // cases we re-initialize once and retry the original request.
        if self.transport == TransportType::StreamableHttp {
            let needs_retry = status == reqwest::StatusCode::NOT_FOUND
                || (status == reqwest::StatusCode::BAD_REQUEST
                    && self.session_id.read().await.is_none());

            if needs_retry {
                tracing::info!(
                    alias = %self.alias,
                    status = %status,
                    "streamable-http session invalid; re-initializing and retrying"
                );
                self.initialize().await?;
                let (retry_status, retry_value) = self.send_once(body).await?;
                if !retry_status.is_success() {
                    return Err(FederationError::LeafError {
                        alias: self.alias.clone(),
                        message: format!("leaf returned HTTP {retry_status} after session re-init"),
                    });
                }
                return Ok(retry_value);
            }
        }

        if !status.is_success() {
            return Err(FederationError::LeafError {
                alias: self.alias.clone(),
                message: format!("leaf returned HTTP {status}"),
            });
        }

        Ok(value)
    }

    /// Send a single request and return the HTTP status alongside the parsed
    /// JSON body. Does NOT retry. Captures the `Mcp-Session-Id` response header
    /// into `self.session_id` for Streamable HTTP transport.
    async fn send_once(
        &self,
        body: &serde_json::Value,
    ) -> Result<(reqwest::StatusCode, serde_json::Value), FederationError> {
        let mut req = self.http.post(&self.url).json(body);

        if let Some(token) = self.resolve_auth_token() {
            req = req.bearer_auth(token);
        }

        if self.transport == TransportType::StreamableHttp {
            // Per the MCP Streamable HTTP spec, POST clients advertise both
            // JSON and SSE so the leaf can pick — some leaves stream even for
            // single-response requests.
            req = req.header(
                reqwest::header::ACCEPT,
                "application/json, text/event-stream",
            );
            if let Some(ref session_id) = *self.session_id.read().await {
                req = req.header("Mcp-Session-Id", session_id.as_str());
            }
        }

        let response = req
            .send()
            .await
            .map_err(|e| FederationError::LeafUnreachable {
                alias: self.alias.clone(),
                source: e,
            })?;

        let status = response.status();

        if self.transport == TransportType::StreamableHttp {
            if let Some(session_id) = response
                .headers()
                .get("mcp-session-id")
                .and_then(|v| v.to_str().ok())
                .map(String::from)
            {
                *self.session_id.write().await = Some(session_id);
            }
        }

        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();

        let bytes = response
            .bytes()
            .await
            .map_err(|e| FederationError::LeafUnreachable {
                alias: self.alias.clone(),
                source: e,
            })?;

        let value = if content_type.starts_with("text/event-stream") {
            // Streamable HTTP leaf answered inline with an SSE-framed JSON-RPC
            // response. Collect `data:` payloads across the body and parse the
            // concatenation as JSON. On parse failure we degrade to Null so
            // callers can still inspect the status code.
            let body_str = std::str::from_utf8(&bytes).unwrap_or("");
            parse_sse_body(body_str).unwrap_or(serde_json::Value::Null)
        } else {
            // Try to parse a JSON body. Some error responses (400/404) may have
            // plain-text bodies; in that case return an empty JSON value so callers
            // can still consult the status code.
            serde_json::from_slice::<serde_json::Value>(&bytes).unwrap_or(serde_json::Value::Null)
        };
        Ok((status, value))
    }
}

/// Parse an SSE-framed body into a single JSON value. Collects the `data:`
/// lines (per SSE spec, one leading space is stripped from each payload),
/// joins them with `\n`, and parses the result as JSON. Returns `None` if
/// there are no `data:` lines or the joined payload is not valid JSON.
///
/// This is the shape MCP leaves use when they answer a POST /mcp with
/// `Content-Type: text/event-stream` and a single event carrying the JSON-RPC
/// response.
pub(crate) fn parse_sse_body(body: &str) -> Option<serde_json::Value> {
    let mut data = String::new();
    for raw in body.lines() {
        // Strip an optional trailing '\r' from CRLF-terminated lines.
        let line = raw.strip_suffix('\r').unwrap_or(raw);
        if line.is_empty() || line.starts_with(':') {
            continue;
        }
        if let Some(rest) = line.strip_prefix("data:") {
            let payload = rest.strip_prefix(' ').unwrap_or(rest);
            if !data.is_empty() {
                data.push('\n');
            }
            data.push_str(payload);
        }
    }
    if data.is_empty() {
        return None;
    }
    serde_json::from_str(&data).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_helpers::make_server_config;

    #[test]
    fn next_request_id_increments_monotonically() {
        let pool = ConnectionPoolConfig::default();
        let cfg = make_server_config("t", "http://t:8080/mcp");
        let client = LeafClient::new(&cfg, &pool);
        assert_eq!(client.next_request_id(), 1);
        assert_eq!(client.next_request_id(), 2);
        assert_eq!(client.next_request_id(), 3);
    }

    #[test]
    fn per_leaf_timeout_applied() {
        // Just ensure client construction accepts the override without panicking.
        let pool = ConnectionPoolConfig::default();
        let mut cfg = make_server_config("t", "http://t:8080/mcp");
        cfg.timeout_seconds = Some(5);
        let _ = LeafClient::new(&cfg, &pool);
    }

    #[test]
    fn parse_sse_body_single_data_line() {
        let body = "data: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"ok\":true}}\n\n";
        let value = parse_sse_body(body).expect("expected valid JSON");
        assert_eq!(value["id"], 1);
        assert_eq!(value["result"]["ok"], true);
    }

    #[test]
    fn parse_sse_body_multiline_data() {
        // Per SSE spec, multiple `data:` lines within one event are joined
        // with a literal newline before being handed to the consumer.
        let body = "data: {\"jsonrpc\":\"2.0\",\n\
                    data: \"id\":2,\n\
                    data: \"result\":42}\n\n";
        let value = parse_sse_body(body).expect("expected valid JSON");
        assert_eq!(value["id"], 2);
        assert_eq!(value["result"], 42);
    }

    #[test]
    fn parse_sse_body_ignores_comments_and_other_fields() {
        let body = ": keepalive\n\
                    event: message\n\
                    id: abc\n\
                    data: {\"result\":\"ok\"}\n\n";
        let value = parse_sse_body(body).expect("expected valid JSON");
        assert_eq!(value["result"], "ok");
    }

    #[test]
    fn parse_sse_body_handles_no_leading_space_after_colon() {
        // `data:foo` with no space is also valid per the spec.
        let body = "data:{\"result\":1}\n\n";
        let value = parse_sse_body(body).expect("expected valid JSON");
        assert_eq!(value["result"], 1);
    }

    #[test]
    fn parse_sse_body_empty_returns_none() {
        assert!(parse_sse_body("").is_none());
        assert!(parse_sse_body(":ping\n\n").is_none()); // only a comment
    }

    #[test]
    fn parse_sse_body_invalid_json_returns_none() {
        assert!(parse_sse_body("data: not-json\n\n").is_none());
    }

    #[test]
    fn parse_sse_body_handles_crlf() {
        let body = "data: {\"ok\":true}\r\n\r\n";
        let value = parse_sse_body(body).expect("expected valid JSON");
        assert_eq!(value["ok"], true);
    }
}
