use chrono::Utc;
use serde::Deserialize;
use std::str::FromStr;
use std::time::Duration;
use tracing::{info, warn};

use alloy::primitives::U256;

use crate::alert::{AlertDecision, AlertManager};
use crate::error::AppResult;
use crate::models::{AppState, CachedData, ConditionGroup, LiquidationConfig, MetricCondition, MonitorState, Order, OrderStatus};

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
    loan_asset: Option<LoanAsset>,
    state: Option<MarketState>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct LoanAsset {
    decimals: Option<u32>,
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
    #[serde(rename = "vaultV2ByAddress")]
    vault_v2_by_address: Option<VaultInfo>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct VaultInfo {
    total_assets_usd: Option<f64>,
    liquidity_usd: Option<f64>,
    idle_assets_usd: Option<f64>,
    force_deallocatable_liquidity_usd: Option<f64>,
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
                .filter(|o| matches!(o.status, OrderStatus::Editing | OrderStatus::Monitoring | OrderStatus::Alerting))
                .cloned()
                .collect()
        };

        if active_orders.is_empty() {
            return Ok(());
        }

        for order in &active_orders {
            // Evaluate based on order type
            let (alert_reasons, market_info_opt, vault_info_opt) =
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
                        let reasons = evaluate_market_conditions(&order.alert_conditions, &mi);
                        cache_market_data(state, &order.market_id, &mi).await;
                        (reasons, Some(mi), None)
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
                        let reasons = evaluate_vault_conditions(&order.alert_conditions, &vi);
                        cache_vault_data(state, &order.market_id, &vi).await;
                        (reasons, None, Some(vi))
                    }
                    _ => continue,
                };
            let alert_triggered = !alert_reasons.is_empty();

            // Evaluate liquidation conditions if configured
            let liq_reasons: Vec<String> = order
                .liquidation
                .as_ref()
                .map(|lc| match order.order_type.as_str() {
                    "market" => market_info_opt.as_ref().map_or(Vec::new(), |mi| evaluate_market_conditions(&lc.conditions, mi)),
                    "vault" => vault_info_opt.as_ref().map_or(Vec::new(), |vi| evaluate_vault_conditions(&lc.conditions, vi)),
                    _ => Vec::new(),
                })
                .unwrap_or_default();
            let liquidation_triggered = !liq_reasons.is_empty();

            // Update monitor state
            self.update_monitor_state(state, order, now).await;

            // State machine transition
            self.transition_state(order, alert_triggered, &alert_reasons, liquidation_triggered, &liq_reasons, state, alert_manager)
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
        alert_reasons: &[String],
        liquidation_triggered: bool,
        liq_reasons: &[String],
        state: &AppState,
        alert_manager: &AlertManager,
    ) {
        let new_status = match (&order.status, alert_triggered, liquidation_triggered) {
            // Editing → Monitoring (first poll after user edit)
            (OrderStatus::Editing, _, _) => Some(OrderStatus::Monitoring),
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

                let reasons_text = |reasons: &[String]| if reasons.is_empty() { String::new() } else { format!("\n触发条件:\n{}", reasons.iter().map(|r| format!("  • {}", r)).collect::<Vec<_>>().join("\n")) };
                let tlabel = if order.order_type == "vault" { "Vault" } else { "Market" };
                match status_copy {
                    OrderStatus::Alerting => {
                        info!("ALERT triggered for order {} (chain={}, type={}, target={})", order.id, order.chain, order.order_type, order.market_id);
                        let msg = format!("🚨 预警已触发\n名称: {}\n链: {}\n类型: {}\n目标: {}{}",
                            order.name, order.chain, tlabel, order.market_id, reasons_text(alert_reasons));
                        alert_manager.notify_user(state, &order.user_address, &msg).await;
                    }
                    OrderStatus::Liquidating => {
                        info!("LIQUIDATION triggered for order {} (chain={}, type={}, target={})", order.id, order.chain, order.order_type, order.market_id);
                        let msg = format!("🔥 强平已触发\n名称: {}\n链: {}\n类型: {}\n目标: {}{}{}",
                            order.name, order.chain, tlabel, order.market_id,
                            reasons_text(alert_reasons), reasons_text(liq_reasons));
                        alert_manager.notify_user(state, &order.user_address, &msg).await;
                        if let Some(ref lc) = order.liquidation {
                            self.spawn_liquidation_task(order.clone(), lc.clone(), state.clone()).await;
                        }
                    }
                    OrderStatus::Monitoring => {
                        info!("Recovery confirmed for order {}", order.id);
                        let msg = format!("✅ 预警已解除\n名称: {}\n链: {}\n类型: {}\n目标: {}",
                            order.name, order.chain, tlabel, order.market_id);
                        alert_manager.notify_user(state, &order.user_address, &msg).await;
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
        let gql = format!("{{ marketById(chainId: {cid}, marketId: \"{mid}\") {{ id loanAsset {{ decimals }} state {{ supplyAssets supplyShares borrowAssets borrowShares supplyApy }} }} }}",
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
            "{{ vaultV2ByAddress(address: \"{vid}\", chainId: {cid}) {{ totalAssetsUsd liquidityUsd idleAssetsUsd forceDeallocatableLiquidityUsd avgNetApy }} }}",
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
        "monad" => 143,
        "katana" => 747474,
        "polygon" => 137,
        "stable" => 988,
        "tempo" => 4217,
        "worldchain" => 480,
        _ => 1,
    }
}

// ---------------------------------------------------------------------------
// Cache
// ---------------------------------------------------------------------------

async fn cache_market_data(state: &AppState, market_id: &str, mi: &MarketInfo) {
    if let Some(ref st) = mi.state {
        let total = st.supply_assets.as_deref().and_then(|s| U256::from_str(s).ok()).unwrap_or(U256::ZERO);
        let borrow = st.borrow_assets.as_deref().and_then(|s| U256::from_str(s).ok()).unwrap_or(U256::ZERO);
        let liq = total.checked_sub(borrow).unwrap_or(U256::ZERO);
        let apy = st.supply_apy.unwrap_or(0.0);
        let decimals = mi.loan_asset.as_ref().and_then(|a| a.decimals).unwrap_or(18);
        let mut cache = state.market_cache.write().await;
        cache.insert(market_id.to_string(), CachedData::Market {
            total: total.to_string(),
            liquidity: liq.to_string(),
            apy: format!("{:.4}", apy),
            decimals,
            updated_at: Utc::now().timestamp(),
        });
    }
}

async fn cache_vault_data(state: &AppState, vault_id: &str, vi: &VaultInfo) {
    let liq = vi.liquidity_usd.unwrap_or(0.0)
        + vi.idle_assets_usd.unwrap_or(0.0)
        + vi.force_deallocatable_liquidity_usd.unwrap_or(0.0);
    let mut cache = state.market_cache.write().await;
    cache.insert(vault_id.to_string(), CachedData::Vault {
        deposits: format!("{:.2}", vi.total_assets_usd.unwrap_or(0.0)),
        liquidity: format!("{:.2}", liq),
        apy: format!("{:.4}", vi.avg_net_apy.unwrap_or(0.0)),
        updated_at: Utc::now().timestamp(),
    });
}

// ---------------------------------------------------------------------------
// Condition evaluation
// ---------------------------------------------------------------------------

/// Evaluate a U256 metric condition, return triggered reasons.
fn evaluate_u256_condition(name: &str, condition: &MetricCondition, current: U256) -> Vec<String> {
    let mut reasons = Vec::new();
    if !condition.is_active() { return reasons; }
    if let Some(ref upper) = condition.upper {
        if let Ok(limit) = U256::from_str(upper) {
            if current > limit {
                reasons.push(format!("{} 当前值 {} > 上限 {}", name, current, limit));
            }
        }
    }
    if let Some(ref lower) = condition.lower {
        if let Ok(limit) = U256::from_str(lower) {
            if current < limit {
                reasons.push(format!("{} 当前值 {} < 下限 {}", name, current, limit));
            }
        }
    }
    reasons
}

/// Evaluate an f64 metric condition, return triggered reasons.
fn evaluate_f64_condition(name: &str, condition: &MetricCondition, current: f64) -> Vec<String> {
    let mut reasons = Vec::new();
    if !condition.is_active() { return reasons; }
    if let Some(ref upper) = condition.upper {
        if let Ok(limit) = upper.parse::<f64>() {
            if current > limit {
                reasons.push(format!("{} 当前值 {:.4} > 上限 {}", name, current, limit));
            }
        }
    }
    if let Some(ref lower) = condition.lower {
        if let Ok(limit) = lower.parse::<f64>() {
            if current < limit {
                reasons.push(format!("{} 当前值 {:.4} < 下限 {}", name, current, limit));
            }
        }
    }
    reasons
}

/// Evaluate market conditions, return triggered metric details.
fn evaluate_market_conditions(cond: &ConditionGroup, market: &MarketInfo) -> Vec<String> {
    let state = match market.state.as_ref() {
        Some(s) => s,
        None => return Vec::new(),
    };
    let total_assets = state.supply_assets.as_deref().and_then(|s| U256::from_str(s).ok()).unwrap_or(U256::ZERO);
    let total_borrow = state.borrow_assets.as_deref().and_then(|s| U256::from_str(s).ok()).unwrap_or(U256::ZERO);
    let liquidity = total_assets.checked_sub(total_borrow).unwrap_or(U256::ZERO);
    let apy = state.supply_apy.unwrap_or(0.0);

    let mut reasons = Vec::new();
    reasons.extend(evaluate_u256_condition("Total Market", &cond.total_market, total_assets));
    reasons.extend(evaluate_u256_condition("Liquidity", &cond.liquidity, liquidity));
    reasons.extend(evaluate_f64_condition("Supply APY", &cond.supply_apy, apy));
    reasons
}

/// Evaluate vault conditions, return triggered metric details.
fn evaluate_vault_conditions(cond: &ConditionGroup, vault: &VaultInfo) -> Vec<String> {
    let total = vault.total_assets_usd.unwrap_or(0.0);
    let liq = vault.liquidity_usd.unwrap_or(0.0)
        + vault.idle_assets_usd.unwrap_or(0.0)
        + vault.force_deallocatable_liquidity_usd.unwrap_or(0.0);
    let apy = vault.avg_net_apy.unwrap_or(0.0);

    let mut reasons = Vec::new();
    reasons.extend(evaluate_f64_condition("Total Deposits", &cond.total_deposits, total));
    reasons.extend(evaluate_f64_condition("Liquidity", &cond.liquidity, liq));
    reasons.extend(evaluate_f64_condition("Net APY", &cond.net_apy, apy));
    reasons
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_evaluate_u256_condition() {
        let c = MetricCondition { enabled: true, upper: Some("100".into()), lower: Some("10".into()) };
        assert!(!evaluate_u256_condition("test", &c, U256::from(200)).is_empty());
        assert!(!evaluate_u256_condition("test", &c, U256::from(5)).is_empty());
        assert!(evaluate_u256_condition("test", &c, U256::from(50)).is_empty());
        let c = MetricCondition { enabled: false, upper: Some("100".into()), lower: None };
        assert!(evaluate_u256_condition("test", &c, U256::from(200)).is_empty());
    }

    #[test]
    fn test_evaluate_f64_condition() {
        let c = MetricCondition { enabled: true, upper: Some("5.0".into()), lower: Some("0.5".into()) };
        assert!(!evaluate_f64_condition("test", &c, 10.0).is_empty());
        assert!(!evaluate_f64_condition("test", &c, 0.1).is_empty());
        assert!(evaluate_f64_condition("test", &c, 1.0).is_empty());
        let c = MetricCondition { enabled: false, upper: Some("5.0".into()), lower: None };
        assert!(evaluate_f64_condition("test", &c, 10.0).is_empty());
    }

    #[test]
    fn test_evaluate_market_conditions() {
        let cond = ConditionGroup { liquidity: MetricCondition { enabled: true, upper: None, lower: Some("1000".into()) }, ..Default::default() };
        let market = MarketInfo { id: "0x1".into(), loan_asset: None, state: Some(MarketState {
            supply_assets: Some("2000".into()), supply_shares: Some("1000".into()),
            borrow_assets: Some("1500".into()), borrow_shares: Some("800".into()), supply_apy: Some(0.05),
        }) };
        // liquidity = 2000 - 1500 = 500, below threshold 1000 → triggered
        assert!(!evaluate_market_conditions(&cond, &market).is_empty());
    }

    #[test]
    fn test_gql_monitor_new() {
        let m = GqlMonitor::new("https://api.morpho.org/graphql", 60);
        assert_eq!(m.gql_url, "https://api.morpho.org/graphql");
        assert_eq!(m.polling_interval_secs, 60);
    }
}
