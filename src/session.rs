use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use base64::Engine;
use jsonwebtoken::{
    decode, encode, Algorithm, DecodingKey, EncodingKey, Header, Validation,
};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;
use uuid::Uuid;

const JWT_ISSUER: &str = "mcp-federation";
const JWT_ALGORITHM: Algorithm = Algorithm::HS256;

#[derive(Debug, Clone)]
pub struct SessionState {
    pub created_at: Instant,
    pub last_active: Instant,
    pub client_info: Option<serde_json::Value>,
    /// Per-session log level set via `logging/setLevel`. `None` means the
    /// client has not configured a preference.
    pub log_level: Option<String>,
}

/// JWT claims embedded in the session id. `sub` is a stable random UUID that
/// identifies the session; the JWT string itself is the key in the in-memory
/// map so touch/terminate can operate on it without redecoding.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionClaims {
    pub sub: String,
    pub iss: String,
    pub iat: u64,
    pub exp: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_info: Option<serde_json::Value>,
}

pub struct SessionManager {
    ttl: Duration,
    sessions: Arc<RwLock<HashMap<String, SessionState>>>,
    encoding_key: EncodingKey,
    decoding_key: DecodingKey,
}

impl SessionManager {
    /// Build a manager with the given TTL and optional base64-encoded HMAC
    /// secret. When `secret` is `None`, generate a fresh 32-byte random key —
    /// sessions won't survive a restart in that case.
    pub fn new(ttl_seconds: u64, secret: Option<String>) -> Self {
        let key_bytes = match secret {
            Some(b64) => match base64::engine::general_purpose::STANDARD.decode(b64.as_bytes()) {
                Ok(bytes) if !bytes.is_empty() => bytes,
                Ok(_) => {
                    tracing::warn!(
                        "session_secret decoded to empty bytes; generating random key"
                    );
                    random_key()
                }
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        "session_secret is not valid base64; generating random key"
                    );
                    random_key()
                }
            },
            None => random_key(),
        };

        Self {
            ttl: Duration::from_secs(ttl_seconds),
            sessions: Arc::new(RwLock::new(HashMap::new())),
            encoding_key: EncodingKey::from_secret(&key_bytes),
            decoding_key: DecodingKey::from_secret(&key_bytes),
        }
    }

    pub fn ttl(&self) -> Duration {
        self.ttl
    }

    /// Mint a new session and return the encoded JWT as the session id.
    pub async fn create_session(&self, client_info: Option<serde_json::Value>) -> String {
        let sub = Uuid::new_v4().to_string();
        let now_secs = unix_now();
        let claims = SessionClaims {
            sub,
            iss: JWT_ISSUER.to_string(),
            iat: now_secs,
            exp: now_secs + self.ttl.as_secs(),
            client_info: client_info.clone(),
        };
        let jwt = encode(&Header::new(JWT_ALGORITHM), &claims, &self.encoding_key)
            .expect("HS256 encoding should never fail with a valid secret");

        let now = Instant::now();
        let state = SessionState {
            created_at: now,
            last_active: now,
            client_info,
            log_level: None,
        };
        self.sessions.write().await.insert(jwt.clone(), state);
        jwt
    }

    /// Decode the JWT (verifies signature + expiry) and confirm the session
    /// is still tracked in memory.
    pub async fn validate_session(&self, id: &str) -> bool {
        if self.decode_claims(id).is_err() {
            return false;
        }
        let sessions = self.sessions.read().await;
        match sessions.get(id) {
            Some(s) => s.last_active.elapsed() < self.ttl,
            None => false,
        }
    }

    /// Reset the session's last_active timestamp. No-op if session is unknown.
    pub async fn touch_session(&self, id: &str) {
        let mut sessions = self.sessions.write().await;
        if let Some(s) = sessions.get_mut(id) {
            s.last_active = Instant::now();
        }
    }

    /// Remove a session. Returns true if a session was removed.
    pub async fn terminate_session(&self, id: &str) -> bool {
        self.sessions.write().await.remove(id).is_some()
    }

    /// Store a log-level preference on the session. Returns false when the
    /// session is unknown (no state change made).
    pub async fn set_log_level(&self, id: &str, level: String) -> bool {
        let mut sessions = self.sessions.write().await;
        match sessions.get_mut(id) {
            Some(s) => {
                s.log_level = Some(level);
                true
            }
            None => false,
        }
    }

    pub async fn get_log_level(&self, id: &str) -> Option<String> {
        self.sessions
            .read()
            .await
            .get(id)
            .and_then(|s| s.log_level.clone())
    }

    /// Remove all expired sessions. Returns the number pruned.
    pub async fn prune_expired(&self) -> usize {
        let ttl = self.ttl;
        let mut sessions = self.sessions.write().await;
        let before = sessions.len();
        sessions.retain(|_, s| s.last_active.elapsed() < ttl);
        before - sessions.len()
    }

    pub async fn active_count(&self) -> usize {
        self.sessions.read().await.len()
    }

    /// Decode + verify the JWT with this manager's secret. Public so callers
    /// can pull `client_info` back out of an already-issued session id.
    pub fn decode_claims(&self, id: &str) -> Result<SessionClaims, jsonwebtoken::errors::Error> {
        let mut validation = Validation::new(JWT_ALGORITHM);
        validation.set_issuer(&[JWT_ISSUER]);
        validation.leeway = 5;
        let data = decode::<SessionClaims>(id, &self.decoding_key, &validation)?;
        Ok(data.claims)
    }
}

fn random_key() -> Vec<u8> {
    let mut bytes = vec![0u8; 32];
    rand::thread_rng().fill_bytes(&mut bytes);
    bytes
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Spawn a background task that periodically prunes expired sessions.
/// The task exits when the returned `Arc<SessionManager>` is dropped
/// (checked via `Arc::strong_count` at the top of each tick).
pub fn spawn_prune_task(
    manager: Arc<SessionManager>,
    interval: Duration,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let weak = Arc::downgrade(&manager);
        drop(manager);
        loop {
            tokio::time::sleep(interval).await;
            let Some(m) = weak.upgrade() else {
                break;
            };
            let pruned = m.prune_expired().await;
            if pruned > 0 {
                tracing::debug!(pruned, "pruned expired MCP sessions");
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_mgr(ttl: u64) -> SessionManager {
        // Stable base64-encoded secret keeps tests reproducible.
        SessionManager::new(
            ttl,
            Some("dGVzdC1zZWNyZXQtMzItYnl0ZXMtZm9yLXVuaXR0ZXN0cw==".to_string()),
        )
    }

    #[tokio::test]
    async fn create_and_validate_session() {
        let mgr = make_mgr(60);
        let id = mgr.create_session(None).await;
        assert!(mgr.validate_session(&id).await);
        assert!(!mgr.validate_session("nonexistent").await);
    }

    #[tokio::test]
    async fn session_ids_are_unique() {
        let mgr = make_mgr(60);
        let a = mgr.create_session(None).await;
        let b = mgr.create_session(None).await;
        assert_ne!(a, b);
        assert_eq!(mgr.active_count().await, 2);
    }

    #[tokio::test]
    async fn terminate_session_removes_it() {
        let mgr = make_mgr(60);
        let id = mgr.create_session(None).await;
        assert!(mgr.terminate_session(&id).await);
        assert!(!mgr.validate_session(&id).await);
        assert!(!mgr.terminate_session(&id).await);
    }

    #[tokio::test]
    async fn touch_session_extends_lifetime() {
        let mgr = make_mgr(60);
        let id = mgr.create_session(None).await;
        {
            let mut sessions = mgr.sessions.write().await;
            let entry = sessions.get_mut(&id).unwrap();
            entry.last_active = Instant::now() - Duration::from_secs(30);
        }
        mgr.touch_session(&id).await;
        let sessions = mgr.sessions.read().await;
        let entry = sessions.get(&id).unwrap();
        assert!(entry.last_active.elapsed() < Duration::from_secs(5));
    }

    #[tokio::test]
    async fn expired_session_not_valid() {
        let mgr = make_mgr(60);
        let id = mgr.create_session(None).await;
        {
            let mut sessions = mgr.sessions.write().await;
            let entry = sessions.get_mut(&id).unwrap();
            entry.last_active = Instant::now() - Duration::from_secs(120);
        }
        assert!(!mgr.validate_session(&id).await);
    }

    #[tokio::test]
    async fn prune_removes_expired_only() {
        let mgr = make_mgr(60);
        let fresh = mgr.create_session(None).await;
        let stale = mgr.create_session(None).await;
        {
            let mut sessions = mgr.sessions.write().await;
            sessions.get_mut(&stale).unwrap().last_active =
                Instant::now() - Duration::from_secs(120);
        }
        assert_eq!(mgr.prune_expired().await, 1);
        assert!(mgr.validate_session(&fresh).await);
        assert!(!mgr.validate_session(&stale).await);
    }

    #[tokio::test]
    async fn stores_client_info() {
        let mgr = make_mgr(60);
        let info = serde_json::json!({"name": "test-client", "version": "1.0"});
        let id = mgr.create_session(Some(info.clone())).await;
        let sessions = mgr.sessions.read().await;
        assert_eq!(sessions.get(&id).unwrap().client_info.as_ref(), Some(&info));
    }

    #[tokio::test]
    async fn session_id_is_a_signed_jwt() {
        let mgr = make_mgr(60);
        let info = serde_json::json!({"name": "cli", "version": "9"});
        let id = mgr.create_session(Some(info.clone())).await;

        // Three base64url dot-separated segments.
        assert_eq!(id.matches('.').count(), 2);

        let claims = mgr.decode_claims(&id).unwrap();
        assert_eq!(claims.iss, JWT_ISSUER);
        assert!(claims.exp > claims.iat);
        assert_eq!(claims.exp - claims.iat, 60);
        assert_eq!(claims.client_info.as_ref(), Some(&info));
    }

    #[tokio::test]
    async fn foreign_secret_rejects_jwt() {
        // A JWT minted under one secret must fail signature validation
        // against a manager that was built with a different secret.
        let issuer = make_mgr(60);
        let id = issuer.create_session(None).await;

        let stranger = SessionManager::new(
            60,
            Some("YW5vdGhlci1zZWNyZXQtMzItYnl0ZXMtZm9yLXRlc3RzXw==".to_string()),
        );
        assert!(stranger.decode_claims(&id).is_err());
        assert!(!stranger.validate_session(&id).await);
    }

    #[tokio::test]
    async fn malformed_id_is_not_valid() {
        let mgr = make_mgr(60);
        assert!(!mgr.validate_session("not-a-jwt").await);
        assert!(!mgr.validate_session("").await);
        assert!(!mgr.validate_session("a.b.c").await);
    }

    #[tokio::test]
    async fn auto_generated_secret_works() {
        // No secret provided — SessionManager should still mint and validate.
        let mgr = SessionManager::new(60, None);
        let id = mgr.create_session(None).await;
        assert!(mgr.validate_session(&id).await);
    }

    #[test]
    fn invalid_base64_secret_falls_back_to_random() {
        // Should not panic even though the secret is not valid base64.
        let _mgr = SessionManager::new(60, Some("!!!not-base64!!!".to_string()));
    }
}
