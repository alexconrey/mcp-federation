//! Kubernetes CRD-based discovery.
//!
//! The `MCPServer` custom resource declares a leaf MCP server the federation
//! should track. When the binary is built with `--features crd` and
//! `federation.crd_discovery.enabled: true`, a controller watches the CRD
//! and calls `Registry::add_leaf` / `Registry::remove_leaf` accordingly.
//!
//! ## CRD schema (see `helm/mcp-federation/crds/mcpserver.yaml`)
//!
//! ```yaml
//! apiVersion: federation.mcp.io/v1alpha1
//! kind: MCPServer
//! metadata:
//!   name: prod-k8s
//!   namespace: mcp-system
//! spec:
//!   alias: prod-k8s
//!   url: http://mcp-k8s.mcp-system.svc:8080/mcp
//!   transport: http-post
//!   tags:
//!     - kubernetes
//!     - production
//! ```
//!
//! Aliases are namespaced (`crd-<namespace>-<name>`) so CR-owned leaves stay
//! distinct from static/DNS/dynamic leaves and can be pruned safely.

use std::sync::Arc;

use serde::{Deserialize, Serialize};

#[cfg(feature = "crd")]
use schemars::JsonSchema;

use crate::config::{ConnectionPoolConfig, CrdDiscoveryConfig};
use crate::registry::Registry;

pub const CRD_API_VERSION: &str = "federation.mcp.io/v1alpha1";
pub const CRD_KIND: &str = "MCPServer";
/// Prefix stamped on aliases owned by the CRD controller.
pub const CRD_ALIAS_PREFIX: &str = "crd-";

/// Spec for the `MCPServer` CRD. Mirrors the YAML in
/// `helm/mcp-federation/crds/mcpserver.yaml` — `camelCase` on the wire so
/// `authToken`/`authTokenFile`/`timeoutSeconds` decode straight from the CR.
#[cfg_attr(
    feature = "crd",
    derive(kube::CustomResource, JsonSchema),
    kube(
        group = "federation.mcp.io",
        version = "v1alpha1",
        kind = "MCPServer",
        namespaced,
        status = "MCPServerStatus",
        printcolumn = r#"{"name":"URL","type":"string","jsonPath":".spec.url"}"#,
        printcolumn = r#"{"name":"Transport","type":"string","jsonPath":".spec.transport"}"#,
        printcolumn = r#"{"name":"Health","type":"string","jsonPath":".status.health"}"#,
        printcolumn = r#"{"name":"Tools","type":"integer","jsonPath":".status.toolCount"}"#,
        printcolumn = r#"{"name":"Last Synced","type":"string","jsonPath":".status.lastSynced"}"#
    )
)]
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct MCPServerSpec {
    /// Federation alias to register the leaf under. Must not contain the
    /// namespace separator (`__`).
    pub alias: String,
    /// Full MCP endpoint URL (e.g. `http://mcp-k8s.mcp-system.svc:8080/mcp`).
    pub url: String,
    /// Transport type. Defaults to `http-post`.
    #[serde(default = "default_crd_transport")]
    pub transport: String,
    /// Optional free-form tags surfaced via `federation__list_servers`.
    #[serde(default)]
    pub tags: Vec<String>,
    /// Optional bearer token for the leaf (rarely used from a CR — prefer
    /// mounting a Secret and pointing `authTokenFile` at it).
    #[serde(default)]
    pub auth_token: Option<String>,
    /// Optional path to a file containing the leaf's bearer token.
    #[serde(default)]
    pub auth_token_file: Option<String>,
    #[serde(default)]
    pub timeout_seconds: Option<u64>,
}

/// Status subresource populated by the controller.
#[cfg_attr(feature = "crd", derive(JsonSchema))]
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(rename_all = "camelCase")]
pub struct MCPServerStatus {
    pub health: Option<String>,
    pub tool_count: Option<i32>,
    /// RFC 3339 timestamp of the most recent successful reconcile.
    pub last_synced: Option<String>,
}

/// Finalizer added to every reconciled `MCPServer` so we get one final
/// reconcile pass (with `deletionTimestamp` set) to remove the leaf from
/// the registry even if the controller was briefly offline during deletion.
pub const FINALIZER: &str = "federation.mcp.io/cleanup";

fn default_crd_transport() -> String {
    "http-post".to_string()
}

/// Build the alias the controller registers a given CR under. Kept public so
/// the controller and any diagnostics tooling agree on the naming rule.
pub fn crd_alias(namespace: &str, name: &str) -> String {
    format!("{CRD_ALIAS_PREFIX}{namespace}-{name}")
}

// ---------------------------------------------------------------------------
// Feature-gated controller
// ---------------------------------------------------------------------------

#[cfg(feature = "crd")]
mod controller {
    use super::*;

    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    use futures::StreamExt;
    use kube::{
        api::{Api, ListParams, Patch, PatchParams},
        runtime::{
            controller::{Action, Controller},
            watcher,
        },
        Client, ResourceExt,
    };

    use crate::config::{HealthCheckConfig, ServerConfig, TlsConfig, TransportType};
    use crate::registry::LeafEntry;

    const FIELD_MANAGER: &str = "mcp-federation";

    struct Ctx {
        client: Client,
        registry: Arc<Registry>,
        pool_config: ConnectionPoolConfig,
    }

    #[derive(Debug, thiserror::Error)]
    enum ReconcileError {
        #[error("invalid transport '{0}' (expected http-post or streamable-http)")]
        InvalidTransport(String),
        #[error("kube api error: {0}")]
        Kube(#[from] kube::Error),
    }

    /// RFC 3339 (UTC, seconds precision) timestamp — hand-rolled so we don't
    /// have to pull in `chrono` just for status timestamps. Uses the proleptic
    /// Gregorian civil-from-days conversion from Howard Hinnant's date
    /// algorithms.
    pub(super) fn rfc3339_now() -> String {
        let secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        rfc3339_from_unix(secs)
    }

    pub(super) fn rfc3339_from_unix(secs: u64) -> String {
        let days = (secs / 86_400) as i64;
        let sod = secs % 86_400;
        let hour = sod / 3_600;
        let minute = (sod % 3_600) / 60;
        let second = sod % 60;

        // Civil from days (Hinnant). Shifted so day 0 = 0000-03-01.
        let z = days + 719_468;
        let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
        let doe = (z - era * 146_097) as u64; // [0, 146096]
        let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
        let y = yoe as i64 + era * 400;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let day = doy - (153 * mp + 2) / 5 + 1;
        let month = if mp < 10 { mp + 3 } else { mp - 9 };
        let year = if month <= 2 { y + 1 } else { y };

        format!(
            "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
            year, month, day, hour, minute, second
        )
    }

    fn parse_transport(s: &str) -> Result<TransportType, ReconcileError> {
        match s {
            "http-post" => Ok(TransportType::HttpPost),
            "streamable-http" => Ok(TransportType::StreamableHttp),
            other => Err(ReconcileError::InvalidTransport(other.to_string())),
        }
    }

    async fn reconcile(obj: Arc<MCPServer>, ctx: Arc<Ctx>) -> Result<Action, ReconcileError> {
        let namespace = obj.namespace().unwrap_or_default();
        let name = obj.name_any();
        let alias = crd_alias(&namespace, &name);
        let api: Api<MCPServer> = Api::namespaced(ctx.client.clone(), &namespace);

        // Kubernetes signals deletion by setting `metadata.deletionTimestamp`.
        // If our finalizer is present, do the cleanup and strip the finalizer
        // so the apiserver can actually GC the CR.
        if obj.metadata.deletion_timestamp.is_some() {
            if obj.finalizers().iter().any(|f| f == FINALIZER) {
                if ctx.registry.remove_leaf(&alias).await.is_some() {
                    tracing::info!(alias = %alias, "CRD finalizer: removed leaf");
                }
                let remaining: Vec<&String> = obj
                    .finalizers()
                    .iter()
                    .filter(|f| f.as_str() != FINALIZER)
                    .collect();
                let patch = serde_json::json!({
                    "metadata": { "finalizers": remaining }
                });
                api.patch(&name, &PatchParams::default(), &Patch::Merge(&patch))
                    .await?;
            }
            return Ok(Action::await_change());
        }

        // Ensure our finalizer is set before we start owning external state
        // (a registered leaf, cached tools). Requeues immediately — the follow-up
        // reconcile sees the updated object and does the real work.
        if !obj.finalizers().iter().any(|f| f == FINALIZER) {
            let mut finalizers: Vec<String> = obj.finalizers().to_vec();
            finalizers.push(FINALIZER.to_string());
            let patch = serde_json::json!({
                "metadata": { "finalizers": finalizers }
            });
            api.patch(&name, &PatchParams::default(), &Patch::Merge(&patch))
                .await?;
            return Ok(Action::requeue(Duration::from_secs(1)));
        }

        let spec = &obj.spec;
        let transport = parse_transport(&spec.transport)?;

        let server_config = ServerConfig {
            alias: alias.clone(),
            url: spec.url.clone(),
            auth_token: spec.auth_token.clone(),
            auth_token_file: spec.auth_token_file.as_ref().map(std::path::PathBuf::from),
            transport,
            tags: {
                let mut tags = spec.tags.clone();
                tags.push("crd-discovered".to_string());
                tags
            },
            health_check: HealthCheckConfig::default(),
            timeout_seconds: spec.timeout_seconds,
            tls: TlsConfig::default(),
        };

        // Replace or insert unconditionally on every reconcile. add_leaf drops
        // any previous entry so an updated URL/token takes effect immediately.
        let entry = Arc::new(LeafEntry::new_with_pool(server_config, &ctx.pool_config));
        ctx.registry.add_leaf(alias.clone(), entry.clone()).await;

        let alias_owned = alias.clone();
        let status_client = ctx.client.clone();
        let status_ns = namespace.clone();
        let status_name = name.clone();
        tokio::spawn(async move {
            let init_ok = match entry.client.initialize().await {
                Ok(init_result) => {
                    entry.mark_healthy().await;
                    if let Some(server_info) = init_result.get("serverInfo").cloned() {
                        *entry.server_info.write().await = Some(server_info);
                    }
                    if let Ok(tools) = entry.client.list_tools().await {
                        *entry.cached_tools.write().await = tools;
                        entry.mark_tools_refreshed().await;
                    }
                    true
                }
                Err(e) => {
                    tracing::warn!(
                        alias = %alias_owned,
                        error = %e,
                        "CRD-discovered leaf failed to initialize"
                    );
                    entry.mark_unhealthy().await;
                    false
                }
            };

            let status = MCPServerStatus {
                health: Some(if init_ok && entry.is_healthy().await {
                    "Healthy".into()
                } else {
                    "Unhealthy".into()
                }),
                tool_count: Some(entry.cached_tools.read().await.len() as i32),
                last_synced: Some(rfc3339_now()),
            };
            let status_patch = serde_json::json!({ "status": status });
            let api: Api<MCPServer> = Api::namespaced(status_client, &status_ns);
            if let Err(e) = api
                .patch_status(
                    &status_name,
                    &PatchParams::apply(FIELD_MANAGER),
                    &Patch::Merge(&status_patch),
                )
                .await
            {
                tracing::warn!(
                    alias = %alias_owned,
                    error = %e,
                    "CRD status writeback failed"
                );
            }
        });

        tracing::info!(alias = %alias, url = %spec.url, "CRD discovery: reconciled leaf");
        Ok(Action::requeue(Duration::from_secs(300)))
    }

    fn error_policy(_obj: Arc<MCPServer>, err: &ReconcileError, _ctx: Arc<Ctx>) -> Action {
        tracing::warn!(error = %err, "CRD reconcile error");
        Action::requeue(Duration::from_secs(30))
    }

    pub async fn run(
        config: CrdDiscoveryConfig,
        registry: Arc<Registry>,
        pool: ConnectionPoolConfig,
    ) {
        let client = match Client::try_default().await {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(error = %e, "no k8s client available; CRD watcher disabled");
                return;
            }
        };

        let api: Api<MCPServer> = if config.namespace.trim().is_empty() {
            Api::all(client.clone())
        } else {
            Api::namespaced(client.clone(), &config.namespace)
        };

        // Guard against a missing CRD: listing surfaces the error early so we
        // don't crash the controller in a hot loop later.
        if let Err(e) = api.list(&ListParams::default().limit(1)).await {
            tracing::warn!(
                error = %e,
                "CRD watcher: initial list failed (CRD not installed?); watcher not started"
            );
            return;
        }

        tracing::info!(
            namespace = %if config.namespace.is_empty() { "<all>".to_string() } else { config.namespace.clone() },
            "starting CRD controller for MCPServer"
        );

        let ctx = Arc::new(Ctx {
            client,
            registry,
            pool_config: pool,
        });
        let controller = Controller::new(api, watcher::Config::default())
            .run(reconcile, error_policy, ctx)
            .for_each(|res| async move {
                match res {
                    Ok((obj, _action)) => {
                        tracing::debug!(name = %obj.name, "CRD reconciled");
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "CRD controller error");
                    }
                }
            });

        tokio::spawn(controller);
    }
}

/// Start the CRD watcher. No-op when the `crd` feature is disabled or when
/// `enabled = false` in config.
#[cfg(feature = "crd")]
pub async fn start_crd_watcher(
    config: CrdDiscoveryConfig,
    registry: Arc<Registry>,
    pool: ConnectionPoolConfig,
) {
    if !config.enabled {
        tracing::debug!("CRD discovery disabled");
        return;
    }
    controller::run(config, registry, pool).await;
}

#[cfg(not(feature = "crd"))]
pub async fn start_crd_watcher(
    config: CrdDiscoveryConfig,
    _registry: Arc<Registry>,
    _pool: ConnectionPoolConfig,
) {
    if config.enabled {
        tracing::warn!(
            "crd_discovery.enabled=true but binary was built without --features crd; \
             CRD watcher is a no-op"
        );
    } else {
        tracing::debug!("CRD discovery disabled");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crd_alias_format() {
        assert_eq!(
            crd_alias("mcp-system", "prod-k8s"),
            "crd-mcp-system-prod-k8s"
        );
    }

    #[test]
    fn spec_deserializes_from_yaml() {
        let yaml = r#"
alias: prod-k8s
url: http://mcp-k8s.mcp-system.svc:8080/mcp
transport: http-post
tags:
  - kubernetes
  - production
"#;
        let spec: MCPServerSpec = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(spec.alias, "prod-k8s");
        assert_eq!(spec.url, "http://mcp-k8s.mcp-system.svc:8080/mcp");
        assert_eq!(spec.transport, "http-post");
        assert_eq!(spec.tags, vec!["kubernetes", "production"]);
    }

    #[test]
    fn spec_transport_default() {
        let yaml = r#"
alias: minimal
url: http://leaf:8080/mcp
"#;
        let spec: MCPServerSpec = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(spec.transport, "http-post");
    }

    #[test]
    fn finalizer_constant_is_stable() {
        // The finalizer string is a promise to any MCPServer CR that already
        // has it on disk — changing it would strand old CRs with an unknown
        // finalizer no one clears. Pin it.
        assert_eq!(FINALIZER, "federation.mcp.io/cleanup");
    }

    #[test]
    fn status_serializes_camelcase() {
        // apiserver stores camelCase; the status subresource must round-trip
        // through the same names or `kubectl get -o jsonpath=.status.toolCount`
        // silently returns nothing.
        let status = MCPServerStatus {
            health: Some("Healthy".into()),
            tool_count: Some(7),
            last_synced: Some("2026-07-24T12:00:00Z".into()),
        };
        let v = serde_json::to_value(&status).unwrap();
        assert_eq!(v["health"], "Healthy");
        assert_eq!(v["toolCount"], 7);
        assert_eq!(v["lastSynced"], "2026-07-24T12:00:00Z");
        assert!(v.get("last_synced").is_none());
    }

    #[test]
    fn status_patch_shape() {
        let status = MCPServerStatus {
            health: Some("Unhealthy".into()),
            tool_count: Some(0),
            last_synced: Some("2026-01-01T00:00:00Z".into()),
        };
        let patch = serde_json::json!({ "status": status });
        assert_eq!(patch["status"]["health"], "Unhealthy");
        assert_eq!(patch["status"]["toolCount"], 0);
        assert_eq!(patch["status"]["lastSynced"], "2026-01-01T00:00:00Z");
    }

    #[cfg(feature = "crd")]
    #[test]
    fn rfc3339_from_unix_known_values() {
        // Epoch.
        assert_eq!(
            super::controller::rfc3339_from_unix(0),
            "1970-01-01T00:00:00Z"
        );
        // 2026-07-24T00:00:00Z = 1_784_851_200 (post leap-day 2024).
        assert_eq!(
            super::controller::rfc3339_from_unix(1_784_851_200),
            "2026-07-24T00:00:00Z"
        );
        // A random second within a day rolls hours/minutes/seconds correctly.
        assert_eq!(
            super::controller::rfc3339_from_unix(1_784_851_200 + 3_723),
            "2026-07-24T01:02:03Z"
        );
        // Leap-year Feb 29 (2024) — verifies the civil-from-days path.
        assert_eq!(
            super::controller::rfc3339_from_unix(1_709_164_800),
            "2024-02-29T00:00:00Z"
        );
    }

    #[test]
    fn spec_deserializes_camelcase_auth_fields() {
        // The CRD schema uses camelCase — make sure we parse authToken /
        // authTokenFile / timeoutSeconds the way apiserver ships them.
        let yaml = r#"
alias: secure-leaf
url: https://secure:8443/mcp
authToken: "shhh"
authTokenFile: "/var/run/secrets/leaf/token"
timeoutSeconds: 60
"#;
        let spec: MCPServerSpec = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(spec.auth_token.as_deref(), Some("shhh"));
        assert_eq!(
            spec.auth_token_file.as_deref(),
            Some("/var/run/secrets/leaf/token")
        );
        assert_eq!(spec.timeout_seconds, Some(60));
    }
}
