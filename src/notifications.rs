//! Notification forwarding broker.
//!
//! Fully-realized MCP notification forwarding needs two SSE plumbing pieces
//! this stub does not implement yet:
//!
//! 1. **Client side** — a `GET /mcp` endpoint that upgrades to Server-Sent
//!    Events per the Streamable HTTP spec. Each connected client would
//!    `subscribe` here and receive notifications relayed from the federation.
//!    The Streamable HTTP agent stubs `GET /mcp` today; this broker becomes
//!    the payload channel it emits from.
//! 2. **Leaf side** — long-lived SSE (or Streamable HTTP) connections to
//!    each leaf server so we can observe notifications leaves emit (e.g.
//!    `notifications/tools/list_changed`). On receipt the federation would
//!    rewrite the namespaced fields where relevant, then `publish` here.
//!
//! Until both sides are wired up we log unroutable notifications so operators
//! can see them; the broker itself is fully functional and unit-tested so the
//! wiring can be added incrementally without changing its API.
//!
//! Namespacing note: notifications forwarded from leaf `X` MUST be augmented
//! so downstream clients can tell them apart (`_leaf: "X"` on the params
//! object at minimum; ideally also rewriting `tool` / `uri` fields to the
//! `X__foo` form the rest of the federation uses).

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::{mpsc, RwLock};
use uuid::Uuid;

/// Fanout channel for MCP notifications. Every subscriber gets an owned
/// `Receiver` that receives a clone of every published payload.
///
/// Channels are bounded (buffer size 64). If a subscriber falls behind and its
/// buffer fills, the send is dropped for that subscriber only (and a warning
/// is logged) so a slow client cannot backpressure the entire broker.
pub struct NotificationBroker {
    subscribers: RwLock<HashMap<String, mpsc::Sender<serde_json::Value>>>,
}

const CHANNEL_BUFFER: usize = 64;

impl NotificationBroker {
    pub fn new() -> Self {
        Self {
            subscribers: RwLock::new(HashMap::new()),
        }
    }

    /// Register a new subscriber. Returns (id, receiver). The id can be used
    /// with [`Self::unsubscribe`] to remove the subscription (e.g. when an
    /// SSE client disconnects).
    pub async fn subscribe(&self) -> (String, mpsc::Receiver<serde_json::Value>) {
        let (tx, rx) = mpsc::channel(CHANNEL_BUFFER);
        let id = Uuid::new_v4().to_string();
        self.subscribers.write().await.insert(id.clone(), tx);
        (id, rx)
    }

    /// Remove a subscriber. Returns true if it was present.
    pub async fn unsubscribe(&self, id: &str) -> bool {
        self.subscribers.write().await.remove(id).is_some()
    }

    /// Broadcast a notification to every subscriber. Returns the number of
    /// subscribers the message was successfully queued to. Dead or full
    /// channels are dropped from the roster.
    pub async fn publish(&self, notification: serde_json::Value) -> usize {
        let mut delivered = 0usize;
        let mut to_remove: Vec<String> = Vec::new();

        {
            let subs = self.subscribers.read().await;
            for (id, tx) in subs.iter() {
                match tx.try_send(notification.clone()) {
                    Ok(()) => delivered += 1,
                    Err(mpsc::error::TrySendError::Full(_)) => {
                        tracing::warn!(
                            subscriber = %id,
                            "notification dropped: subscriber channel full"
                        );
                    }
                    Err(mpsc::error::TrySendError::Closed(_)) => {
                        to_remove.push(id.clone());
                    }
                }
            }
        }

        if !to_remove.is_empty() {
            let mut subs = self.subscribers.write().await;
            for id in to_remove {
                subs.remove(&id);
            }
        }
        delivered
    }

    pub async fn subscriber_count(&self) -> usize {
        self.subscribers.read().await.len()
    }
}

impl Default for NotificationBroker {
    fn default() -> Self {
        Self::new()
    }
}

/// Log a notification received from a leaf that we can't forward yet. This is
/// the hook the (future) leaf-side SSE reader will call before invoking
/// `broker.publish`. Kept as its own function so operators can grep for the
/// call site while the plumbing is being built out.
pub fn log_unroutable_leaf_notification(
    alias: &str,
    method: &str,
    _broker: &Arc<NotificationBroker>,
) {
    tracing::warn!(
        alias = %alias,
        method = %method,
        "leaf sent notification but federation SSE forwarding is not yet wired up"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn subscribe_then_receive() {
        let broker = NotificationBroker::new();
        let (_id, mut rx) = broker.subscribe().await;
        assert_eq!(broker.subscriber_count().await, 1);

        let delivered = broker
            .publish(serde_json::json!({"method": "notifications/tools/list_changed"}))
            .await;
        assert_eq!(delivered, 1);

        let msg = rx.recv().await.expect("expected a message");
        assert_eq!(msg["method"], "notifications/tools/list_changed");
    }

    #[tokio::test]
    async fn multiple_subscribers_all_receive() {
        let broker = NotificationBroker::new();
        let (_id_a, mut rx_a) = broker.subscribe().await;
        let (_id_b, mut rx_b) = broker.subscribe().await;

        let n = broker.publish(serde_json::json!({"n": 1})).await;
        assert_eq!(n, 2);

        assert_eq!(rx_a.recv().await.unwrap()["n"], 1);
        assert_eq!(rx_b.recv().await.unwrap()["n"], 1);
    }

    #[tokio::test]
    async fn unsubscribe_removes_receiver() {
        let broker = NotificationBroker::new();
        let (id, _rx) = broker.subscribe().await;
        assert!(broker.unsubscribe(&id).await);
        assert_eq!(broker.subscriber_count().await, 0);
        assert!(!broker.unsubscribe(&id).await);
    }

    #[tokio::test]
    async fn publish_prunes_closed_channels() {
        let broker = NotificationBroker::new();
        let (_id, rx) = broker.subscribe().await;
        drop(rx);

        let delivered = broker.publish(serde_json::json!({"x": 1})).await;
        assert_eq!(delivered, 0);
        assert_eq!(broker.subscriber_count().await, 0);
    }
}
