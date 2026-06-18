use chrono::Utc;
use serde::Deserialize;
use std::str::FromStr;
use std::time::Duration;
use tracing::{info, warn};

use alloy::primitives::U256;

use crate::alert::{AlertDecision, AlertManager};
use crate::error::AppResult;
use crate::models::{AppState, ConditionGroup, LiquidationConfig, MetricCondition, MonitorState, Order, OrderStatus};

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
    #[serde(rename = "marketById")]
    market_by_id: Option<MarketInfo>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct MarketInfo {
    #[allow(dead_code)]
    id: String,
    state: Option<MarketState>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct MarketState {
    #[serde(default, deserialize_with = "deser_uint_string")]
    supply_assets: Option<String>,
    #[serde(default)]
    supply_shares: Option<String>,
    #[serde(default, deserialize_with = "deser_uint_string")]
    borrow_assets: Option<String>,
    #[serde(default)]
    borrow_shares: Option<String>,
    supply_apy: Option<f64>,
}

fn deser_uint_string<'de, D>(d: D) -> Result<Option<String>, D::Error>
where D: serde::Deserializer<'de> {
    let v: serde_json::Value = serde::Deserialize::deserialize(d)?;
    match v {
        serde_json::Value::Number(n) => Ok(Some(n.to_string())),
        serde_json::Value::String(s) => Ok(Some(s)),
        serde_json::Value::Null => Ok(None),
        _ => Ok(None),
    }
}

#[derive(Debug, Deserialize)]
struct PositionData {
    position: Option<PositionInfo>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PositionInfo {
    supply_shares: Option<String>,
    borrow_shares: Option<String>,
    collateral: Option<String>,
}

#[derive(Debug, Deserialize)]
struct VaultData {
    vault_v2_by_address: Option<VaultInfo>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct VaultInfo {
    total_assets_usd: Option<f64>,
    liquidity_usd: Option<f64>,
    avg_net_apy: Option<f64>,
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

        // Only poll orders that are actively being monitored or alerting
        let active_orders: Vec<Order> = {
            let orders = state.orders.read().await;
            orders
                .values()
                .filter(|o| matches!(o.status, OrderStatus::Monitoring | OrderStatus::Alerting))
                .cloned()
                .collect()
        };

        if active_orders.is_empty() {
            return Ok(());
        }

        for order in &active_orders {
            // Evaluate based on order type
            let (metrics_triggered, market_info_opt, vault_info_opt) =
                match order.order_type.as_str() {
                    "market" => {
                        let mi = match self.query_market(&order.chain, &order.market_id).await {
                            Ok(m) => m,
                            Err(e) => {
                                warn!("GQL market query failed for order {} (market={}): {}",
                                    order.id, order.market_id, e);
                                if e.to_string().contains("Market not found") {
                                    self.mark_invalid(order, state, alert_manager).await;
                                }
                                continue;
                            }
                        };
                        let triggered = evaluate_market_conditions(&order.alert_conditions, &mi);
                        (triggered, Some(mi), None)
                    }
                    "vault" => {
                        let vi = match self.query_vault(&order.chain, &order.market_id).await {
                            Ok(v) => v,
                            Err(e) => {
                                warn!("GQL vault query failed for order {} (vault={}): {}",
                                    order.id, order.market_id, e);
                                continue;
                            }
                        };
                        let triggered = evaluate_vault_conditions(&order.alert_conditions, &vi);
                        (triggered, None, Some(vi))
                    }
                    _ => continue,
                };

            // Evaluate liquidation conditions if configured
            let liquidation_triggered = order
                .liquidation
                .as_ref()
                .map(|lc| match order.order_type.as_str() {
                    "market" => market_info_opt
                        .as_ref()
                        .map_or(false, |mi| evaluate_market_conditions(&lc.conditions, mi)),
                    "vault" => vault_info_opt
                        .as_ref()
                        .map_or(false, |vi| evaluate_vault_conditions(&lc.conditions, vi)),
                    _ => false,
                })
                .unwrap_or(false);

            // Update monitor state
            self.update_monitor_state(state, order, now).await;

            // State machine transition
            self.transition_state(order, metrics_triggered, liquidation_triggered, state, alert_manager)
                .await;
        }

        Ok(())
    }

    async fn mark_invalid(&self, order: &Order, state: &AppState, alert_manager: &AlertManager) {
        {
            let mut orders = state.orders.write().await;
            if let Some(o) = orders.get_mut(&order.id) {
                o.status = OrderStatus::Ended;
                o.updated_at = Utc::now().timestamp();
                warn!("Marked order {} as Ended: market not found", order.id);
            }
        }
        let _ = crate::api::orders::persist_orders(state).await;
        alert_manager
            .notify_user(state, &order.user_address, &format!(
                "⚠️ 订单自动作废\n订单 {} 的市场/金库 {} 在 GraphQL 中未找到，可能已过期。",
                order.id, order.market_id
            ))
            .await;
    }

    async fn update_monitor_state(&self, state: &AppState, order: &Order, now: i64) {
        let key = AlertManager::state_key(&order.chain, &order.market_id, &order.user_address);
        let mut states = state.monitor_states.write().await;
        states.entry(key).or_insert(MonitorState {
            chain: order.chain.clone(),
            market_id: order.market_id.clone(),
            user_address: order.user_address.clone(),
            collateral_amount: U256::ZERO,
            borrow_amount: U256::ZERO,
            health_factor: U256::ZERO,
            last_updated: now,
        });
    }

    async fn transition_state(
        &self,
        order: &Order,
        alert_triggered: bool,
        liquidation_triggered: bool,
        state: &AppState,
        alert_manager: &AlertManager,
    ) {
        let new_status = match (&order.status, alert_triggered, liquidation_triggered) {
            // Monitoring → Alerting on alert trigger
            (OrderStatus::Monitoring, true, false) => Some(OrderStatus::Alerting),
            // Monitoring → Liquidating on liquidation trigger (skip alert)
            (OrderStatus::Monitoring, _, true) => Some(OrderStatus::Liquidating),
            // Alerting → Monitoring on recovery
            (OrderStatus::Alerting, false, false) => {
                let decision = alert_manager
                    .evaluate_risk(
                        &order.chain,
                        &order.market_id,
                        &order.user_address,
                        false,
                    )
                    .await;
                if decision == AlertDecision::Recovered {
                    Some(OrderStatus::Monitoring)
                } else {
                    None
                }
            }
            // Alerting → Liquidating
            (OrderStatus::Alerting, _, true) => Some(OrderStatus::Liquidating),
            _ => None,
        };

        if let Some(status) = new_status {
            let mut orders = state.orders.write().await;
            if let Some(o) = orders.get_mut(&order.id) {
                o.status = status.clone();
                o.updated_at = Utc::now().timestamp();
                let status_copy = status;
                drop(orders);
                let _ = crate::api::orders::persist_orders(state).await;

                match status_copy {
                    OrderStatus::Alerting => {
                        info!(
                            "ALERT triggered for order {} (chain={}, type={}, target={})",
                            order.id, order.chain, order.order_type, order.market_id
                        );
                        alert_manager
                            .notify_user(state, &order.user_address, &format!(
                                "🚨 预警已触发\n链: {}\n类型: {}\n目标: {}\n请关注市场变化。",
                                order.chain,
                                if order.order_type == "vault" { "Vault" } else { "Market" },
                                order.market_id
                            ))
                            .await;
                    }
                    OrderStatus::Liquidating => {
                        info!(
                            "LIQUIDATION triggered for order {} (chain={}, type={}, target={})",
                            order.id, order.chain, order.order_type, order.market_id
                        );
                        alert_manager
                            .notify_user(state, &order.user_address, &format!(
                                "🔥 强平已触发\n链: {}\n类型: {}\n目标: {}\n系统正在执行提款...",
                                order.chain,
                                if order.order_type == "vault" { "Vault" } else { "Market" },
                                order.market_id
                            ))
                            .await;
                        // Spawn liquidation task
                        if let Some(ref lc) = order.liquidation {
                            self.spawn_liquidation_task(
                                order.clone(),
                                lc.clone(),
                                state.clone(),
                            )
                            .await;
                        }
                    }
                    OrderStatus::Monitoring => {
                        info!("Recovery confirmed for order {}", order.id);
                        alert_manager
                            .notify_user(state, &order.user_address, &format!(
                                "✅ 预警已解除\n链: {}\n目标: {}",
                                order.chain, order.market_id
                            ))
                            .await;
                    }
                    _ => {}
                }
            }
        }
    }

    async fn spawn_liquidation_task(
        &self,
        order: Order,
        lc: LiquidationConfig,
        state: AppState,
    ) {
        let order_id = order.id.clone();
        info!("Spawning liquidation task for order {}", order_id);

        tokio::spawn(async move {
            // Build an executor from app config
            let private_key = &state.config.hot_wallet.private_key;
            if private_key.is_empty() {
                warn!("Cannot execute liquidation for order {}: no hot wallet key configured", order_id);
                return;
            }

            let morpho_addr = crate::monitor::morpho_address(&order.chain);
            let rpc_url = state
                .config
                .chains
                .chain_rpc_http(&order.chain)
                .unwrap_or_default();

            if rpc_url.is_empty() {
                warn!("Cannot execute liquidation for order {}: no RPC for chain {}", order_id, order.chain);
                return;
            }

            let gas_min = state.config.hot_wallet.gas_min_balance.parse::<f64>().unwrap_or(0.1);
            let gas_min_u256 = U256::from((gas_min * 1e18) as u64);

            let executor = match crate::executor::BotExecutor::new(
                private_key,
                morpho_addr,
                &rpc_url,
                state.config.flashbots.as_ref().map(|f| f.rpc_url.as_str()),
                gas_min_u256,
            ) {
                Ok(e) => e,
                Err(e) => {
                    warn!("Failed to create executor for order {}: {}", order_id, e);
                    return;
                }
            };

            match executor
                .execute_withdrawal_with_retry(&lc.authorization, &lc.signature, &order)
                .await
            {
                Ok(tx_hash) => {
                    info!("Liquidation succeeded for order {}: {}", order_id, tx_hash);
                    {
                        let mut orders = state.orders.write().await;
                        if let Some(o) = orders.get_mut(&order_id) {
                            o.status = OrderStatus::Ended;
                            o.updated_at = Utc::now().timestamp();
                        }
                    }
                    let _ = crate::api::orders::persist_orders(&state).await;
                    let alert_manager = AlertManager::new();
                    alert_manager
                        .notify_user(&state, &order.user_address, &format!(
                            "✅ 强平已执行\n链: {}\n目标: {}\n交易: {}",
                            order.chain, order.market_id, tx_hash
                        ))
                        .await;
                }
                Err(e) => {
                    warn!("Liquidation failed for order {}: {}", order_id, e);
                    {
                        let mut orders = state.orders.write().await;
                        if let Some(o) = orders.get_mut(&order_id) {
                            o.status = OrderStatus::Ended;
                            o.updated_at = Utc::now().timestamp();
                        }
                    }
                    let _ = crate::api::orders::persist_orders(&state).await;
                    let alert_manager = AlertManager::new();
                    alert_manager
                        .notify_user(&state, &order.user_address, &format!(
                            "❌ 强平执行失败\n链: {}\n目标: {}\n原因: {}",
                            order.chain, order.market_id, e
                        ))
                        .await;
                }
            }
        });
    }

    /// Query market state from Morpho GraphQL.
    async fn query_market(&self, chain: &str, market_id: &str) -> AppResult<MarketInfo> {
        let cid = chain_id(chain);
        let gql = format!("{{ marketById(chainId: {cid}, marketId: \"{mid}\") {{ id state {{ supplyAssets supplyShares borrowAssets borrowShares supplyApy }} }} }}",
            cid = cid, mid = market_id);
        let query = serde_json::json!({"query": gql}).to_string();

        let resp_raw = self
            .http
            .post(&self.gql_url)
            .header("Content-Type", "application/json")
            .body(query.clone())
            .send()
            .await
            .map_err(|e| {
                crate::error::AppError::RpcError(format!("GQL market query failed: {}", e))
            })?;
        let resp_text = resp_raw.text().await.map_err(|e| {
            crate::error::AppError::RpcError(format!("GQL market read failed: {}", e))
        })?;
        let resp: GqlResponse<MarketData> = serde_json::from_str(&resp_text).map_err(|e| {
            crate::error::AppError::RpcError(format!("GQL market parse: {} body={:.300}", e, resp_text))
        })?;

        resp.data
            .and_then(|d| d.market_by_id)
            .ok_or_else(|| crate::error::AppError::RpcError("Market not found".into()))
    }

    /// Query vault state from Morpho GraphQL.
    async fn query_vault(&self, chain: &str, vault_id: &str) -> AppResult<VaultInfo> {
        let cid = chain_id(chain);
        let gql = format!(
            "{{ vaultV2ByAddress(address: \"{vid}\", chainId: {cid}) {{ totalAssetsUsd liquidityUsd avgNetApy }} }}",
            vid = vault_id, cid = cid
        );
        let query = serde_json::json!({"query": gql}).to_string();

        let resp_raw = self
            .http
            .post(&self.gql_url)
            .header("Content-Type", "application/json")
            .body(query)
            .send()
            .await
            .map_err(|e| {
                crate::error::AppError::RpcError(format!("GQL vault query failed: {}", e))
            })?;
        let resp_text = resp_raw.text().await.map_err(|e| {
            crate::error::AppError::RpcError(format!("GQL vault read failed: {}", e))
        })?;
        let resp: GqlResponse<VaultData> = serde_json::from_str(&resp_text).map_err(|e| {
            crate::error::AppError::RpcError(format!("GQL vault parse: {} body={:.300}", e, resp_text))
        })?;

        resp.data
            .and_then(|d| d.vault_v2_by_address)
            .ok_or_else(|| crate::error::AppError::RpcError("Vault not found".into()))
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
// Helpers
// ---------------------------------------------------------------------------

fn chain_id(chain: &str) -> u32 {
    match chain {
        "ethereum" => 1,
        "base" => 8453,
        "optimism" => 10,
        "arbitrum" => 42161,
        "unichain" => 130,
        "hyperevm" => 999,
        _ => 1,
    }
}

// ---------------------------------------------------------------------------
// Condition evaluation
// ---------------------------------------------------------------------------

/// Evaluate a U256 metric condition against a current value.
fn evaluate_u256_condition(condition: &MetricCondition, current: U256) -> bool {
    if !condition.is_active() {
        return false;
    }
    if let Some(ref upper) = condition.upper {
        if let Ok(limit) = U256::from_str(upper) {
            if current > limit {
                return true;
            }
        }
    }
    if let Some(ref lower) = condition.lower {
        if let Ok(limit) = U256::from_str(lower) {
            if current < limit {
                return true;
            }
        }
    }
    false
}

/// Evaluate an f64 metric condition against a current value (APY, USD).
fn evaluate_f64_condition(condition: &MetricCondition, current: f64) -> bool {
    if !condition.is_active() {
        return false;
    }
    if let Some(ref upper) = condition.upper {
        if let Ok(limit) = upper.parse::<f64>() {
            if current > limit {
                return true;
            }
        }
    }
    if let Some(ref lower) = condition.lower {
        if let Ok(limit) = lower.parse::<f64>() {
            if current < limit {
                return true;
            }
        }
    }
    false
}

/// Evaluate market conditions against market GQL data.
fn evaluate_market_conditions(cond: &ConditionGroup, market: &MarketInfo) -> bool {
    let state = match market.state.as_ref() {
        Some(s) => s,
        None => return false,
    };

    let total_assets = state
        .supply_assets
        .as_deref()
        .and_then(|s| U256::from_str(s).ok())
        .unwrap_or(U256::ZERO);
    let total_borrow = state
        .borrow_assets
        .as_deref()
        .and_then(|s| U256::from_str(s).ok())
        .unwrap_or(U256::ZERO);
    let liquidity = total_assets.checked_sub(total_borrow).unwrap_or(U256::ZERO);
    let apy = state.supply_apy.unwrap_or(0.0);

    evaluate_u256_condition(&cond.total_market, total_assets)
        || evaluate_u256_condition(&cond.liquidity, liquidity)
        || evaluate_f64_condition(&cond.supply_apy, apy)
}

/// Evaluate vault conditions against vault GQL data.
fn evaluate_vault_conditions(cond: &ConditionGroup, vault: &VaultInfo) -> bool {
    let total = vault.total_assets_usd.unwrap_or(0.0);
    let liq = vault.liquidity_usd.unwrap_or(0.0);
    let apy = vault.avg_net_apy.unwrap_or(0.0);

    evaluate_f64_condition(&cond.total_deposits, total)
        || evaluate_f64_condition(&cond.liquidity, liq)
        || evaluate_f64_condition(&cond.net_apy, apy)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_evaluate_u256_condition() {
        let c = MetricCondition {
            enabled: true,
            upper: Some("100".into()),
            lower: Some("10".into()),
        };
        assert!(evaluate_u256_condition(&c, U256::from(200)));
        assert!(evaluate_u256_condition(&c, U256::from(5)));
        assert!(!evaluate_u256_condition(&c, U256::from(50)));

        let c = MetricCondition {
            enabled: false,
            upper: Some("100".into()),
            lower: None,
        };
        assert!(!evaluate_u256_condition(&c, U256::from(200)));
    }

    #[test]
    fn test_evaluate_f64_condition() {
        let c = MetricCondition {
            enabled: true,
            upper: Some("5.0".into()),
            lower: Some("0.5".into()),
        };
        assert!(evaluate_f64_condition(&c, 10.0));
        assert!(evaluate_f64_condition(&c, 0.1));
        assert!(!evaluate_f64_condition(&c, 1.0));

        let c = MetricCondition {
            enabled: false,
            upper: Some("5.0".into()),
            lower: None,
        };
        assert!(!evaluate_f64_condition(&c, 10.0));
    }

    #[test]
    fn test_evaluate_market_conditions() {
        let cond = ConditionGroup {
            liquidity: MetricCondition {
                enabled: true,
                upper: None,
                lower: Some("1000".into()),
            },
            ..Default::default()
        };
        let market = MarketInfo {
            id: "0x1".into(),
            state: Some(MarketState {
                supply_assets: Some("2000".into()),
                supply_shares: Some("1000".into()),
                borrow_assets: Some("1500".into()),
                borrow_shares: Some("800".into()),
                supply_apy: Some(0.05),
            }),
        };
        // liquidity = 2000 - 1500 = 500, below threshold 1000 → triggered
        assert!(evaluate_market_conditions(&cond, &market));
    }

    #[test]
    fn test_gql_monitor_new() {
        let m = GqlMonitor::new("https://api.morpho.org/graphql", 60);
        assert_eq!(m.gql_url, "https://api.morpho.org/graphql");
        assert_eq!(m.polling_interval_secs, 60);
    }
}
