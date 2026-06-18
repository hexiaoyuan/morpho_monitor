use axum::extract::{Path, State};
use axum::routing::get;
use axum::{Json, Router};
use chrono::Utc;
use uuid::Uuid;

use crate::auth::AuthUser;
use crate::error::{AppError, AppResult};
use crate::models::{
    ApiResponse, AppState, CreateOrderRequest, LiquidationConfig, Order, OrderStatus,
};

/// Build the orders sub-router.
pub fn order_routes() -> Router<AppState> {
    Router::new()
        .route("/", get(list_orders).post(create_order))
        .route("/{id}", get(get_order).put(update_order).delete(delete_order))
}

/// Validate a CreateOrderRequest before building an Order.
fn validate_request(body: &CreateOrderRequest) -> AppResult<()> {
    if body.chain.is_empty() {
        return Err(AppError::Validation("chain is required".into()));
    }
    if body.market_id.is_empty() {
        return Err(AppError::Validation("market_id is required".into()));
    }
    if body.order_type != "market" && body.order_type != "vault" {
        return Err(AppError::Validation(
            "order_type must be 'market' or 'vault'".into(),
        ));
    }

    let alert_active = body.alert_conditions.has_active_conditions();
    let liquidation_active = body
        .liquidation_conditions
        .as_ref()
        .map_or(false, |c| c.has_active_conditions());

    if !alert_active && !liquidation_active {
        return Err(AppError::Validation(
            "At least one condition (alert_conditions or liquidation_conditions) must be enabled with thresholds".into(),
        ));
    }

    if liquidation_active {
        let auth = body
            .authorization
            .as_ref()
            .ok_or_else(|| {
                AppError::Validation(
                    "authorization is required when liquidation conditions are enabled".into(),
                )
            })?;
        // Minimal check: signature must be non-empty
        let sig = body.signature.as_deref().unwrap_or("");
        if sig.is_empty() {
            return Err(AppError::Validation(
                "signature is required when liquidation conditions are enabled".into(),
            ));
        }
        // Prevent accidental self-authorization: authorizer must differ from authorized
        if auth.authorizer == auth.authorized {
            return Err(AppError::Validation(
                "authorizer must differ from authorized (hot wallet must be authorized)".into(),
            ));
        }
    }

    Ok(())
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
    validate_request(&body)?;

    let now = Utc::now().timestamp();

    let liquidation = if let Some(conditions) = body.liquidation_conditions {
        if conditions.has_active_conditions() {
            let auth = body.authorization.expect("validated above");
            let sig = body.signature.expect("validated above");
            Some(LiquidationConfig {
                conditions,
                authorization: auth,
                signature: sig,
            })
        } else {
            None
        }
    } else {
        None
    };

    let order = Order {
        id: Uuid::new_v4().to_string(),
        user_address: user.address.clone(),
        name: body.name,
        chain: body.chain,
        order_type: body.order_type,
        market_id: body.market_id,
        alert_conditions: body.alert_conditions,
        liquidation,
        status: OrderStatus::Monitoring,
        created_at: now,
        updated_at: now,
    };

    {
        let mut orders = state.orders.write().await;
        orders.insert(order.id.clone(), order.clone());
    }

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

    if !user.is_admin() && order.user_address != user.address {
        return Err(AppError::Forbidden("Not your order".into()));
    }

    Ok(Json(ApiResponse::ok(order.clone())))
}

/// PUT /api/orders/:id — update an order.
async fn update_order(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<String>,
    Json(body): Json<CreateOrderRequest>,
) -> Result<Json<ApiResponse<Order>>, AppError> {
    validate_request(&body)?;

    let mut orders = state.orders.write().await;
    let order = orders
        .get_mut(&id)
        .ok_or_else(|| AppError::NotFound(format!("Order {} not found", id)))?;

    if !user.is_admin() && order.user_address != user.address {
        return Err(AppError::Forbidden("Not your order".into()));
    }

    let now = Utc::now().timestamp();

    order.chain = body.chain;
    order.name = body.name;
    order.order_type = body.order_type;
    order.market_id = body.market_id;
    order.alert_conditions = body.alert_conditions;

    if let Some(conditions) = body.liquidation_conditions {
        if conditions.has_active_conditions() {
            let auth = body.authorization.expect("validated above");
            let sig = body.signature.expect("validated above");
            order.liquidation = Some(LiquidationConfig {
                conditions,
                authorization: auth,
                signature: sig,
            });
        } else {
            order.liquidation = None;
        }
    } else {
        order.liquidation = None;
    }

    order.updated_at = now;
    let updated = order.clone();
    drop(orders);
    persist_orders(&state).await?;

    Ok(Json(ApiResponse::ok(updated)))
}

/// DELETE /api/orders/:id — delete an order from any status.
async fn delete_order(
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

    order.status = OrderStatus::Ended;
    order.updated_at = Utc::now().timestamp();
    let ended = order.clone();
    drop(orders);
    persist_orders(&state).await?;

    Ok(Json(ApiResponse::ok(ended)))
}

/// Persist orders to JSON file.
pub async fn persist_orders(state: &AppState) -> AppResult<()> {
    let orders = state.orders.read().await;
    let json = serde_json::to_string_pretty(&*orders).map_err(|e| {
        AppError::Storage(format!("Failed to serialize orders: {}", e))
    })?;
    let path = format!("{}/orders.json", state.data_dir);

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

    use crate::config::{
        AdminConfig, AppConfig, ChainConfig, ChainsConfig, HotWalletConfig, ServerConfig,
    };
    use crate::models::{ConditionGroup, MetricCondition};

    fn make_test_state() -> AppState {
        std::fs::create_dir_all("data").ok();
        AppState {
            orders: Arc::new(RwLock::new(HashMap::new())),
            whitelist: Arc::new(RwLock::new(HashMap::new())),
            alert_configs: Arc::new(RwLock::new(HashMap::new())),
            monitor_states: Arc::new(RwLock::new(HashMap::new())),
            nonce_store: Arc::new(RwLock::new(HashMap::new())),
            config: Arc::new(AppConfig {
                server: ServerConfig {
                    host: "127.0.0.1".into(),
                    port: 3000,
                    data_dir: "data".into(),
                },
                admin: AdminConfig {
                    address: "0xAdmin00000000000000000000000000000000000000".into(),
                },
                hot_wallet: HotWalletConfig {
                    private_key: "0xdead".into(),
                    gas_min_balance: "0.1".into(),
                },
                gql_url: "https://api.morpho.org/graphql".into(),
                chains: ChainsConfig {
                    ethereum: Some(ChainConfig {
                        rpc_ws: None,
                        rpc_http: Some("https://eth.example.com".into()),
                        polling_interval_secs: 12,
                    }),
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

    fn make_test_request() -> CreateOrderRequest {
        CreateOrderRequest {
            chain: "ethereum".into(),
            name: "Test".into(),
            order_type: "market".into(),
            market_id: "0xMarket1".into(),
            alert_conditions: ConditionGroup {
                liquidity: MetricCondition {
                    enabled: true,
                    upper: None,
                    lower: Some("1.05".into()),
                },
                ..Default::default()
            },
            liquidation_conditions: None,
            authorization: None,
            signature: None,
        }
    }

    #[tokio::test]
    async fn test_create_and_list_orders() {
        let state = make_test_state();
        let user = AuthUser {
            address: "0xuser1".into(),
            role: "user".into(),
        };

        let result = create_order(State(state.clone()), user.clone(), Json(make_test_request()))
            .await
            .unwrap();
        assert_eq!(result.0, axum::http::StatusCode::CREATED);

        let list = list_orders(State(state.clone()), user)
            .await
            .unwrap();
        assert_eq!(list.0.data.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn test_create_order_missing_market_id() {
        let state = make_test_state();
        let user = AuthUser {
            address: "0xuser1".into(),
            role: "user".into(),
        };

        let mut req = make_test_request();
        req.market_id = String::new();
        let result = create_order(State(state), user, Json(req)).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("market_id"));
    }

    #[tokio::test]
    async fn test_create_order_no_conditions() {
        let state = make_test_state();
        let user = AuthUser {
            address: "0xuser1".into(),
            role: "user".into(),
        };

        let req = CreateOrderRequest {
            chain: "ethereum".into(),
            name: "Test".into(),
            order_type: "market".into(),
            market_id: "0xMarket1".into(),
            alert_conditions: ConditionGroup::default(),
            liquidation_conditions: None,
            authorization: None,
            signature: None,
        };
        let result = create_order(State(state), user, Json(req)).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_create_order_liquidation_no_auth() {
        let state = make_test_state();
        let user = AuthUser {
            address: "0xuser1".into(),
            role: "user".into(),
        };

        let req = CreateOrderRequest {
            chain: "ethereum".into(),
            name: "Test".into(),
            order_type: "market".into(),
            market_id: "0xMarket1".into(),
            alert_conditions: ConditionGroup::default(),
            liquidation_conditions: Some(ConditionGroup {
                liquidity: MetricCondition {
                    enabled: true,
                    upper: None,
                    lower: Some("1.0".into()),
                },
                ..Default::default()
            }),
            authorization: None,
            signature: None,
        };
        let result = create_order(State(state), user, Json(req)).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("authorization"));
    }

    #[tokio::test]
    async fn test_get_order_not_found() {
        let state = make_test_state();
        let user = AuthUser {
            address: "0xuser1".into(),
            role: "user".into(),
        };

        let result = get_order(State(state), user, Path("nonexistent".into())).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_delete_order() {
        let state = make_test_state();
        let user = AuthUser {
            address: "0xuser1".into(),
            role: "user".into(),
        };

        let created = create_order(State(state.clone()), user.clone(), Json(make_test_request()))
            .await
            .unwrap();
        let order_id = created.1.data.clone().unwrap().id;

        let deleted = delete_order(State(state.clone()), user.clone(), Path(order_id.clone()))
            .await
            .unwrap();
        assert_eq!(deleted.0.data.unwrap().status, OrderStatus::Ended);

        let list = list_orders(State(state.clone()), user).await.unwrap();
        let orders = list.0.data.unwrap();
        let order = orders.iter().find(|o| o.id == order_id).unwrap();
        assert_eq!(order.status, OrderStatus::Ended);
    }

    #[tokio::test]
    async fn test_forbidden_access() {
        let state = make_test_state();
        let owner = AuthUser {
            address: "0xowner".into(),
            role: "user".into(),
        };
        let other = AuthUser {
            address: "0xother".into(),
            role: "user".into(),
        };

        let created = create_order(State(state.clone()), owner, Json(make_test_request()))
            .await
            .unwrap();
        let order_id = created.1.data.clone().unwrap().id;

        let result = get_order(State(state.clone()), other.clone(), Path(order_id.clone())).await;
        assert!(result.is_err());

        let result = delete_order(State(state.clone()), other, Path(order_id)).await;
        assert!(result.is_err());
    }
}
