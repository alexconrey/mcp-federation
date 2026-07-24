use std::sync::Arc;

use crate::aggregator::{parse_namespaced_name, NAMESPACE_SEPARATOR};
use crate::error::FederationError;
use crate::registry::{LeafHealth, Registry};

pub struct Router {
    registry: Arc<Registry>,
}

impl Router {
    pub fn new(registry: Arc<Registry>) -> Self {
        Self { registry }
    }

    pub async fn route_tool_call(
        &self,
        namespaced_tool: &str,
        arguments: &serde_json::Value,
    ) -> Result<serde_json::Value, FederationError> {
        let (alias, tool_name) = parse_namespaced_name(namespaced_tool).ok_or_else(|| {
            FederationError::InvalidToolName(format!(
                "tool name must be in format 'alias{NAMESPACE_SEPARATOR}tool_name', got: {namespaced_tool}"
            ))
        })?;

        // Handle federation-native tools
        if alias == "federation" {
            return self.handle_native_tool(tool_name, arguments).await;
        }

        let leaf = self
            .registry
            .get_leaf(alias)
            .await
            .ok_or_else(|| FederationError::UnknownLeaf(alias.to_string()))?;

        if !leaf.is_healthy().await {
            return Err(FederationError::LeafUnhealthy(alias.to_string()));
        }

        leaf.client.call_tool(tool_name, arguments).await
    }

    pub async fn route_resource_read(
        &self,
        namespaced_uri: &str,
    ) -> Result<serde_json::Value, FederationError> {
        let (alias, uri) = parse_namespaced_name(namespaced_uri).ok_or_else(|| {
            FederationError::InvalidToolName(format!(
                "resource URI must be prefixed with alias, got: {namespaced_uri}"
            ))
        })?;

        let leaf = self
            .registry
            .get_leaf(alias)
            .await
            .ok_or_else(|| FederationError::UnknownLeaf(alias.to_string()))?;

        if !leaf.is_healthy().await {
            return Err(FederationError::LeafUnhealthy(alias.to_string()));
        }

        leaf.client.read_resource(uri).await
    }

    pub async fn route_prompt_get(
        &self,
        namespaced_name: &str,
        arguments: &serde_json::Value,
    ) -> Result<serde_json::Value, FederationError> {
        let (alias, prompt_name) = parse_namespaced_name(namespaced_name).ok_or_else(|| {
            FederationError::InvalidToolName(format!(
                "prompt name must be prefixed with alias, got: {namespaced_name}"
            ))
        })?;

        let leaf = self
            .registry
            .get_leaf(alias)
            .await
            .ok_or_else(|| FederationError::UnknownLeaf(alias.to_string()))?;

        if !leaf.is_healthy().await {
            return Err(FederationError::LeafUnhealthy(alias.to_string()));
        }

        leaf.client.get_prompt(prompt_name, arguments).await
    }

    async fn handle_native_tool(
        &self,
        tool_name: &str,
        arguments: &serde_json::Value,
    ) -> Result<serde_json::Value, FederationError> {
        match tool_name {
            "list_servers" => {
                let leaves = self.registry.all_leaves().await;
                let mut servers = Vec::new();

                for leaf in &leaves {
                    let health = *leaf.health.read().await;
                    let tool_count = leaf.cached_tools.read().await.len();
                    let health_str = match health {
                        LeafHealth::Unknown => "unknown",
                        LeafHealth::Healthy => "healthy",
                        LeafHealth::Unhealthy => "unhealthy",
                    };
                    let is_federation = leaf.is_federation().await;

                    servers.push(serde_json::json!({
                        "alias": leaf.config.alias,
                        "url": leaf.config.url,
                        "transport": format!("{:?}", leaf.config.transport),
                        "tags": leaf.config.tags,
                        "health": health_str,
                        "tool_count": tool_count,
                        "is_federation": is_federation,
                    }));
                }

                Ok(serde_json::json!({
                    "content": [{
                        "type": "text",
                        "text": serde_json::to_string_pretty(&servers).unwrap_or_default()
                    }]
                }))
            }

            "topology" => {
                // Reports the federation tree: this node + each leaf plus a
                // flag indicating which leaves are themselves federations.
                // Nested topology data (children of federated leaves) isn't
                // fetched inline — clients can walk deeper by calling
                // `<alias>__federation__topology` on any is_federation leaf.
                let leaves = self.registry.all_leaves().await;
                let mut leaf_entries = Vec::new();
                for leaf in &leaves {
                    let health = *leaf.health.read().await;
                    let tool_count = leaf.cached_tools.read().await.len();
                    let health_str = match health {
                        LeafHealth::Unknown => "unknown",
                        LeafHealth::Healthy => "healthy",
                        LeafHealth::Unhealthy => "unhealthy",
                    };
                    let server_info = leaf.server_info.read().await.clone();
                    let is_federation = leaf.is_federation().await;
                    leaf_entries.push(serde_json::json!({
                        "alias": leaf.config.alias,
                        "url": leaf.config.url,
                        "transport": format!("{:?}", leaf.config.transport),
                        "tags": leaf.config.tags,
                        "health": health_str,
                        "tool_count": tool_count,
                        "is_federation": is_federation,
                        "server_info": server_info,
                    }));
                }

                let topology = serde_json::json!({
                    "node": {
                        "name": "mcp-federation",
                        "version": env!("CARGO_PKG_VERSION"),
                        "leaf_count": leaves.len(),
                    },
                    "leaves": leaf_entries,
                });

                Ok(serde_json::json!({
                    "content": [{
                        "type": "text",
                        "text": serde_json::to_string_pretty(&topology).unwrap_or_default()
                    }]
                }))
            }

            "server_info" => {
                let alias = arguments
                    .get("alias")
                    .and_then(|a| a.as_str())
                    .ok_or_else(|| {
                        FederationError::LeafError {
                            alias: "federation".to_string(),
                            message: "missing required argument: alias".to_string(),
                        }
                    })?;

                let leaf = self
                    .registry
                    .get_leaf(alias)
                    .await
                    .ok_or_else(|| FederationError::UnknownLeaf(alias.to_string()))?;

                let health = *leaf.health.read().await;
                let tools = leaf.cached_tools.read().await;
                let tool_names: Vec<&str> = tools
                    .iter()
                    .filter_map(|t| t.get("name").and_then(|n| n.as_str()))
                    .collect();

                let info = serde_json::json!({
                    "alias": leaf.config.alias,
                    "url": leaf.config.url,
                    "transport": format!("{:?}", leaf.config.transport),
                    "tags": leaf.config.tags,
                    "health": format!("{health:?}"),
                    "consecutive_failures": *leaf.consecutive_failures.read().await,
                    "tool_count": tool_names.len(),
                    "tools": tool_names,
                });

                Ok(serde_json::json!({
                    "content": [{
                        "type": "text",
                        "text": serde_json::to_string_pretty(&info).unwrap_or_default()
                    }]
                }))
            }

            "refresh" => {
                let results = if let Some(alias) = arguments.get("alias").and_then(|a| a.as_str())
                {
                    let result = self.registry.refresh_tools(alias).await;
                    vec![(alias.to_string(), result)]
                } else {
                    self.registry.refresh_all_tools().await
                };

                let mut summary = Vec::new();
                for (alias, result) in &results {
                    match result {
                        Ok(count) => summary.push(format!("{alias}: refreshed ({count} tools)")),
                        Err(e) => summary.push(format!("{alias}: error ({e})")),
                    }
                }

                Ok(serde_json::json!({
                    "content": [{
                        "type": "text",
                        "text": summary.join("\n")
                    }]
                }))
            }

            "search_tools" => {
                let query = arguments
                    .get("query")
                    .and_then(|q| q.as_str())
                    .ok_or_else(|| FederationError::LeafError {
                        alias: "federation".to_string(),
                        message: "missing required argument: query".to_string(),
                    })?;
                let query_lower = query.to_lowercase();
                let filter_alias = arguments.get("alias").and_then(|a| a.as_str());

                let leaves = match filter_alias {
                    Some(alias) => {
                        let leaf = self
                            .registry
                            .get_leaf(alias)
                            .await
                            .ok_or_else(|| FederationError::UnknownLeaf(alias.to_string()))?;
                        vec![leaf]
                    }
                    None => self.registry.all_leaves().await,
                };

                let mut matches = Vec::new();
                for leaf in &leaves {
                    let tools = leaf.cached_tools.read().await;
                    for tool in tools.iter() {
                        let name = tool.get("name").and_then(|v| v.as_str()).unwrap_or("");
                        let description = tool
                            .get("description")
                            .and_then(|v| v.as_str())
                            .unwrap_or("");
                        if name.to_lowercase().contains(&query_lower)
                            || description.to_lowercase().contains(&query_lower)
                        {
                            matches.push(serde_json::json!({
                                "alias": leaf.config.alias,
                                "name": format!("{}{NAMESPACE_SEPARATOR}{name}", leaf.config.alias),
                                "tool_name": name,
                                "description": description,
                            }));
                        }
                    }
                }

                Ok(serde_json::json!({
                    "content": [{
                        "type": "text",
                        "text": serde_json::to_string_pretty(&matches).unwrap_or_default()
                    }]
                }))
            }

            _ => Err(FederationError::ToolNotFound(format!(
                "federation__{tool_name}"
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_helpers::{make_registry_with_tools, make_tool};

    fn empty_args() -> serde_json::Value {
        serde_json::json!({})
    }

    #[tokio::test]
    async fn reject_tool_name_without_separator() {
        let registry = make_registry_with_tools(vec![]).await;
        let router = Router::new(registry);
        let result = router.route_tool_call("no_separator", &empty_args()).await;
        assert!(matches!(result, Err(FederationError::InvalidToolName(_))));
    }

    #[tokio::test]
    async fn reject_unknown_leaf_alias() {
        let registry = make_registry_with_tools(vec![]).await;
        let router = Router::new(registry);
        let result = router
            .route_tool_call("nonexistent__list_pods", &empty_args())
            .await;
        assert!(matches!(result, Err(FederationError::UnknownLeaf(_))));
    }

    #[tokio::test]
    async fn reject_unhealthy_leaf() {
        let registry = make_registry_with_tools(vec![(
            "sick",
            "http://sick:8080/mcp",
            vec![make_tool("list_pods", "List pods")],
        )])
        .await;

        // Mark it unhealthy
        let leaf = registry.get_leaf("sick").await.unwrap();
        leaf.mark_unhealthy().await;

        let router = Router::new(registry);
        let result = router
            .route_tool_call("sick__list_pods", &empty_args())
            .await;
        assert!(matches!(result, Err(FederationError::LeafUnhealthy(_))));
    }

    #[tokio::test]
    async fn native_list_servers_returns_all() {
        let registry = make_registry_with_tools(vec![
            (
                "alpha",
                "http://alpha:8080/mcp",
                vec![make_tool("list_pods", "List pods")],
            ),
            (
                "beta",
                "http://beta:8080/mcp",
                vec![
                    make_tool("list_pods", "List pods"),
                    make_tool("get_pod", "Get a pod"),
                ],
            ),
        ])
        .await;

        let router = Router::new(registry);
        let result = router
            .route_tool_call("federation__list_servers", &empty_args())
            .await
            .unwrap();

        let text = result["content"][0]["text"].as_str().unwrap();
        let servers: Vec<serde_json::Value> = serde_json::from_str(text).unwrap();
        assert_eq!(servers.len(), 2);

        let aliases: Vec<&str> = servers
            .iter()
            .map(|s| s["alias"].as_str().unwrap())
            .collect();
        assert!(aliases.contains(&"alpha"));
        assert!(aliases.contains(&"beta"));
    }

    #[tokio::test]
    async fn native_server_info_returns_details() {
        let registry = make_registry_with_tools(vec![(
            "prod",
            "http://prod:8080/mcp",
            vec![
                make_tool("list_pods", "List pods"),
                make_tool("get_pod", "Get a pod"),
            ],
        )])
        .await;

        let router = Router::new(registry);
        let args = serde_json::json!({"alias": "prod"});
        let result = router
            .route_tool_call("federation__server_info", &args)
            .await
            .unwrap();

        let text = result["content"][0]["text"].as_str().unwrap();
        let info: serde_json::Value = serde_json::from_str(text).unwrap();
        assert_eq!(info["alias"], "prod");
        assert_eq!(info["tool_count"], 2);
    }

    #[tokio::test]
    async fn native_server_info_unknown_alias() {
        let registry = make_registry_with_tools(vec![]).await;
        let router = Router::new(registry);
        let args = serde_json::json!({"alias": "nonexistent"});
        let result = router
            .route_tool_call("federation__server_info", &args)
            .await;
        assert!(matches!(result, Err(FederationError::UnknownLeaf(_))));
    }

    #[tokio::test]
    async fn native_server_info_missing_alias_arg() {
        let registry = make_registry_with_tools(vec![]).await;
        let router = Router::new(registry);
        let result = router
            .route_tool_call("federation__server_info", &empty_args())
            .await;
        assert!(matches!(result, Err(FederationError::LeafError { .. })));
    }

    #[tokio::test]
    async fn native_unknown_federation_tool() {
        let registry = make_registry_with_tools(vec![]).await;
        let router = Router::new(registry);
        let result = router
            .route_tool_call("federation__bogus", &empty_args())
            .await;
        assert!(matches!(result, Err(FederationError::ToolNotFound(_))));
    }

    #[tokio::test]
    async fn route_resource_read_rejects_no_separator() {
        let registry = make_registry_with_tools(vec![]).await;
        let router = Router::new(registry);
        let result = router.route_resource_read("k8s://default/pods/foo").await;
        assert!(matches!(result, Err(FederationError::InvalidToolName(_))));
    }

    #[tokio::test]
    async fn route_prompt_get_rejects_unknown_leaf() {
        let registry = make_registry_with_tools(vec![]).await;
        let router = Router::new(registry);
        let result = router
            .route_prompt_get("nonexistent__diagnose-pod", &empty_args())
            .await;
        assert!(matches!(result, Err(FederationError::UnknownLeaf(_))));
    }

    async fn search_registry() -> Arc<Registry> {
        make_registry_with_tools(vec![
            (
                "alpha",
                "http://alpha:8080/mcp",
                vec![
                    make_tool("list_pods", "List pods in a namespace"),
                    make_tool("get_pod", "Get a specific pod"),
                    make_tool("scale_deployment", "Scale a deployment replica count"),
                ],
            ),
            (
                "beta",
                "http://beta:8080/mcp",
                vec![
                    make_tool("list_pods", "Enumerate pods"),
                    make_tool("get_nodes", "Return node info"),
                ],
            ),
        ])
        .await
    }

    #[tokio::test]
    async fn search_tools_matches_name_across_leaves() {
        let registry = search_registry().await;
        let router = Router::new(registry);
        let args = serde_json::json!({"query": "list_pods"});
        let result = router
            .route_tool_call("federation__search_tools", &args)
            .await
            .unwrap();

        let text = result["content"][0]["text"].as_str().unwrap();
        let matches: Vec<serde_json::Value> = serde_json::from_str(text).unwrap();
        assert_eq!(matches.len(), 2);
        let names: Vec<&str> = matches
            .iter()
            .map(|m| m["name"].as_str().unwrap())
            .collect();
        assert!(names.contains(&"alpha__list_pods"));
        assert!(names.contains(&"beta__list_pods"));
    }

    #[tokio::test]
    async fn search_tools_matches_description_case_insensitive() {
        let registry = search_registry().await;
        let router = Router::new(registry);
        let args = serde_json::json!({"query": "NAMESPACE"});
        let result = router
            .route_tool_call("federation__search_tools", &args)
            .await
            .unwrap();

        let text = result["content"][0]["text"].as_str().unwrap();
        let matches: Vec<serde_json::Value> = serde_json::from_str(text).unwrap();
        // Only alpha's list_pods description mentions "namespace"
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0]["name"], "alpha__list_pods");
    }

    #[tokio::test]
    async fn search_tools_restricted_to_alias() {
        let registry = search_registry().await;
        let router = Router::new(registry);
        let args = serde_json::json!({"query": "pod", "alias": "beta"});
        let result = router
            .route_tool_call("federation__search_tools", &args)
            .await
            .unwrap();

        let text = result["content"][0]["text"].as_str().unwrap();
        let matches: Vec<serde_json::Value> = serde_json::from_str(text).unwrap();
        for m in &matches {
            assert_eq!(m["alias"], "beta");
        }
        let names: Vec<&str> = matches
            .iter()
            .map(|m| m["name"].as_str().unwrap())
            .collect();
        assert!(names.contains(&"beta__list_pods"));
        assert!(!names.iter().any(|n| n.starts_with("alpha__")));
    }

    #[tokio::test]
    async fn search_tools_returns_empty_for_no_matches() {
        let registry = search_registry().await;
        let router = Router::new(registry);
        let args = serde_json::json!({"query": "totally-nonexistent-token"});
        let result = router
            .route_tool_call("federation__search_tools", &args)
            .await
            .unwrap();

        let text = result["content"][0]["text"].as_str().unwrap();
        let matches: Vec<serde_json::Value> = serde_json::from_str(text).unwrap();
        assert!(matches.is_empty());
    }

    #[tokio::test]
    async fn search_tools_missing_query_arg() {
        let registry = search_registry().await;
        let router = Router::new(registry);
        let result = router
            .route_tool_call("federation__search_tools", &empty_args())
            .await;
        assert!(matches!(result, Err(FederationError::LeafError { .. })));
    }

    #[tokio::test]
    async fn search_tools_unknown_alias() {
        let registry = search_registry().await;
        let router = Router::new(registry);
        let args = serde_json::json!({"query": "pod", "alias": "does-not-exist"});
        let result = router
            .route_tool_call("federation__search_tools", &args)
            .await;
        assert!(matches!(result, Err(FederationError::UnknownLeaf(_))));
    }
}
