use std::path::PathBuf;
use std::sync::Arc;

use axum::extract::Request;
use axum::http::{header, StatusCode};
use axum::middleware::Next;
use axum::response::IntoResponse;
use clap::Parser;
use metrics_exporter_prometheus::PrometheusHandle;
use tower_http::cors::CorsLayer;
use utoipa::OpenApi;
use utoipa_swagger_ui::SwaggerUi;

use mcp_federation::aggregator::Aggregator;
use mcp_federation::api::{self, ApiState};
use mcp_federation::config::TransportType;
use mcp_federation::config::{ClientConfig, FederationConfig};
use mcp_federation::crd_discovery;
use mcp_federation::dns_discovery;
use mcp_federation::health::start_health_monitor;
use mcp_federation::mcp::{JsonRpcError, JsonRpcRequest, JsonRpcResponse};
use mcp_federation::notifications::NotificationBroker;
use mcp_federation::rate_limit::RateLimiter;
use mcp_federation::rbac::{self, RequestContext};
use mcp_federation::registry::Registry;
use mcp_federation::router::Router;
use mcp_federation::server::{self, FederationState};
use mcp_federation::session::{self, SessionManager};

#[derive(OpenApi)]
#[openapi(
    info(
        title = "mcp-federation",
        version = env!("CARGO_PKG_VERSION"),
        description = "MCP federation server. Aggregates and routes tool calls across \
                       multiple MCP (Model Context Protocol) leaf servers over JSON-RPC 2.0.",
        license(name = "MIT"),
    ),
    paths(server::handle_mcp, server::delete_mcp, server::get_mcp, server::healthz),
    components(schemas(JsonRpcRequest, JsonRpcResponse, JsonRpcError)),
    tags(
        (name = "mcp", description = "MCP JSON-RPC endpoint"),
        (name = "health", description = "Health check"),
    )
)]
struct ApiDoc;

#[derive(Parser)]
#[command(name = "mcp-federation", about = "MCP federation server")]
struct Cli {
    /// Path to federation config file.
    #[arg(long, env = "MCP_FEDERATION_CONFIG", default_value = "federation.yaml")]
    config: PathBuf,

    /// Run in HTTP server mode.
    /// Without this flag, the server runs in stdio mode.
    #[arg(long)]
    http: bool,

    /// HTTP listen address (only used with --http).
    /// Overrides the listen address in the config file.
    #[arg(long, env = "MCP_FEDERATION_LISTEN")]
    listen: Option<String>,

    /// Bearer token for client authentication.
    /// Overrides the auth_token in the config file.
    #[arg(long, env = "MCP_FEDERATION_AUTH_TOKEN")]
    auth_token: Option<String>,

    /// Path to file containing bearer token for client authentication.
    /// The file is re-read on each request so tokens can be rotated
    /// without a restart. Takes precedence only when `--auth-token` is unset.
    #[arg(long, env = "MCP_FEDERATION_AUTH_TOKEN_FILE")]
    auth_token_file: Option<PathBuf>,

    /// Path to TLS certificate PEM file (enables HTTPS when paired with --tls-key).
    #[arg(long, env = "TLS_CERT")]
    tls_cert: Option<String>,

    /// Path to TLS private key PEM file (enables HTTPS when paired with --tls-cert).
    #[arg(long, env = "TLS_KEY")]
    tls_key: Option<String>,

    /// Log output format: "text" (default) or "json".
    #[arg(long, default_value = "text", env = "LOG_FORMAT")]
    log_format: String,

    /// Load config, initialize leaf connections, print all aggregated tools, then exit.
    #[arg(long)]
    list_tools: bool,

    /// Validate the config file and exit. Does not start any server or connect
    /// to leaves. Prints "Config OK: N servers configured" on success (exit
    /// 0), or the error message and exit 1 on failure.
    #[arg(long)]
    validate: bool,
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();

    // Handle --validate before doing anything else: no tracing, no server, no
    // leaf connections. Just parse+validate the config and exit.
    if cli.validate {
        match FederationConfig::from_file(&cli.config) {
            Ok(cfg) => {
                println!("Config OK: {} servers configured", cfg.servers.len());
                std::process::exit(0);
            }
            Err(e) => {
                eprintln!("Config error: {e}");
                std::process::exit(1);
            }
        }
    }

    let env_filter =
        tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into());

    if cli.log_format == "json" {
        tracing_subscriber::fmt()
            .with_env_filter(env_filter)
            .json()
            .init();
    } else {
        tracing_subscriber::fmt().with_env_filter(env_filter).init();
    }

    let config = FederationConfig::from_file(&cli.config).unwrap_or_else(|e| {
        eprintln!("Failed to load config: {e}");
        std::process::exit(1);
    });

    let listen = cli
        .listen
        .clone()
        .unwrap_or_else(|| config.federation.listen.clone());

    let auth_token = cli
        .auth_token
        .clone()
        .or_else(|| config.federation.auth_token.clone());

    let auth_token_file = cli
        .auth_token_file
        .clone()
        .or_else(|| config.federation.auth_token_file.clone());

    let server_count = config.servers.len();
    let tool_cache_ttl = std::time::Duration::from_secs(config.federation.tool_cache_ttl_seconds);
    let pool_config = config.federation.connection_pool.clone();
    let session_ttl_seconds = config.federation.session_ttl_seconds;
    let session_secret = config.federation.session_secret.clone();
    let shutdown_timeout_seconds = config.federation.shutdown_timeout_seconds;
    let allowed_origins = config.federation.allowed_origins.clone();
    let rate_limit_config = config.federation.rate_limit.clone();
    let clients = config.federation.clients.clone();
    let dns_discovery_config = config.federation.dns_discovery.clone();
    let crd_discovery_config = config.federation.crd_discovery.clone();
    let registry = Arc::new(Registry::from_configs_with_pool(
        config.servers,
        pool_config.clone(),
    ));

    // --list-tools: initialize, dump aggregated tools, exit.
    if cli.list_tools {
        match tokio::time::timeout(
            std::time::Duration::from_secs(10),
            registry.initialize_all(),
        )
        .await
        {
            Ok(()) => {}
            Err(_) => eprintln!("warning: leaf initialization timed out after 10s"),
        }
        let aggregator = Aggregator::with_ttl(registry.clone(), tool_cache_ttl);
        print_tools_table(&aggregator, &registry).await;
        return;
    }

    tracing::info!(
        server_count = server_count,
        "initializing leaf server connections"
    );
    registry.initialize_all().await;

    let healthy_count = registry.healthy_leaves().await.len();
    tracing::info!(
        healthy = healthy_count,
        total = server_count,
        "leaf server initialization complete"
    );

    // Start health monitoring in background
    start_health_monitor(registry.clone()).await;

    // Start optional discovery workers. Both are no-ops when disabled in config.
    dns_discovery::spawn_dns_discovery(dns_discovery_config, registry.clone());
    crd_discovery::start_crd_watcher(crd_discovery_config, registry.clone(), pool_config).await;

    let sessions = Arc::new(SessionManager::new(session_ttl_seconds, session_secret));
    let _prune_handle =
        session::spawn_prune_task(sessions.clone(), std::time::Duration::from_secs(60));

    let notifications = Arc::new(NotificationBroker::new());

    // Best-effort: for every leaf using the Streamable HTTP transport, open a
    // long-lived SSE listener. Leaves that don't support GET SSE will fail
    // fast — that's fine, we just log and move on.
    spawn_leaf_notification_listeners(registry.clone(), notifications.clone()).await;

    let state = Arc::new(FederationState {
        aggregator: Aggregator::with_ttl(registry.clone(), tool_cache_ttl),
        router: Router::new(registry.clone()),
        registry: registry.clone(),
        sessions,
        allowed_origins,
        rate_limiter: Arc::new(RateLimiter::new(rate_limit_config)),
        notifications,
    });

    if cli.http {
        spawn_config_reloader(cli.config.clone(), registry.clone());

        match (&cli.tls_cert, &cli.tls_key) {
            (Some(cert), Some(key)) => {
                run_https(
                    state,
                    &listen,
                    cert,
                    key,
                    auth_token,
                    auth_token_file,
                    clients,
                    shutdown_timeout_seconds,
                )
                .await;
            }
            (None, None) => {
                run_http(state, &listen, auth_token, auth_token_file, clients).await;
            }
            _ => {
                eprintln!("Error: --tls-cert and --tls-key must both be provided for TLS");
                std::process::exit(1);
            }
        }
    } else {
        run_stdio_with_shutdown(state).await;
    }
}

// ---------------------------------------------------------------------------
// Leaf notification listeners
// ---------------------------------------------------------------------------

/// For each leaf configured with `transport: streamable-http`, open a
/// long-lived SSE listener that forwards leaf-emitted notifications into the
/// federation's broker. Non-streamable leaves are silently skipped. Failures
/// on individual leaves are logged and swallowed — the federation still starts.
async fn spawn_leaf_notification_listeners(
    registry: Arc<Registry>,
    broker: Arc<NotificationBroker>,
) {
    let leaves = registry.all_leaves().await;
    for leaf in leaves {
        if leaf.config.transport != TransportType::StreamableHttp {
            continue;
        }
        let alias = leaf.config.alias.clone();
        let broker = broker.clone();
        let client = leaf.client.clone();
        match client.listen_notifications(broker).await {
            Ok(_handle) => {
                tracing::info!(alias = %alias, "started SSE notification listener");
            }
            Err(e) => {
                tracing::debug!(
                    alias = %alias,
                    error = %e,
                    "leaf notification listener not started (leaf may not support SSE)"
                );
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Graceful shutdown (SIGTERM / SIGINT)
// ---------------------------------------------------------------------------

async fn shutdown_signal() {
    let ctrl_c = tokio::signal::ctrl_c();
    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install SIGTERM handler")
            .recv()
            .await;
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {}
        _ = terminate => {}
    }
    tracing::info!("shutdown signal received");
}

async fn run_stdio_with_shutdown(state: Arc<FederationState>) {
    tokio::select! {
        _ = server::run_stdio(state) => {}
        _ = shutdown_signal() => {
            tracing::info!("shutting down gracefully");
        }
    }
}

// ---------------------------------------------------------------------------
// Config hot-reload on SIGHUP (Unix only)
// ---------------------------------------------------------------------------

#[cfg(unix)]
fn spawn_config_reloader(config_path: PathBuf, registry: Arc<Registry>) {
    use mcp_federation::registry::LeafEntry;
    use std::collections::HashSet;

    tokio::spawn(async move {
        let mut sighup = match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())
        {
            Ok(s) => s,
            Err(e) => {
                tracing::error!(error = %e, "failed to install SIGHUP handler");
                return;
            }
        };

        loop {
            if sighup.recv().await.is_none() {
                break;
            }
            tracing::info!(
                path = %config_path.display(),
                "SIGHUP received; reloading config"
            );

            let cfg = match FederationConfig::from_file(&config_path) {
                Ok(c) => c,
                Err(e) => {
                    tracing::error!(error = %e, "config reload failed; keeping current registry");
                    continue;
                }
            };

            let pool_config = registry.pool_config().clone();
            let existing: HashSet<String> = registry.aliases().await.into_iter().collect();
            let mut incoming: HashSet<String> = HashSet::new();

            for server in cfg.servers {
                let alias = server.alias.clone();
                incoming.insert(alias.clone());

                let (is_new, changed) = match registry.get_leaf(&alias).await {
                    None => (true, false),
                    Some(current) => {
                        let same = current.config.url == server.url
                            && current.config.auth_token == server.auth_token
                            && current.config.auth_token_file == server.auth_token_file
                            && current.config.transport == server.transport
                            && current.config.timeout_seconds == server.timeout_seconds;
                        (false, !same)
                    }
                };

                if is_new || changed {
                    let entry = Arc::new(LeafEntry::new_with_pool(server, &pool_config));
                    registry.add_leaf(alias.clone(), entry.clone()).await;
                    let alias_owned = alias.clone();
                    tokio::spawn(async move {
                        match entry.client.initialize().await {
                            Ok(_) => {
                                entry.mark_healthy().await;
                                if let Ok(tools) = entry.client.list_tools().await {
                                    *entry.cached_tools.write().await = tools;
                                    entry.mark_tools_refreshed().await;
                                }
                            }
                            Err(e) => {
                                tracing::warn!(
                                    alias = %alias_owned,
                                    error = %e,
                                    "reloaded leaf failed to initialize"
                                );
                                entry.mark_unhealthy().await;
                            }
                        }
                    });
                    if is_new {
                        tracing::info!(alias = %alias, "added leaf via config reload");
                    } else {
                        tracing::info!(alias = %alias, "replaced leaf via config reload");
                    }
                }
            }

            for alias in existing.difference(&incoming) {
                if registry.remove_leaf(alias).await.is_some() {
                    tracing::info!(alias = %alias, "removed leaf via config reload");
                }
            }
        }
    });
}

#[cfg(not(unix))]
fn spawn_config_reloader(_config_path: PathBuf, _registry: Arc<Registry>) {
    // SIGHUP is a Unix concept; no-op on other platforms.
}

// ---------------------------------------------------------------------------
// Bearer token auth middleware
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct AuthState {
    token: Option<String>,
    token_file: Option<PathBuf>,
    clients: Vec<ClientConfig>,
}

impl AuthState {
    fn resolve_token(&self) -> Option<String> {
        if let Some(ref t) = self.token {
            return Some(t.clone());
        }
        if let Some(ref path) = self.token_file {
            match std::fs::read_to_string(path) {
                Ok(contents) => return Some(contents.trim().to_string()),
                Err(e) => {
                    tracing::warn!(
                        path = %path.display(),
                        error = %e,
                        "failed to read federation auth_token_file"
                    );
                }
            }
        }
        None
    }
}

async fn auth_middleware(
    axum::extract::State(auth): axum::extract::State<AuthState>,
    mut request: Request,
    next: Next,
) -> impl IntoResponse {
    let path = request.uri().path();
    let skip = path == "/healthz"
        || path == "/metrics"
        || path == "/status"
        || path.starts_with("/swagger-ui")
        || path == "/openapi.json";

    match auth.resolve_token() {
        // Server has no auth token configured — attach an anonymous context so
        // handlers still see a RequestContext.
        None => {
            if !skip {
                request.extensions_mut().insert(RequestContext::anonymous());
            }
        }
        // Server enforces auth — resolve the presented bearer token against
        // both the admin token and any per-client entries.
        Some(admin) if !skip => {
            let presented = request
                .headers()
                .get(header::AUTHORIZATION)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.strip_prefix("Bearer "));

            let ctx = match presented {
                Some(tok) => rbac::resolve(tok, &admin, &auth.clients),
                None => None,
            };

            match ctx {
                Some(c) => {
                    request.extensions_mut().insert(c);
                }
                None => return StatusCode::UNAUTHORIZED.into_response(),
            }
        }
        Some(_) => {
            // Skip path — no context needed.
        }
    }

    next.run(request).await.into_response()
}

// ---------------------------------------------------------------------------
// HTTP mode
// ---------------------------------------------------------------------------

fn build_router(
    state: Arc<FederationState>,
    auth_token: Option<String>,
    auth_token_file: Option<PathBuf>,
    clients: Vec<ClientConfig>,
    prometheus_handle: PrometheusHandle,
) -> axum::Router {
    let auth_state = AuthState {
        token: auth_token,
        token_file: auth_token_file,
        clients,
    };

    // The admin REST API for dynamic registration. Nested under /api/v1 so
    // paths resolve to /api/v1/servers/*. Shares the same auth middleware as
    // /mcp — paths under /api/v1 are not in the skip list. Uses `nest_service`
    // because the api router carries its own `ApiState` and finalizes at
    // `Router<()>`, which differs from the outer `Router<Arc<FederationState>>`.
    let api_router = api::router(ApiState::new(state.registry.clone()));

    axum::Router::new()
        .route(
            "/mcp",
            axum::routing::post(server::handle_mcp)
                .get(server::get_mcp)
                .delete(server::delete_mcp),
        )
        .route("/healthz", axum::routing::get(server::healthz))
        .route("/metrics", axum::routing::get(server::metrics_handler))
        .route("/status", axum::routing::get(server::status_handler))
        .nest_service("/api/v1", api_router)
        .merge(SwaggerUi::new("/swagger-ui").url("/openapi.json", ApiDoc::openapi()))
        .layer(axum::middleware::from_fn_with_state(
            auth_state,
            auth_middleware,
        ))
        .layer(axum::Extension(prometheus_handle))
        .layer(CorsLayer::permissive())
        .with_state(state)
}

async fn run_http(
    state: Arc<FederationState>,
    listen: &str,
    auth_token: Option<String>,
    auth_token_file: Option<PathBuf>,
    clients: Vec<ClientConfig>,
) {
    let prometheus_handle = metrics_exporter_prometheus::PrometheusBuilder::new()
        .install_recorder()
        .expect("Failed to install Prometheus recorder");

    let app = build_router(
        state,
        auth_token,
        auth_token_file,
        clients,
        prometheus_handle,
    );

    let listener = tokio::net::TcpListener::bind(listen)
        .await
        .expect("Failed to bind listener");

    tracing::info!("mcp-federation HTTP server listening on {listen}");

    if let Err(e) = axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
    {
        tracing::error!(error = %e, "http server error");
    }
    tracing::info!("shutting down gracefully");
}

async fn run_https(
    state: Arc<FederationState>,
    listen: &str,
    cert_path: &str,
    key_path: &str,
    auth_token: Option<String>,
    auth_token_file: Option<PathBuf>,
    clients: Vec<ClientConfig>,
    shutdown_timeout_seconds: u64,
) {
    let prometheus_handle = metrics_exporter_prometheus::PrometheusBuilder::new()
        .install_recorder()
        .expect("Failed to install Prometheus recorder");

    let app = build_router(
        state,
        auth_token,
        auth_token_file,
        clients,
        prometheus_handle,
    );

    let config = axum_server::tls_rustls::RustlsConfig::from_pem_file(cert_path, key_path)
        .await
        .expect("Failed to load TLS cert/key");

    let addr: std::net::SocketAddr = listen.parse().expect("Invalid listen address");

    tracing::info!("mcp-federation HTTPS server listening on {listen}");

    let handle = axum_server::Handle::new();
    let handle_clone = handle.clone();
    let shutdown_timeout = std::time::Duration::from_secs(shutdown_timeout_seconds);
    tokio::spawn(async move {
        shutdown_signal().await;
        tracing::info!(
            timeout_seconds = shutdown_timeout_seconds,
            "shutting down gracefully"
        );
        handle_clone.graceful_shutdown(Some(shutdown_timeout));
    });

    if let Err(e) = axum_server::bind_rustls(addr, config)
        .handle(handle)
        .serve(app.into_make_service())
        .await
    {
        tracing::error!(error = %e, "https server error");
    }
}

async fn print_tools_table(aggregator: &Aggregator, registry: &Registry) {
    let tools = aggregator.aggregated_tools().await;

    let mut rows: Vec<(String, String, String)> = Vec::with_capacity(tools.len());
    for tool in &tools {
        let namespaced = tool.get("name").and_then(|v| v.as_str()).unwrap_or("");
        let description = tool
            .get("description")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let (alias, name) = match namespaced.split_once("__") {
            Some((a, n)) => (a.to_string(), n.to_string()),
            None => (String::new(), namespaced.to_string()),
        };
        rows.push((alias, name, description));
    }
    rows.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));

    let alias_w = rows
        .iter()
        .map(|r| r.0.len())
        .chain(std::iter::once("Leaf".len()))
        .max()
        .unwrap_or(4);
    let name_w = rows
        .iter()
        .map(|r| r.1.len())
        .chain(std::iter::once("Tool".len()))
        .max()
        .unwrap_or(4);

    println!(
        "{:<alias_w$}  {:<name_w$}  Description",
        "Leaf",
        "Tool",
        alias_w = alias_w,
        name_w = name_w,
    );
    println!(
        "{:<alias_w$}  {:<name_w$}  -----------",
        "-".repeat(alias_w),
        "-".repeat(name_w),
        alias_w = alias_w,
        name_w = name_w,
    );
    for (alias, name, desc) in &rows {
        println!(
            "{:<alias_w$}  {:<name_w$}  {}",
            alias,
            name,
            desc,
            alias_w = alias_w,
            name_w = name_w,
        );
    }

    let leaves = registry.all_leaves().await;
    let mut unreachable: Vec<String> = Vec::new();
    for leaf in &leaves {
        if *leaf.health.read().await == mcp_federation::registry::LeafHealth::Unhealthy {
            unreachable.push(leaf.config.alias.clone());
        }
    }
    if !unreachable.is_empty() {
        eprintln!(
            "\nwarning: unreachable leaves omitted: {}",
            unreachable.join(", ")
        );
    }
}
