//! Optional operational HTTP surface: `GET /healthz`, `GET /metrics`.
//!
//! Started only when `HTTP_PORT` is configured; never started in
//! `--run-mode once`. Binds 0.0.0.0 — meant for private networks / container
//! port mappings, not public exposure.

use std::sync::Arc;

use axum::{extract::State, http::StatusCode, routing::get, Router};

use crate::metrics::Metrics;

#[derive(Clone)]
struct AppState {
    metrics: Arc<Metrics>,
    /// Seconds since the last successful scan after which /healthz reports 503.
    unhealthy_after_secs: u64,
}

/// Spawns the HTTP listener as a background task.
pub fn spawn(port: u16, metrics: Arc<Metrics>, unhealthy_after_secs: u64) {
    let state = AppState {
        metrics,
        unhealthy_after_secs,
    };
    tokio::spawn(async move {
        let app = Router::new()
            .route("/healthz", get(healthz))
            .route("/metrics", get(metrics_text))
            .with_state(state);
        let listener = match tokio::net::TcpListener::bind(("0.0.0.0", port)).await {
            Ok(l) => l,
            Err(e) => {
                tracing::error!(error = %e, port, "metrics listener failed to bind");
                return;
            }
        };
        tracing::info!(port, "metrics/health endpoint listening");
        if let Err(e) = axum::serve(listener, app).await {
            tracing::error!(error = %e, "metrics server exited");
        }
    });
}

async fn healthz(State(s): State<AppState>) -> (StatusCode, &'static str) {
    if s.metrics.healthy(s.unhealthy_after_secs) {
        (StatusCode::OK, "ok")
    } else {
        (StatusCode::SERVICE_UNAVAILABLE, "no recent successful scan")
    }
}

async fn metrics_text(State(s): State<AppState>) -> String {
    s.metrics.render()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a router identical to the one `spawn` serves, without binding
    /// a real port — lets handler tests run without network I/O.
    fn test_router(metrics: Arc<Metrics>, unhealthy_after_secs: u64) -> Router {
        let state = AppState {
            metrics,
            unhealthy_after_secs,
        };
        Router::new()
            .route("/healthz", get(healthz))
            .route("/metrics", get(metrics_text))
            .with_state(state)
    }

    /// Exercises the real HTTP path (bind + request), not just handler
    /// functions, so routing and status-code plumbing are covered too.
    #[tokio::test]
    async fn healthz_reflects_scan_health_and_metrics_serves_prometheus_text() {
        let metrics = Arc::new(Metrics::default());
        let app = test_router(Arc::clone(&metrics), 1800);

        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("bind ephemeral port");
        let addr = listener.local_addr().expect("listener has a local addr");
        tokio::spawn(async move {
            axum::serve(listener, app).await.expect("server run loop");
        });

        let base = format!("http://{addr}");
        let client = reqwest::Client::new();

        // No scan yet: 503.
        let resp = client
            .get(format!("{base}/healthz"))
            .send()
            .await
            .expect("healthz request");
        assert_eq!(resp.status(), reqwest::StatusCode::SERVICE_UNAVAILABLE);

        metrics.mark_scan_success();

        // After a successful scan: 200.
        let resp = client
            .get(format!("{base}/healthz"))
            .send()
            .await
            .expect("healthz request");
        assert_eq!(resp.status(), reqwest::StatusCode::OK);

        metrics
            .scans_total
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let resp = client
            .get(format!("{base}/metrics"))
            .send()
            .await
            .expect("metrics request");
        assert_eq!(resp.status(), reqwest::StatusCode::OK);
        let body = resp.text().await.expect("metrics body");
        assert!(body.contains("templar_liquidator_scans_total 1"));
    }
}
