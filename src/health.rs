use std::sync::Arc;
use std::time::Duration;

use crate::registry::{LeafEntry, Registry};

/// Cap on how long the exponential-backoff schedule can push the health check
/// interval when a leaf keeps failing.
const MAX_BACKOFF: Duration = Duration::from_secs(300);

pub async fn start_health_monitor(registry: Arc<Registry>) {
    let leaves = registry.all_leaves().await;
    for leaf in leaves {
        if !leaf.config.health_check.enabled {
            continue;
        }
        tokio::spawn(monitor_leaf(leaf));
    }
}

async fn monitor_leaf(leaf: Arc<LeafEntry>) {
    let base_interval = Duration::from_secs(leaf.config.health_check.interval_seconds);
    let threshold = leaf.config.health_check.failure_threshold.max(1);
    let multiplier = if leaf.config.health_check.backoff_multiplier >= 1.0 {
        leaf.config.health_check.backoff_multiplier
    } else {
        1.0
    };
    let alias = leaf.config.alias.clone();

    // First tick — we already ran initialize().
    tokio::time::sleep(base_interval).await;

    loop {
        match leaf.client.health_check().await {
            Ok(()) => {
                let was_unhealthy = !leaf.is_healthy().await;
                leaf.mark_healthy().await;

                if was_unhealthy {
                    tracing::info!(alias = %alias, "leaf server recovered");
                    match leaf.client.list_tools().await {
                        Ok(tools) => {
                            tracing::info!(
                                alias = %alias,
                                tool_count = tools.len(),
                                "refreshed tools after recovery"
                            );
                            *leaf.cached_tools.write().await = tools;
                            leaf.mark_tools_refreshed().await;
                        }
                        Err(e) => {
                            tracing::warn!(
                                alias = %alias,
                                error = %e,
                                "recovered but failed to refresh tools"
                            );
                        }
                    }
                }
            }
            Err(e) => {
                let failures = leaf.record_failure().await;
                if failures >= threshold {
                    // Flip the health state; the failure counter has already
                    // been incremented by record_failure, so we don't call
                    // mark_unhealthy (which would double-count).
                    *leaf.health.write().await = crate::registry::LeafHealth::Unhealthy;
                }
                tracing::warn!(
                    alias = %alias,
                    error = %e,
                    consecutive_failures = failures,
                    threshold = threshold,
                    "leaf server health check failed"
                );
            }
        }

        let health = *leaf.health.read().await;
        metrics::gauge!(
            "federation_leaf_health",
            "alias" => alias.clone()
        )
        .set(if health == crate::registry::LeafHealth::Healthy {
            1.0
        } else {
            0.0
        });

        let sleep_for = next_delay(
            base_interval,
            multiplier,
            threshold,
            *leaf.consecutive_failures.read().await,
        );
        tokio::time::sleep(sleep_for).await;
    }
}

/// Compute the next sleep interval. When `failures` is below the threshold we
/// stay at `base`. Beyond the threshold we grow geometrically by `multiplier`,
/// capped at [`MAX_BACKOFF`].
fn next_delay(base: Duration, multiplier: f64, threshold: u32, failures: u32) -> Duration {
    if failures < threshold || multiplier <= 1.0 {
        return base.min(MAX_BACKOFF);
    }
    let exponent = (failures - threshold) as i32;
    let factor = multiplier.powi(exponent);
    let secs = (base.as_secs_f64() * factor).min(MAX_BACKOFF.as_secs_f64());
    Duration::from_secs_f64(secs)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn below_threshold_uses_base_interval() {
        let d = next_delay(Duration::from_secs(30), 2.0, 3, 2);
        assert_eq!(d, Duration::from_secs(30));
    }

    #[test]
    fn at_threshold_uses_base_interval() {
        // Exponent is 0 → factor 1.0 → base interval.
        let d = next_delay(Duration::from_secs(30), 2.0, 3, 3);
        assert_eq!(d, Duration::from_secs(30));
    }

    #[test]
    fn above_threshold_grows_geometrically() {
        let base = Duration::from_secs(10);
        let d1 = next_delay(base, 2.0, 3, 4);
        let d2 = next_delay(base, 2.0, 3, 5);
        let d3 = next_delay(base, 2.0, 3, 6);
        assert_eq!(d1, Duration::from_secs(20));
        assert_eq!(d2, Duration::from_secs(40));
        assert_eq!(d3, Duration::from_secs(80));
    }

    #[test]
    fn caps_at_max_backoff() {
        let base = Duration::from_secs(60);
        // 60 * 2^10 = 61,440s — clamped to 300s.
        let d = next_delay(base, 2.0, 3, 13);
        assert_eq!(d, MAX_BACKOFF);
    }

    #[test]
    fn multiplier_of_one_disables_backoff() {
        let d = next_delay(Duration::from_secs(30), 1.0, 3, 100);
        assert_eq!(d, Duration::from_secs(30));
    }
}
