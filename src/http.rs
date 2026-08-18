//! Optional operational HTTP surface: `GET /healthz`, `GET /metrics`.
//!
//! Started only when `HTTP_PORT` is configured; never started in
//! `--run-mode once`. Binds 0.0.0.0 — meant for private networks / container
//! port mappings, not public exposure.
//!
//! `/healthz` is a **readiness** signal (can this process currently do its
//! job?), not a liveness one. Wiring it to a liveness probe would
//! restart-loop a bot stuck on a persistent problem (e.g. every market
//! unreachable) without fixing anything — restarting doesn't change whether
//! the RPC endpoint is up.

use std::sync::Arc;

use axum::{
    extract::State,
    http::{header, StatusCode},
    response::IntoResponse,
    routing::get,
    Router,
};

use crate::metrics::Metrics;

#[derive(Clone)]
struct AppState {
    metrics: Arc<Metrics>,
    /// Seconds since the last successful scan after which /healthz reports 503.
    unhealthy_after_secs: u64,
}

/// Builds the route table. Shared by `spawn` and the tests below, so a
/// renamed or removed route can't leave a hand-rebuilt test table green.
fn router(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/metrics", get(metrics_text))
        .with_state(state)
}

/// Spawns the HTTP listener as a background task.
pub fn spawn(port: u16, metrics: Arc<Metrics>, unhealthy_after_secs: u64) {
    let state = AppState {
        metrics,
        unhealthy_after_secs,
    };
    tokio::spawn(async move {
        let app = router(state);
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

async fn metrics_text(State(s): State<AppState>) -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/plain; version=0.0.4")],
        s.metrics.render(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Exercises the real HTTP path (bind + request) through the exact
    /// route table `spawn` serves, not just the handler functions, so
    /// routing and status-code plumbing are covered too.
    #[tokio::test]
    async fn healthz_reflects_scan_health_and_metrics_serves_prometheus_text() {
        let metrics = Arc::new(Metrics::default());
        let state = AppState {
            metrics: Arc::clone(&metrics),
            unhealthy_after_secs: 1800,
        };
        let app = router(state);

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

        metrics.inc_scan();
        let resp = client
            .get(format!("{base}/metrics"))
            .send()
            .await
            .expect("metrics request");
        assert_eq!(resp.status(), reqwest::StatusCode::OK);
        assert_eq!(
            resp.headers()
                .get("content-type")
                .expect("content-type header present")
                .to_str()
                .expect("content-type is ASCII"),
            "text/plain; version=0.0.4"
        );
        let body = resp.text().await.expect("metrics body");
        assert!(body.contains("templar_liquidator_scans_total 1"));
    }
}
