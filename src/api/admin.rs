use axum::extract::{Path, State};
use axum::routing::{delete, get};
use axum::{Json, Router};
use chrono::Utc;

use crate::auth::{require_admin, AuthUser};
use crate::error::{AppError, AppResult};
use crate::models::{ApiResponse, AppState, WhitelistEntry, WhitelistRequest};

/// Build the admin sub-router.
pub fn admin_routes() -> Router<AppState> {
    Router::new()
        .route("/whitelist", get(list_whitelist).post(add_to_whitelist))
        .route("/whitelist/{address}", delete(remove_from_whitelist))
}

/// GET /api/admin/whitelist
async fn list_whitelist(
    State(state): State<AppState>,
    admin: AuthUser,
) -> Result<Json<ApiResponse<Vec<WhitelistEntry>>>, AppError> {
    require_admin(&admin)?;
    let whitelist = state.whitelist.read().await;
    let entries: Vec<WhitelistEntry> = whitelist.values().cloned().collect();
    Ok(Json(ApiResponse::ok(entries)))
}

/// POST /api/admin/whitelist
async fn add_to_whitelist(
    State(state): State<AppState>,
    admin: AuthUser,
    Json(body): Json<WhitelistRequest>,
) -> Result<(axum::http::StatusCode, Json<ApiResponse<WhitelistEntry>>), AppError> {
    require_admin(&admin)?;

    // Validate address format
    let addr = body.address.to_lowercase();
    if !addr.starts_with("0x") || addr.len() != 42 {
        return Err(AppError::Validation("Invalid Ethereum address format".into()));
    }
    if body.nickname.trim().is_empty() {
        return Err(AppError::Validation("Nickname is required".into()));
    }

    let entry = WhitelistEntry {
        address: addr.clone(),
        nickname: body.nickname.trim().to_string(),
        added_at: Utc::now().timestamp(),
    };

    {
        let mut whitelist = state.whitelist.write().await;
        whitelist.insert(addr, entry.clone());
    }

    persist_whitelist(&state).await?;

    Ok((
        axum::http::StatusCode::CREATED,
        Json(ApiResponse::ok(entry)),
    ))
}

/// DELETE /api/admin/whitelist/:address
async fn remove_from_whitelist(
    State(state): State<AppState>,
    admin: AuthUser,
    Path(address): Path<String>,
) -> Result<Json<ApiResponse<()>>, AppError> {
    require_admin(&admin)?;

    let addr = address.to_lowercase();
    let removed = {
        let mut whitelist = state.whitelist.write().await;
        whitelist.remove(&addr)
    };

    if removed.is_none() {
        return Err(AppError::NotFound(format!(
            "Address {} not found in whitelist",
            addr
        )));
    }

    persist_whitelist(&state).await?;

    Ok(Json(ApiResponse {
        success: true,
        data: Some(()),
        error: None,
    }))
}

/// Persist whitelist to JSON file.
async fn persist_whitelist(state: &AppState) -> AppResult<()> {
    let whitelist = state.whitelist.read().await;
    let json = serde_json::to_string_pretty(&*whitelist).map_err(|e| {
        AppError::Storage(format!("Failed to serialize whitelist: {}", e))
    })?;
    let path = format!("{}/whitelist.json", state.data_dir);

    if let Some(parent) = std::path::Path::new(&path).parent() {
        std::fs::create_dir_all(parent).ok();
    }

    std::fs::write(&path, &json)
        .map_err(|e| AppError::Storage(format!("Failed to write whitelist: {}", e)))?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Arc;
    use tokio::sync::RwLock;

    use crate::config::{AdminConfig, AppConfig, ChainConfig, ChainsConfig, HotWalletConfig, ServerConfig};

    fn make_test_state() -> AppState {
        std::fs::create_dir_all("data").ok();
        AppState {
            orders: Arc::new(RwLock::new(HashMap::new())),
            whitelist: Arc::new(RwLock::new(HashMap::new())),
            alert_configs: Arc::new(RwLock::new(HashMap::new())),
            monitor_states: Arc::new(RwLock::new(HashMap::new())),
            nonce_store: Arc::new(RwLock::new(HashMap::new())),
            config: Arc::new(AppConfig {
                server: ServerConfig { host: "127.0.0.1".into(), port: 3000, data_dir: "data".into() },
                admin: AdminConfig { address: "0xAdmin00000000000000000000000000000000000000".into() },
                hot_wallet: HotWalletConfig { private_key: "0xdead".into(), gas_min_balance: "0.1".into() },
                gql_url: "https://api.morpho.org/graphql".into(),
                gql_polling_interval_secs: 12,
                gql_batch_size: 100,
                chains: ChainsConfig {
                    ethereum: Some(ChainConfig { rpc_ws: None, rpc_http: Some("https://eth.example.com".into()), polling_interval_secs: 12 }),
                    base: None,
                    optimism: None,
                    arbitrum: None,
                    unichain: None,
                    hyperevm: None, monad: None, katana: None, polygon: None, stable: None, tempo: None, worldchain: None,
                },
                flashbots: None,
            }),
            jwt_secret: "test-jwt-secret".into(),
            data_dir: "data".into(),
            market_cache: Arc::new(RwLock::new(std::collections::HashMap::new())),
        }
    }

    #[tokio::test]
    async fn test_add_and_list_whitelist() {
        let state = make_test_state();
        let admin = AuthUser { address: "0xAdmin00000000000000000000000000000000000000".into(), role: "admin".into() };

        // Add
        let req = WhitelistRequest {
            address: "0x1234567890123456789012345678901234567890".into(),
            nickname: "Alice".into(),
        };
        let result = add_to_whitelist(State(state.clone()), admin.clone(), Json(req))
            .await
            .unwrap();
        assert_eq!(result.0, axum::http::StatusCode::CREATED);

        // List
        let list = list_whitelist(State(state.clone()), admin.clone()).await.unwrap();
        let entries = list.0.data.unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].nickname, "Alice");
    }

    #[tokio::test]
    async fn test_remove_from_whitelist() {
        let state = make_test_state();
        let admin = AuthUser { address: "0xAdmin00000000000000000000000000000000000000".into(), role: "admin".into() };

        // Add
        let req = WhitelistRequest {
            address: "0x1234567890123456789012345678901234567890".into(),
            nickname: "Bob".into(),
        };
        let _ = add_to_whitelist(State(state.clone()), admin.clone(), Json(req))
            .await
            .unwrap();

        // Remove
        let result = remove_from_whitelist(
            State(state.clone()),
            admin.clone(),
            Path("0x1234567890123456789012345678901234567890".into()),
        )
        .await;
        assert!(result.is_ok());

        // Verify removed
        let list = list_whitelist(State(state), admin).await.unwrap();
        assert!(list.0.data.unwrap().is_empty());
    }

    #[tokio::test]
    async fn test_add_whitelist_invalid_address() {
        let state = make_test_state();
        let admin = AuthUser { address: "0xAdmin00000000000000000000000000000000000000".into(), role: "admin".into() };

        let req = WhitelistRequest {
            address: "not_an_address".into(),
            nickname: "Test".into(),
        };
        let result = add_to_whitelist(State(state), admin, Json(req)).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_non_admin_cannot_add() {
        let state = make_test_state();
        let user = AuthUser { address: "0xuser".into(), role: "user".into() };

        let req = WhitelistRequest {
            address: "0x1234567890123456789012345678901234567890".into(),
            nickname: "Test".into(),
        };
        let result = add_to_whitelist(State(state), user, Json(req)).await;
        assert!(result.is_err());
    }
}
