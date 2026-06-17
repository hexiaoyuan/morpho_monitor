use axum::extract::{Path, State};
use axum::routing::get;
use axum::{Json, Router};
use chrono::Utc;
use uuid::Uuid;

use crate::auth::AuthUser;
use crate::error::{AppError, AppResult};
use crate::models::{
    ApiResponse, AppState, CreateOrderRequest, Order, OrderStatus,
};

/// Build the orders sub-router.
pub fn order_routes() -> Router<AppState> {
    Router::new()
        .route("/", get(list_orders).post(create_order))
        .route("/{id}", get(get_order).put(update_order).delete(cancel_order))
}

/// GET /api/orders — list orders for the authenticated user (or all for admin).
async fn list_orders(
    State(state): State<AppState>,
    user: AuthUser,
) -> Result<Json<ApiResponse<Vec<Order>>>, AppError> {
    let orders = state.orders.read().await;

    let result: Vec<Order> = if user.is_admin() {
        orders.values().cloned().collect()
    } else {
        orders
            .values()
            .filter(|o| o.user_address == user.address)
            .cloned()
            .collect()
    };

    Ok(Json(ApiResponse::ok(result)))
}

/// POST /api/orders — create a new conditional order.
async fn create_order(
    State(state): State<AppState>,
    user: AuthUser,
    Json(body): Json<CreateOrderRequest>,
) -> Result<(axum::http::StatusCode, Json<ApiResponse<Order>>), AppError> {
    // Validate inputs
    if body.market_id.is_empty() {
        return Err(AppError::Validation("market_id is required".into()));
    }
    if body.signature.is_empty() {
        return Err(AppError::Validation("signature is required".into()));
    }

    let now = Utc::now().timestamp();
    let order = Order {
        id: Uuid::new_v4().to_string(),
        user_address: user.address.clone(),
        chain: body.chain,
        market_id: body.market_id,
        trigger_type: body.trigger_type,
        trigger_threshold: body.trigger_threshold,
        action: body.action,
        authorization: body.authorization,
        signature: body.signature,
        status: OrderStatus::Active,
        created_at: now,
        updated_at: now,
        feishu_target: body.feishu_target,
    };

    {
        let mut orders = state.orders.write().await;
        orders.insert(order.id.clone(), order.clone());
    }

    // Persist to JSON
    persist_orders(&state).await?;

    Ok((
        axum::http::StatusCode::CREATED,
        Json(ApiResponse::ok(order)),
    ))
}

/// GET /api/orders/:id — get a single order.
async fn get_order(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<Order>>, AppError> {
    let orders = state.orders.read().await;
    let order = orders
        .get(&id)
        .ok_or_else(|| AppError::NotFound(format!("Order {} not found", id)))?;

    // Only the owner or admin can view
    if !user.is_admin() && order.user_address != user.address {
        return Err(AppError::Forbidden("Not your order".into()));
    }

    Ok(Json(ApiResponse::ok(order.clone())))
}

/// PUT /api/orders/:id — update an order (e.g. change threshold, status).
async fn update_order(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<String>,
    Json(body): Json<CreateOrderRequest>,
) -> Result<Json<ApiResponse<Order>>, AppError> {
    let mut orders = state.orders.write().await;
    let order = orders
        .get_mut(&id)
        .ok_or_else(|| AppError::NotFound(format!("Order {} not found", id)))?;

    if !user.is_admin() && order.user_address != user.address {
        return Err(AppError::Forbidden("Not your order".into()));
    }

    order.chain = body.chain;
    order.market_id = body.market_id;
    order.trigger_type = body.trigger_type;
    order.trigger_threshold = body.trigger_threshold;
    order.action = body.action;
    order.authorization = body.authorization;
    order.signature = body.signature;
    order.feishu_target = body.feishu_target;
    order.updated_at = Utc::now().timestamp();

    let updated = order.clone();
    drop(orders);
    persist_orders(&state).await?;

    Ok(Json(ApiResponse::ok(updated)))
}

/// DELETE /api/orders/:id — cancel an order.
async fn cancel_order(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<Order>>, AppError> {
    let mut orders = state.orders.write().await;
    let order = orders
        .get_mut(&id)
        .ok_or_else(|| AppError::NotFound(format!("Order {} not found", id)))?;

    if !user.is_admin() && order.user_address != user.address {
        return Err(AppError::Forbidden("Not your order".into()));
    }

    order.status = OrderStatus::Cancelled;
    order.updated_at = Utc::now().timestamp();
    let cancelled = order.clone();
    drop(orders);
    persist_orders(&state).await?;

    Ok(Json(ApiResponse::ok(cancelled)))
}

/// Persist orders to JSON file.
async fn persist_orders(state: &AppState) -> AppResult<()> {
    let orders = state.orders.read().await;
    let json = serde_json::to_string_pretty(&*orders).map_err(|e| {
        AppError::Storage(format!("Failed to serialize orders: {}", e))
    })?;
    let path = format!("{}/orders.json", state.data_dir);

    // Ensure data directory exists
    if let Some(parent) = std::path::Path::new(&path).parent() {
        std::fs::create_dir_all(parent).ok();
    }

    std::fs::write(&path, &json).map_err(|e| {
        AppError::Storage(format!("Failed to write orders: {}", e))
    })?;
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
    use crate::models::{ActionType, Authorization, TriggerType};

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
                    base: None,
                    optimism: None,
                    arbitrum: None,
                    unichain: None,
                    hyperevm: None,
                },
                flashbots: None,
            }),
            jwt_secret: "test-jwt-secret".into(),
            data_dir: "data".into(),
        }
    }

    fn make_test_order() -> CreateOrderRequest {
        CreateOrderRequest {
            chain: "ethereum".into(),
            market_id: "0xMarket1".into(),
            trigger_type: TriggerType::HealthFactorBelow,
            trigger_threshold: "1.05".into(),
            action: ActionType::ClosePosition,
            authorization: Authorization {
                authorizer: alloy::primitives::Address::ZERO,
                authorized: alloy::primitives::Address::ZERO,
                is_authorized: true,
                nonce: alloy::primitives::U256::ZERO,
                deadline: alloy::primitives::U256::ZERO,
            },
            signature: "0xdeadbeef".into(),
            feishu_target: Some("ou_test".into()),
        }
    }

    #[tokio::test]
    async fn test_create_and_list_orders() {
        let state = make_test_state();

        // Create via the function directly
        let body = make_test_order();
        let user = AuthUser { address: "0xuser1".into(), role: "user".into() };

        let result = create_order(
            State(state.clone()),
            user.clone(),
            Json(body),
        )
        .await;
        assert!(result.is_ok());

        // List orders for the user
        let list_result = list_orders(State(state.clone()), user).await.unwrap();
        let list = list_result.0;
        assert_eq!(list.data.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn test_create_order_missing_fields() {
        let state = make_test_state();
        let user = AuthUser { address: "0xuser1".into(), role: "user".into() };

        let body = CreateOrderRequest {
            chain: "ethereum".into(),
            market_id: "".into(), // empty!
            trigger_type: TriggerType::HealthFactorBelow,
            trigger_threshold: "1.05".into(),
            action: ActionType::ClosePosition,
            authorization: Authorization {
                authorizer: alloy::primitives::Address::ZERO,
                authorized: alloy::primitives::Address::ZERO,
                is_authorized: true,
                nonce: alloy::primitives::U256::ZERO,
                deadline: alloy::primitives::U256::ZERO,
            },
            signature: "0xabc".into(),
            feishu_target: None,
        };

        let result = create_order(State(state), user, Json(body)).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("market_id"));
    }

    #[tokio::test]
    async fn test_get_order_not_found() {
        let state = make_test_state();
        let user = AuthUser { address: "0xuser1".into(), role: "user".into() };

        let result = get_order(State(state), user, Path("nonexistent".into())).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_cancel_order() {
        let state = make_test_state();
        let user = AuthUser { address: "0xuser1".into(), role: "user".into() };

        // Create
        let body = make_test_order();
        let created = create_order(State(state.clone()), user.clone(), Json(body))
            .await
            .unwrap();
        let order_id = created.1.data.clone().unwrap().id;

        // Cancel
        let cancelled = cancel_order(State(state.clone()), user.clone(), Path(order_id.clone()))
            .await
            .unwrap();
        assert_eq!(cancelled.0.data.clone().unwrap().status, OrderStatus::Cancelled);

        // Verify in list
        let list = list_orders(State(state.clone()), user.clone()).await.unwrap();
        let orders = list.0.data.clone().unwrap();
        let order = orders.iter().find(|o| o.id == order_id).unwrap();
        assert_eq!(order.status, OrderStatus::Cancelled);
    }

    #[tokio::test]
    async fn test_forbidden_access() {
        let state = make_test_state();
        let owner = AuthUser { address: "0xowner".into(), role: "user".into() };
        let other = AuthUser { address: "0xother".into(), role: "user".into() };

        let body = make_test_order();
        let created = create_order(State(state.clone()), owner, Json(body))
            .await
            .unwrap();
        let order_id = created.1.data.clone().unwrap().id;

        // Other user cannot view
        let result = get_order(State(state.clone()), other.clone(), Path(order_id.clone())).await;
        assert!(result.is_err());

        // Other user cannot cancel
        let result = cancel_order(State(state.clone()), other.clone(), Path(order_id.clone())).await;
        assert!(result.is_err());
    }
}
