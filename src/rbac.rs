use crate::config::ClientConfig;

/// Per-request authorization context. Populated by the auth middleware after a
/// bearer token has been resolved.
///
/// * `client_id` — a short identifier for audit/log output. `Some("admin")`
///   for the main admin token, `Some("client-<idx>")` for a matched per-client
///   entry (or the token prefix in tests), and `None` when no auth is
///   configured on the server.
/// * `allowed_servers` — `None` means unrestricted access (admin token, or no
///   per-client entries configured, or a client entry that lists `"*"`). A
///   `Some(list)` restricts visibility and dispatch to those aliases.
#[derive(Debug, Clone, Default)]
pub struct RequestContext {
    pub client_id: Option<String>,
    pub allowed_servers: Option<Vec<String>>,
}

impl RequestContext {
    /// Anonymous context — no auth was enforced on the server side, so the
    /// caller has full visibility. Used when `auth_token` is unset.
    pub fn anonymous() -> Self {
        Self {
            client_id: None,
            allowed_servers: None,
        }
    }

    /// Admin context — matched the main `auth_token`. Full visibility.
    pub fn admin() -> Self {
        Self {
            client_id: Some("admin".to_string()),
            allowed_servers: None,
        }
    }

    /// Return `true` if the caller may see/invoke the given leaf alias.
    /// Federation-native tools (alias `"federation"`) are always allowed.
    pub fn allows_alias(&self, alias: &str) -> bool {
        if alias == "federation" {
            return true;
        }
        match &self.allowed_servers {
            None => true,
            Some(list) => list.iter().any(|s| s == "*" || s == alias),
        }
    }

    /// A short identifier suitable for log/audit fields. Falls back to
    /// `"anonymous"` when no identity is attached.
    pub fn client_label(&self) -> &str {
        self.client_id.as_deref().unwrap_or("anonymous")
    }
}

/// Resolve a bearer token against the admin token and the per-client list.
///
/// Returns:
/// * `Some(RequestContext)` when the token is recognized (admin or client).
/// * `None` when the token matches nothing. The auth middleware must reject
///   the request in that case.
///
/// Behavior when `auth_token` is unset (server has no auth enforcement):
/// this function is not called — callers should short-circuit to
/// `RequestContext::anonymous()`.
pub fn resolve(
    presented: &str,
    admin_token: &str,
    clients: &[ClientConfig],
) -> Option<RequestContext> {
    if presented == admin_token {
        return Some(RequestContext::admin());
    }
    for (idx, client) in clients.iter().enumerate() {
        if client.token == presented {
            let id = format!("client-{idx}");
            let allowed = if client.allowed_servers.iter().any(|s| s == "*") {
                None
            } else {
                Some(client.allowed_servers.clone())
            };
            return Some(RequestContext {
                client_id: Some(id),
                allowed_servers: allowed,
            });
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn client(token: &str, servers: &[&str]) -> ClientConfig {
        ClientConfig {
            token: token.to_string(),
            allowed_servers: servers.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn anonymous_has_full_access() {
        let ctx = RequestContext::anonymous();
        assert!(ctx.allows_alias("anything"));
        assert!(ctx.allows_alias("federation"));
        assert_eq!(ctx.client_label(), "anonymous");
    }

    #[test]
    fn admin_has_full_access() {
        let ctx = RequestContext::admin();
        assert!(ctx.allows_alias("prod"));
        assert!(ctx.allows_alias("staging"));
        assert_eq!(ctx.client_label(), "admin");
    }

    #[test]
    fn resolve_admin_token() {
        let ctx = resolve("secret", "secret", &[]).unwrap();
        assert_eq!(ctx.client_id.as_deref(), Some("admin"));
        assert!(ctx.allowed_servers.is_none());
    }

    #[test]
    fn resolve_client_token_restricts_access() {
        let clients = vec![client("team-a", &["prod-k8s"])];
        let ctx = resolve("team-a", "admin", &clients).unwrap();
        assert_eq!(ctx.client_id.as_deref(), Some("client-0"));
        assert!(ctx.allows_alias("prod-k8s"));
        assert!(!ctx.allows_alias("staging-k8s"));
        // Federation-native tools remain accessible.
        assert!(ctx.allows_alias("federation"));
    }

    #[test]
    fn resolve_wildcard_grants_full_access() {
        let clients = vec![client("readonly", &["*"])];
        let ctx = resolve("readonly", "admin", &clients).unwrap();
        assert!(ctx.allowed_servers.is_none());
        assert!(ctx.allows_alias("prod-k8s"));
        assert!(ctx.allows_alias("staging-k8s"));
    }

    #[test]
    fn resolve_unknown_token_returns_none() {
        let clients = vec![client("known", &["prod"])];
        assert!(resolve("unknown", "admin", &clients).is_none());
    }

    #[test]
    fn admin_token_shadows_matching_client_token() {
        // If the admin token happens to also be listed as a client, admin wins.
        let clients = vec![client("dup", &["prod"])];
        let ctx = resolve("dup", "dup", &clients).unwrap();
        assert_eq!(ctx.client_id.as_deref(), Some("admin"));
        assert!(ctx.allowed_servers.is_none());
    }

    #[test]
    fn allows_alias_multi_server_list() {
        let ctx = RequestContext {
            client_id: Some("client-1".to_string()),
            allowed_servers: Some(vec!["a".to_string(), "b".to_string()]),
        };
        assert!(ctx.allows_alias("a"));
        assert!(ctx.allows_alias("b"));
        assert!(!ctx.allows_alias("c"));
    }
}
