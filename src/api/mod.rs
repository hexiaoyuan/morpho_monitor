pub mod auth;
pub mod alerts;
pub mod admin;
pub mod orders;

use axum::{Router, routing::get};

use crate::models::AppState;

/// Build the complete API router tree.
pub fn build_router(state: AppState) -> Router {
    Router::new()
        // Public auth routes
        .nest("/api/auth", auth::auth_routes())
        // Protected order routes
        .nest("/api/orders", orders::order_routes())
        // Protected alert config routes
        .nest("/api/alerts", alerts::alert_routes())
        // Admin-only routes
        .nest("/api/admin", admin::admin_routes())
        // Health check
        .route("/api/health", get(health_check))
        .with_state(state)
}

/// Simple health check endpoint.
async fn health_check() -> axum::Json<serde_json::Value> {
    axum::Json(serde_json::json!({
        "status": "ok",
        "service": "morpho_monitor",
    }))
}
