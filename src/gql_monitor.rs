use chrono::Utc;
use serde::Deserialize;
use std::str::FromStr;
use std::time::Duration;
use tracing::{info, warn};

use alloy::primitives::U256;

use crate::alert::{AlertDecision, AlertManager};
use crate::error::AppResult;
use crate::models::{AppState, MonitorState, Order, OrderStatus, TriggerType};

// ---------------------------------------------------------------------------
// Morpho GraphQL response types
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct GqlResponse<T> {
    data: Option<T>,
    #[allow(dead_code)]
    errors: Option<Vec<serde_json::Value>>,
}

#[derive(Debug, Deserialize)]
struct MarketData {
    market: Option<MarketInfo>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct MarketInfo {
    #[allow(unused)]
    id: String,
    state: Option<MarketState>,
    #[allow(unused)]
    market_params: Option<MarketParams>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct MarketState {
    total_supply_assets: Option<String>,
    total_supply_shares: Option<String>,
    total_borrow_assets: Option<String>,
    total_borrow_shares: Option<String>,
    #[allow(unused)]
    supply_apy: Option<f64>,
    #[allow(unused)]
    borrow_apy: Option<f64>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct MarketParams {
    #[allow(unused)]
    lltv: Option<String>,
}

#[derive(Debug, Deserialize)]
struct PositionData {
    position: Option<PositionInfo>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PositionInfo {
    #[allow(unused)]
    supply_shares: Option<String>,
    borrow_shares: Option<String>,
    collateral: Option<String>,
}

// ---------------------------------------------------------------------------
// GQL Monitor
// ---------------------------------------------------------------------------

/// Polls Morpho's GraphQL API for market + position data.
/// Lower real-time fidelity than direct RPC, but requires zero node config.
pub struct GqlMonitor {
    pub gql_url: String,
    pub polling_interval_secs: u64,
    http: reqwest::Client,
}

impl GqlMonitor {
    pub fn new(gql_url: &str, polling_interval_secs: u64) -> Self {
        Self {
            gql_url: gql_url.to_string(),
            polling_interval_secs,
            http: reqwest::Client::new(),
        }
    }

    /// Run the polling loop indefinitely.
    pub async fn run(&self, state: AppState, alert_manager: AlertManager) {
        info!(
            "GQL monitor started: {} (polling every {}s)",
            self.gql_url, self.polling_interval_secs
        );

        let mut interval = tokio::time::interval(Duration::from_secs(self.polling_interval_secs));

        loop {
            interval.tick().await;
            if let Err(e) = self.poll(&state, &alert_manager).await {
                warn!("GQL poll error: {}", e);
            }
        }
    }

    async fn poll(&self, state: &AppState, alert_manager: &AlertManager) -> AppResult<()> {
        let now = Utc::now().timestamp();

        // Group active orders by (chain, market_id, user_address)
        let active_orders: Vec<Order> = {
            let orders = state.orders.read().await;
            orders
                .values()
                .filter(|o| o.status == OrderStatus::Active)
                .cloned()
                .collect()
        };

        if active_orders.is_empty() {
            return Ok(());
        }

        for order in &active_orders {
            // Query market state from GQL — skip this order on error, don't abort the loop
            let market_info = match self.query_market(&order.market_id).await {
                Ok(m) => m,
                Err(e) => {
                    warn!("GQL market query failed for order {} (market={}): {}",
                        order.id, order.market_id, e);
                    // If market is genuinely not found, mark the order invalid
                    if e.to_string().contains("Market not found") {
                        let mut orders = state.orders.write().await;
                        if let Some(o) = orders.get_mut(&order.id) {
                            o.status = OrderStatus::Invalid;
                            o.updated_at = Utc::now().timestamp();
                            warn!("Marked order {} as Invalid: market not found", order.id);
                        }
                        alert_manager.notify_user(state, &order.user_address, &format!(
                            "⚠️ 订单自动作废\n订单 {} 的市场 {} 在 GraphQL 中未找到，可能已过期。",
                            order.id, order.market_id
                        )).await;
                    }
                    continue;
                }
            };
            // Query position from GQL
            let position = match self.query_position(&order.market_id, &order.user_address).await {
                Ok(p) => p,
                Err(e) => {
                    warn!("GQL position query failed for order {} (user={}): {}",
                        order.id, order.user_address, e);
                    continue;
                }
            };

            // Compute health factor
            let health_factor = compute_health_factor(&market_info, &position);

            // Update local monitor state
            let state_key =
                AlertManager::state_key(&order.chain, &order.market_id, &order.user_address);
            {
                let mut states = state.monitor_states.write().await;
                states.insert(
                    state_key.clone(),
                    MonitorState {
                        chain: order.chain.clone(),
                        market_id: order.market_id.clone(),
                        user_address: order.user_address.clone(),
                        collateral_amount: position
                            .collateral
                            .as_deref()
                            .and_then(|s| U256::from_str(s).ok())
                            .unwrap_or_default(),
                        borrow_amount: position
                            .borrow_shares
                            .as_deref()
                            .and_then(|s| U256::from_str(s).ok())
                            .unwrap_or_default(),
                        health_factor,
                        last_updated: now,
                    },
                );
            }

            // Evaluate risk — use U256 directly, same as RPC monitor
            let threshold =
                U256::from_str(&order.trigger_threshold).unwrap_or(U256::ZERO);

            let is_risky = match order.trigger_type {
                TriggerType::HealthFactorBelow => {
                    health_factor > U256::ZERO && health_factor < threshold
                }
                TriggerType::LltvAbove => health_factor > threshold,
            };

            let decision = alert_manager
                .evaluate_risk(&order.chain, &order.market_id, &order.user_address, is_risky)
                .await;

            self.handle_decision(decision, order, state, alert_manager, &order.chain)
                .await;
        }

        Ok(())
    }

    async fn handle_decision(
        &self,
        decision: AlertDecision,
        order: &Order,
        state: &AppState,
        alert_manager: &AlertManager,
        chain: &str,
    ) {
        match decision {
            AlertDecision::TriggerAlert => {
                info!("GQL ALERT for order {} (chain={}, market={}, user={})", order.id, chain, order.market_id, order.user_address);
                alert_manager.notify_user(state, &order.user_address, &format!(
                    "🚨 风险预警 (GQL)\n链: {}\n市场: {}\n阈值: {}",
                    chain, order.market_id, order.trigger_threshold
                )).await;
                // TODO: Trigger bot executor for this order
            }
            AlertDecision::Recovered => {
                info!("GQL recovery for order {}", order.id);
                alert_manager.notify_user(state, &order.user_address, &format!(
                    "✅ 风险已解除\n链: {}\n市场: {}",
                    chain, order.market_id
                )).await;
            }
            AlertDecision::Suppress => {}
        }
    }

    /// Query market state from Morpho GraphQL.
    async fn query_market(&self, market_id: &str) -> AppResult<MarketInfo> {
        let query = format!(
            r#"{{"query":"{{ market(id: \\"{}\\") {{ id state {{ totalSupplyAssets totalSupplyShares totalBorrowAssets totalBorrowShares supplyApy }} marketParams {{ lltv }} }} }}"}}"#,
            market_id
        );

        let resp: GqlResponse<MarketData> = self
            .http
            .post(&self.gql_url)
            .header("Content-Type", "application/json")
            .body(query)
            .send()
            .await
            .map_err(|e| {
                crate::error::AppError::RpcError(format!("GQL market query failed: {}", e))
            })?
            .json()
            .await
            .map_err(|e| {
                crate::error::AppError::RpcError(format!("GQL market parse failed: {}", e))
            })?;

        resp.data
            .and_then(|d| d.market)
            .ok_or_else(|| crate::error::AppError::RpcError("Market not found".into()))
    }

    /// Query user position from Morpho GraphQL.
    async fn query_position(&self, market_id: &str, user: &str) -> AppResult<PositionInfo> {
        let query = format!(
            r#"{{"query":"{{ position(marketId: \\"{}\\", user: \\"{}\\") {{ supplyShares borrowShares collateral }} }}"}}"#,
            market_id, user
        );

        let resp: GqlResponse<PositionData> = self
            .http
            .post(&self.gql_url)
            .header("Content-Type", "application/json")
            .body(query)
            .send()
            .await
            .map_err(|e| {
                crate::error::AppError::RpcError(format!("GQL position query failed: {}", e))
            })?
            .json()
            .await
            .map_err(|e| {
                crate::error::AppError::RpcError(format!("GQL position parse failed: {}", e))
            })?;

        Ok(resp
            .data
            .and_then(|d| d.position)
            .unwrap_or(PositionInfo {
                supply_shares: None,
                borrow_shares: None,
                collateral: None,
            }))
    }
}

// ---------------------------------------------------------------------------
// Health factor computation
// ---------------------------------------------------------------------------

/// Compute a rough health factor from market + position data.
/// health = (collateral * price * lltv) / (borrowShares * borrowPrice)
/// When on-chain oracle data isn't available, we approximate using supply/borrow
/// ratios from the market state as a price proxy.
fn compute_health_factor(market: &MarketInfo, position: &PositionInfo) -> U256 {
    let supply_assets = market
        .state
        .as_ref()
        .and_then(|s| s.total_supply_assets.as_deref())
        .and_then(|s| U256::from_str(s).ok())
        .unwrap_or(U256::ZERO);

    let supply_shares_total = market
        .state
        .as_ref()
        .and_then(|s| s.total_supply_shares.as_deref())
        .and_then(|s| U256::from_str(s).ok())
        .unwrap_or(U256::from(1)); // avoid div by zero

    let borrow_assets = market
        .state
        .as_ref()
        .and_then(|s| s.total_borrow_assets.as_deref())
        .and_then(|s| U256::from_str(s).ok())
        .unwrap_or(U256::ZERO);

    let borrow_shares_total = market
        .state
        .as_ref()
        .and_then(|s| s.total_borrow_shares.as_deref())
        .and_then(|s| U256::from_str(s).ok())
        .unwrap_or(U256::from(1));

    let user_collateral = position
        .collateral
        .as_deref()
        .and_then(|s| U256::from_str(s).ok())
        .unwrap_or(U256::ZERO);

    let user_borrow_shares = position
        .borrow_shares
        .as_deref()
        .and_then(|s| U256::from_str(s).ok())
        .unwrap_or(U256::ZERO);

    if user_collateral.is_zero() || user_borrow_shares.is_zero() {
        return U256::MAX; // no borrow = perfectly safe
    }

    // Approximate: collateralPrice ≈ supplyAssets / supplyShares
    //              borrowPrice ≈ borrowAssets / borrowShares
    let collateral_value = user_collateral
        .checked_mul(supply_assets)
        .and_then(|v| v.checked_div(supply_shares_total))
        .unwrap_or(U256::ZERO);

    let borrow_value = user_borrow_shares
        .checked_mul(borrow_assets)
        .and_then(|v| v.checked_div(borrow_shares_total))
        .unwrap_or(U256::from(1));

    if borrow_value.is_zero() {
        return U256::MAX;
    }

    // Scale by 1e18 for precision
    collateral_value
        .checked_mul(U256::from(10u128.pow(18)))
        .and_then(|v| v.checked_div(borrow_value))
        .unwrap_or(U256::ZERO)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_compute_health_factor_no_borrow() {
        let market = MarketInfo {
            id: "0x1".into(),
            state: Some(MarketState {
                total_supply_assets: Some("1000000000000000000".into()),
                total_supply_shares: Some("1000000000000000000".into()),
                total_borrow_assets: Some("500000000000000000".into()),
                total_borrow_shares: Some("500000000000000000".into()),
                supply_apy: Some(0.05),
                borrow_apy: None,
            }),
            market_params: None,
        };
        let position = PositionInfo {
            supply_shares: None,
            borrow_shares: Some("0".into()),
            collateral: Some("1000000000000000000".into()),
        };
        let hf = compute_health_factor(&market, &position);
        assert_eq!(hf, U256::MAX); // no borrow = safe
    }

    #[test]
    fn test_compute_health_factor_normal() {
        let market = MarketInfo {
            id: "0x1".into(),
            state: Some(MarketState {
                total_supply_assets: Some("2000000000000000000".into()),
                total_supply_shares: Some("2000000000000000000".into()),
                total_borrow_assets: Some("1000000000000000000".into()),
                total_borrow_shares: Some("1000000000000000000".into()),
                supply_apy: Some(0.05),
                borrow_apy: None,
            }),
            market_params: None,
        };
        // collateral = 2e18, borrowShares = 0.5e18
        // collateral_value = 2e18 * (2e18/2e18) = 2e18
        // borrow_value = 0.5e18 * (1e18/1e18) = 0.5e18
        // hf = 2e18 * 1e18 / 0.5e18 = 4e18 → HF = 4.0
        let position = PositionInfo {
            supply_shares: None,
            borrow_shares: Some("500000000000000000".into()),
            collateral: Some("2000000000000000000".into()),
        };
        let hf = compute_health_factor(&market, &position);
        assert_eq!(hf, U256::from(4000000000000000000u128)); // 4.0e18
    }

    #[test]
    fn test_gql_monitor_new() {
        let m = GqlMonitor::new("https://api.morpho.org/graphql", 60);
        assert_eq!(m.gql_url, "https://api.morpho.org/graphql");
        assert_eq!(m.polling_interval_secs, 60);
    }
}
