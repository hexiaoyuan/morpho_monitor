use axum::extract::State;
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::Utc;
use uuid::Uuid;

use crate::auth::{create_jwt, verify_siwe};
use crate::error::AppError;
use crate::models::{ApiResponse, AppState, LoginRequest, LoginResponse};

/// Build the auth sub-router.
pub fn auth_routes() -> Router<AppState> {
    Router::new()
        .route("/nonce", get(get_nonce))
        .route("/login", post(login))
}

/// GET /api/auth/nonce?address=0x…
async fn get_nonce(
    State(state): State<AppState>,
    axum::extract::Query(params): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Result<Json<ApiResponse<String>>, AppError> {
    let address = params
        .get("address")
        .ok_or_else(|| AppError::Validation("Missing 'address' query parameter".into()))?;

    // Basic address format validation
    if !address.starts_with("0x") || address.len() != 42 {
        return Err(AppError::Validation("Invalid Ethereum address format".into()));
    }

    let nonce = Uuid::new_v4().to_string();
    let expires_at = Utc::now().timestamp() + 300; // 5 minutes

    {
        let mut store = state.nonce_store.write().await;
        store.insert(address.to_lowercase(), (nonce.clone(), expires_at));
    }

    Ok(Json(ApiResponse::ok(nonce)))
}

/// POST /api/auth/login
async fn login(
    State(state): State<AppState>,
    Json(body): Json<LoginRequest>,
) -> Result<Json<ApiResponse<LoginResponse>>, AppError> {
    // Parse SIWE message to extract address before verification
    let siwe_msg: siwe::Message = body.message.parse().map_err(|e| {
        AppError::AuthError(format!("Invalid SIWE message format: {}", e))
    })?;

    let user_address = format!("0x{}", hex::encode(siwe_msg.address)).to_lowercase();

    // Verify the nonce exists for this address
    let expected_nonce = {
        let mut store = state.nonce_store.write().await;
        let key = user_address.clone();
        if let Some((nonce, expires_at)) = store.get(&key) {
            if Utc::now().timestamp() <= *expires_at {
                let n = nonce.clone();
                store.remove(&key);
                Some(n)
            } else {
                store.remove(&key);
                None
            }
        } else {
            None
        }
    }
    .ok_or_else(|| {
        AppError::AuthError("Nonce not found or expired. Please request a new nonce.".into())
    })?;

    // Verify SIWE signature
    verify_siwe(&body.message, &body.signature, &expected_nonce).await?;

    // Determine role
    let admin_address = state.config.admin.address.to_lowercase();
    let role = if user_address == admin_address {
        "admin"
    } else {
        // Check whitelist
        let whitelist = state.whitelist.read().await;
        if !whitelist.contains_key(&user_address) {
            return Err(AppError::Forbidden(
                "Address not in whitelist. Contact admin to get access.".into(),
            ));
        }
        "user"
    };

    // Issue JWT
    let token = create_jwt(&user_address, role, &state.jwt_secret, 168)?; // 7 days

    Ok(Json(ApiResponse::ok(LoginResponse {
        token,
        address: user_address,
        role: role.to_string(),
    })))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use std::collections::HashMap;
    use std::sync::Arc;
    use tokio::sync::RwLock;
    use tower::ServiceExt;

    use crate::config::{AdminConfig, AppConfig, ChainConfig, ChainsConfig, HotWalletConfig, ServerConfig};

    fn make_test_state() -> AppState {
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
        }
    }

    fn make_router() -> Router {
        auth_routes().with_state(make_test_state())
    }

    #[tokio::test]
    async fn test_get_nonce() {
        let app = make_router();

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/nonce?address=0x1234567890123456789012345678901234567890")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["success"], true);
        assert!(json["data"].is_string());
    }

    #[tokio::test]
    async fn test_get_nonce_missing_address() {
        let app = make_router();

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/nonce")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn test_get_nonce_invalid_address() {
        let app = make_router();

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/nonce?address=not_an_address")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn test_login_invalid_siwe_message() {
        let app = make_router();

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/login")
                    .header("Content-Type", "application/json")
                    .body(Body::from(
                        serde_json::to_string(&serde_json::json!({
                            "message": "not a valid siwe message",
                            "signature": "0xabcd"
                        }))
                        .unwrap(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }
}
