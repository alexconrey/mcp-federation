use serde::Deserialize;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

use crate::error::FederationError;

#[derive(Debug, Clone, Deserialize)]
pub struct FederationConfig {
    pub federation: FederationSettings,
    pub servers: Vec<ServerConfig>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct FederationSettings {
    #[serde(default = "default_listen")]
    pub listen: String,
    #[serde(default = "default_endpoint")]
    pub endpoint: String,
    pub auth_token: Option<String>,
    pub auth_token_file: Option<PathBuf>,
    #[serde(default)]
    pub connection_pool: ConnectionPoolConfig,
    #[serde(default = "default_tool_cache_ttl_seconds")]
    pub tool_cache_ttl_seconds: u64,
    /// Streamable HTTP session TTL. Sessions expire after this many seconds
    /// of inactivity. Default 1800 (30 minutes).
    #[serde(default = "default_session_ttl_seconds")]
    pub session_ttl_seconds: u64,
    /// If non-empty, restricts the `Origin` request header on `/mcp` to this
    /// list (DNS rebinding protection). Empty = allow all origins.
    #[serde(default)]
    pub allowed_origins: Vec<String>,
    /// Per-client bearer-token entries used for RBAC. Empty (default) means no
    /// client-specific filtering — the sole `auth_token` grants full access.
    #[serde(default)]
    pub clients: Vec<ClientConfig>,
    /// Per-client token-bucket rate limiter. Disabled by default.
    #[serde(default)]
    pub rate_limit: RateLimitConfig,
    /// Optional DNS SRV record-based discovery. When enabled, the federation
    /// polls the configured SRV name and dynamically registers/removes leaf
    /// servers as records appear or disappear.
    #[serde(default)]
    pub dns_discovery: DnsDiscoveryConfig,
    /// Optional Kubernetes CRD-based discovery. When enabled AND the binary
    /// was built with the `crd` cargo feature, a controller watches the
    /// `MCPServer` custom resource and registers leaves accordingly.
    #[serde(default)]
    pub crd_discovery: CrdDiscoveryConfig,
    /// Optional base64-encoded HMAC secret for signing session JWTs. If unset,
    /// a random 32-byte secret is generated at startup (sessions won't survive
    /// a restart). Set this to a stable value to keep sessions valid across
    /// deploys / rolling restarts.
    #[serde(default)]
    pub session_secret: Option<String>,
    /// Grace period, in seconds, applied to in-flight connections on shutdown
    /// (currently only wired into the HTTPS path). Default 15.
    #[serde(default = "default_shutdown_timeout_seconds")]
    pub shutdown_timeout_seconds: u64,
}

/// Kubernetes CRD-based discovery configuration. The controller only starts
/// when both `enabled` is true and the binary was built with the `crd`
/// feature; otherwise the setting is inert.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct CrdDiscoveryConfig {
    #[serde(default)]
    pub enabled: bool,
    /// Namespace to watch. Empty string means all namespaces.
    #[serde(default)]
    pub namespace: String,
}

/// DNS SRV-based discovery configuration.
///
/// When `enabled` is true, a background task polls the configured `srv_name`
/// SRV record on the given interval. For each `target:port` in the record set
/// a leaf server is dynamically registered under a sanitized alias derived
/// from the target hostname; disappearing targets are deregistered.
#[derive(Debug, Clone, Deserialize)]
pub struct DnsDiscoveryConfig {
    #[serde(default)]
    pub enabled: bool,
    /// Fully-qualified SRV record to query, e.g. `_mcp._tcp.example.com`.
    #[serde(default)]
    pub srv_name: String,
    #[serde(default = "default_dns_poll_interval")]
    pub poll_interval_seconds: u64,
    #[serde(default = "default_transport")]
    pub default_transport: TransportType,
    /// Optional URL path appended to each discovered `http://target:port` URL.
    /// Defaults to `/mcp`.
    #[serde(default = "default_dns_url_path")]
    pub url_path: String,
    /// Optional URL scheme. Defaults to `http`.
    #[serde(default = "default_dns_scheme")]
    pub scheme: String,
}

impl Default for DnsDiscoveryConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            srv_name: String::new(),
            poll_interval_seconds: default_dns_poll_interval(),
            default_transport: default_transport(),
            url_path: default_dns_url_path(),
            scheme: default_dns_scheme(),
        }
    }
}

fn default_dns_poll_interval() -> u64 {
    60
}

fn default_dns_url_path() -> String {
    "/mcp".to_string()
}

fn default_dns_scheme() -> String {
    "http".to_string()
}

/// Per-client RBAC entry. A bearer token grants access to a specific set of
/// leaf server aliases. Use `"*"` in `allowed_servers` to grant full access
/// (equivalent to the admin `auth_token`).
#[derive(Debug, Clone, Deserialize)]
pub struct ClientConfig {
    pub token: String,
    #[serde(default)]
    pub allowed_servers: Vec<String>,
}

/// Per-client token-bucket rate limiter configuration.
#[derive(Debug, Clone, Deserialize)]
pub struct RateLimitConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_rps")]
    pub requests_per_second: u32,
    #[serde(default = "default_burst")]
    pub burst_size: u32,
}

impl Default for RateLimitConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            requests_per_second: default_rps(),
            burst_size: default_burst(),
        }
    }
}

fn default_rps() -> u32 {
    10
}

fn default_burst() -> u32 {
    20
}

#[derive(Debug, Clone, Deserialize)]
pub struct ConnectionPoolConfig {
    #[serde(default = "default_max_idle_per_host")]
    pub max_idle_per_host: usize,
    #[serde(default = "default_idle_timeout_seconds")]
    pub idle_timeout_seconds: u64,
}

impl Default for ConnectionPoolConfig {
    fn default() -> Self {
        Self {
            max_idle_per_host: default_max_idle_per_host(),
            idle_timeout_seconds: default_idle_timeout_seconds(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct ServerConfig {
    pub alias: String,
    pub url: String,
    pub auth_token: Option<String>,
    pub auth_token_file: Option<PathBuf>,
    #[serde(default = "default_transport")]
    pub transport: TransportType,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub health_check: HealthCheckConfig,
    /// Per-leaf request timeout. Falls back to 30s when unset.
    pub timeout_seconds: Option<u64>,
    #[serde(default)]
    pub tls: TlsConfig,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TlsConfig {
    #[serde(default = "default_tls_verify")]
    pub verify: bool,
    #[serde(default)]
    pub ca_cert_path: Option<PathBuf>,
}

impl Default for TlsConfig {
    fn default() -> Self {
        Self {
            verify: default_tls_verify(),
            ca_cert_path: None,
        }
    }
}

fn default_tls_verify() -> bool {
    true
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(rename_all = "kebab-case")]
pub enum TransportType {
    HttpPost,
    StreamableHttp,
}

#[derive(Debug, Clone, Deserialize)]
pub struct HealthCheckConfig {
    #[serde(default = "default_health_enabled")]
    pub enabled: bool,
    #[serde(default = "default_health_interval")]
    pub interval_seconds: u64,
    #[serde(default = "default_health_timeout")]
    pub timeout_seconds: u64,
    #[serde(default = "default_failure_threshold")]
    pub failure_threshold: u32,
    #[serde(default = "default_backoff_multiplier")]
    pub backoff_multiplier: f64,
}

impl Default for HealthCheckConfig {
    fn default() -> Self {
        Self {
            enabled: default_health_enabled(),
            interval_seconds: default_health_interval(),
            timeout_seconds: default_health_timeout(),
            failure_threshold: default_failure_threshold(),
            backoff_multiplier: default_backoff_multiplier(),
        }
    }
}

fn default_listen() -> String {
    "0.0.0.0:8080".to_string()
}

fn default_endpoint() -> String {
    "/mcp".to_string()
}

fn default_transport() -> TransportType {
    TransportType::HttpPost
}

fn default_health_enabled() -> bool {
    true
}

fn default_health_interval() -> u64 {
    30
}

fn default_health_timeout() -> u64 {
    5
}

fn default_failure_threshold() -> u32 {
    3
}

fn default_backoff_multiplier() -> f64 {
    2.0
}

fn default_max_idle_per_host() -> usize {
    10
}

fn default_idle_timeout_seconds() -> u64 {
    90
}

fn default_tool_cache_ttl_seconds() -> u64 {
    300
}

fn default_session_ttl_seconds() -> u64 {
    1800
}

fn default_shutdown_timeout_seconds() -> u64 {
    15
}

/// Replace `${VAR_NAME}` occurrences with the value of the matching environment
/// variable. Unset variables expand to an empty string. An unclosed `${` leaves
/// the remainder of the string untouched.
pub fn interpolate_env_vars(content: &str) -> String {
    let mut result = String::with_capacity(content.len());
    let mut remainder = content;
    while let Some(start) = remainder.find("${") {
        result.push_str(&remainder[..start]);
        let after_open = &remainder[start + 2..];
        match after_open.find('}') {
            Some(end) => {
                let var_name = &after_open[..end];
                let value = std::env::var(var_name).unwrap_or_default();
                result.push_str(&value);
                remainder = &after_open[end + 1..];
            }
            None => {
                result.push_str(&remainder[start..]);
                return result;
            }
        }
    }
    result.push_str(remainder);
    result
}

impl FederationConfig {
    pub fn from_file(path: &Path) -> Result<Self, FederationError> {
        let content = std::fs::read_to_string(path).map_err(|e| {
            FederationError::Config(format!(
                "failed to read config file {}: {e}",
                path.display()
            ))
        })?;
        Self::parse(&content)
    }

    pub fn parse(content: &str) -> Result<Self, FederationError> {
        let interpolated = interpolate_env_vars(content);
        let config: FederationConfig = serde_yaml::from_str(&interpolated)
            .map_err(|e| FederationError::Config(e.to_string()))?;
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> Result<(), FederationError> {
        if self.servers.is_empty() {
            return Err(FederationError::Config(
                "at least one server must be configured".to_string(),
            ));
        }

        let mut aliases = HashSet::new();
        for server in &self.servers {
            if server.alias.is_empty() {
                return Err(FederationError::Config(
                    "server alias cannot be empty".to_string(),
                ));
            }
            if server.alias.contains("__") {
                return Err(FederationError::Config(format!(
                    "server alias '{}' cannot contain '__' (reserved as namespace separator)",
                    server.alias
                )));
            }
            if !aliases.insert(&server.alias) {
                return Err(FederationError::Config(format!(
                    "duplicate server alias: '{}'",
                    server.alias
                )));
            }
            if server.url.is_empty() {
                return Err(FederationError::Config(format!(
                    "server '{}' has empty url",
                    server.alias
                )));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_valid_config() {
        let yaml = r#"
federation:
  listen: "0.0.0.0:9090"
  endpoint: "/mcp"
  auth_token: "my-secret"

servers:
  - alias: "prod-k8s"
    url: "http://mcp-k8s-prod:8080/mcp"
    auth_token: "leaf-token"
    transport: http-post
    tags: ["kubernetes", "production"]
    health_check:
      enabled: true
      interval_seconds: 15
      timeout_seconds: 3

  - alias: "staging-k8s"
    url: "http://mcp-k8s-staging:8080/mcp"
    transport: streamable-http
"#;
        let config = FederationConfig::parse(yaml).unwrap();
        assert_eq!(config.federation.listen, "0.0.0.0:9090");
        assert_eq!(config.federation.auth_token, Some("my-secret".to_string()));
        assert_eq!(config.servers.len(), 2);
        assert_eq!(config.servers[0].alias, "prod-k8s");
        assert_eq!(config.servers[0].transport, TransportType::HttpPost);
        assert_eq!(config.servers[0].health_check.interval_seconds, 15);
        assert_eq!(config.servers[1].transport, TransportType::StreamableHttp);
        assert!(config.servers[1].health_check.enabled);
    }

    #[test]
    fn reject_empty_servers() {
        let yaml = r#"
federation:
  listen: "0.0.0.0:8080"
servers: []
"#;
        let err = FederationConfig::parse(yaml).unwrap_err();
        assert!(err.to_string().contains("at least one server"));
    }

    #[test]
    fn reject_duplicate_aliases() {
        let yaml = r#"
federation:
  listen: "0.0.0.0:8080"
servers:
  - alias: "prod"
    url: "http://a:8080/mcp"
  - alias: "prod"
    url: "http://b:8080/mcp"
"#;
        let err = FederationConfig::parse(yaml).unwrap_err();
        assert!(err.to_string().contains("duplicate server alias"));
    }

    #[test]
    fn reject_alias_with_separator() {
        let yaml = r#"
federation:
  listen: "0.0.0.0:8080"
servers:
  - alias: "prod__k8s"
    url: "http://a:8080/mcp"
"#;
        let err = FederationConfig::parse(yaml).unwrap_err();
        assert!(err.to_string().contains("cannot contain '__'"));
    }

    #[test]
    fn defaults_applied() {
        let yaml = r#"
federation: {}
servers:
  - alias: "test"
    url: "http://test:8080/mcp"
"#;
        let config = FederationConfig::parse(yaml).unwrap();
        assert_eq!(config.federation.listen, "0.0.0.0:8080");
        assert_eq!(config.federation.endpoint, "/mcp");
        assert_eq!(config.servers[0].transport, TransportType::HttpPost);
        assert!(config.servers[0].health_check.enabled);
        assert_eq!(config.servers[0].health_check.interval_seconds, 30);
        assert_eq!(config.servers[0].health_check.failure_threshold, 3);
        assert!((config.servers[0].health_check.backoff_multiplier - 2.0).abs() < f64::EPSILON);
        assert!(config.servers[0].timeout_seconds.is_none());
        assert_eq!(config.federation.connection_pool.max_idle_per_host, 10);
        assert_eq!(config.federation.connection_pool.idle_timeout_seconds, 90);
        assert_eq!(config.federation.tool_cache_ttl_seconds, 300);
        assert_eq!(config.federation.session_ttl_seconds, 1800);
        assert_eq!(config.federation.shutdown_timeout_seconds, 15);
        assert!(config.federation.session_secret.is_none());
        assert!(!config.federation.crd_discovery.enabled);
        assert!(config.federation.crd_discovery.namespace.is_empty());
        assert!(config.federation.allowed_origins.is_empty());
        assert!(config.federation.clients.is_empty());
        assert!(!config.federation.rate_limit.enabled);
        assert_eq!(config.federation.rate_limit.requests_per_second, 10);
        assert_eq!(config.federation.rate_limit.burst_size, 20);
    }

    #[test]
    fn clients_config_parses() {
        let yaml = r#"
federation:
  auth_token: "admin"
  clients:
    - token: "team-a-token"
      allowed_servers: ["prod-k8s"]
    - token: "team-b-token"
      allowed_servers: ["prod-k8s", "staging-k8s"]
    - token: "readonly-token"
      allowed_servers: ["*"]
servers:
  - alias: "prod-k8s"
    url: "http://prod:8080/mcp"
  - alias: "staging-k8s"
    url: "http://staging:8080/mcp"
"#;
        let config = FederationConfig::parse(yaml).unwrap();
        let clients = &config.federation.clients;
        assert_eq!(clients.len(), 3);
        assert_eq!(clients[0].token, "team-a-token");
        assert_eq!(clients[0].allowed_servers, vec!["prod-k8s".to_string()]);
        assert_eq!(
            clients[1].allowed_servers,
            vec!["prod-k8s".to_string(), "staging-k8s".to_string()]
        );
        assert_eq!(clients[2].allowed_servers, vec!["*".to_string()]);
    }

    #[test]
    fn rate_limit_config_parses() {
        let yaml = r#"
federation:
  rate_limit:
    enabled: true
    requests_per_second: 25
    burst_size: 50
servers:
  - alias: "leaf"
    url: "http://leaf:8080/mcp"
"#;
        let config = FederationConfig::parse(yaml).unwrap();
        assert!(config.federation.rate_limit.enabled);
        assert_eq!(config.federation.rate_limit.requests_per_second, 25);
        assert_eq!(config.federation.rate_limit.burst_size, 50);
    }

    #[test]
    fn rate_limit_partial_config_uses_defaults() {
        let yaml = r#"
federation:
  rate_limit:
    enabled: true
servers:
  - alias: "leaf"
    url: "http://leaf:8080/mcp"
"#;
        let config = FederationConfig::parse(yaml).unwrap();
        assert!(config.federation.rate_limit.enabled);
        assert_eq!(config.federation.rate_limit.requests_per_second, 10);
        assert_eq!(config.federation.rate_limit.burst_size, 20);
    }

    #[test]
    fn session_settings_parse() {
        let yaml = r#"
federation:
  session_ttl_seconds: 60
  allowed_origins:
    - "https://console.example.com"
    - "https://tools.example.com"
servers:
  - alias: "leaf"
    url: "http://leaf:8080/mcp"
"#;
        let config = FederationConfig::parse(yaml).unwrap();
        assert_eq!(config.federation.session_ttl_seconds, 60);
        assert_eq!(config.federation.allowed_origins.len(), 2);
        assert_eq!(
            config.federation.allowed_origins[0],
            "https://console.example.com"
        );
    }

    #[test]
    fn tool_cache_ttl_parses() {
        let yaml = r#"
federation:
  tool_cache_ttl_seconds: 60
servers:
  - alias: "leaf"
    url: "http://leaf:8080/mcp"
"#;
        let config = FederationConfig::parse(yaml).unwrap();
        assert_eq!(config.federation.tool_cache_ttl_seconds, 60);
    }

    #[test]
    fn per_leaf_timeout_parses() {
        let yaml = r#"
federation: {}
servers:
  - alias: "slow"
    url: "http://slow:8080/mcp"
    timeout_seconds: 120
"#;
        let config = FederationConfig::parse(yaml).unwrap();
        assert_eq!(config.servers[0].timeout_seconds, Some(120));
    }

    #[test]
    fn connection_pool_parses() {
        let yaml = r#"
federation:
  connection_pool:
    max_idle_per_host: 42
    idle_timeout_seconds: 300
servers:
  - alias: "leaf"
    url: "http://leaf:8080/mcp"
"#;
        let config = FederationConfig::parse(yaml).unwrap();
        assert_eq!(config.federation.connection_pool.max_idle_per_host, 42);
        assert_eq!(config.federation.connection_pool.idle_timeout_seconds, 300);
    }

    #[test]
    fn circuit_breaker_config_parses() {
        let yaml = r#"
federation: {}
servers:
  - alias: "flaky"
    url: "http://flaky:8080/mcp"
    health_check:
      failure_threshold: 5
      backoff_multiplier: 3.0
"#;
        let config = FederationConfig::parse(yaml).unwrap();
        assert_eq!(config.servers[0].health_check.failure_threshold, 5);
        assert!((config.servers[0].health_check.backoff_multiplier - 3.0).abs() < f64::EPSILON);
    }

    #[test]
    fn env_var_interpolation_replaces_set_var() {
        std::env::set_var("MCP_FED_TEST_TOKEN_A", "sekret-abc");
        let yaml = r#"
federation:
  auth_token: "${MCP_FED_TEST_TOKEN_A}"
servers:
  - alias: "leaf"
    url: "http://leaf:8080/mcp"
"#;
        let config = FederationConfig::parse(yaml).unwrap();
        assert_eq!(config.federation.auth_token, Some("sekret-abc".to_string()));
        std::env::remove_var("MCP_FED_TEST_TOKEN_A");
    }

    #[test]
    fn env_var_interpolation_missing_var_becomes_empty() {
        std::env::remove_var("MCP_FED_TEST_MISSING_XYZ");
        let yaml = r#"
federation: {}
servers:
  - alias: "leaf"
    url: "http://leaf:8080/mcp"
    auth_token: "${MCP_FED_TEST_MISSING_XYZ}"
"#;
        let config = FederationConfig::parse(yaml).unwrap();
        assert_eq!(config.servers[0].auth_token, Some("".to_string()));
    }

    #[test]
    fn env_var_interpolation_multiple_and_embedded() {
        std::env::set_var("MCP_FED_TEST_HOST", "leaf.example.com");
        std::env::set_var("MCP_FED_TEST_PORT", "9000");
        let yaml = r#"
federation: {}
servers:
  - alias: "leaf"
    url: "http://${MCP_FED_TEST_HOST}:${MCP_FED_TEST_PORT}/mcp"
"#;
        let config = FederationConfig::parse(yaml).unwrap();
        assert_eq!(config.servers[0].url, "http://leaf.example.com:9000/mcp");
        std::env::remove_var("MCP_FED_TEST_HOST");
        std::env::remove_var("MCP_FED_TEST_PORT");
    }

    #[test]
    fn env_var_interpolation_unclosed_brace_left_alone() {
        // A stray "${" without a closing brace should not panic or drop data.
        let raw = "hello ${UNCLOSED and more";
        let out = interpolate_env_vars(raw);
        assert_eq!(out, "hello ${UNCLOSED and more");
    }

    #[test]
    fn tls_defaults_verify_true_and_no_ca() {
        let yaml = r#"
federation: {}
servers:
  - alias: "leaf"
    url: "https://leaf:8443/mcp"
"#;
        let config = FederationConfig::parse(yaml).unwrap();
        assert!(config.servers[0].tls.verify);
        assert!(config.servers[0].tls.ca_cert_path.is_none());
    }

    #[test]
    fn tls_verify_false_and_ca_cert_parse() {
        let yaml = r#"
federation: {}
servers:
  - alias: "leaf"
    url: "https://leaf:8443/mcp"
    tls:
      verify: false
      ca_cert_path: "/etc/pki/leaf-ca.pem"
"#;
        let config = FederationConfig::parse(yaml).unwrap();
        assert!(!config.servers[0].tls.verify);
        assert_eq!(
            config.servers[0].tls.ca_cert_path.as_deref(),
            Some(std::path::Path::new("/etc/pki/leaf-ca.pem"))
        );
    }

    #[test]
    fn federation_auth_token_file_parses() {
        let yaml = r#"
federation:
  auth_token_file: "/var/run/secrets/federation/token"
servers:
  - alias: "leaf"
    url: "http://leaf:8080/mcp"
"#;
        let config = FederationConfig::parse(yaml).unwrap();
        assert_eq!(
            config.federation.auth_token_file.as_deref(),
            Some(std::path::Path::new("/var/run/secrets/federation/token"))
        );
    }

    #[test]
    fn dns_discovery_defaults_disabled() {
        let yaml = r#"
federation: {}
servers:
  - alias: "leaf"
    url: "http://leaf:8080/mcp"
"#;
        let config = FederationConfig::parse(yaml).unwrap();
        assert!(!config.federation.dns_discovery.enabled);
        assert_eq!(config.federation.dns_discovery.poll_interval_seconds, 60);
        assert_eq!(
            config.federation.dns_discovery.default_transport,
            TransportType::HttpPost
        );
        assert_eq!(config.federation.dns_discovery.url_path, "/mcp");
        assert_eq!(config.federation.dns_discovery.scheme, "http");
    }

    #[test]
    fn dns_discovery_config_parses() {
        let yaml = r#"
federation:
  dns_discovery:
    enabled: true
    srv_name: "_mcp._tcp.example.com"
    poll_interval_seconds: 30
    default_transport: streamable-http
    url_path: "/mcp"
    scheme: "https"
servers:
  - alias: "leaf"
    url: "http://leaf:8080/mcp"
"#;
        let config = FederationConfig::parse(yaml).unwrap();
        let dns = &config.federation.dns_discovery;
        assert!(dns.enabled);
        assert_eq!(dns.srv_name, "_mcp._tcp.example.com");
        assert_eq!(dns.poll_interval_seconds, 30);
        assert_eq!(dns.default_transport, TransportType::StreamableHttp);
        assert_eq!(dns.scheme, "https");
    }

    #[test]
    fn crd_discovery_config_parses() {
        let yaml = r#"
federation:
  crd_discovery:
    enabled: true
    namespace: "mcp-system"
servers:
  - alias: "leaf"
    url: "http://leaf:8080/mcp"
"#;
        let config = FederationConfig::parse(yaml).unwrap();
        assert!(config.federation.crd_discovery.enabled);
        assert_eq!(config.federation.crd_discovery.namespace, "mcp-system");
    }

    #[test]
    fn shutdown_timeout_parses() {
        let yaml = r#"
federation:
  shutdown_timeout_seconds: 45
servers:
  - alias: "leaf"
    url: "http://leaf:8080/mcp"
"#;
        let config = FederationConfig::parse(yaml).unwrap();
        assert_eq!(config.federation.shutdown_timeout_seconds, 45);
    }

    #[test]
    fn session_secret_parses() {
        let yaml = r#"
federation:
  session_secret: "c2VjcmV0LWJhc2U2NA=="
servers:
  - alias: "leaf"
    url: "http://leaf:8080/mcp"
"#;
        let config = FederationConfig::parse(yaml).unwrap();
        assert_eq!(
            config.federation.session_secret.as_deref(),
            Some("c2VjcmV0LWJhc2U2NA==")
        );
    }

    #[test]
    fn leaf_auth_token_file_parses() {
        let yaml = r#"
federation: {}
servers:
  - alias: "leaf"
    url: "http://leaf:8080/mcp"
    auth_token_file: "/var/run/secrets/leaf/token"
"#;
        let config = FederationConfig::parse(yaml).unwrap();
        assert_eq!(
            config.servers[0].auth_token_file.as_deref(),
            Some(std::path::Path::new("/var/run/secrets/leaf/token"))
        );
    }
}
