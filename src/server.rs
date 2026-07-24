use std::convert::Infallible;
use std::sync::Arc;
use std::time::Instant;

use axum::extract::State;
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::sse::{Event, Sse};
use axum::response::IntoResponse;
use axum::Json;
use futures::stream::{self, StreamExt};
use metrics::{counter, histogram};
use tokio_stream::wrappers::ReceiverStream;
use tracing::Instrument;
use uuid::Uuid;

use crate::aggregator::{parse_namespaced_name, Aggregator};
use crate::mcp::{error_response, method_not_found, success_response, JsonRpcRequest};
use crate::notifications::NotificationBroker;
use crate::rate_limit::RateLimiter;
use crate::rbac::RequestContext;
use crate::registry::{LeafHealth, Registry};
use crate::router::Router;
use crate::session::SessionManager;

pub const MCP_SESSION_HEADER: &str = "mcp-session-id";

pub struct FederationState {
    pub aggregator: Aggregator,
    pub router: Router,
    pub registry: Arc<Registry>,
    pub sessions: Arc<SessionManager>,
    /// Allowed origins for `/mcp` requests (DNS rebinding protection).
    /// Empty means "allow all".
    pub allowed_origins: Vec<String>,
    /// Per-client token-bucket rate limiter. When disabled, `check` is a
    /// no-op and the limiter is effectively transparent.
    pub rate_limiter: Arc<RateLimiter>,
    /// Fan-out for server-initiated notifications. Client `GET /mcp` SSE
    /// streams subscribe here; leaf-side listeners publish here.
    pub notifications: Arc<NotificationBroker>,
}

#[utoipa::path(
    post,
    path = "/mcp",
    tag = "mcp",
    summary = "MCP Streamable HTTP endpoint",
    description = "Handles MCP protocol methods over the Streamable HTTP transport. \
                   `initialize` returns an `Mcp-Session-Id` response header; subsequent \
                   requests must echo it back. Sessions expire after `session_ttl_seconds` \
                   of inactivity. Backward-compatible with plain HTTP POST clients that \
                   do not use session headers.",
    request_body(content = JsonRpcRequest, content_type = "application/json"),
    responses(
        (status = 200, description = "JSON-RPC response", body = crate::mcp::JsonRpcResponse),
        (status = 400, description = "Missing Mcp-Session-Id header on non-initialize request"),
        (status = 403, description = "Origin header not in allowed_origins"),
        (status = 404, description = "Session expired or unknown"),
    )
)]
pub async fn handle_mcp(
    State(state): State<Arc<FederationState>>,
    headers: HeaderMap,
    ctx: Option<axum::Extension<RequestContext>>,
    Json(request): Json<JsonRpcRequest>,
) -> axum::response::Response {
    if let Err(resp) = check_origin(&state, &headers) {
        return resp;
    }

    let is_initialize = request.method == "initialize";
    let incoming_session = extract_session_id(&headers);

    // Non-initialize requests: enforce session semantics ONLY when the client
    // uses session headers or when a session has already been created for this
    // connection. Plain HTTP POST clients (no session header, no prior session)
    // continue to work as before.
    if !is_initialize {
        if let Some(ref sid) = incoming_session {
            if !state.sessions.validate_session(sid).await {
                return (StatusCode::NOT_FOUND, "session expired or unknown").into_response();
            }
            state.sessions.touch_session(sid).await;
        }
        // Notifications are best-effort even without a session header — no 400.
        // Requests with an id but no session header are treated as plain HTTP
        // POST for backward compatibility (see CLAUDE spec item on backward
        // compat with stdio and simple HTTP clients).
    }

    let request_ctx = ctx.map(|axum::Extension(c)| c).unwrap_or_default();

    // Rate limit BEFORE dispatch. The bucket is keyed by client id so distinct
    // callers cannot starve each other. Anonymous callers share one bucket.
    let rl_key = request_ctx.client_label();
    if !state.rate_limiter.check(rl_key) {
        let resp = error_response(&request, -32000, "rate limit exceeded");
        return (StatusCode::TOO_MANY_REQUESTS, Json(resp)).into_response();
    }

    let response = dispatch(&state, request, &incoming_session, &request_ctx).await;

    // Optional session-id header for `initialize` responses. Kept in a locally
    // scoped tuple so both the JSON and SSE branches below can attach it.
    let session_header: Option<(&'static str, String)> = if is_initialize
        && response.error.is_none()
    {
        let client_info = extract_client_info(&response);
        let session_id = state.sessions.create_session(client_info).await;
        Some((MCP_SESSION_HEADER, session_id))
    } else {
        None
    };

    if accepts_sse(&headers) {
        // Streamable HTTP SSE response: wrap the single JSON-RPC reply as one
        // `data:` event. Content-Type is set automatically by `Sse`.
        let json = serde_json::to_string(&response).unwrap_or_default();
        let event_stream = stream::once(async move {
            Ok::<_, Infallible>(Event::default().data(json))
        });
        let mut resp = Sse::new(event_stream).into_response();
        if let Some((name, value)) = session_header {
            match value.parse() {
                Ok(v) => {
                    resp.headers_mut().insert(name, v);
                }
                Err(e) => {
                    tracing::error!(error = %e, "generated session id is not a valid header value");
                }
            }
        }
        return resp;
    }

    let mut response_headers = HeaderMap::new();
    response_headers.insert(
        header::CONTENT_TYPE,
        "application/json".parse().expect("static content-type"),
    );

    if let Some((name, value)) = session_header {
        match value.parse() {
            Ok(v) => {
                response_headers.insert(name, v);
            }
            Err(e) => {
                tracing::error!(error = %e, "generated session id is not a valid header value");
            }
        }
    }

    (StatusCode::OK, response_headers, Json(response)).into_response()
}

#[utoipa::path(
    delete,
    path = "/mcp",
    tag = "mcp",
    summary = "Terminate an MCP session",
    description = "Explicitly terminate the session identified by the `Mcp-Session-Id` \
                   request header. Returns 200 if the session was removed, 400 if the \
                   header is missing, or 404 if the session was already gone.",
    responses(
        (status = 200, description = "Session terminated"),
        (status = 400, description = "Missing Mcp-Session-Id header"),
        (status = 403, description = "Origin header not in allowed_origins"),
        (status = 404, description = "Session expired or unknown"),
    )
)]
pub async fn delete_mcp(
    State(state): State<Arc<FederationState>>,
    headers: HeaderMap,
) -> axum::response::Response {
    if let Err(resp) = check_origin(&state, &headers) {
        return resp;
    }
    let Some(sid) = extract_session_id(&headers) else {
        return (StatusCode::BAD_REQUEST, "missing Mcp-Session-Id header").into_response();
    };
    if state.sessions.terminate_session(&sid).await {
        StatusCode::OK.into_response()
    } else {
        (StatusCode::NOT_FOUND, "session expired or unknown").into_response()
    }
}

#[utoipa::path(
    get,
    path = "/mcp",
    tag = "mcp",
    summary = "Server-initiated notification stream (SSE)",
    description = "Opens a Server-Sent Events stream carrying server-initiated \
                   JSON-RPC notifications for the caller's session. Requires a \
                   valid `Mcp-Session-Id` header. Each SSE `data:` line contains \
                   one JSON-encoded notification.",
    responses(
        (status = 200, description = "SSE stream established", content_type = "text/event-stream"),
        (status = 400, description = "Missing Mcp-Session-Id header"),
        (status = 403, description = "Origin header not in allowed_origins"),
        (status = 404, description = "Session expired or unknown"),
    )
)]
pub async fn get_mcp(
    State(state): State<Arc<FederationState>>,
    headers: HeaderMap,
) -> axum::response::Response {
    if let Err(resp) = check_origin(&state, &headers) {
        return resp;
    }
    let Some(sid) = extract_session_id(&headers) else {
        return (StatusCode::BAD_REQUEST, "missing Mcp-Session-Id header").into_response();
    };
    if !state.sessions.validate_session(&sid).await {
        return (StatusCode::NOT_FOUND, "session expired or unknown").into_response();
    }
    state.sessions.touch_session(&sid).await;

    let broker = state.notifications.clone();
    let (sub_id, rx) = broker.subscribe().await;

    // Wrapper drops → unsubscribe. Wrapped in a stream so axum drives it.
    let guard = SubscriptionGuard {
        broker: broker.clone(),
        sub_id: sub_id.clone(),
    };
    let receiver = ReceiverStream::new(rx).map(move |notification| {
        let json = serde_json::to_string(&notification).unwrap_or_default();
        Ok::<_, Infallible>(Event::default().data(json))
    });
    // Attach the guard to each yielded item so its lifetime is tied to the
    // stream. When the client disconnects the stream is dropped, the guard is
    // dropped, and Drop calls broker.unsubscribe via a spawned task.
    let guarded = GuardedStream {
        inner: receiver,
        _guard: guard,
    };

    Sse::new(guarded).into_response()
}

/// RAII cleanup: on drop, spawn a task that removes the subscription from the
/// broker. `Drop` cannot be async, so the removal happens on the runtime.
struct SubscriptionGuard {
    broker: Arc<NotificationBroker>,
    sub_id: String,
}

impl Drop for SubscriptionGuard {
    fn drop(&mut self) {
        let broker = self.broker.clone();
        let sub_id = std::mem::take(&mut self.sub_id);
        // If the runtime is already gone (e.g. test teardown) the spawn will
        // fail silently; publish() will prune the dead channel on next send.
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                broker.unsubscribe(&sub_id).await;
            });
        }
    }
}

/// Pins a `SubscriptionGuard` to the SSE stream's lifetime. Field order
/// matters: `inner` is dropped before `_guard`, so the subscription is
/// released only after the stream is fully torn down.
struct GuardedStream<S> {
    inner: S,
    _guard: SubscriptionGuard,
}

impl<S> futures::Stream for GuardedStream<S>
where
    S: futures::Stream + Unpin,
{
    type Item = S::Item;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        std::pin::Pin::new(&mut self.inner).poll_next(cx)
    }
}

fn accepts_sse(headers: &HeaderMap) -> bool {
    headers
        .get(header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.split(',').any(|s| s.trim().starts_with("text/event-stream")))
        .unwrap_or(false)
}

fn extract_session_id(headers: &HeaderMap) -> Option<String> {
    headers
        .get(MCP_SESSION_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned)
}

fn extract_client_info(response: &crate::mcp::JsonRpcResponse) -> Option<serde_json::Value> {
    response
        .result
        .as_ref()
        .and_then(|r| r.get("clientInfo"))
        .cloned()
}

fn check_origin(
    state: &FederationState,
    headers: &HeaderMap,
) -> Result<(), axum::response::Response> {
    if state.allowed_origins.is_empty() {
        return Ok(());
    }
    let origin = headers
        .get(header::ORIGIN)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if origin.is_empty() || !state.allowed_origins.iter().any(|o| o == origin) {
        return Err((StatusCode::FORBIDDEN, "origin not allowed").into_response());
    }
    Ok(())
}

#[utoipa::path(
    get,
    path = "/healthz",
    tag = "health",
    summary = "Health check",
    description = "Returns 200 OK when the federation server is running.",
    responses(
        (status = 200, description = "Server is healthy", body = String, example = json!("ok")),
    )
)]
pub async fn healthz() -> &'static str {
    "ok"
}

pub async fn metrics_handler(
    axum::Extension(handle): axum::Extension<metrics_exporter_prometheus::PrometheusHandle>,
) -> String {
    handle.render()
}

pub async fn status_handler(
    State(state): State<Arc<FederationState>>,
) -> axum::response::Response {
    let html = render_status_html(&state.registry).await;
    (
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        html,
    )
        .into_response()
}

pub async fn render_status_html(registry: &Registry) -> String {
    let leaves = registry.all_leaves().await;
    let leaf_count = leaves.len();

    let mut rows = String::new();
    for leaf in &leaves {
        let health = *leaf.health.read().await;
        let (health_label, health_color) = match health {
            LeafHealth::Healthy => ("healthy", "#2e7d32"),
            LeafHealth::Unhealthy => ("unhealthy", "#c62828"),
            LeafHealth::Unknown => ("unknown", "#757575"),
        };
        let tool_count = leaf.cached_tools.read().await.len();
        let tags = if leaf.config.tags.is_empty() {
            String::from("-")
        } else {
            html_escape(&leaf.config.tags.join(", "))
        };
        let last_check = match *leaf.last_health_check.read().await {
            Some(instant) => format!("{}s ago", instant.elapsed().as_secs()),
            None => String::from("never"),
        };

        rows.push_str(&format!(
            "<tr>\
                <td>{alias}</td>\
                <td><code>{url}</code></td>\
                <td style=\"color:{health_color};font-weight:bold\">{health_label}</td>\
                <td>{tool_count}</td>\
                <td>{tags}</td>\
                <td>{last_check}</td>\
            </tr>",
            alias = html_escape(&leaf.config.alias),
            url = html_escape(&leaf.config.url),
        ));
    }

    format!(
        r#"<!DOCTYPE html>
<html lang="en"><head>
<meta charset="utf-8">
<title>mcp-federation status</title>
<style>
body {{ font-family: -apple-system, system-ui, sans-serif; margin: 2rem; color: #212121; }}
h1 {{ margin-bottom: 0.25rem; }}
.meta {{ color: #666; margin-bottom: 1.5rem; }}
table {{ border-collapse: collapse; width: 100%; }}
th, td {{ padding: 0.5rem 0.75rem; border-bottom: 1px solid #ddd; text-align: left; }}
th {{ background: #f5f5f5; }}
code {{ font-family: SFMono-Regular, Menlo, monospace; font-size: 0.9em; }}
</style>
</head><body>
<h1>mcp-federation</h1>
<div class="meta">version {version} &middot; {leaf_count} leaf server(s)</div>
<table>
<thead><tr>
<th>Alias</th><th>URL</th><th>Health</th><th>Tools</th><th>Tags</th><th>Last check</th>
</tr></thead>
<tbody>
{rows}
</tbody>
</table>
</body></html>"#,
        version = env!("CARGO_PKG_VERSION"),
    )
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

pub async fn dispatch(
    state: &FederationState,
    request: JsonRpcRequest,
    session_id: &Option<String>,
    ctx: &RequestContext,
) -> crate::mcp::JsonRpcResponse {
    let trace_id = Uuid::new_v4().to_string();
    let span = tracing::info_span!(
        "mcp_request",
        trace_id = %trace_id,
        method = %request.method,
        client = %ctx.client_label(),
        tool = tracing::field::Empty,
        alias = tracing::field::Empty,
    );

    async {
        counter!("federation_requests_total", "method" => request.method.clone()).increment(1);
        tracing::info!("processing request");

        match request.method.as_str() {
            "initialize" => {
                let resp = handle_initialize(&request);
                tracing::info!(
                    event = "session_init",
                    client = ctx.client_label(),
                    "audit: session initialize"
                );
                resp
            }
            "notifications/initialized" => {
                // Notification — no response needed per JSON-RPC spec,
                // but we return an empty result for stdio compatibility
                success_response(&request, serde_json::json!({}))
            }
            "tools/list" => handle_tools_list(state, &request, ctx).await,
            "tools/call" => handle_tools_call(state, &request, ctx).await,
            "resources/list" => handle_resources_list(state, &request).await,
            "resources/read" => handle_resources_read(state, &request).await,
            "prompts/list" => handle_prompts_list(state, &request).await,
            "prompts/get" => handle_prompts_get(state, &request).await,
            "logging/setLevel" => handle_logging_set_level(state, &request, session_id).await,
            "sampling/createMessage" => handle_sampling_create_message(&request),
            _ => method_not_found(&request),
        }
    }
    .instrument(span)
    .await
}

// The federation advertises `logging` and `sampling` because the handlers
// below give clients a spec-compliant response for each. `logging/setLevel`
// stores the requested level on the session so future per-session log
// filtering can consult it; `sampling/createMessage` returns a JSON-RPC error
// per method (advertising the capability at handshake time while rejecting
// specific requests is explicitly permitted by the MCP spec).
fn handle_initialize(request: &JsonRpcRequest) -> crate::mcp::JsonRpcResponse {
    success_response(
        request,
        serde_json::json!({
            "protocolVersion": "2025-11-25",
            "capabilities": {
                "tools": {},
                "resources": {},
                "prompts": {},
                "logging": {},
                "sampling": {}
            },
            "serverInfo": {
                "name": "mcp-federation",
                "version": env!("CARGO_PKG_VERSION")
            }
        }),
    )
}

const VALID_LOG_LEVELS: &[&str] = &[
    "debug", "info", "notice", "warning", "error", "critical", "alert", "emergency",
];

async fn handle_logging_set_level(
    state: &FederationState,
    request: &JsonRpcRequest,
    session_id: &Option<String>,
) -> crate::mcp::JsonRpcResponse {
    let level = match request.params.get("level").and_then(|v| v.as_str()) {
        Some(s) => s.to_string(),
        None => {
            return error_response(request, -32602, "missing required 'level' parameter");
        }
    };
    if !VALID_LOG_LEVELS.contains(&level.as_str()) {
        return error_response(
            request,
            -32602,
            &format!(
                "invalid log level '{level}'; expected one of: {}",
                VALID_LOG_LEVELS.join(", ")
            ),
        );
    }

    // Only stateful when the client presented a session. Sessionless callers
    // still get an OK — the level is simply not persisted.
    if let Some(sid) = session_id {
        state.sessions.set_log_level(sid, level.clone()).await;
    }

    tracing::info!(
        event = "logging_set_level",
        session = session_id.as_deref().unwrap_or("<none>"),
        level = %level,
        "audit: logging/setLevel"
    );

    success_response(request, serde_json::json!({}))
}

fn handle_sampling_create_message(request: &JsonRpcRequest) -> crate::mcp::JsonRpcResponse {
    // Federation does not host its own LLM; sampling is a client capability
    // the federation cannot proxy without knowing which leaf initiated the
    // request. Return -32601 (Method not found) per spec — clients may still
    // negotiate sampling directly with a leaf that supports it.
    error_response(
        request,
        -32601,
        "sampling not supported by this federation server — connect directly \
         to a leaf server that supports sampling",
    )
}

/// Return true iff `ctx` may see or invoke the given namespaced tool. Tools
/// that don't parse as `alias__tool` (should not happen after aggregation) are
/// allowed through so the caller sees the same error the router would raise.
fn tool_allowed(ctx: &RequestContext, namespaced: &str) -> bool {
    match parse_namespaced_name(namespaced) {
        Some((alias, _)) => ctx.allows_alias(alias),
        None => true,
    }
}

async fn handle_tools_list(
    state: &FederationState,
    request: &JsonRpcRequest,
    ctx: &RequestContext,
) -> crate::mcp::JsonRpcResponse {
    let mut tools = state.aggregator.aggregated_tools().await;
    if ctx.allowed_servers.is_some() {
        tools.retain(|t| {
            t.get("name")
                .and_then(|n| n.as_str())
                .map(|name| tool_allowed(ctx, name))
                .unwrap_or(true)
        });
    }
    tracing::info!(
        event = "tools_list",
        client = ctx.client_label(),
        tool_count = tools.len(),
        "audit: tools/list"
    );
    success_response(request, serde_json::json!({ "tools": tools }))
}

async fn handle_tools_call(
    state: &FederationState,
    request: &JsonRpcRequest,
    ctx: &RequestContext,
) -> crate::mcp::JsonRpcResponse {
    let params = &request.params;
    let tool_name = params["name"].as_str().unwrap_or("");
    let arguments = &params["arguments"];

    let alias = parse_namespaced_name(tool_name)
        .map(|(a, _)| a.to_string())
        .unwrap_or_else(|| "unknown".to_string());

    tracing::Span::current().record("tool", tool_name);
    tracing::Span::current().record("alias", alias.as_str());

    // RBAC: reject tools whose leaf alias is not in the client's allow-list.
    // Federation-native tools always pass. Invalid names are handed to the
    // router so it emits a consistent InvalidToolName error.
    if parse_namespaced_name(tool_name).is_some() && !ctx.allows_alias(&alias) {
        counter!(
            "federation_tool_call_errors_total",
            "tool" => tool_name.to_string(),
            "alias" => alias.clone(),
        )
        .increment(1);
        tracing::warn!(
            event = "tool_call",
            client = ctx.client_label(),
            alias = alias.as_str(),
            tool = tool_name,
            status = "forbidden",
            duration_ms = 0u64,
            "audit: tool call denied by RBAC"
        );
        return error_response(
            request,
            -32000,
            &format!("access denied: client is not permitted to use leaf '{alias}'"),
        );
    }

    let start = Instant::now();

    counter!(
        "federation_tool_calls_total",
        "tool" => tool_name.to_string(),
        "alias" => alias.clone(),
    )
    .increment(1);

    let result = state.router.route_tool_call(tool_name, arguments).await;
    let duration = start.elapsed().as_secs_f64();
    let duration_ms = (duration * 1000.0) as u64;

    histogram!(
        "federation_tool_call_duration_seconds",
        "tool" => tool_name.to_string(),
        "alias" => alias.clone(),
    )
    .record(duration);

    let status = if result.is_ok() { "success" } else { "error" };
    tracing::info!(
        event = "tool_call",
        client = ctx.client_label(),
        alias = alias.as_str(),
        tool = tool_name,
        status = status,
        duration_ms = duration_ms,
        "audit: tool call"
    );

    match result {
        Ok(value) => success_response(request, value),
        Err(e) => {
            counter!(
                "federation_tool_call_errors_total",
                "tool" => tool_name.to_string(),
                "alias" => alias.clone(),
            )
            .increment(1);
            error_response(request, -32000, &e.to_string())
        }
    }
}

async fn handle_resources_list(
    state: &FederationState,
    request: &JsonRpcRequest,
) -> crate::mcp::JsonRpcResponse {
    let resources = state.aggregator.aggregated_resources().await;
    success_response(request, serde_json::json!({ "resources": resources }))
}

async fn handle_resources_read(
    state: &FederationState,
    request: &JsonRpcRequest,
) -> crate::mcp::JsonRpcResponse {
    let uri = request.params["uri"].as_str().unwrap_or("");
    match state.router.route_resource_read(uri).await {
        Ok(value) => success_response(request, value),
        Err(e) => error_response(request, -32000, &e.to_string()),
    }
}

async fn handle_prompts_list(
    state: &FederationState,
    request: &JsonRpcRequest,
) -> crate::mcp::JsonRpcResponse {
    let prompts = state.aggregator.aggregated_prompts().await;
    success_response(request, serde_json::json!({ "prompts": prompts }))
}

async fn handle_prompts_get(
    state: &FederationState,
    request: &JsonRpcRequest,
) -> crate::mcp::JsonRpcResponse {
    let name = request.params["name"].as_str().unwrap_or("");
    let arguments = &request.params["arguments"];
    match state.router.route_prompt_get(name, arguments).await {
        Ok(value) => success_response(request, value),
        Err(e) => error_response(request, -32000, &e.to_string()),
    }
}

// ---------------------------------------------------------------------------
// Stdio transport
// ---------------------------------------------------------------------------

pub async fn run_stdio(state: Arc<FederationState>) {
    use std::io::BufRead;

    let stdin = std::io::stdin();
    let reader = stdin.lock();

    for line in reader.lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => break,
        };

        if line.trim().is_empty() {
            continue;
        }

        let request: JsonRpcRequest = match serde_json::from_str(&line) {
            Ok(r) => r,
            Err(e) => {
                let err = serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": null,
                    "error": { "code": -32700, "message": format!("Parse error: {e}") }
                });
                println!("{}", serde_json::to_string(&err).unwrap());
                continue;
            }
        };

        let response = dispatch(&state, request, &None, &RequestContext::anonymous()).await;
        println!("{}", serde_json::to_string(&response).unwrap());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::aggregator::Aggregator;
    use crate::router::Router;
    use crate::test_helpers::{make_registry_with_tools, make_tool};

    async fn make_state(
        entries: Vec<(&str, &str, Vec<serde_json::Value>)>,
    ) -> FederationState {
        let registry = make_registry_with_tools(entries).await;
        FederationState {
            aggregator: Aggregator::new(registry.clone()),
            router: Router::new(registry.clone()),
            registry,
            sessions: Arc::new(SessionManager::new(1800, None)),
            allowed_origins: vec![],
            rate_limiter: Arc::new(RateLimiter::new(
                crate::config::RateLimitConfig::default(),
            )),
            notifications: Arc::new(NotificationBroker::new()),
        }
    }

    fn make_request(method: &str, params: serde_json::Value) -> JsonRpcRequest {
        serde_json::from_value(serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": method,
            "params": params,
        }))
        .unwrap()
    }

    #[tokio::test]
    async fn dispatch_initialize() {
        let state = make_state(vec![]).await;
        let req = make_request("initialize", serde_json::json!({}));
        let resp = dispatch(&state, req, &None, &RequestContext::admin()).await;

        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["result"]["serverInfo"]["name"], "mcp-federation");
        assert!(json["result"]["capabilities"]["tools"].is_object());
        assert!(json.get("error").is_none());
    }

    #[tokio::test]
    async fn dispatch_notifications_initialized() {
        let state = make_state(vec![]).await;
        let req = make_request("notifications/initialized", serde_json::json!({}));
        let resp = dispatch(&state, req, &None, &RequestContext::admin()).await;

        let json = serde_json::to_value(&resp).unwrap();
        assert!(json.get("error").is_none());
    }

    #[tokio::test]
    async fn dispatch_tools_list_aggregates() {
        let state = make_state(vec![
            (
                "a",
                "http://a:8080/mcp",
                vec![make_tool("list_pods", "List pods")],
            ),
            (
                "b",
                "http://b:8080/mcp",
                vec![make_tool("get_pod", "Get a pod")],
            ),
        ])
        .await;

        let req = make_request("tools/list", serde_json::json!({}));
        let resp = dispatch(&state, req, &None, &RequestContext::admin()).await;

        let json = serde_json::to_value(&resp).unwrap();
        let tools = json["result"]["tools"].as_array().unwrap();

        let names: Vec<&str> = tools
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();

        // 2 leaf tools + 5 native tools = 7
        assert_eq!(tools.len(), 7);
        assert!(names.contains(&"a__list_pods"));
        assert!(names.contains(&"b__get_pod"));
        assert!(names.contains(&"federation__list_servers"));
    }

    #[tokio::test]
    async fn dispatch_tools_call_native_tool() {
        let state = make_state(vec![(
            "prod",
            "http://prod:8080/mcp",
            vec![make_tool("list_pods", "List pods")],
        )])
        .await;

        let req = make_request(
            "tools/call",
            serde_json::json!({
                "name": "federation__list_servers",
                "arguments": {}
            }),
        );
        let resp = dispatch(&state, req, &None, &RequestContext::admin()).await;

        let json = serde_json::to_value(&resp).unwrap();
        assert!(json.get("error").is_none());
        let text = json["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("prod"));
    }

    #[tokio::test]
    async fn dispatch_tools_call_invalid_name() {
        let state = make_state(vec![]).await;

        let req = make_request(
            "tools/call",
            serde_json::json!({
                "name": "no_separator_here",
                "arguments": {}
            }),
        );
        let resp = dispatch(&state, req, &None, &RequestContext::admin()).await;

        let json = serde_json::to_value(&resp).unwrap();
        assert!(json["error"]["code"].as_i64().is_some());
        assert!(json["error"]["message"]
            .as_str()
            .unwrap()
            .contains("invalid tool name"));
    }

    #[tokio::test]
    async fn dispatch_method_not_found() {
        let state = make_state(vec![]).await;
        let req = make_request("totally/bogus", serde_json::json!({}));
        let resp = dispatch(&state, req, &None, &RequestContext::admin()).await;

        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["error"]["code"], -32601);
        assert!(json["error"]["message"]
            .as_str()
            .unwrap()
            .contains("totally/bogus"));
    }

    #[tokio::test]
    async fn dispatch_resources_list() {
        let state = make_state(vec![]).await;
        let req = make_request("resources/list", serde_json::json!({}));
        let resp = dispatch(&state, req, &None, &RequestContext::admin()).await;

        let json = serde_json::to_value(&resp).unwrap();
        assert!(json["result"]["resources"].is_array());
        assert!(json.get("error").is_none());
    }

    #[tokio::test]
    async fn dispatch_prompts_list() {
        let state = make_state(vec![]).await;
        let req = make_request("prompts/list", serde_json::json!({}));
        let resp = dispatch(&state, req, &None, &RequestContext::admin()).await;

        let json = serde_json::to_value(&resp).unwrap();
        assert!(json["result"]["prompts"].is_array());
        assert!(json.get("error").is_none());
    }

    #[tokio::test]
    async fn status_html_contains_leaf_alias_and_tool_count() {
        let state = make_state(vec![
            (
                "alpha",
                "http://alpha:8080/mcp",
                vec![
                    make_tool("list_pods", "List pods"),
                    make_tool("get_pod", "Get a pod"),
                ],
            ),
            (
                "beta",
                "http://beta:8080/mcp",
                vec![make_tool("get_nodes", "Get nodes")],
            ),
        ])
        .await;

        let html = render_status_html(&state.registry).await;
        assert!(html.starts_with("<!DOCTYPE html>"));
        assert!(html.contains("mcp-federation"));
        assert!(html.contains("alpha"));
        assert!(html.contains("beta"));
        // alpha has 2 tools, beta has 1
        assert!(html.contains("<td>2</td>"));
        assert!(html.contains("<td>1</td>"));
        // Both leaves are marked healthy in make_registry_with_tools
        assert!(html.contains("healthy"));
    }

    #[tokio::test]
    async fn status_html_shows_unhealthy_leaf() {
        let state = make_state(vec![(
            "sick",
            "http://sick:8080/mcp",
            vec![],
        )])
        .await;
        state
            .registry
            .get_leaf("sick")
            .await
            .unwrap()
            .mark_unhealthy()
            .await;

        let html = render_status_html(&state.registry).await;
        assert!(html.contains("unhealthy"));
    }

    #[tokio::test]
    async fn tools_list_filters_by_rbac_allowed_servers() {
        let state = make_state(vec![
            (
                "prod",
                "http://prod:8080/mcp",
                vec![make_tool("list_pods", "List prod pods")],
            ),
            (
                "staging",
                "http://staging:8080/mcp",
                vec![make_tool("list_pods", "List staging pods")],
            ),
        ])
        .await;

        let ctx = RequestContext {
            client_id: Some("client-0".to_string()),
            allowed_servers: Some(vec!["prod".to_string()]),
        };
        let req = make_request("tools/list", serde_json::json!({}));
        let resp = dispatch(&state, req, &None, &ctx).await;

        let json = serde_json::to_value(&resp).unwrap();
        let names: Vec<&str> = json["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();

        assert!(names.contains(&"prod__list_pods"));
        assert!(!names.contains(&"staging__list_pods"));
        // Native federation tools remain visible to everyone.
        assert!(names.contains(&"federation__list_servers"));
    }

    #[tokio::test]
    async fn tools_list_wildcard_shows_everything() {
        let state = make_state(vec![
            (
                "prod",
                "http://prod:8080/mcp",
                vec![make_tool("list_pods", "List prod pods")],
            ),
            (
                "staging",
                "http://staging:8080/mcp",
                vec![make_tool("list_pods", "List staging pods")],
            ),
        ])
        .await;

        // Wildcard client — resolve() translates "*" to allowed_servers=None.
        let ctx = RequestContext {
            client_id: Some("wildcard".to_string()),
            allowed_servers: None,
        };
        let req = make_request("tools/list", serde_json::json!({}));
        let resp = dispatch(&state, req, &None, &ctx).await;

        let names: Vec<String> = serde_json::to_value(&resp).unwrap()["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap().to_string())
            .collect();

        assert!(names.contains(&"prod__list_pods".to_string()));
        assert!(names.contains(&"staging__list_pods".to_string()));
    }

    #[tokio::test]
    async fn tools_call_denied_when_alias_not_in_allowlist() {
        let state = make_state(vec![
            (
                "prod",
                "http://prod:8080/mcp",
                vec![make_tool("list_pods", "List prod pods")],
            ),
            (
                "staging",
                "http://staging:8080/mcp",
                vec![make_tool("list_pods", "List staging pods")],
            ),
        ])
        .await;

        let ctx = RequestContext {
            client_id: Some("client-0".to_string()),
            allowed_servers: Some(vec!["prod".to_string()]),
        };
        let req = make_request(
            "tools/call",
            serde_json::json!({
                "name": "staging__list_pods",
                "arguments": {}
            }),
        );
        let resp = dispatch(&state, req, &None, &ctx).await;

        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["error"]["code"], -32000);
        assert!(json["error"]["message"]
            .as_str()
            .unwrap()
            .contains("access denied"));
    }

    #[tokio::test]
    async fn tools_call_allowed_when_alias_in_allowlist() {
        // Only native `federation__` tools can be exercised end-to-end here
        // without a live leaf. Restrict to `federation` (implicit) and confirm
        // the native call still succeeds.
        let state = make_state(vec![(
            "prod",
            "http://prod:8080/mcp",
            vec![make_tool("list_pods", "List prod pods")],
        )])
        .await;

        let ctx = RequestContext {
            client_id: Some("scoped".to_string()),
            allowed_servers: Some(vec![]),
        };
        let req = make_request(
            "tools/call",
            serde_json::json!({
                "name": "federation__list_servers",
                "arguments": {}
            }),
        );
        let resp = dispatch(&state, req, &None, &ctx).await;

        let json = serde_json::to_value(&resp).unwrap();
        assert!(json.get("error").is_none());
    }

    #[tokio::test]
    async fn status_html_escapes_leaf_alias() {
        // Alias with a character that would corrupt the HTML if not escaped.
        // make_server_config accepts the raw string; validate() isn't run here.
        let state = make_state(vec![(
            "risky<script>",
            "http://x:8080/mcp",
            vec![],
        )])
        .await;

        let html = render_status_html(&state.registry).await;
        assert!(html.contains("risky&lt;script&gt;"));
        assert!(!html.contains("risky<script>"));
    }

    // -------------------------------------------------------------------
    // Streamable HTTP handler tests (HTTP-level, exercising handle_mcp,
    // delete_mcp, get_mcp end-to-end via tower::ServiceExt).
    // -------------------------------------------------------------------

    use axum::body::Body;
    use axum::http::{Method, Request as HttpRequest};
    use tower::ServiceExt;

    fn make_router(state: Arc<FederationState>) -> axum::Router {
        axum::Router::new()
            .route(
                "/mcp",
                axum::routing::post(handle_mcp)
                    .get(get_mcp)
                    .delete(delete_mcp),
            )
            .with_state(state)
    }

    async fn body_to_json(body: axum::body::Body) -> serde_json::Value {
        let bytes = axum::body::to_bytes(body, usize::MAX).await.unwrap();
        serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null)
    }

    #[tokio::test]
    async fn initialize_returns_session_header() {
        let state = Arc::new(make_state(vec![]).await);
        let app = make_router(state.clone());

        let req = HttpRequest::builder()
            .method(Method::POST)
            .uri("/mcp")
            .header("content-type", "application/json")
            .body(Body::from(
                serde_json::json!({
                    "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {}
                })
                .to_string(),
            ))
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let sid = resp
            .headers()
            .get(MCP_SESSION_HEADER)
            .expect("initialize must set Mcp-Session-Id")
            .to_str()
            .unwrap()
            .to_string();
        assert!(!sid.is_empty());
        assert!(state.sessions.validate_session(&sid).await);
    }

    #[tokio::test]
    async fn subsequent_request_with_valid_session_ok() {
        let state = Arc::new(make_state(vec![]).await);
        let sid = state.sessions.create_session(None).await;

        let req = HttpRequest::builder()
            .method(Method::POST)
            .uri("/mcp")
            .header("content-type", "application/json")
            .header(MCP_SESSION_HEADER, &sid)
            .body(Body::from(
                serde_json::json!({
                    "jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {}
                })
                .to_string(),
            ))
            .unwrap();

        let resp = make_router(state).oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_to_json(resp.into_body()).await;
        assert!(body["result"]["tools"].is_array());
    }

    #[tokio::test]
    async fn subsequent_request_with_unknown_session_returns_404() {
        let state = Arc::new(make_state(vec![]).await);

        let req = HttpRequest::builder()
            .method(Method::POST)
            .uri("/mcp")
            .header("content-type", "application/json")
            .header(MCP_SESSION_HEADER, "does-not-exist")
            .body(Body::from(
                serde_json::json!({
                    "jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {}
                })
                .to_string(),
            ))
            .unwrap();

        let resp = make_router(state).oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn subsequent_request_without_session_header_works_for_backward_compat() {
        // Backward-compatible with plain HTTP POST clients that never call
        // initialize with a session — dispatch still executes.
        let state = Arc::new(make_state(vec![]).await);

        let req = HttpRequest::builder()
            .method(Method::POST)
            .uri("/mcp")
            .header("content-type", "application/json")
            .body(Body::from(
                serde_json::json!({
                    "jsonrpc": "2.0", "id": 3, "method": "tools/list", "params": {}
                })
                .to_string(),
            ))
            .unwrap();

        let resp = make_router(state).oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn delete_terminates_session() {
        let state = Arc::new(make_state(vec![]).await);
        let sid = state.sessions.create_session(None).await;
        assert!(state.sessions.validate_session(&sid).await);

        let req = HttpRequest::builder()
            .method(Method::DELETE)
            .uri("/mcp")
            .header(MCP_SESSION_HEADER, &sid)
            .body(Body::empty())
            .unwrap();

        let resp = make_router(state.clone()).oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(!state.sessions.validate_session(&sid).await);
    }

    #[tokio::test]
    async fn delete_without_header_returns_400() {
        let state = Arc::new(make_state(vec![]).await);
        let req = HttpRequest::builder()
            .method(Method::DELETE)
            .uri("/mcp")
            .body(Body::empty())
            .unwrap();
        let resp = make_router(state).oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn delete_unknown_session_returns_404() {
        let state = Arc::new(make_state(vec![]).await);
        let req = HttpRequest::builder()
            .method(Method::DELETE)
            .uri("/mcp")
            .header(MCP_SESSION_HEADER, "never-existed")
            .body(Body::empty())
            .unwrap();
        let resp = make_router(state).oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn get_mcp_without_session_returns_400() {
        let state = Arc::new(make_state(vec![]).await);
        let req = HttpRequest::builder()
            .method(Method::GET)
            .uri("/mcp")
            .body(Body::empty())
            .unwrap();
        let resp = make_router(state).oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn get_mcp_with_unknown_session_returns_404() {
        let state = Arc::new(make_state(vec![]).await);
        let req = HttpRequest::builder()
            .method(Method::GET)
            .uri("/mcp")
            .header(MCP_SESSION_HEADER, "does-not-exist")
            .body(Body::empty())
            .unwrap();
        let resp = make_router(state).oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn get_mcp_with_valid_session_streams_notification() {
        let state = Arc::new(make_state(vec![]).await);
        let sid = state.sessions.create_session(None).await;

        let req = HttpRequest::builder()
            .method(Method::GET)
            .uri("/mcp")
            .header(MCP_SESSION_HEADER, &sid)
            .body(Body::empty())
            .unwrap();

        let resp = make_router(state.clone()).oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let ct = resp
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        assert!(
            ct.starts_with("text/event-stream"),
            "expected SSE content-type, got {ct}"
        );

        // Kick off a publisher after a short delay so it fires strictly
        // after our subscriber's channel is registered.
        let notifications = state.notifications.clone();
        let publish_task = tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            notifications
                .publish(serde_json::json!({
                    "jsonrpc": "2.0",
                    "method": "notifications/tools/list_changed",
                    "params": {}
                }))
                .await;
        });

        // Read frames until the SSE event containing our notification arrives.
        // The stream is unbounded so we time-box the wait.
        let body = resp.into_body();
        let mut stream = body.into_data_stream();
        let read = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            async move {
                let mut buf = String::new();
                while let Some(chunk) = stream.next().await {
                    let bytes = chunk.expect("chunk ok");
                    buf.push_str(std::str::from_utf8(&bytes).unwrap_or(""));
                    if buf.contains("notifications/tools/list_changed") {
                        return buf;
                    }
                }
                buf
            },
        )
        .await
        .expect("SSE event should arrive within timeout");
        publish_task.await.unwrap();

        assert!(
            read.contains("notifications/tools/list_changed"),
            "SSE body did not contain published notification: {read}"
        );
    }

    #[tokio::test]
    async fn post_mcp_with_accept_sse_returns_event_stream() {
        // A POST whose Accept header includes text/event-stream should be
        // framed as a single SSE `data:` event instead of application/json.
        let state = Arc::new(make_state(vec![]).await);
        let req = HttpRequest::builder()
            .method(Method::POST)
            .uri("/mcp")
            .header("content-type", "application/json")
            .header(header::ACCEPT, "text/event-stream")
            .body(Body::from(
                serde_json::json!({
                    "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {}
                })
                .to_string(),
            ))
            .unwrap();

        let resp = make_router(state).oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let ct = resp
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        assert!(
            ct.starts_with("text/event-stream"),
            "expected SSE content-type, got {ct}"
        );
        // Session id header still attached even on SSE responses.
        assert!(resp.headers().get(MCP_SESSION_HEADER).is_some());

        let bytes = axum::body::to_bytes(resp.into_body(), 4096).await.unwrap();
        let text = std::str::from_utf8(&bytes).unwrap();
        assert!(text.contains("data:"));
        assert!(text.contains("mcp-federation"));
    }

    #[tokio::test]
    async fn initialize_advertises_logging_and_sampling_capabilities() {
        let state = make_state(vec![]).await;
        let req = make_request("initialize", serde_json::json!({}));
        let resp = dispatch(&state, req, &None, &RequestContext::admin()).await;
        let json = serde_json::to_value(&resp).unwrap();
        assert!(json["result"]["capabilities"]["logging"].is_object());
        assert!(json["result"]["capabilities"]["sampling"].is_object());
    }

    #[tokio::test]
    async fn logging_set_level_stores_preference_on_session() {
        let state = make_state(vec![]).await;
        let sid = state.sessions.create_session(None).await;
        let req = make_request(
            "logging/setLevel",
            serde_json::json!({"level": "debug"}),
        );
        let resp = dispatch(&state, req, &Some(sid.clone()), &RequestContext::admin()).await;
        let json = serde_json::to_value(&resp).unwrap();
        assert!(json.get("error").is_none(), "unexpected error: {json}");
        assert_eq!(
            state.sessions.get_log_level(&sid).await.as_deref(),
            Some("debug")
        );
    }

    #[tokio::test]
    async fn logging_set_level_rejects_unknown_level() {
        let state = make_state(vec![]).await;
        let req = make_request(
            "logging/setLevel",
            serde_json::json!({"level": "screaming"}),
        );
        let resp = dispatch(&state, req, &None, &RequestContext::admin()).await;
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["error"]["code"], -32602);
    }

    #[tokio::test]
    async fn logging_set_level_missing_level_returns_error() {
        let state = make_state(vec![]).await;
        let req = make_request("logging/setLevel", serde_json::json!({}));
        let resp = dispatch(&state, req, &None, &RequestContext::admin()).await;
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["error"]["code"], -32602);
    }

    #[tokio::test]
    async fn sampling_create_message_returns_method_not_supported() {
        let state = make_state(vec![]).await;
        let req = make_request(
            "sampling/createMessage",
            serde_json::json!({"messages": []}),
        );
        let resp = dispatch(&state, req, &None, &RequestContext::admin()).await;
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["error"]["code"], -32601);
        assert!(json["error"]["message"]
            .as_str()
            .unwrap()
            .contains("sampling not supported"));
    }

    #[tokio::test]
    async fn origin_denied_when_not_allowlisted() {
        let mut state = make_state(vec![]).await;
        state.allowed_origins = vec!["https://ok.example.com".to_string()];
        let state = Arc::new(state);

        let req = HttpRequest::builder()
            .method(Method::POST)
            .uri("/mcp")
            .header("content-type", "application/json")
            .header("origin", "https://evil.example.com")
            .body(Body::from(
                serde_json::json!({
                    "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {}
                })
                .to_string(),
            ))
            .unwrap();
        let resp = make_router(state).oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn origin_allowed_when_in_allowlist() {
        let mut state = make_state(vec![]).await;
        state.allowed_origins = vec!["https://ok.example.com".to_string()];
        let state = Arc::new(state);

        let req = HttpRequest::builder()
            .method(Method::POST)
            .uri("/mcp")
            .header("content-type", "application/json")
            .header("origin", "https://ok.example.com")
            .body(Body::from(
                serde_json::json!({
                    "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {}
                })
                .to_string(),
            ))
            .unwrap();
        let resp = make_router(state).oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }
}
