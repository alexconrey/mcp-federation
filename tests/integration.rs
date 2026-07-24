use std::net::SocketAddr;
use std::sync::Arc;

use axum::extract::State;
use axum::http::header;
use axum::response::IntoResponse;
use axum::Json;
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;

use mcp_federation::aggregator::Aggregator;
use mcp_federation::config::{FederationConfig, RateLimitConfig};
use mcp_federation::notifications::NotificationBroker;
use mcp_federation::rate_limit::RateLimiter;
use mcp_federation::rbac::RequestContext;
use mcp_federation::registry::Registry;
use mcp_federation::router::Router;
use mcp_federation::server::{self, FederationState};
use mcp_federation::session::SessionManager;

fn make_state(registry: Arc<Registry>) -> FederationState {
    FederationState {
        aggregator: Aggregator::new(registry.clone()),
        router: Router::new(registry.clone()),
        registry,
        sessions: Arc::new(SessionManager::new(1800, None)),
        allowed_origins: vec![],
        rate_limiter: Arc::new(RateLimiter::new(RateLimitConfig::default())),
        notifications: Arc::new(NotificationBroker::new()),
    }
}

// ---------------------------------------------------------------------------
// Mock MCP leaf server
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct MockRequest {
    #[allow(dead_code)]
    jsonrpc: String,
    id: Option<serde_json::Value>,
    method: String,
    #[serde(default)]
    params: serde_json::Value,
}

#[derive(Serialize)]
struct MockResponse {
    jsonrpc: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<MockError>,
}

#[derive(Serialize)]
struct MockError {
    code: i32,
    message: String,
}

#[derive(Clone)]
struct MockState {
    tools: Arc<Vec<serde_json::Value>>,
    call_log: Arc<RwLock<Vec<(String, serde_json::Value)>>>,
}

async fn mock_mcp_handler(
    State(state): State<MockState>,
    Json(req): Json<MockRequest>,
) -> impl IntoResponse {
    let result = match req.method.as_str() {
        "initialize" => Some(serde_json::json!({
            "protocolVersion": "2025-11-25",
            "capabilities": { "tools": {} },
            "serverInfo": { "name": "mock-mcp", "version": "0.1.0" }
        })),
        "notifications/initialized" => Some(serde_json::json!({})),
        "tools/list" => Some(serde_json::json!({ "tools": *state.tools })),
        "tools/call" => {
            let tool_name = req.params["name"].as_str().unwrap_or("").to_string();
            let arguments = req.params["arguments"].clone();
            state
                .call_log
                .write()
                .await
                .push((tool_name.clone(), arguments));

            Some(serde_json::json!({
                "content": [{
                    "type": "text",
                    "text": format!("mock response for {tool_name}")
                }]
            }))
        }
        "resources/list" => Some(serde_json::json!({
            "resources": [{
                "uri": "k8s://default/pods/test-pod",
                "name": "Test Pod",
                "mimeType": "application/json"
            }]
        })),
        "resources/read" => {
            let uri = req.params["uri"].as_str().unwrap_or("");
            Some(serde_json::json!({
                "contents": [{
                    "uri": uri,
                    "mimeType": "application/json",
                    "text": "{\"kind\": \"Pod\"}"
                }]
            }))
        }
        "prompts/list" => Some(serde_json::json!({
            "prompts": [{
                "name": "diagnose-pod",
                "description": "Diagnose a pod",
                "arguments": []
            }]
        })),
        "prompts/get" => {
            let name = req.params["name"].as_str().unwrap_or("");
            Some(serde_json::json!({
                "description": format!("Prompt: {name}"),
                "messages": [{"role": "user", "content": {"type": "text", "text": "diagnose it"}}]
            }))
        }
        _ => None,
    };

    let response = match result {
        Some(r) => MockResponse {
            jsonrpc: "2.0".to_string(),
            id: req.id,
            result: Some(r),
            error: None,
        },
        None => MockResponse {
            jsonrpc: "2.0".to_string(),
            id: req.id,
            result: None,
            error: Some(MockError {
                code: -32601,
                message: format!("Method not found: {}", req.method),
            }),
        },
    };

    ([(header::CONTENT_TYPE, "application/json")], Json(response))
}

async fn mock_healthz() -> &'static str {
    "ok"
}

async fn start_mock_leaf(
    tools: Vec<serde_json::Value>,
) -> (SocketAddr, Arc<RwLock<Vec<(String, serde_json::Value)>>>) {
    let call_log = Arc::new(RwLock::new(Vec::new()));
    let state = MockState {
        tools: Arc::new(tools),
        call_log: call_log.clone(),
    };

    let app = axum::Router::new()
        .route("/mcp", axum::routing::post(mock_mcp_handler))
        .route("/healthz", axum::routing::get(mock_healthz))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    (addr, call_log)
}

fn make_tool(name: &str, description: &str) -> serde_json::Value {
    serde_json::json!({
        "name": name,
        "description": description,
        "inputSchema": {
            "type": "object",
            "properties": { "namespace": { "type": "string" } },
            "required": []
        }
    })
}

// ---------------------------------------------------------------------------
// Integration tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn full_flow_initialize_aggregate_route() {
    let (addr, call_log) = start_mock_leaf(vec![
        make_tool("list_pods", "List pods"),
        make_tool("get_pod", "Get a pod"),
    ])
    .await;

    let yaml = format!(
        r#"
federation:
  listen: "127.0.0.1:0"
servers:
  - alias: "test-leaf"
    url: "http://127.0.0.1:{}/mcp"
    transport: http-post
    health_check:
      enabled: false
"#,
        addr.port()
    );

    let config = FederationConfig::parse(&yaml).unwrap();
    let registry = Arc::new(Registry::from_configs(config.servers));
    registry.initialize_all().await;

    let state = make_state(registry);

    // Verify tools/list returns namespaced tools
    let req = serde_json::from_value(serde_json::json!({
        "jsonrpc": "2.0", "id": 1, "method": "tools/list", "params": {}
    }))
    .unwrap();
    let resp = server::dispatch(&state, req, &None, &RequestContext::admin()).await;
    let json = serde_json::to_value(&resp).unwrap();
    let tools = json["result"]["tools"].as_array().unwrap();

    let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();

    assert!(names.contains(&"test-leaf__list_pods"));
    assert!(names.contains(&"test-leaf__get_pod"));
    assert!(names.contains(&"federation__list_servers"));

    // Route a tool call through to the mock leaf
    let req = serde_json::from_value(serde_json::json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/call",
        "params": {
            "name": "test-leaf__list_pods",
            "arguments": {"namespace": "default"}
        }
    }))
    .unwrap();
    let resp = server::dispatch(&state, req, &None, &RequestContext::admin()).await;
    let json = serde_json::to_value(&resp).unwrap();

    assert!(json.get("error").is_none());
    let text = json["result"]["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("mock response for list_pods"));

    // Verify the mock received the un-namespaced tool name
    let log = call_log.read().await;
    assert_eq!(log.len(), 1);
    assert_eq!(log[0].0, "list_pods");
    assert_eq!(log[0].1["namespace"], "default");
}

#[tokio::test]
async fn multi_leaf_same_tool_name() {
    let (addr_a, _) = start_mock_leaf(vec![make_tool("list_pods", "Cluster A pods")]).await;
    let (addr_b, _) = start_mock_leaf(vec![make_tool("list_pods", "Cluster B pods")]).await;

    let yaml = format!(
        r#"
federation:
  listen: "127.0.0.1:0"
servers:
  - alias: "cluster-a"
    url: "http://127.0.0.1:{}/mcp"
    health_check:
      enabled: false
  - alias: "cluster-b"
    url: "http://127.0.0.1:{}/mcp"
    health_check:
      enabled: false
"#,
        addr_a.port(),
        addr_b.port()
    );

    let config = FederationConfig::parse(&yaml).unwrap();
    let registry = Arc::new(Registry::from_configs(config.servers));
    registry.initialize_all().await;

    let state = make_state(registry);

    let req = serde_json::from_value(serde_json::json!({
        "jsonrpc": "2.0", "id": 1, "method": "tools/list", "params": {}
    }))
    .unwrap();
    let resp = server::dispatch(&state, req, &None, &RequestContext::admin()).await;
    let json = serde_json::to_value(&resp).unwrap();
    let tools = json["result"]["tools"].as_array().unwrap();

    let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();

    // Both clusters' list_pods appear with distinct prefixes
    assert!(names.contains(&"cluster-a__list_pods"));
    assert!(names.contains(&"cluster-b__list_pods"));

    // Descriptions are namespaced too
    let a_tool = tools
        .iter()
        .find(|t| t["name"] == "cluster-a__list_pods")
        .unwrap();
    assert!(a_tool["description"]
        .as_str()
        .unwrap()
        .starts_with("[cluster-a]"));
}

#[tokio::test]
async fn health_check_marks_leaf_status() {
    let (addr, _) = start_mock_leaf(vec![make_tool("list_pods", "List pods")]).await;

    let yaml = format!(
        r#"
federation:
  listen: "127.0.0.1:0"
servers:
  - alias: "healthy-leaf"
    url: "http://127.0.0.1:{}/mcp"
    health_check:
      enabled: false
"#,
        addr.port()
    );

    let config = FederationConfig::parse(&yaml).unwrap();
    let registry = Arc::new(Registry::from_configs(config.servers));
    registry.initialize_all().await;

    let leaf = registry.get_leaf("healthy-leaf").await.unwrap();
    assert!(leaf.is_healthy().await);

    // Verify health check endpoint works
    let result = leaf.client.health_check().await;
    assert!(result.is_ok());
}

#[tokio::test]
async fn unreachable_leaf_marked_unhealthy() {
    let yaml = r#"
federation:
  listen: "127.0.0.1:0"
servers:
  - alias: "dead-leaf"
    url: "http://127.0.0.1:1/mcp"
    health_check:
      enabled: false
      timeout_seconds: 1
"#;

    let config = FederationConfig::parse(yaml).unwrap();
    let registry = Arc::new(Registry::from_configs(config.servers));
    registry.initialize_all().await;

    let leaf = registry.get_leaf("dead-leaf").await.unwrap();
    // Should be marked unhealthy after failed init
    assert!(!leaf.is_healthy().await);

    // Should not appear in aggregated tools
    let state = make_state(registry);

    let req = serde_json::from_value(serde_json::json!({
        "jsonrpc": "2.0", "id": 1, "method": "tools/list", "params": {}
    }))
    .unwrap();
    let resp = server::dispatch(&state, req, &None, &RequestContext::admin()).await;
    let json = serde_json::to_value(&resp).unwrap();
    let tools = json["result"]["tools"].as_array().unwrap();

    let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();

    // Only native federation tools, no leaf tools
    assert!(!names.iter().any(|n| n.starts_with("dead-leaf__")));
}

#[tokio::test]
async fn resource_and_prompt_aggregation_end_to_end() {
    let (addr, _) = start_mock_leaf(vec![]).await;

    let yaml = format!(
        r#"
federation:
  listen: "127.0.0.1:0"
servers:
  - alias: "res-leaf"
    url: "http://127.0.0.1:{}/mcp"
    health_check:
      enabled: false
"#,
        addr.port()
    );

    let config = FederationConfig::parse(&yaml).unwrap();
    let registry = Arc::new(Registry::from_configs(config.servers));
    registry.initialize_all().await;

    let state = make_state(registry);

    // resources/list
    let req = serde_json::from_value(serde_json::json!({
        "jsonrpc": "2.0", "id": 1, "method": "resources/list", "params": {}
    }))
    .unwrap();
    let resp = server::dispatch(&state, req, &None, &RequestContext::admin()).await;
    let json = serde_json::to_value(&resp).unwrap();
    let resources = json["result"]["resources"].as_array().unwrap();
    assert!(!resources.is_empty());
    assert!(resources[0]["uri"]
        .as_str()
        .unwrap()
        .starts_with("res-leaf__"));

    // prompts/list
    let req = serde_json::from_value(serde_json::json!({
        "jsonrpc": "2.0", "id": 2, "method": "prompts/list", "params": {}
    }))
    .unwrap();
    let resp = server::dispatch(&state, req, &None, &RequestContext::admin()).await;
    let json = serde_json::to_value(&resp).unwrap();
    let prompts = json["result"]["prompts"].as_array().unwrap();
    assert!(!prompts.is_empty());
    assert_eq!(prompts[0]["name"], "res-leaf__diagnose-pod");
}

#[tokio::test]
async fn federation_native_refresh_tool() {
    let (addr, _) = start_mock_leaf(vec![make_tool("list_pods", "List pods")]).await;

    let yaml = format!(
        r#"
federation:
  listen: "127.0.0.1:0"
servers:
  - alias: "refresh-leaf"
    url: "http://127.0.0.1:{}/mcp"
    health_check:
      enabled: false
"#,
        addr.port()
    );

    let config = FederationConfig::parse(&yaml).unwrap();
    let registry = Arc::new(Registry::from_configs(config.servers));
    registry.initialize_all().await;

    let state = make_state(registry);

    // Call federation__refresh for a specific alias
    let req = serde_json::from_value(serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": {
            "name": "federation__refresh",
            "arguments": {"alias": "refresh-leaf"}
        }
    }))
    .unwrap();
    let resp = server::dispatch(&state, req, &None, &RequestContext::admin()).await;
    let json = serde_json::to_value(&resp).unwrap();
    assert!(json.get("error").is_none());
    let text = json["result"]["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("refresh-leaf: refreshed"));

    // Call federation__refresh for all
    let req = serde_json::from_value(serde_json::json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/call",
        "params": {
            "name": "federation__refresh",
            "arguments": {}
        }
    }))
    .unwrap();
    let resp = server::dispatch(&state, req, &None, &RequestContext::admin()).await;
    let json = serde_json::to_value(&resp).unwrap();
    assert!(json.get("error").is_none());
}

#[tokio::test]
async fn http_endpoint_with_auth() {
    let (addr, _) = start_mock_leaf(vec![make_tool("list_pods", "List pods")]).await;

    let yaml = format!(
        r#"
federation:
  listen: "127.0.0.1:0"
  auth_token: "test-secret-token"
servers:
  - alias: "auth-leaf"
    url: "http://127.0.0.1:{}/mcp"
    health_check:
      enabled: false
"#,
        addr.port()
    );

    let config = FederationConfig::parse(&yaml).unwrap();
    let registry = Arc::new(Registry::from_configs(config.servers));
    registry.initialize_all().await;

    let state = Arc::new(make_state(registry));

    // Build the federation HTTP server with auth
    let auth_state = AuthState {
        token: Some("test-secret-token".to_string()),
        clients: vec![],
    };

    let app = axum::Router::new()
        .route("/mcp", axum::routing::post(server::handle_mcp))
        .route("/healthz", axum::routing::get(server::healthz))
        .layer(axum::middleware::from_fn_with_state(
            auth_state,
            auth_middleware,
        ))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let fed_addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    let client = reqwest::Client::new();

    // Request without auth should fail
    let resp = client
        .post(format!("http://127.0.0.1:{}/mcp", fed_addr.port()))
        .json(&serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {}
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401);

    // Request with wrong auth should fail
    let resp = client
        .post(format!("http://127.0.0.1:{}/mcp", fed_addr.port()))
        .bearer_auth("wrong-token")
        .json(&serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {}
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401);

    // Request with correct auth should succeed
    let resp = client
        .post(format!("http://127.0.0.1:{}/mcp", fed_addr.port()))
        .bearer_auth("test-secret-token")
        .json(&serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {}
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["result"]["serverInfo"]["name"], "mcp-federation");

    // Healthz should work without auth
    let resp = client
        .get(format!("http://127.0.0.1:{}/healthz", fed_addr.port()))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
}

#[tokio::test]
async fn rbac_filters_and_denies_across_http() {
    use mcp_federation::config::ClientConfig;

    let (addr_prod, _) = start_mock_leaf(vec![make_tool("list_pods", "Prod pods")]).await;
    let (addr_stg, _) = start_mock_leaf(vec![make_tool("list_pods", "Staging pods")]).await;

    let yaml = format!(
        r#"
federation:
  listen: "127.0.0.1:0"
  auth_token: "admin-token"
  clients:
    - token: "team-a-token"
      allowed_servers: ["prod-k8s"]
    - token: "readonly-token"
      allowed_servers: ["*"]
servers:
  - alias: "prod-k8s"
    url: "http://127.0.0.1:{}/mcp"
    health_check:
      enabled: false
  - alias: "staging-k8s"
    url: "http://127.0.0.1:{}/mcp"
    health_check:
      enabled: false
"#,
        addr_prod.port(),
        addr_stg.port()
    );

    let config = FederationConfig::parse(&yaml).unwrap();
    let clients: Vec<ClientConfig> = config.federation.clients.clone();
    let registry = Arc::new(Registry::from_configs(config.servers));
    registry.initialize_all().await;

    let state = Arc::new(make_state(registry));

    let auth_state = AuthState {
        token: Some("admin-token".to_string()),
        clients,
    };

    let app = axum::Router::new()
        .route("/mcp", axum::routing::post(server::handle_mcp))
        .route("/healthz", axum::routing::get(server::healthz))
        .layer(axum::middleware::from_fn_with_state(
            auth_state,
            auth_middleware,
        ))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let fed_addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    let client = reqwest::Client::new();
    let url = format!("http://127.0.0.1:{}/mcp", fed_addr.port());

    // Admin sees both leaves.
    let resp = client
        .post(&url)
        .bearer_auth("admin-token")
        .json(&serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": "tools/list", "params": {}
        }))
        .send()
        .await
        .unwrap();
    let body: serde_json::Value = resp.json().await.unwrap();
    let names: Vec<String> = body["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap().to_string())
        .collect();
    assert!(names.contains(&"prod-k8s__list_pods".to_string()));
    assert!(names.contains(&"staging-k8s__list_pods".to_string()));

    // team-a sees only prod-k8s.
    let resp = client
        .post(&url)
        .bearer_auth("team-a-token")
        .json(&serde_json::json!({
            "jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {}
        }))
        .send()
        .await
        .unwrap();
    let body: serde_json::Value = resp.json().await.unwrap();
    let names: Vec<String> = body["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap().to_string())
        .collect();
    assert!(names.contains(&"prod-k8s__list_pods".to_string()));
    assert!(!names.contains(&"staging-k8s__list_pods".to_string()));
    assert!(names.contains(&"federation__list_servers".to_string()));

    // team-a calling a forbidden leaf is denied with -32000 and no leaf hit.
    let resp = client
        .post(&url)
        .bearer_auth("team-a-token")
        .json(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tools/call",
            "params": {
                "name": "staging-k8s__list_pods",
                "arguments": {}
            }
        }))
        .send()
        .await
        .unwrap();
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["code"], -32000);
    assert!(body["error"]["message"]
        .as_str()
        .unwrap()
        .contains("access denied"));

    // Wildcard client sees everything.
    let resp = client
        .post(&url)
        .bearer_auth("readonly-token")
        .json(&serde_json::json!({
            "jsonrpc": "2.0", "id": 4, "method": "tools/list", "params": {}
        }))
        .send()
        .await
        .unwrap();
    let body: serde_json::Value = resp.json().await.unwrap();
    let names: Vec<String> = body["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap().to_string())
        .collect();
    assert!(names.contains(&"prod-k8s__list_pods".to_string()));
    assert!(names.contains(&"staging-k8s__list_pods".to_string()));

    // Unknown token is rejected at the middleware.
    let resp = client
        .post(&url)
        .bearer_auth("mystery-token")
        .json(&serde_json::json!({
            "jsonrpc": "2.0", "id": 5, "method": "tools/list", "params": {}
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401);
}

// ---------------------------------------------------------------------------
// Auth middleware (replicated here since it's in main.rs, not the lib).
//
// Mirrors the real middleware in `main.rs` closely enough to exercise the
// RBAC integration path: the presented bearer token is matched against the
// admin token and any per-client entries, and the resolved `RequestContext`
// is stashed on the request as an axum extension for the handler to read.
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct AuthState {
    token: Option<String>,
    clients: Vec<mcp_federation::config::ClientConfig>,
}

async fn auth_middleware(
    State(auth): State<AuthState>,
    mut request: axum::extract::Request,
    next: axum::middleware::Next,
) -> impl IntoResponse {
    use axum::http::StatusCode;

    let path = request.uri().path();
    let skip = path == "/healthz" || path == "/metrics";

    if let Some(ref expected) = auth.token {
        if !skip {
            let presented = request
                .headers()
                .get(header::AUTHORIZATION)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.strip_prefix("Bearer "));

            let ctx = match presented {
                Some(tok) => mcp_federation::rbac::resolve(tok, expected, &auth.clients),
                None => None,
            };

            match ctx {
                Some(c) => {
                    request.extensions_mut().insert(c);
                }
                None => return StatusCode::UNAUTHORIZED.into_response(),
            }
        }
    } else if !skip {
        request.extensions_mut().insert(RequestContext::anonymous());
    }

    next.run(request).await.into_response()
}

// ---------------------------------------------------------------------------
// Streamable HTTP mock leaf — exercises the LeafClient's session lifecycle,
// including 404 → re-initialize → retry.
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct StreamableMockState {
    /// Currently accepted session ID. `None` until initialize.
    session_id: Arc<RwLock<Option<String>>>,
    /// Number of initialize calls the mock has served.
    init_count: Arc<std::sync::atomic::AtomicUsize>,
    /// Return 404 exactly once on the next non-initialize call, then behave
    /// normally. Simulates session expiry mid-flight.
    fail_next_with_404: Arc<std::sync::atomic::AtomicBool>,
    /// Aggregated call log (method, arguments) — used to verify retry happens.
    call_log: Arc<RwLock<Vec<(String, serde_json::Value)>>>,
}

async fn streamable_mock_handler(
    State(state): State<StreamableMockState>,
    headers: axum::http::HeaderMap,
    Json(req): Json<MockRequest>,
) -> axum::response::Response {
    use axum::http::StatusCode;
    use std::sync::atomic::Ordering;

    let incoming_session = headers
        .get("mcp-session-id")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);

    state
        .call_log
        .write()
        .await
        .push((req.method.clone(), req.params.clone()));

    if req.method == "initialize" {
        state.init_count.fetch_add(1, Ordering::Relaxed);
        let new_id = format!("sid-{}", state.init_count.load(Ordering::Relaxed));
        *state.session_id.write().await = Some(new_id.clone());

        let body = MockResponse {
            jsonrpc: "2.0".to_string(),
            id: req.id,
            result: Some(serde_json::json!({
                "protocolVersion": "2025-11-25",
                "capabilities": { "tools": {} },
                "serverInfo": { "name": "streamable-mock", "version": "0.1.0" }
            })),
            error: None,
        };
        let mut resp = (StatusCode::OK, Json(body)).into_response();
        resp.headers_mut().insert(
            "mcp-session-id",
            new_id.parse().expect("valid ascii session id"),
        );
        return resp;
    }

    if req.method == "notifications/initialized" {
        // Best-effort — accept with empty result.
        let body = MockResponse {
            jsonrpc: "2.0".to_string(),
            id: req.id,
            result: Some(serde_json::json!({})),
            error: None,
        };
        return (StatusCode::OK, Json(body)).into_response();
    }

    // Optionally fail the very next non-initialize call with 404 to simulate
    // an expired session.
    if state.fail_next_with_404.swap(false, Ordering::SeqCst) {
        return (StatusCode::NOT_FOUND, "session expired").into_response();
    }

    // Session gate: require the header to match the currently-accepted id.
    let current = state.session_id.read().await.clone();
    match (current, incoming_session) {
        (Some(cur), Some(incoming)) if cur == incoming => { /* ok */ }
        (Some(_), _) => {
            return (StatusCode::NOT_FOUND, "session mismatch").into_response();
        }
        (None, _) => {
            return (StatusCode::BAD_REQUEST, "must initialize first").into_response();
        }
    }

    let result = match req.method.as_str() {
        "tools/list" => Some(serde_json::json!({
            "tools": [{
                "name": "ping",
                "description": "Ping the streamable mock",
                "inputSchema": {"type": "object", "properties": {}, "required": []}
            }]
        })),
        "tools/call" => {
            let name = req.params["name"].as_str().unwrap_or("");
            Some(serde_json::json!({
                "content": [{"type": "text", "text": format!("pong: {name}")}]
            }))
        }
        _ => None,
    };

    let body = match result {
        Some(r) => MockResponse {
            jsonrpc: "2.0".to_string(),
            id: req.id,
            result: Some(r),
            error: None,
        },
        None => MockResponse {
            jsonrpc: "2.0".to_string(),
            id: req.id,
            result: None,
            error: Some(MockError {
                code: -32601,
                message: format!("Method not found: {}", req.method),
            }),
        },
    };
    (StatusCode::OK, Json(body)).into_response()
}

async fn start_streamable_mock_leaf() -> (SocketAddr, StreamableMockState) {
    let state = StreamableMockState {
        session_id: Arc::new(RwLock::new(None)),
        init_count: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        fail_next_with_404: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        call_log: Arc::new(RwLock::new(Vec::new())),
    };

    let app = axum::Router::new()
        .route("/mcp", axum::routing::post(streamable_mock_handler))
        .route("/healthz", axum::routing::get(mock_healthz))
        .with_state(state.clone());

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (addr, state)
}

#[tokio::test]
async fn streamable_http_leaf_session_lifecycle() {
    use std::sync::atomic::Ordering;

    let (addr, mock) = start_streamable_mock_leaf().await;

    let yaml = format!(
        r#"
federation:
  listen: "127.0.0.1:0"
servers:
  - alias: "stream-leaf"
    url: "http://127.0.0.1:{}/mcp"
    transport: streamable-http
    health_check:
      enabled: false
"#,
        addr.port()
    );

    let config = FederationConfig::parse(&yaml).unwrap();
    let registry = Arc::new(Registry::from_configs(config.servers));
    registry.initialize_all().await;

    // Baseline: one initialize should have gone through, session captured.
    assert_eq!(mock.init_count.load(Ordering::Relaxed), 1);

    let state = make_state(registry);

    // First tools/list — passes with the currently-held session.
    let req = serde_json::from_value(serde_json::json!({
        "jsonrpc": "2.0", "id": 1, "method": "tools/list", "params": {}
    }))
    .unwrap();
    let resp = server::dispatch(&state, req, &None, &RequestContext::admin()).await;
    let json = serde_json::to_value(&resp).unwrap();
    let names: Vec<&str> = json["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"stream-leaf__ping"));

    // Now force the next request to return 404 (session expired) — the leaf
    // client should re-initialize and retry once, transparently.
    mock.fail_next_with_404.store(true, Ordering::SeqCst);

    let req = serde_json::from_value(serde_json::json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/call",
        "params": {
            "name": "stream-leaf__ping",
            "arguments": {}
        }
    }))
    .unwrap();
    let resp = server::dispatch(&state, req, &None, &RequestContext::admin()).await;
    let json = serde_json::to_value(&resp).unwrap();
    assert!(
        json.get("error").is_none(),
        "expected success after re-init retry, got {json}"
    );
    let text = json["result"]["content"][0]["text"].as_str().unwrap();
    assert_eq!(text, "pong: ping");

    // Verify the mock served a second initialize (proof of the retry path).
    assert_eq!(mock.init_count.load(Ordering::Relaxed), 2);
}

// ---------------------------------------------------------------------------
// Dynamic registration REST API
// ---------------------------------------------------------------------------

#[tokio::test]
async fn dynamic_registration_end_to_end() {
    use mcp_federation::api::{self, ApiState};

    let (addr, _) = start_mock_leaf(vec![make_tool("list_pods", "List pods")]).await;

    // Fresh, empty registry.
    let registry = Arc::new(Registry::new());
    let app = api::router(ApiState::new(registry.clone()));

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let api_addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    let client = reqwest::Client::new();
    let base = format!("http://127.0.0.1:{}", api_addr.port());

    // 1. GET before registering — empty list.
    let resp = client.get(format!("{base}/servers")).send().await.unwrap();
    assert_eq!(resp.status(), 200);
    let arr: Vec<serde_json::Value> = resp.json().await.unwrap();
    assert!(arr.is_empty());

    // 2. Register a leaf that points at the mock server.
    let resp = client
        .post(format!("{base}/servers"))
        .json(&serde_json::json!({
            "alias": "dyn-leaf",
            "url": format!("http://127.0.0.1:{}/mcp", addr.port()),
            "transport": "http-post",
            "tags": ["dynamic"],
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 201);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["alias"], "dyn-leaf");
    assert_eq!(body["tags"][0], "dynamic");

    // 3. Register duplicate → 409.
    let resp = client
        .post(format!("{base}/servers"))
        .json(&serde_json::json!({
            "alias": "dyn-leaf",
            "url": "http://x:8080/mcp"
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 409);

    // 4. GET specific server.
    let resp = client
        .get(format!("{base}/servers/dyn-leaf"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["alias"], "dyn-leaf");

    // 5. GET missing server → 404.
    let resp = client
        .get(format!("{base}/servers/missing"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);

    // 6. Registry now contains the leaf.
    assert!(registry.get_leaf("dyn-leaf").await.is_some());

    // 7. DELETE removes it.
    let resp = client
        .delete(format!("{base}/servers/dyn-leaf"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert!(registry.get_leaf("dyn-leaf").await.is_none());

    // 8. DELETE again → 404.
    let resp = client
        .delete(format!("{base}/servers/dyn-leaf"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);
}

// ---------------------------------------------------------------------------
// Federation topology tool (multi-level federation detection)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn topology_reports_federation_children() {
    use mcp_federation::registry::LeafEntry;

    let registry = Arc::new(Registry::new());

    // Two leaves: one is a regular MCP server, one is another mcp-federation.
    let plain = Arc::new(LeafEntry::new(mock_server_config("plain-leaf")));
    plain.mark_healthy().await;
    *plain.server_info.write().await = Some(serde_json::json!({
        "name": "mcp-k8s",
        "version": "1.2.3"
    }));
    registry.add_leaf("plain-leaf".to_string(), plain).await;

    let child_fed = Arc::new(LeafEntry::new(mock_server_config("child-fed")));
    child_fed.mark_healthy().await;
    *child_fed.server_info.write().await = Some(serde_json::json!({
        "name": "mcp-federation",
        "version": "0.1.0"
    }));
    registry.add_leaf("child-fed".to_string(), child_fed).await;

    let state = make_state(registry);

    let req = serde_json::from_value(serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": {
            "name": "federation__topology",
            "arguments": {}
        }
    }))
    .unwrap();
    let resp = server::dispatch(&state, req, &None, &RequestContext::admin()).await;
    let json = serde_json::to_value(&resp).unwrap();
    assert!(json.get("error").is_none());

    let text = json["result"]["content"][0]["text"].as_str().unwrap();
    let topology: serde_json::Value = serde_json::from_str(text).unwrap();
    assert_eq!(topology["node"]["name"], "mcp-federation");
    assert_eq!(topology["node"]["leaf_count"], 2);

    let leaves = topology["leaves"].as_array().unwrap();
    assert_eq!(leaves.len(), 2);

    let by_alias: std::collections::HashMap<String, &serde_json::Value> = leaves
        .iter()
        .map(|l| (l["alias"].as_str().unwrap().to_string(), l))
        .collect();
    assert_eq!(by_alias["plain-leaf"]["is_federation"], false);
    assert_eq!(by_alias["child-fed"]["is_federation"], true);
}

fn mock_server_config(alias: &str) -> mcp_federation::config::ServerConfig {
    use mcp_federation::config::{HealthCheckConfig, ServerConfig, TlsConfig, TransportType};
    ServerConfig {
        alias: alias.to_string(),
        url: format!("http://{alias}:8080/mcp"),
        auth_token: None,
        auth_token_file: None,
        transport: TransportType::HttpPost,
        tags: vec![],
        health_check: HealthCheckConfig {
            enabled: false,
            interval_seconds: 30,
            timeout_seconds: 5,
            failure_threshold: 3,
            backoff_multiplier: 2.0,
        },
        timeout_seconds: None,
        tls: TlsConfig::default(),
    }
}

// ---------------------------------------------------------------------------
// Two mock mcp-k8s servers with independent routing
// ---------------------------------------------------------------------------

/// Simulates two mcp-k8s instances serving different clusters. Each mock
/// advertises a realistic subset of mcp-k8s tools and returns cluster-specific
/// data in tool call responses. The test verifies:
///
/// 1. Both leaves initialize and their tools appear under distinct namespaces
/// 2. Calling `srv1__list_pods` routes to srv1 and returns srv1-specific data
/// 3. Calling `srv2__list_pods` routes to srv2 and returns srv2-specific data
/// 4. Each mock only receives the un-namespaced tool name and correct arguments
/// 5. A tool unique to one leaf is absent from the other's namespace
#[tokio::test]
async fn dual_mcp_k8s_routing() {
    // --- Mock mcp-k8s leaves with cluster-specific behavior ---

    #[derive(Clone)]
    struct K8sMockState {
        cluster_name: String,
        tools: Arc<Vec<serde_json::Value>>,
        call_log: Arc<RwLock<Vec<(String, serde_json::Value)>>>,
    }

    async fn k8s_mock_handler(
        State(state): State<K8sMockState>,
        Json(req): Json<MockRequest>,
    ) -> impl IntoResponse {
        let result = match req.method.as_str() {
            "initialize" => Some(serde_json::json!({
                "protocolVersion": "2025-11-25",
                "capabilities": { "tools": {}, "resources": {}, "prompts": {} },
                "serverInfo": {
                    "name": "mcp-k8s",
                    "version": "0.1.0"
                }
            })),
            "notifications/initialized" => Some(serde_json::json!({})),
            "tools/list" => Some(serde_json::json!({ "tools": *state.tools })),
            "tools/call" => {
                let tool_name = req.params["name"].as_str().unwrap_or("").to_string();
                let arguments = req.params["arguments"].clone();
                state
                    .call_log
                    .write()
                    .await
                    .push((tool_name.clone(), arguments.clone()));

                let ns = arguments
                    .get("namespace")
                    .and_then(|v| v.as_str())
                    .unwrap_or("default");

                let response_text = match tool_name.as_str() {
                    "list_pods" => format!(
                        "Pods in {ns} on {cluster}:\n- {cluster}-app-abc123\n- {cluster}-worker-def456",
                        cluster = state.cluster_name
                    ),
                    "get_pod" => {
                        let pod = arguments.get("name").and_then(|v| v.as_str()).unwrap_or("unknown");
                        format!(
                            "Pod {pod} in {ns} on {cluster}: Running, 2/2 containers ready",
                            cluster = state.cluster_name
                        )
                    }
                    "list_namespaces" => format!(
                        "Namespaces on {cluster}: default, kube-system, {cluster}-apps",
                        cluster = state.cluster_name
                    ),
                    "list_nodes" => format!(
                        "Nodes on {cluster}: {cluster}-node-1 (Ready), {cluster}-node-2 (Ready)",
                        cluster = state.cluster_name
                    ),
                    "get_deployment" => {
                        let dep = arguments.get("name").and_then(|v| v.as_str()).unwrap_or("unknown");
                        format!(
                            "Deployment {dep} in {ns} on {cluster}: 3/3 replicas available",
                            cluster = state.cluster_name
                        )
                    }
                    _ => format!("{cluster} response for {tool_name}", cluster = state.cluster_name),
                };

                Some(serde_json::json!({
                    "content": [{"type": "text", "text": response_text}]
                }))
            }
            "resources/list" => Some(serde_json::json!({ "resources": [] })),
            "prompts/list" => Some(serde_json::json!({ "prompts": [] })),
            _ => None,
        };

        let response = match result {
            Some(r) => MockResponse {
                jsonrpc: "2.0".to_string(),
                id: req.id,
                result: Some(r),
                error: None,
            },
            None => MockResponse {
                jsonrpc: "2.0".to_string(),
                id: req.id,
                result: None,
                error: Some(MockError {
                    code: -32601,
                    message: format!("Method not found: {}", req.method),
                }),
            },
        };

        ([(header::CONTENT_TYPE, "application/json")], Json(response))
    }

    // Shared k8s tools both clusters expose
    let shared_tools = vec![
        make_tool("list_pods", "List pods in a namespace"),
        make_tool("get_pod", "Get a specific pod by name"),
        make_tool("list_namespaces", "List all namespaces"),
        make_tool("list_nodes", "List cluster nodes"),
        make_tool("get_deployment", "Get a deployment by name"),
    ];

    // srv2 has an extra tool that srv1 doesn't
    let mut srv2_tools = shared_tools.clone();
    srv2_tools.push(make_tool("list_network_policies", "List network policies"));

    // Start srv1 (prod cluster)
    let srv1_log = Arc::new(RwLock::new(Vec::new()));
    let srv1_state = K8sMockState {
        cluster_name: "prod-cluster".to_string(),
        tools: Arc::new(shared_tools),
        call_log: srv1_log.clone(),
    };
    let srv1_app = axum::Router::new()
        .route("/mcp", axum::routing::post(k8s_mock_handler))
        .route("/healthz", axum::routing::get(mock_healthz))
        .with_state(srv1_state);
    let srv1_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let srv1_addr = srv1_listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(srv1_listener, srv1_app).await.unwrap() });

    // Start srv2 (staging cluster)
    let srv2_log = Arc::new(RwLock::new(Vec::new()));
    let srv2_state = K8sMockState {
        cluster_name: "staging-cluster".to_string(),
        tools: Arc::new(srv2_tools),
        call_log: srv2_log.clone(),
    };
    let srv2_app = axum::Router::new()
        .route("/mcp", axum::routing::post(k8s_mock_handler))
        .route("/healthz", axum::routing::get(mock_healthz))
        .with_state(srv2_state);
    let srv2_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let srv2_addr = srv2_listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(srv2_listener, srv2_app).await.unwrap() });

    // Configure federation with both leaves
    let yaml = format!(
        r#"
federation:
  listen: "127.0.0.1:0"
servers:
  - alias: "srv1"
    url: "http://127.0.0.1:{}/mcp"
    transport: http-post
    tags: ["kubernetes", "production"]
    health_check:
      enabled: false
  - alias: "srv2"
    url: "http://127.0.0.1:{}/mcp"
    transport: http-post
    tags: ["kubernetes", "staging"]
    health_check:
      enabled: false
"#,
        srv1_addr.port(),
        srv2_addr.port(),
    );

    let config = FederationConfig::parse(&yaml).unwrap();
    let registry = Arc::new(Registry::from_configs(config.servers));
    registry.initialize_all().await;

    let state = make_state(registry);

    // --- Verify tools/list shows both namespaced sets ---

    let req = serde_json::from_value(serde_json::json!({
        "jsonrpc": "2.0", "id": 1, "method": "tools/list", "params": {}
    }))
    .unwrap();
    let resp = server::dispatch(&state, req, &None, &RequestContext::admin()).await;
    let json = serde_json::to_value(&resp).unwrap();
    let tools = json["result"]["tools"].as_array().unwrap();

    let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();

    // Both clusters' tools appear with distinct prefixes
    assert!(
        names.contains(&"srv1__list_pods"),
        "missing srv1__list_pods"
    );
    assert!(
        names.contains(&"srv2__list_pods"),
        "missing srv2__list_pods"
    );
    assert!(
        names.contains(&"srv1__get_deployment"),
        "missing srv1__get_deployment"
    );
    assert!(
        names.contains(&"srv2__get_deployment"),
        "missing srv2__get_deployment"
    );

    // srv2-only tool appears under srv2 but not srv1
    assert!(
        names.contains(&"srv2__list_network_policies"),
        "missing srv2__list_network_policies"
    );
    assert!(
        !names.contains(&"srv1__list_network_policies"),
        "srv1 should NOT have list_network_policies"
    );

    // Descriptions are prefixed with the alias
    let srv1_list_pods = tools
        .iter()
        .find(|t| t["name"] == "srv1__list_pods")
        .unwrap();
    assert!(srv1_list_pods["description"]
        .as_str()
        .unwrap()
        .starts_with("[srv1]"));
    let srv2_list_pods = tools
        .iter()
        .find(|t| t["name"] == "srv2__list_pods")
        .unwrap();
    assert!(srv2_list_pods["description"]
        .as_str()
        .unwrap()
        .starts_with("[srv2]"));

    // Federation native tools are also present
    assert!(names.contains(&"federation__list_servers"));
    assert!(names.contains(&"federation__topology"));

    // --- Route list_pods to srv1 → get prod-cluster data ---

    let req = serde_json::from_value(serde_json::json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/call",
        "params": {
            "name": "srv1__list_pods",
            "arguments": {"namespace": "kube-system"}
        }
    }))
    .unwrap();
    let resp = server::dispatch(&state, req, &None, &RequestContext::admin()).await;
    let json = serde_json::to_value(&resp).unwrap();
    assert!(json.get("error").is_none(), "srv1 list_pods failed: {json}");
    let text = json["result"]["content"][0]["text"].as_str().unwrap();
    assert!(
        text.contains("prod-cluster"),
        "expected prod-cluster in response, got: {text}"
    );
    assert!(
        text.contains("kube-system"),
        "expected kube-system in response, got: {text}"
    );

    // --- Route list_pods to srv2 → get staging-cluster data ---

    let req = serde_json::from_value(serde_json::json!({
        "jsonrpc": "2.0",
        "id": 3,
        "method": "tools/call",
        "params": {
            "name": "srv2__list_pods",
            "arguments": {"namespace": "default"}
        }
    }))
    .unwrap();
    let resp = server::dispatch(&state, req, &None, &RequestContext::admin()).await;
    let json = serde_json::to_value(&resp).unwrap();
    assert!(json.get("error").is_none(), "srv2 list_pods failed: {json}");
    let text = json["result"]["content"][0]["text"].as_str().unwrap();
    assert!(
        text.contains("staging-cluster"),
        "expected staging-cluster in response, got: {text}"
    );

    // --- Route get_deployment to srv1 ---

    let req = serde_json::from_value(serde_json::json!({
        "jsonrpc": "2.0",
        "id": 4,
        "method": "tools/call",
        "params": {
            "name": "srv1__get_deployment",
            "arguments": {"namespace": "default", "name": "nginx"}
        }
    }))
    .unwrap();
    let resp = server::dispatch(&state, req, &None, &RequestContext::admin()).await;
    let json = serde_json::to_value(&resp).unwrap();
    assert!(json.get("error").is_none());
    let text = json["result"]["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("prod-cluster"));
    assert!(text.contains("nginx"));

    // --- Route srv2-only tool ---

    let req = serde_json::from_value(serde_json::json!({
        "jsonrpc": "2.0",
        "id": 5,
        "method": "tools/call",
        "params": {
            "name": "srv2__list_network_policies",
            "arguments": {"namespace": "default"}
        }
    }))
    .unwrap();
    let resp = server::dispatch(&state, req, &None, &RequestContext::admin()).await;
    let json = serde_json::to_value(&resp).unwrap();
    assert!(json.get("error").is_none());
    let text = json["result"]["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("staging-cluster"));

    // --- Verify call logs: each mock only got its own calls ---

    let srv1_calls = srv1_log.read().await;
    assert_eq!(srv1_calls.len(), 2, "srv1 should have 2 tool calls");
    assert_eq!(srv1_calls[0].0, "list_pods");
    assert_eq!(srv1_calls[0].1["namespace"], "kube-system");
    assert_eq!(srv1_calls[1].0, "get_deployment");
    assert_eq!(srv1_calls[1].1["name"], "nginx");

    let srv2_calls = srv2_log.read().await;
    assert_eq!(srv2_calls.len(), 2, "srv2 should have 2 tool calls");
    assert_eq!(srv2_calls[0].0, "list_pods");
    assert_eq!(srv2_calls[0].1["namespace"], "default");
    assert_eq!(srv2_calls[1].0, "list_network_policies");

    // --- Verify topology shows both as mcp-k8s leaves ---

    let req = serde_json::from_value(serde_json::json!({
        "jsonrpc": "2.0",
        "id": 6,
        "method": "tools/call",
        "params": {
            "name": "federation__topology",
            "arguments": {}
        }
    }))
    .unwrap();
    let resp = server::dispatch(&state, req, &None, &RequestContext::admin()).await;
    let json = serde_json::to_value(&resp).unwrap();
    let text = json["result"]["content"][0]["text"].as_str().unwrap();
    let topology: serde_json::Value = serde_json::from_str(text).unwrap();

    let leaves = topology["leaves"].as_array().unwrap();
    assert_eq!(leaves.len(), 2);
    for leaf in leaves {
        assert_eq!(
            leaf["is_federation"], false,
            "mcp-k8s leaves are not federations"
        );
        let info = &leaf["server_info"];
        assert_eq!(info["name"], "mcp-k8s");
    }

    // --- Verify federation__list_servers shows tags ---

    let req = serde_json::from_value(serde_json::json!({
        "jsonrpc": "2.0",
        "id": 7,
        "method": "tools/call",
        "params": {
            "name": "federation__list_servers",
            "arguments": {}
        }
    }))
    .unwrap();
    let resp = server::dispatch(&state, req, &None, &RequestContext::admin()).await;
    let json = serde_json::to_value(&resp).unwrap();
    let text = json["result"]["content"][0]["text"].as_str().unwrap();
    let servers: Vec<serde_json::Value> = serde_json::from_str(text).unwrap();
    assert_eq!(servers.len(), 2);

    let by_alias: std::collections::HashMap<String, &serde_json::Value> = servers
        .iter()
        .map(|s| (s["alias"].as_str().unwrap().to_string(), s))
        .collect();
    assert_eq!(by_alias["srv1"]["health"], "healthy");
    assert_eq!(by_alias["srv2"]["health"], "healthy");
    assert_eq!(by_alias["srv1"]["tool_count"], 5);
    assert_eq!(by_alias["srv2"]["tool_count"], 6);
}

// ---------------------------------------------------------------------------
// SSE leaf-to-client notification forwarding
// ---------------------------------------------------------------------------

/// Mock leaf that answers `GET /mcp` with an SSE stream containing a single
/// `notifications/tools/list_changed` event.
async fn start_sse_leaf() -> SocketAddr {
    use axum::response::sse::{Event, KeepAlive, Sse};

    async fn sse_handler() -> impl IntoResponse {
        let stream = futures::stream::iter(vec![Ok::<_, std::convert::Infallible>(
            Event::default().data(
                serde_json::json!({
                    "jsonrpc": "2.0",
                    "method": "notifications/tools/list_changed",
                    "params": {}
                })
                .to_string(),
            ),
        )]);
        Sse::new(stream).keep_alive(KeepAlive::default())
    }

    async fn post_stub() -> &'static str {
        "not used"
    }

    let app = axum::Router::new().route("/mcp", axum::routing::get(sse_handler).post(post_stub));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    addr
}

#[tokio::test]
async fn leaf_sse_notifications_forwarded_to_broker() {
    use mcp_federation::config::{ConnectionPoolConfig, TransportType};
    use mcp_federation::leaf_client::LeafClient;

    let addr = start_sse_leaf().await;

    let mut cfg = mock_server_config("sse-leaf");
    cfg.url = format!("http://127.0.0.1:{}/mcp", addr.port());
    cfg.transport = TransportType::StreamableHttp;

    let client = Arc::new(LeafClient::new(&cfg, &ConnectionPoolConfig::default()));
    let broker = Arc::new(NotificationBroker::new());
    let (_id, mut rx) = broker.subscribe().await;

    let _handle = client
        .listen_notifications(broker.clone())
        .await
        .expect("listener should start on streamable-http leaf");

    let msg = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
        .await
        .expect("notification should arrive")
        .expect("channel open");

    assert_eq!(msg["method"], "notifications/tools/list_changed");
    // `_leaf` tag added by the listener so downstream subscribers can identify
    // which leaf emitted the event.
    assert_eq!(msg["params"]["_leaf"], "sse-leaf");
}
