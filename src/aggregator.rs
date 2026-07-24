use std::sync::Arc;
use std::time::Duration;

use metrics::gauge;

use crate::registry::Registry;

pub const NAMESPACE_SEPARATOR: &str = "__";

pub struct Aggregator {
    registry: Arc<Registry>,
    tool_cache_ttl: Duration,
}

impl Aggregator {
    pub fn new(registry: Arc<Registry>) -> Self {
        Self::with_ttl(registry, Duration::from_secs(300))
    }

    pub fn with_ttl(registry: Arc<Registry>, tool_cache_ttl: Duration) -> Self {
        Self {
            registry,
            tool_cache_ttl,
        }
    }

    pub async fn aggregated_tools(&self) -> Vec<serde_json::Value> {
        let mut all_tools = Vec::new();

        // Add federation-native tools first
        all_tools.extend(self.native_tools().await);

        // Aggregate tools from all healthy leaves
        let leaves = self.registry.healthy_leaves().await;

        // Trigger background refresh for stale leaves (TTL > 0).
        if !self.tool_cache_ttl.is_zero() {
            for leaf in &leaves {
                if leaf.tools_stale(self.tool_cache_ttl).await {
                    let leaf = leaf.clone();
                    tokio::spawn(async move {
                        match leaf.client.list_tools().await {
                            Ok(tools) => {
                                *leaf.cached_tools.write().await = tools;
                                leaf.mark_tools_refreshed().await;
                            }
                            Err(e) => {
                                tracing::debug!(
                                    alias = %leaf.config.alias,
                                    error = %e,
                                    "background tool refresh failed"
                                );
                            }
                        }
                    });
                }
            }
        }

        for leaf in &leaves {
            let tools = leaf.cached_tools.read().await;
            gauge!(
                "federation_leaf_tool_count",
                "alias" => leaf.config.alias.clone(),
            )
            .set(tools.len() as f64);
            for tool in tools.iter() {
                if let Some(namespaced) = namespace_tool(&leaf.config.alias, tool) {
                    all_tools.push(namespaced);
                }
            }
        }

        all_tools
    }

    pub async fn aggregated_resources(&self) -> Vec<serde_json::Value> {
        let mut all_resources = Vec::new();
        let leaves = self.registry.healthy_leaves().await;

        for leaf in &leaves {
            let resources = leaf.cached_resources.read().await;
            for resource in resources.iter() {
                if let Some(namespaced) = namespace_resource(&leaf.config.alias, resource) {
                    all_resources.push(namespaced);
                }
            }
        }

        all_resources
    }

    pub async fn aggregated_prompts(&self) -> Vec<serde_json::Value> {
        let mut all_prompts = Vec::new();
        let leaves = self.registry.healthy_leaves().await;

        for leaf in &leaves {
            let prompts = leaf.cached_prompts.read().await;
            for prompt in prompts.iter() {
                if let Some(namespaced) = namespace_prompt(&leaf.config.alias, prompt) {
                    all_prompts.push(namespaced);
                }
            }
        }

        all_prompts
    }

    async fn native_tools(&self) -> Vec<serde_json::Value> {
        vec![
            serde_json::json!({
                "name": format!("federation{NAMESPACE_SEPARATOR}list_servers"),
                "description": "[federation] List all configured leaf MCP servers and their health status",
                "inputSchema": {
                    "type": "object",
                    "properties": {},
                    "required": []
                }
            }),
            serde_json::json!({
                "name": format!("federation{NAMESPACE_SEPARATOR}server_info"),
                "description": "[federation] Get detailed info about a specific leaf MCP server",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "alias": {
                            "type": "string",
                            "description": "The server alias to get info for"
                        }
                    },
                    "required": ["alias"]
                }
            }),
            serde_json::json!({
                "name": format!("federation{NAMESPACE_SEPARATOR}refresh"),
                "description": "[federation] Force-refresh tool lists from all or a specific leaf server",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "alias": {
                            "type": "string",
                            "description": "Optional: refresh only this server (omit for all)"
                        }
                    },
                    "required": []
                }
            }),
            serde_json::json!({
                "name": format!("federation{NAMESPACE_SEPARATOR}search_tools"),
                "description": "[federation] Search for tools across all leaf servers by name or description keyword",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "query": {
                            "type": "string",
                            "description": "Search term to match against tool names and descriptions"
                        },
                        "alias": {
                            "type": "string",
                            "description": "Optional: restrict search to a specific leaf server"
                        }
                    },
                    "required": ["query"]
                }
            }),
            serde_json::json!({
                "name": format!("federation{NAMESPACE_SEPARATOR}topology"),
                "description": "[federation] Return the federation topology — this node plus each leaf, with is_federation flags for nested mcp-federation instances",
                "inputSchema": {
                    "type": "object",
                    "properties": {},
                    "required": []
                }
            }),
        ]
    }
}

fn namespace_tool(alias: &str, tool: &serde_json::Value) -> Option<serde_json::Value> {
    let name = tool.get("name")?.as_str()?;
    let description = tool
        .get("description")
        .and_then(|d| d.as_str())
        .unwrap_or("");

    let mut namespaced = tool.clone();
    namespaced["name"] =
        serde_json::Value::String(format!("{alias}{NAMESPACE_SEPARATOR}{name}"));
    namespaced["description"] =
        serde_json::Value::String(format!("[{alias}] {description}"));

    Some(namespaced)
}

fn namespace_resource(alias: &str, resource: &serde_json::Value) -> Option<serde_json::Value> {
    let uri = resource.get("uri")?.as_str()?;
    let name = resource
        .get("name")
        .and_then(|n| n.as_str())
        .unwrap_or("");

    let mut namespaced = resource.clone();
    namespaced["uri"] =
        serde_json::Value::String(format!("{alias}{NAMESPACE_SEPARATOR}{uri}"));
    namespaced["name"] =
        serde_json::Value::String(format!("[{alias}] {name}"));

    Some(namespaced)
}

fn namespace_prompt(alias: &str, prompt: &serde_json::Value) -> Option<serde_json::Value> {
    let name = prompt.get("name")?.as_str()?;
    let description = prompt
        .get("description")
        .and_then(|d| d.as_str())
        .unwrap_or("");

    let mut namespaced = prompt.clone();
    namespaced["name"] =
        serde_json::Value::String(format!("{alias}{NAMESPACE_SEPARATOR}{name}"));
    namespaced["description"] =
        serde_json::Value::String(format!("[{alias}] {description}"));

    Some(namespaced)
}

pub fn parse_namespaced_name(namespaced: &str) -> Option<(&str, &str)> {
    namespaced.split_once(NAMESPACE_SEPARATOR)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_namespace_tool() {
        let tool = serde_json::json!({
            "name": "list_pods",
            "description": "List pods in a namespace",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "namespace": { "type": "string" }
                }
            }
        });

        let namespaced = namespace_tool("prod-k8s", &tool).unwrap();
        assert_eq!(namespaced["name"], "prod-k8s__list_pods");
        assert_eq!(
            namespaced["description"],
            "[prod-k8s] List pods in a namespace"
        );
        // Schema is preserved unchanged
        assert_eq!(namespaced["inputSchema"], tool["inputSchema"]);
    }

    #[test]
    fn test_parse_namespaced_name() {
        assert_eq!(
            parse_namespaced_name("prod-k8s__list_pods"),
            Some(("prod-k8s", "list_pods"))
        );
        assert_eq!(
            parse_namespaced_name("federation__list_servers"),
            Some(("federation", "list_servers"))
        );
        assert_eq!(parse_namespaced_name("no_separator"), None);
    }

    #[test]
    fn test_namespace_resource() {
        let resource = serde_json::json!({
            "uri": "k8s://default/pods/my-pod",
            "name": "Kubernetes Pod",
            "mimeType": "application/json"
        });

        let namespaced = namespace_resource("prod-k8s", &resource).unwrap();
        assert_eq!(namespaced["uri"], "prod-k8s__k8s://default/pods/my-pod");
        assert_eq!(namespaced["name"], "[prod-k8s] Kubernetes Pod");
    }

    #[test]
    fn test_namespace_prompt() {
        let prompt = serde_json::json!({
            "name": "diagnose-pod",
            "description": "Diagnose why a pod is failing"
        });

        let namespaced = namespace_prompt("staging", &prompt).unwrap();
        assert_eq!(namespaced["name"], "staging__diagnose-pod");
        assert_eq!(
            namespaced["description"],
            "[staging] Diagnose why a pod is failing"
        );
    }

    #[test]
    fn namespace_tool_preserves_input_schema() {
        let tool = serde_json::json!({
            "name": "get_pod",
            "description": "Get a pod",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "namespace": { "type": "string" },
                    "name": { "type": "string" }
                },
                "required": ["namespace", "name"]
            }
        });

        let namespaced = namespace_tool("cluster-a", &tool).unwrap();
        assert_eq!(
            namespaced["inputSchema"]["required"],
            serde_json::json!(["namespace", "name"])
        );
    }

    #[test]
    fn namespace_tool_returns_none_for_missing_name() {
        let tool = serde_json::json!({"description": "no name field"});
        assert!(namespace_tool("x", &tool).is_none());
    }

    #[tokio::test]
    async fn aggregated_tools_from_multiple_leaves() {
        use crate::test_helpers::{make_registry_with_tools, make_tool};

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

        let aggregator = Aggregator::new(registry);
        let tools = aggregator.aggregated_tools().await;

        let names: Vec<String> = tools
            .iter()
            .map(|t| t["name"].as_str().unwrap().to_string())
            .collect();

        // 5 native tools + 1 from alpha + 2 from beta = 8
        assert_eq!(tools.len(), 8);
        assert!(names.contains(&"alpha__list_pods".to_string()));
        assert!(names.contains(&"beta__list_pods".to_string()));
        assert!(names.contains(&"beta__get_pod".to_string()));
        assert!(names.contains(&"federation__list_servers".to_string()));
        assert!(names.contains(&"federation__search_tools".to_string()));
    }

    #[tokio::test]
    async fn aggregated_tools_excludes_unhealthy_leaves() {
        use crate::test_helpers::{make_registry_with_tools, make_tool};

        let registry = make_registry_with_tools(vec![
            (
                "up",
                "http://up:8080/mcp",
                vec![make_tool("list_pods", "List pods")],
            ),
            (
                "down",
                "http://down:8080/mcp",
                vec![make_tool("get_nodes", "Get nodes")],
            ),
        ])
        .await;

        // Mark one as unhealthy
        let leaf = registry.get_leaf("down").await.unwrap();
        leaf.mark_unhealthy().await;

        let aggregator = Aggregator::new(registry);
        let tools = aggregator.aggregated_tools().await;

        let names: Vec<String> = tools
            .iter()
            .map(|t| t["name"].as_str().unwrap().to_string())
            .collect();

        assert!(names.contains(&"up__list_pods".to_string()));
        assert!(!names.contains(&"down__get_nodes".to_string()));
    }

    #[tokio::test]
    async fn aggregated_resources_namespaced() {
        use crate::test_helpers::{make_registry_with_all, make_resource};

        let registry = make_registry_with_all(vec![(
            "prod",
            "http://prod:8080/mcp",
            vec![],
            vec![make_resource(
                "k8s://default/pods/my-pod",
                "Kubernetes Pod",
            )],
            vec![],
        )])
        .await;

        let aggregator = Aggregator::new(registry);
        let resources = aggregator.aggregated_resources().await;
        assert_eq!(resources.len(), 1);
        assert_eq!(resources[0]["uri"], "prod__k8s://default/pods/my-pod");
        assert_eq!(resources[0]["name"], "[prod] Kubernetes Pod");
    }

    #[tokio::test]
    async fn aggregated_prompts_namespaced() {
        use crate::test_helpers::{make_prompt, make_registry_with_all};

        let registry = make_registry_with_all(vec![(
            "staging",
            "http://staging:8080/mcp",
            vec![],
            vec![],
            vec![make_prompt("diagnose-pod", "Diagnose a pod")],
        )])
        .await;

        let aggregator = Aggregator::new(registry);
        let prompts = aggregator.aggregated_prompts().await;
        assert_eq!(prompts.len(), 1);
        assert_eq!(prompts[0]["name"], "staging__diagnose-pod");
        assert_eq!(prompts[0]["description"], "[staging] Diagnose a pod");
    }

    #[tokio::test]
    async fn native_tools_always_present() {
        use crate::test_helpers::make_registry_with_tools;

        let registry = make_registry_with_tools(vec![]).await;
        let aggregator = Aggregator::new(registry);
        let tools = aggregator.aggregated_tools().await;

        let names: Vec<String> = tools
            .iter()
            .map(|t| t["name"].as_str().unwrap().to_string())
            .collect();

        assert!(names.contains(&"federation__list_servers".to_string()));
        assert!(names.contains(&"federation__server_info".to_string()));
        assert!(names.contains(&"federation__refresh".to_string()));
        assert!(names.contains(&"federation__search_tools".to_string()));
        assert!(names.contains(&"federation__topology".to_string()));
        assert_eq!(tools.len(), 5);
    }
}
