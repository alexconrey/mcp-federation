//! Admin REST API for dynamic leaf-server registration.
//!
//! These endpoints let leaf servers self-register (and deregister) at
//! runtime without editing `federation.yaml` and sending SIGHUP. They live
//! under `/api/v1/servers` and are guarded by the same bearer-token auth
//! middleware as `/mcp`.
//!
//! These are plain REST — they are NOT part of the MCP JSON-RPC surface.

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use crate::config::{HealthCheckConfig, ServerConfig, TlsConfig, TransportType};
use crate::registry::{LeafEntry, LeafHealth, Registry};

/// Prefix used for aliases registered via the dynamic REST API. Lets us
/// distinguish these from static config leaves, DNS-discovered leaves
/// (`dns-`), and CRD-discovered leaves (`crd-`) when reconciling.
///
/// Applied only when the caller omits the prefix in their request — callers
/// are free to supply already-prefixed or arbitrary aliases if they need to
/// match a particular naming convention.
pub const DYNAMIC_ALIAS_PREFIX: &str = "";

/// Application state shared by the admin API handlers. Wraps just the
/// Registry — the aggregator and router see updates transparently since they
/// hold their own `Arc<Registry>`.
#[derive(Clone)]
pub struct ApiState {
    pub registry: Arc<Registry>,
}

impl ApiState {
    pub fn new(registry: Arc<Registry>) -> Self {
        Self { registry }
    }
}

/// Request body for `POST /api/v1/servers`.
#[derive(Debug, Deserialize)]
pub struct RegisterRequest {
    pub alias: String,
    pub url: String,
    #[serde(default = "default_transport")]
    pub transport: TransportType,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub auth_token: Option<String>,
    #[serde(default)]
    pub timeout_seconds: Option<u64>,
}

fn default_transport() -> TransportType {
    TransportType::HttpPost
}

/// Server info returned by list/get/register.
#[derive(Debug, Serialize)]
pub struct ServerInfoResponse {
    pub alias: String,
    pub url: String,
    pub transport: String,
    pub tags: Vec<String>,
    pub health: String,
    pub tool_count: usize,
}

#[derive(Debug, Serialize)]
struct ErrorBody {
    error: String,
}

/// Build the admin API router. Mount this under `/api/v1` from the parent
/// router so paths resolve to `/api/v1/servers/*`.
pub fn router(state: ApiState) -> Router {
    Router::new()
        .route("/servers", post(register_server).get(list_servers))
        .route(
            "/servers/{alias}",
            get(get_server).delete(deregister_server),
        )
        .with_state(state)
}

async fn register_server(
    State(state): State<ApiState>,
    Json(req): Json<RegisterRequest>,
) -> axum::response::Response {
    if req.alias.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(ErrorBody {
                error: "alias must not be empty".to_string(),
            }),
        )
            .into_response();
    }
    if req.alias.contains("__") {
        return (
            StatusCode::BAD_REQUEST,
            Json(ErrorBody {
                error: format!(
                    "alias '{}' cannot contain '__' (reserved as namespace separator)",
                    req.alias
                ),
            }),
        )
            .into_response();
    }
    if req.url.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(ErrorBody {
                error: "url must not be empty".to_string(),
            }),
        )
            .into_response();
    }

    if state.registry.get_leaf(&req.alias).await.is_some() {
        return (
            StatusCode::CONFLICT,
            Json(ErrorBody {
                error: format!("server '{}' already registered", req.alias),
            }),
        )
            .into_response();
    }

    let config = ServerConfig {
        alias: req.alias.clone(),
        url: req.url.clone(),
        auth_token: req.auth_token.clone(),
        auth_token_file: None,
        transport: req.transport.clone(),
        tags: req.tags.clone(),
        health_check: HealthCheckConfig::default(),
        timeout_seconds: req.timeout_seconds,
        tls: TlsConfig::default(),
    };

    let entry = Arc::new(LeafEntry::new_with_pool(
        config,
        state.registry.pool_config(),
    ));
    state
        .registry
        .add_leaf(req.alias.clone(), entry.clone())
        .await;

    // Kick off the MCP handshake + tool cache warm-up in the background so
    // the REST call returns promptly even for slow leaves.
    let alias_owned = req.alias.clone();
    tokio::spawn(async move {
        match entry.client.initialize().await {
            Ok(init_result) => {
                entry.mark_healthy().await;
                if let Some(server_info) = init_result.get("serverInfo").cloned() {
                    *entry.server_info.write().await = Some(server_info);
                }
                if let Ok(tools) = entry.client.list_tools().await {
                    *entry.cached_tools.write().await = tools;
                    entry.mark_tools_refreshed().await;
                }
                tracing::info!(
                    alias = %alias_owned,
                    "dynamic-registered leaf initialized"
                );
            }
            Err(e) => {
                tracing::warn!(
                    alias = %alias_owned,
                    error = %e,
                    "dynamic-registered leaf failed to initialize"
                );
                entry.mark_unhealthy().await;
            }
        }
    });

    let leaf = state
        .registry
        .get_leaf(&req.alias)
        .await
        .expect("just inserted");
    let body = to_server_info(&leaf).await;
    tracing::info!(alias = %req.alias, "registered leaf via REST API");
    (StatusCode::CREATED, Json(body)).into_response()
}

async fn deregister_server(
    State(state): State<ApiState>,
    Path(alias): Path<String>,
) -> axum::response::Response {
    match state.registry.remove_leaf(&alias).await {
        Some(leaf) => {
            let body = to_server_info(&leaf).await;
            tracing::info!(alias = %alias, "deregistered leaf via REST API");
            (StatusCode::OK, Json(body)).into_response()
        }
        None => (
            StatusCode::NOT_FOUND,
            Json(ErrorBody {
                error: format!("server '{alias}' not found"),
            }),
        )
            .into_response(),
    }
}

async fn list_servers(State(state): State<ApiState>) -> axum::response::Response {
    let leaves = state.registry.all_leaves().await;
    let mut infos: Vec<ServerInfoResponse> = Vec::with_capacity(leaves.len());
    for leaf in &leaves {
        infos.push(to_server_info(leaf).await);
    }
    (StatusCode::OK, Json(infos)).into_response()
}

async fn get_server(
    State(state): State<ApiState>,
    Path(alias): Path<String>,
) -> axum::response::Response {
    match state.registry.get_leaf(&alias).await {
        Some(leaf) => (StatusCode::OK, Json(to_server_info(&leaf).await)).into_response(),
        None => (
            StatusCode::NOT_FOUND,
            Json(ErrorBody {
                error: format!("server '{alias}' not found"),
            }),
        )
            .into_response(),
    }
}

async fn to_server_info(leaf: &Arc<LeafEntry>) -> ServerInfoResponse {
    let health = *leaf.health.read().await;
    let health_str = match health {
        LeafHealth::Unknown => "unknown",
        LeafHealth::Healthy => "healthy",
        LeafHealth::Unhealthy => "unhealthy",
    };
    let tool_count = leaf.cached_tools.read().await.len();
    ServerInfoResponse {
        alias: leaf.config.alias.clone(),
        url: leaf.config.url.clone(),
        transport: format!("{:?}", leaf.config.transport),
        tags: leaf.config.tags.clone(),
        health: health_str.to_string(),
        tool_count,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    async fn make_app() -> Router {
        let registry = Arc::new(Registry::new());
        let state = ApiState { registry };
        router(state)
    }

    fn json_body<T: Serialize>(v: &T) -> Body {
        Body::from(serde_json::to_vec(v).unwrap())
    }

    async fn body_json(response: axum::response::Response) -> serde_json::Value {
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        serde_json::from_slice(&body).unwrap()
    }

    #[tokio::test]
    async fn register_creates_and_returns_201() {
        let app = make_app().await;
        let req = Request::builder()
            .method("POST")
            .uri("/servers")
            .header("content-type", "application/json")
            .body(json_body(&serde_json::json!({
                "alias": "new-k8s",
                "url": "http://new-k8s:8080/mcp",
                "transport": "http-post",
                "tags": ["kubernetes"],
            })))
            .unwrap();
        let response = app.oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::CREATED);
        let json = body_json(response).await;
        assert_eq!(json["alias"], "new-k8s");
        assert_eq!(json["url"], "http://new-k8s:8080/mcp");
    }

    #[tokio::test]
    async fn register_conflict_returns_409() {
        let app = make_app().await;

        // First registration succeeds.
        let req = Request::builder()
            .method("POST")
            .uri("/servers")
            .header("content-type", "application/json")
            .body(json_body(&serde_json::json!({
                "alias": "dup",
                "url": "http://dup:8080/mcp"
            })))
            .unwrap();
        let response = app.clone().oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::CREATED);

        // Second with same alias fails.
        let req = Request::builder()
            .method("POST")
            .uri("/servers")
            .header("content-type", "application/json")
            .body(json_body(&serde_json::json!({
                "alias": "dup",
                "url": "http://dup2:8080/mcp"
            })))
            .unwrap();
        let response = app.oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::CONFLICT);
    }

    #[tokio::test]
    async fn register_rejects_alias_with_separator() {
        let app = make_app().await;
        let req = Request::builder()
            .method("POST")
            .uri("/servers")
            .header("content-type", "application/json")
            .body(json_body(&serde_json::json!({
                "alias": "bad__alias",
                "url": "http://x:8080/mcp"
            })))
            .unwrap();
        let response = app.oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn register_rejects_empty_url() {
        let app = make_app().await;
        let req = Request::builder()
            .method("POST")
            .uri("/servers")
            .header("content-type", "application/json")
            .body(json_body(&serde_json::json!({
                "alias": "leaf",
                "url": ""
            })))
            .unwrap();
        let response = app.oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn list_returns_all_servers() {
        let registry = Arc::new(Registry::new());
        let state = ApiState {
            registry: registry.clone(),
        };
        let app = router(state);

        for i in 0..3 {
            let req = Request::builder()
                .method("POST")
                .uri("/servers")
                .header("content-type", "application/json")
                .body(json_body(&serde_json::json!({
                    "alias": format!("leaf-{i}"),
                    "url": format!("http://leaf-{i}:8080/mcp"),
                })))
                .unwrap();
            let response = app.clone().oneshot(req).await.unwrap();
            assert_eq!(response.status(), StatusCode::CREATED);
        }

        let req = Request::builder()
            .method("GET")
            .uri("/servers")
            .body(Body::empty())
            .unwrap();
        let response = app.oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let json = body_json(response).await;
        let arr = json.as_array().unwrap();
        assert_eq!(arr.len(), 3);
    }

    #[tokio::test]
    async fn get_server_returns_specific() {
        let app = make_app().await;
        let req = Request::builder()
            .method("POST")
            .uri("/servers")
            .header("content-type", "application/json")
            .body(json_body(&serde_json::json!({
                "alias": "target",
                "url": "http://target:8080/mcp"
            })))
            .unwrap();
        app.clone().oneshot(req).await.unwrap();

        let req = Request::builder()
            .method("GET")
            .uri("/servers/target")
            .body(Body::empty())
            .unwrap();
        let response = app.oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let json = body_json(response).await;
        assert_eq!(json["alias"], "target");
    }

    #[tokio::test]
    async fn get_server_missing_returns_404() {
        let app = make_app().await;
        let req = Request::builder()
            .method("GET")
            .uri("/servers/nonexistent")
            .body(Body::empty())
            .unwrap();
        let response = app.oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn deregister_removes_and_returns_info() {
        let app = make_app().await;
        let req = Request::builder()
            .method("POST")
            .uri("/servers")
            .header("content-type", "application/json")
            .body(json_body(&serde_json::json!({
                "alias": "removable",
                "url": "http://removable:8080/mcp"
            })))
            .unwrap();
        app.clone().oneshot(req).await.unwrap();

        let req = Request::builder()
            .method("DELETE")
            .uri("/servers/removable")
            .body(Body::empty())
            .unwrap();
        let response = app.clone().oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let json = body_json(response).await;
        assert_eq!(json["alias"], "removable");

        // Second delete is a 404.
        let req = Request::builder()
            .method("DELETE")
            .uri("/servers/removable")
            .body(Body::empty())
            .unwrap();
        let response = app.oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }
}
