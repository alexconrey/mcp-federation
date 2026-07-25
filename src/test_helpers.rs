use std::sync::Arc;

use crate::config::{HealthCheckConfig, ServerConfig, TlsConfig, TransportType};
use crate::registry::{LeafEntry, Registry};

pub fn make_server_config(alias: &str, url: &str) -> ServerConfig {
    ServerConfig {
        alias: alias.to_string(),
        url: url.to_string(),
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

pub fn make_tool(name: &str, description: &str) -> serde_json::Value {
    serde_json::json!({
        "name": name,
        "description": description,
        "inputSchema": {
            "type": "object",
            "properties": {
                "namespace": { "type": "string" }
            },
            "required": []
        }
    })
}

pub fn make_resource(uri: &str, name: &str) -> serde_json::Value {
    serde_json::json!({
        "uri": uri,
        "name": name,
        "mimeType": "application/json"
    })
}

pub fn make_prompt(name: &str, description: &str) -> serde_json::Value {
    serde_json::json!({
        "name": name,
        "description": description,
        "arguments": []
    })
}

pub async fn make_registry_with_tools(
    entries: Vec<(&str, &str, Vec<serde_json::Value>)>,
) -> Arc<Registry> {
    let registry = Registry::new();
    {
        let mut leaves = registry.leaves_mut().await;
        for (alias, url, tools) in entries {
            let config = make_server_config(alias, url);
            let entry = Arc::new(LeafEntry::new(config));
            entry.mark_healthy().await;
            *entry.cached_tools.write().await = tools;
            leaves.insert(alias.to_string(), entry);
        }
    }
    Arc::new(registry)
}

type AllEntry<'a> = (
    &'a str,
    &'a str,
    Vec<serde_json::Value>,
    Vec<serde_json::Value>,
    Vec<serde_json::Value>,
);

pub async fn make_registry_with_all(entries: Vec<AllEntry<'_>>) -> Arc<Registry> {
    let registry = Registry::new();
    {
        let mut leaves = registry.leaves_mut().await;
        for (alias, url, tools, resources, prompts) in entries {
            let config = make_server_config(alias, url);
            let entry = Arc::new(LeafEntry::new(config));
            entry.mark_healthy().await;
            *entry.cached_tools.write().await = tools;
            *entry.cached_resources.write().await = resources;
            *entry.cached_prompts.write().await = prompts;
            leaves.insert(alias.to_string(), entry);
        }
    }
    Arc::new(registry)
}
