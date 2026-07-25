//! DNS SRV-based leaf discovery.
//!
//! When enabled, spawns a background task that polls a DNS SRV record on a
//! fixed interval. Each `priority weight port target` entry is registered as
//! a leaf server (or removed once it disappears from the record). This keeps
//! the federation's registry in sync with whatever the SRV record advertises
//! without requiring config-file edits or a restart.
//!
//! Alias derivation: the leaf's SRV `target` hostname is sanitized (trailing
//! `.` removed, `.` replaced with `-`, lowercased) so it satisfies the same
//! naming rules as file-configured leaves.
//!
//! Implementation: we shell out to `dig SRV +short` rather than pulling in a
//! full DNS resolver crate. This keeps the dependency surface tiny; if the
//! `dig` binary isn't available on the host the task logs an error and stops
//! polling instead of crashing the server.

use std::collections::{HashMap, HashSet};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use tokio::process::Command;

use crate::config::{DnsDiscoveryConfig, ServerConfig, TlsConfig};
use crate::registry::{LeafEntry, Registry};

/// Prefix used to mark DNS-discovered aliases so we can distinguish them from
/// file-configured leaves and safely prune only records we own.
const DNS_ALIAS_PREFIX: &str = "dns-";

#[derive(Debug, Clone, PartialEq, Eq)]
struct SrvRecord {
    port: u16,
    target: String,
}

/// Spawn the DNS discovery background task. Returns immediately when the
/// discovery config is disabled or misconfigured (empty SRV name).
pub fn spawn_dns_discovery(config: DnsDiscoveryConfig, registry: Arc<Registry>) {
    if !config.enabled {
        tracing::debug!("DNS discovery disabled");
        return;
    }
    if config.srv_name.trim().is_empty() {
        tracing::warn!("DNS discovery enabled but srv_name is empty; skipping");
        return;
    }

    let interval = Duration::from_secs(config.poll_interval_seconds.max(1));
    tracing::info!(
        srv_name = %config.srv_name,
        poll_interval_seconds = config.poll_interval_seconds,
        "starting DNS SRV discovery"
    );

    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(interval);
        // Skip the initial immediate tick and go straight to the polling
        // schedule; the caller is expected to have loaded static config first.
        ticker.tick().await;

        loop {
            ticker.tick().await;
            if let Err(e) = poll_once(&config, &registry).await {
                tracing::warn!(error = %e, "DNS discovery poll failed");
            }
        }
    });
}

async fn poll_once(config: &DnsDiscoveryConfig, registry: &Arc<Registry>) -> Result<(), String> {
    let records = query_srv(&config.srv_name).await?;
    reconcile(registry, config, &records).await;
    Ok(())
}

async fn query_srv(name: &str) -> Result<Vec<SrvRecord>, String> {
    let output = Command::new("dig")
        .arg("SRV")
        .arg("+short")
        .arg(name)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .map_err(|e| format!("failed to run dig: {e}"))?;

    if !output.status.success() {
        return Err(format!(
            "dig exited {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    Ok(parse_srv_output(&stdout))
}

fn parse_srv_output(stdout: &str) -> Vec<SrvRecord> {
    stdout.lines().filter_map(parse_srv_line).collect()
}

fn parse_srv_line(line: &str) -> Option<SrvRecord> {
    // Expected: "<priority> <weight> <port> <target>"
    let mut parts = line.split_whitespace();
    let _priority = parts.next()?;
    let _weight = parts.next()?;
    let port: u16 = parts.next()?.parse().ok()?;
    let target = parts.next()?.trim_end_matches('.').to_string();
    if target.is_empty() {
        return None;
    }
    Some(SrvRecord { port, target })
}

async fn reconcile(registry: &Arc<Registry>, config: &DnsDiscoveryConfig, records: &[SrvRecord]) {
    let desired: HashMap<String, &SrvRecord> = records
        .iter()
        .map(|r| (sanitize_alias(&r.target), r))
        .collect();

    // Existing DNS-owned aliases (prefix filter avoids touching file-config
    // leaves or dynamically-registered leaves added via the REST API).
    let existing_dns_aliases: HashSet<String> = registry
        .aliases()
        .await
        .into_iter()
        .filter(|a| a.starts_with(DNS_ALIAS_PREFIX))
        .collect();

    // Add any newly-advertised targets we don't already track.
    for (alias, record) in &desired {
        if existing_dns_aliases.contains(alias) {
            continue;
        }
        let url = format!(
            "{}://{}:{}{}",
            config.scheme, record.target, record.port, config.url_path
        );
        let server_config = ServerConfig {
            alias: alias.clone(),
            url: url.clone(),
            auth_token: None,
            auth_token_file: None,
            transport: config.default_transport.clone(),
            tags: vec!["dns-discovered".to_string()],
            health_check: Default::default(),
            timeout_seconds: None,
            tls: TlsConfig::default(),
        };
        let entry = Arc::new(LeafEntry::new_with_pool(
            server_config,
            registry.pool_config(),
        ));
        registry.add_leaf(alias.clone(), entry.clone()).await;
        tracing::info!(alias = %alias, url = %url, "DNS discovery: added leaf");

        let alias_owned = alias.clone();
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
                }
                Err(e) => {
                    tracing::warn!(
                        alias = %alias_owned,
                        error = %e,
                        "DNS-discovered leaf failed to initialize"
                    );
                    entry.mark_unhealthy().await;
                }
            }
        });
    }

    // Prune aliases we own that are no longer advertised.
    for alias in existing_dns_aliases.difference(&desired.keys().cloned().collect()) {
        if registry.remove_leaf(alias).await.is_some() {
            tracing::info!(alias = %alias, "DNS discovery: removed leaf (no longer advertised)");
        }
    }
}

fn sanitize_alias(target: &str) -> String {
    let cleaned: String = target
        .trim_end_matches('.')
        .to_ascii_lowercase()
        .chars()
        .map(|c| match c {
            'a'..='z' | '0'..='9' | '-' => c,
            '.' | '_' => '-',
            _ => '-',
        })
        .collect();
    format!("{DNS_ALIAS_PREFIX}{cleaned}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_valid_srv_line() {
        let r = parse_srv_line("10 5 8080 mcp-1.example.com.").unwrap();
        assert_eq!(r.port, 8080);
        // Trailing dot is stripped.
        assert_eq!(r.target, "mcp-1.example.com");
    }

    #[test]
    fn parse_srv_line_no_trailing_dot() {
        let r = parse_srv_line("0 0 9090 leaf").unwrap();
        assert_eq!(r.port, 9090);
        assert_eq!(r.target, "leaf");
    }

    #[test]
    fn parse_srv_line_rejects_malformed() {
        assert!(parse_srv_line("").is_none());
        assert!(parse_srv_line("just three fields").is_none());
        assert!(parse_srv_line("a b c d").is_none()); // port not numeric
    }

    #[test]
    fn parse_srv_output_multiple_lines() {
        let stdout = "10 5 8080 alpha.svc.\n10 5 8080 beta.svc.\n";
        let records = parse_srv_output(stdout);
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].target, "alpha.svc");
        assert_eq!(records[1].target, "beta.svc");
    }

    #[test]
    fn parse_srv_output_skips_blank_and_malformed() {
        let stdout = "\n10 5 8080 good.svc.\ngarbage\n";
        let records = parse_srv_output(stdout);
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].target, "good.svc");
    }

    #[test]
    fn sanitize_alias_lowercases_and_replaces_dots() {
        assert_eq!(
            sanitize_alias("Leaf-1.example.com."),
            "dns-leaf-1-example-com"
        );
    }

    #[test]
    fn sanitize_alias_replaces_underscores_and_bad_chars() {
        assert_eq!(sanitize_alias("weird_host!.svc"), "dns-weird-host--svc");
    }

    #[tokio::test]
    async fn reconcile_adds_and_removes() {
        let registry = Arc::new(Registry::new());
        let config = DnsDiscoveryConfig {
            enabled: true,
            srv_name: "_mcp._tcp.example.com".to_string(),
            poll_interval_seconds: 60,
            default_transport: crate::config::TransportType::HttpPost,
            url_path: "/mcp".to_string(),
            scheme: "http".to_string(),
        };

        // Initial reconcile — two records.
        let records = vec![
            SrvRecord {
                port: 8080,
                target: "alpha.svc".to_string(),
            },
            SrvRecord {
                port: 8080,
                target: "beta.svc".to_string(),
            },
        ];
        reconcile(&registry, &config, &records).await;

        let aliases: HashSet<String> = registry.aliases().await.into_iter().collect();
        assert!(aliases.contains("dns-alpha-svc"));
        assert!(aliases.contains("dns-beta-svc"));

        // Beta drops out.
        let records = vec![SrvRecord {
            port: 8080,
            target: "alpha.svc".to_string(),
        }];
        reconcile(&registry, &config, &records).await;

        let aliases: HashSet<String> = registry.aliases().await.into_iter().collect();
        assert!(aliases.contains("dns-alpha-svc"));
        assert!(!aliases.contains("dns-beta-svc"));
    }

    #[tokio::test]
    async fn reconcile_does_not_touch_non_dns_leaves() {
        use crate::test_helpers::make_server_config;

        let registry = Arc::new(Registry::new());
        // Manually add a file-configured leaf (no dns- prefix).
        registry
            .add_leaf(
                "static-leaf".to_string(),
                Arc::new(LeafEntry::new(make_server_config(
                    "static-leaf",
                    "http://static:8080/mcp",
                ))),
            )
            .await;

        let config = DnsDiscoveryConfig {
            enabled: true,
            srv_name: "_mcp._tcp.example.com".to_string(),
            poll_interval_seconds: 60,
            default_transport: crate::config::TransportType::HttpPost,
            url_path: "/mcp".to_string(),
            scheme: "http".to_string(),
        };

        // Reconcile with an empty record set — should NOT remove the static leaf.
        reconcile(&registry, &config, &[]).await;
        assert!(registry.get_leaf("static-leaf").await.is_some());
    }
}
