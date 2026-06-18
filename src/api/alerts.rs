use axum::extract::State;
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::Utc;

use crate::alert::AlertManager;
use crate::auth::AuthUser;
use crate::error::{AppError, AppResult};
use crate::models::{AlertConfig, AlertConfigRequest, AlertTestRequest, ApiResponse, AppState};

/// Build the alerts sub-router.
pub fn alert_routes() -> Router<AppState> {
    Router::new()
        .route("/", get(get_alert_config).put(update_alert_config))
        .route("/test", post(test_notification))
}

/// GET /api/alerts
async fn get_alert_config(
    State(state): State<AppState>,
    user: AuthUser,
) -> Result<Json<ApiResponse<Option<AlertConfig>>>, AppError> {
    let configs = state.alert_configs.read().await;
    let config = configs.get(&user.address).cloned();
    Ok(Json(ApiResponse::ok(config)))
}

/// PUT /api/alerts
async fn update_alert_config(
    State(state): State<AppState>,
    user: AuthUser,
    Json(body): Json<AlertConfigRequest>,
) -> Result<Json<ApiResponse<AlertConfig>>, AppError> {
    if body.nickname.trim().is_empty() {
        return Err(AppError::Validation("nickname is required".into()));
    }
    if body.app_id.trim().is_empty() {
        return Err(AppError::Validation("app_id is required".into()));
    }
    if body.app_secret.trim().is_empty() {
        return Err(AppError::Validation("app_secret is required".into()));
    }
    if body.user_openid.trim().is_empty() {
        return Err(AppError::Validation("user_openid is required".into()));
    }

    let config = AlertConfig {
        user_address: user.address.clone(),
        nickname: body.nickname.trim().to_string(),
        app_id: body.app_id.trim().to_string(),
        app_secret: body.app_secret.trim().to_string(),
        user_openid: body.user_openid.trim().to_string(),
        updated_at: Utc::now().timestamp(),
    };

    {
        let mut configs = state.alert_configs.write().await;
        configs.insert(user.address.clone(), config.clone());
    }

    persist_alerts(&state).await?;

    Ok(Json(ApiResponse::ok(config)))
}

/// POST /api/alerts/test
async fn test_notification(
    State(state): State<AppState>,
    user: AuthUser,
    Json(body): Json<AlertTestRequest>,
) -> Result<Json<ApiResponse<String>>, AppError> {
    let text = body.text.trim().to_string();
    if text.is_empty() {
        return Err(AppError::Validation("text is required".into()));
    }

    let cfg = {
        let configs = state.alert_configs.read().await;
        configs.get(&user.address).cloned().ok_or_else(|| {
            AppError::Validation("请先配置飞书通知参数（昵称、App ID、App Secret、OpenID）".into())
        })?
    };

    if cfg.app_id.is_empty() || cfg.user_openid.is_empty() {
        return Err(AppError::Validation("飞书参数不完整，请检查配置".into()));
    }

    let alert_manager = AlertManager::new();
    let short = if cfg.user_address.len() >= 10 { &cfg.user_address[..10] } else { cfg.user_address.as_str() };
    let content = format!("🧪 测试消息\n👤 {} [{}]\n内容: {}", cfg.nickname, short, text);
    alert_manager.send_to_user(&cfg, &content).await?;

    Ok(Json(ApiResponse::ok("测试消息已发送".into())))
}

async fn persist_alerts(state: &AppState) -> AppResult<()> {
    let configs = state.alert_configs.read().await;
    let json = serde_json::to_string_pretty(&*configs)
        .map_err(|e| AppError::Storage(format!("serialize alerts: {}", e)))?;
    let path = format!("{}/alerts.json", state.data_dir);
    if let Some(parent) = std::path::Path::new(&path).parent() {
        std::fs::create_dir_all(parent).ok();
    }
    std::fs::write(&path, &json).map_err(|e| AppError::Storage(format!("write alerts: {}", e)))?;
    Ok(())
}

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
                chains: ChainsConfig {
                    ethereum: Some(ChainConfig { rpc_ws: None, rpc_http: Some("https://eth.example.com".into()), polling_interval_secs: 12 }),
                    base: None, optimism: None, arbitrum: None, unichain: None, hyperevm: None, monad: None, katana: None, polygon: None, stable: None, tempo: None, worldchain: None,
                },
                flashbots: None,
            }),
            jwt_secret: "test-jwt-secret".into(),
            data_dir: "data".into(),
        }
    }

    #[tokio::test]
    async fn test_get_alert_config_empty() {
        let state = make_test_state();
        let user = AuthUser { address: "0xuser1".into(), role: "user".into() };
        let result = get_alert_config(State(state), user).await.unwrap();
        assert!(result.0.data.unwrap().is_none());
    }

    #[tokio::test]
    async fn test_update_and_get_alert_config() {
        let state = make_test_state();
        let user = AuthUser { address: "0xuser1".into(), role: "user".into() };
        let req = AlertConfigRequest {
            nickname: "Alice".into(),
            app_id: "cli_test".into(),
            app_secret: "secret".into(),
            user_openid: "ou_test123".into(),
        };
        let result = update_alert_config(State(state.clone()), user.clone(), Json(req)).await.unwrap();
        assert_eq!(result.0.data.unwrap().user_openid, "ou_test123");
        let result = get_alert_config(State(state.clone()), user.clone()).await.unwrap();
        let config = result.0.data.unwrap().unwrap();
        assert_eq!(config.nickname, "Alice");
        assert_eq!(config.app_id, "cli_test");
    }

    #[tokio::test]
    async fn test_update_alert_config_missing_fields() {
        let state = make_test_state();
        let user = AuthUser { address: "0xuser1".into(), role: "user".into() };
        let req = AlertConfigRequest {
            nickname: "".into(), app_id: "c".into(), app_secret: "s".into(), user_openid: "o".into(),
        };
        assert!(update_alert_config(State(state), user, Json(req)).await.is_err());
    }
}
