use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::RwLock;

use crate::config::{ConnectionPoolConfig, ServerConfig};
use crate::leaf_client::LeafClient;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum LeafHealth {
    Unknown,
    Healthy,
    Unhealthy,
}

pub struct LeafEntry {
    pub config: ServerConfig,
    pub client: Arc<LeafClient>,
    pub cached_tools: RwLock<Vec<serde_json::Value>>,
    pub cached_resources: RwLock<Vec<serde_json::Value>>,
    pub cached_prompts: RwLock<Vec<serde_json::Value>>,
    pub health: RwLock<LeafHealth>,
    pub last_health_check: RwLock<Option<Instant>>,
    pub last_tool_refresh: RwLock<Option<Instant>>,
    pub consecutive_failures: RwLock<u32>,
    /// Full `serverInfo` object captured from the leaf's `initialize` response.
    /// Used to detect nested federations (`serverInfo.name == "mcp-federation"`).
    pub server_info: RwLock<Option<serde_json::Value>>,
}

impl LeafEntry {
    pub fn new(config: ServerConfig) -> Self {
        Self::new_with_pool(config, &ConnectionPoolConfig::default())
    }

    pub fn new_with_pool(config: ServerConfig, pool: &ConnectionPoolConfig) -> Self {
        let client = Arc::new(LeafClient::new(&config, pool));
        Self {
            config,
            client,
            cached_tools: RwLock::new(Vec::new()),
            cached_resources: RwLock::new(Vec::new()),
            cached_prompts: RwLock::new(Vec::new()),
            health: RwLock::new(LeafHealth::Unknown),
            last_health_check: RwLock::new(None),
            last_tool_refresh: RwLock::new(None),
            consecutive_failures: RwLock::new(0),
            server_info: RwLock::new(None),
        }
    }

    pub async fn mark_healthy(&self) {
        *self.health.write().await = LeafHealth::Healthy;
        *self.last_health_check.write().await = Some(Instant::now());
        *self.consecutive_failures.write().await = 0;
    }

    pub async fn mark_unhealthy(&self) {
        *self.health.write().await = LeafHealth::Unhealthy;
        *self.last_health_check.write().await = Some(Instant::now());
        let mut failures = self.consecutive_failures.write().await;
        *failures += 1;
    }

    /// Increment the consecutive-failure counter without flipping the leaf's
    /// health state. Used by the circuit breaker so a single blip does not
    /// immediately mark a leaf unhealthy.
    pub async fn record_failure(&self) -> u32 {
        *self.last_health_check.write().await = Some(Instant::now());
        let mut failures = self.consecutive_failures.write().await;
        *failures += 1;
        *failures
    }

    pub async fn is_healthy(&self) -> bool {
        *self.health.read().await != LeafHealth::Unhealthy
    }

    pub async fn mark_tools_refreshed(&self) {
        *self.last_tool_refresh.write().await = Some(Instant::now());
    }

    pub async fn tools_stale(&self, ttl: Duration) -> bool {
        match *self.last_tool_refresh.read().await {
            None => true,
            Some(last) => last.elapsed() >= ttl,
        }
    }

    /// Returns true when the leaf's cached `serverInfo.name` identifies it as
    /// another `mcp-federation` instance. Used to build nested-federation
    /// topology reports.
    pub async fn is_federation(&self) -> bool {
        self.server_info
            .read()
            .await
            .as_ref()
            .and_then(|v| v.get("name"))
            .and_then(|n| n.as_str())
            .map(|s| s == "mcp-federation")
            .unwrap_or(false)
    }
}

pub struct Registry {
    leaves: RwLock<HashMap<String, Arc<LeafEntry>>>,
    pool_config: ConnectionPoolConfig,
}

impl Registry {
    pub fn new() -> Self {
        Self::with_pool(ConnectionPoolConfig::default())
    }

    pub fn with_pool(pool_config: ConnectionPoolConfig) -> Self {
        Self {
            leaves: RwLock::new(HashMap::new()),
            pool_config,
        }
    }

    pub fn from_configs(configs: Vec<ServerConfig>) -> Self {
        Self::from_configs_with_pool(configs, ConnectionPoolConfig::default())
    }

    pub fn from_configs_with_pool(
        configs: Vec<ServerConfig>,
        pool_config: ConnectionPoolConfig,
    ) -> Self {
        let mut leaves = HashMap::new();
        for config in configs {
            let alias = config.alias.clone();
            leaves.insert(
                alias,
                Arc::new(LeafEntry::new_with_pool(config, &pool_config)),
            );
        }
        Self {
            leaves: RwLock::new(leaves),
            pool_config,
        }
    }

    pub fn pool_config(&self) -> &ConnectionPoolConfig {
        &self.pool_config
    }

    pub async fn get_leaf(&self, alias: &str) -> Option<Arc<LeafEntry>> {
        self.leaves.read().await.get(alias).cloned()
    }

    pub async fn all_leaves(&self) -> Vec<Arc<LeafEntry>> {
        self.leaves.read().await.values().cloned().collect()
    }

    pub async fn healthy_leaves(&self) -> Vec<Arc<LeafEntry>> {
        let leaves = self.leaves.read().await;
        let mut healthy = Vec::new();
        for leaf in leaves.values() {
            if leaf.is_healthy().await {
                healthy.push(leaf.clone());
            }
        }
        healthy
    }

    pub async fn aliases(&self) -> Vec<String> {
        self.leaves.read().await.keys().cloned().collect()
    }

    #[cfg(test)]
    pub async fn leaves_mut(
        &self,
    ) -> tokio::sync::RwLockWriteGuard<'_, HashMap<String, Arc<LeafEntry>>> {
        self.leaves.write().await
    }

    /// Insert (or replace) a leaf under the given alias. Returns the previous
    /// entry if one existed.
    pub async fn add_leaf(&self, alias: String, entry: Arc<LeafEntry>) -> Option<Arc<LeafEntry>> {
        self.leaves.write().await.insert(alias, entry)
    }

    /// Remove and return the leaf with the given alias.
    pub async fn remove_leaf(&self, alias: &str) -> Option<Arc<LeafEntry>> {
        self.leaves.write().await.remove(alias)
    }

    pub async fn initialize_all(&self) {
        let leaves = self.all_leaves().await;
        let mut handles = Vec::new();

        for leaf in leaves {
            handles.push(tokio::spawn(async move {
                match leaf.client.initialize().await {
                    Ok(init_result) => {
                        tracing::info!(alias = %leaf.config.alias, "leaf server initialized");
                        leaf.mark_healthy().await;

                        if let Some(server_info) = init_result.get("serverInfo").cloned() {
                            *leaf.server_info.write().await = Some(server_info);
                        }

                        match leaf.client.list_tools().await {
                            Ok(tools) => {
                                tracing::info!(
                                    alias = %leaf.config.alias,
                                    tool_count = tools.len(),
                                    "cached tools from leaf"
                                );
                                *leaf.cached_tools.write().await = tools;
                                leaf.mark_tools_refreshed().await;
                            }
                            Err(e) => {
                                tracing::warn!(
                                    alias = %leaf.config.alias,
                                    error = %e,
                                    "failed to list tools from leaf"
                                );
                            }
                        }

                        match leaf.client.list_resources().await {
                            Ok(resources) => {
                                *leaf.cached_resources.write().await = resources;
                            }
                            Err(e) => {
                                tracing::debug!(
                                    alias = %leaf.config.alias,
                                    error = %e,
                                    "failed to list resources from leaf (may not support resources)"
                                );
                            }
                        }

                        match leaf.client.list_prompts().await {
                            Ok(prompts) => {
                                *leaf.cached_prompts.write().await = prompts;
                            }
                            Err(e) => {
                                tracing::debug!(
                                    alias = %leaf.config.alias,
                                    error = %e,
                                    "failed to list prompts from leaf (may not support prompts)"
                                );
                            }
                        }
                    }
                    Err(e) => {
                        tracing::warn!(
                            alias = %leaf.config.alias,
                            error = %e,
                            "failed to initialize leaf server"
                        );
                        leaf.mark_unhealthy().await;
                    }
                }
            }));
        }

        for handle in handles {
            let _ = handle.await;
        }
    }

    pub async fn refresh_tools(&self, alias: &str) -> Result<usize, String> {
        let leaf = self
            .get_leaf(alias)
            .await
            .ok_or_else(|| format!("unknown leaf: {alias}"))?;

        let tools = leaf.client.list_tools().await.map_err(|e| e.to_string())?;

        let count = tools.len();
        *leaf.cached_tools.write().await = tools;
        leaf.mark_tools_refreshed().await;
        Ok(count)
    }

    pub async fn refresh_all_tools(&self) -> Vec<(String, Result<usize, String>)> {
        let leaves = self.all_leaves().await;
        let mut handles = Vec::new();

        for leaf in leaves {
            let alias = leaf.config.alias.clone();
            handles.push(tokio::spawn(async move {
                let result = match leaf.client.list_tools().await {
                    Ok(tools) => {
                        let count = tools.len();
                        *leaf.cached_tools.write().await = tools;
                        leaf.mark_tools_refreshed().await;
                        Ok(count)
                    }
                    Err(e) => Err(e.to_string()),
                };
                (alias, result)
            }));
        }

        let mut results = Vec::new();
        for handle in handles {
            if let Ok(pair) = handle.await {
                results.push(pair);
            }
        }
        results
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_helpers::make_server_config;

    #[tokio::test]
    async fn from_configs_populates_leaves() {
        let configs = vec![
            make_server_config("alpha", "http://alpha:8080/mcp"),
            make_server_config("beta", "http://beta:8080/mcp"),
        ];
        let registry = Registry::from_configs(configs);

        assert!(registry.get_leaf("alpha").await.is_some());
        assert!(registry.get_leaf("beta").await.is_some());
        assert!(registry.get_leaf("gamma").await.is_none());
    }

    #[tokio::test]
    async fn all_leaves_returns_every_entry() {
        let configs = vec![
            make_server_config("a", "http://a:8080/mcp"),
            make_server_config("b", "http://b:8080/mcp"),
            make_server_config("c", "http://c:8080/mcp"),
        ];
        let registry = Registry::from_configs(configs);
        assert_eq!(registry.all_leaves().await.len(), 3);
    }

    #[tokio::test]
    async fn aliases_returns_all_names() {
        let configs = vec![
            make_server_config("x", "http://x:8080/mcp"),
            make_server_config("y", "http://y:8080/mcp"),
        ];
        let registry = Registry::from_configs(configs);
        let mut aliases = registry.aliases().await;
        aliases.sort();
        assert_eq!(aliases, vec!["x", "y"]);
    }

    #[tokio::test]
    async fn new_leaf_starts_with_unknown_health() {
        let config = make_server_config("test", "http://test:8080/mcp");
        let entry = LeafEntry::new(config);
        assert_eq!(*entry.health.read().await, LeafHealth::Unknown);
        assert!(entry.is_healthy().await); // Unknown counts as healthy
    }

    #[tokio::test]
    async fn mark_healthy_resets_failures() {
        let config = make_server_config("test", "http://test:8080/mcp");
        let entry = LeafEntry::new(config);

        entry.mark_unhealthy().await;
        entry.mark_unhealthy().await;
        assert_eq!(*entry.consecutive_failures.read().await, 2);
        assert!(!entry.is_healthy().await);

        entry.mark_healthy().await;
        assert_eq!(*entry.consecutive_failures.read().await, 0);
        assert!(entry.is_healthy().await);
        assert!(entry.last_health_check.read().await.is_some());
    }

    #[tokio::test]
    async fn mark_unhealthy_increments_failures() {
        let config = make_server_config("test", "http://test:8080/mcp");
        let entry = LeafEntry::new(config);

        entry.mark_unhealthy().await;
        assert_eq!(*entry.consecutive_failures.read().await, 1);

        entry.mark_unhealthy().await;
        assert_eq!(*entry.consecutive_failures.read().await, 2);

        entry.mark_unhealthy().await;
        assert_eq!(*entry.consecutive_failures.read().await, 3);
    }

    #[tokio::test]
    async fn healthy_leaves_excludes_unhealthy() {
        let configs = vec![
            make_server_config("up", "http://up:8080/mcp"),
            make_server_config("down", "http://down:8080/mcp"),
            make_server_config("unknown", "http://unknown:8080/mcp"),
        ];
        let registry = Registry::from_configs(configs);

        let up = registry.get_leaf("up").await.unwrap();
        up.mark_healthy().await;

        let down = registry.get_leaf("down").await.unwrap();
        down.mark_unhealthy().await;

        // "unknown" stays at LeafHealth::Unknown — should be included (not yet proven unhealthy)

        let healthy = registry.healthy_leaves().await;
        let healthy_aliases: Vec<String> = healthy.iter().map(|l| l.config.alias.clone()).collect();

        assert!(healthy_aliases.contains(&"up".to_string()));
        assert!(healthy_aliases.contains(&"unknown".to_string()));
        assert!(!healthy_aliases.contains(&"down".to_string()));
    }

    #[tokio::test]
    async fn empty_registry() {
        let registry = Registry::new();
        assert!(registry.all_leaves().await.is_empty());
        assert!(registry.healthy_leaves().await.is_empty());
        assert!(registry.aliases().await.is_empty());
        assert!(registry.get_leaf("anything").await.is_none());
    }

    #[tokio::test]
    async fn record_failure_leaves_health_intact() {
        let config = make_server_config("test", "http://test:8080/mcp");
        let entry = LeafEntry::new(config);
        entry.mark_healthy().await;

        let n = entry.record_failure().await;
        assert_eq!(n, 1);
        // Health state stays Healthy — record_failure only increments the
        // counter so callers can enforce a threshold before flipping.
        assert_eq!(*entry.health.read().await, LeafHealth::Healthy);
    }

    #[tokio::test]
    async fn add_and_remove_leaf_dynamic() {
        let registry = Registry::new();
        assert!(registry.get_leaf("new").await.is_none());

        let entry = Arc::new(LeafEntry::new(make_server_config(
            "new",
            "http://new:8080/mcp",
        )));
        assert!(registry.add_leaf("new".to_string(), entry).await.is_none());
        assert!(registry.get_leaf("new").await.is_some());

        let replacement = Arc::new(LeafEntry::new(make_server_config(
            "new",
            "http://new2:8080/mcp",
        )));
        let prior = registry.add_leaf("new".to_string(), replacement).await;
        assert!(prior.is_some());

        let removed = registry.remove_leaf("new").await;
        assert!(removed.is_some());
        assert!(registry.get_leaf("new").await.is_none());
        assert!(registry.remove_leaf("new").await.is_none());
    }

    #[tokio::test]
    async fn tools_stale_returns_true_when_never_refreshed() {
        let config = make_server_config("t", "http://t:8080/mcp");
        let entry = LeafEntry::new(config);
        assert!(entry.tools_stale(Duration::from_secs(60)).await);
    }

    #[tokio::test]
    async fn tools_stale_returns_false_after_recent_refresh() {
        let config = make_server_config("t", "http://t:8080/mcp");
        let entry = LeafEntry::new(config);
        entry.mark_tools_refreshed().await;
        assert!(!entry.tools_stale(Duration::from_secs(60)).await);
    }

    #[tokio::test]
    async fn tools_stale_returns_true_with_zero_ttl() {
        let config = make_server_config("t", "http://t:8080/mcp");
        let entry = LeafEntry::new(config);
        entry.mark_tools_refreshed().await;
        // Zero TTL: anything past the refresh instant is stale.
        assert!(entry.tools_stale(Duration::ZERO).await);
    }

    #[tokio::test]
    async fn refresh_all_tools_runs_concurrently() {
        // Sanity check: refresh_all_tools returns one entry per leaf even
        // when the underlying leaf endpoints are unreachable. This
        // exercises the parallel spawn path.
        let configs = vec![
            make_server_config("a", "http://127.0.0.1:1/mcp"),
            make_server_config("b", "http://127.0.0.1:2/mcp"),
            make_server_config("c", "http://127.0.0.1:3/mcp"),
        ];
        let registry = Registry::from_configs(configs);
        let results = registry.refresh_all_tools().await;
        assert_eq!(results.len(), 3);
        // All three should have failed (nothing listening on those ports)
        // but the point is the call returns and each leaf is represented.
        let mut aliases: Vec<String> = results.into_iter().map(|(a, _)| a).collect();
        aliases.sort();
        assert_eq!(aliases, vec!["a", "b", "c"]);
    }
}
