pub mod auth;
pub mod alerts;
pub mod admin;
pub mod orders;

use std::collections::HashMap;

use axum::extract::State;
use axum::routing::get;
use axum::{Json, Router};

use crate::auth::AuthUser;
use crate::error::AppError;
use crate::models::{ApiResponse, AppState, CachedData};

/// Build the complete API router tree.
pub fn build_router(state: AppState) -> Router {
    Router::new()
        .nest("/api/auth", auth::auth_routes())
        .nest("/api/orders", orders::order_routes())
        .nest("/api/alerts", alerts::alert_routes())
        .nest("/api/admin", admin::admin_routes())
        .route("/api/cache", get(get_cache))
        .route("/api/health", get(health_check))
        .with_state(state)
}

/// GET /api/cache?ids=id1,id2,... — returns cached market/vault GQL data
async fn get_cache(
    State(state): State<AppState>,
    _user: AuthUser,
    axum::extract::Query(params): axum::extract::Query<HashMap<String, String>>,
) -> Result<Json<ApiResponse<HashMap<String, CachedData>>>, AppError> {
    let ids: Vec<&str> = params
        .get("ids")
        .map(|s| s.split(',').collect())
        .unwrap_or_default();
    let cache = state.gql_cache.read().await;
    let result: HashMap<String, CachedData> = ids
        .iter()
        .filter_map(|id| cache.get(*id).map(|v| (id.to_string(), v.clone())))
        .collect();
    Ok(Json(ApiResponse::ok(result)))
}

/// Simple health check endpoint.
async fn health_check() -> axum::Json<serde_json::Value> {
    axum::Json(serde_json::json!({
        "status": "ok",
        "service": "morpho_monitor",
    }))
}
