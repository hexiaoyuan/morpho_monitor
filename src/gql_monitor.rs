use chrono::Utc;
use serde::Deserialize;
use std::collections::HashMap;
use std::str::FromStr;
use std::time::Duration;
use tracing::{debug, info, trace, warn};

use alloy::primitives::U256;

use crate::alert::{AlertDecision, AlertManager};
use crate::error::AppResult;
use crate::models::{
    AppState, CachedData, ConditionGroup, LiquidationConfig, MetricCondition, MonitorState, Order,
    OrderStatus,
};

// ---------------------------------------------------------------------------
// Morpho GraphQL response types
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct GqlResponse<T> {
    data: Option<T>,
    errors: Option<Vec<serde_json::Value>>,
}

#[derive(Debug, Deserialize)]
struct MarketData {
    #[serde(rename = "marketById")]
    market_by_id: Option<MarketInfo>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct MarketInfo {
    id: String,
    loan_asset: Option<LoanAsset>,
    state: Option<MarketState>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct LoanAsset {
    decimals: Option<u32>,
}

#[derive(Debug, Clone, Deserialize)]
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
where
    D: serde::Deserializer<'de>,
{
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

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct VaultInfo {
    id: Option<String>,
    name: Option<String>,
    asset: Option<VaultAsset>,
    total_assets_usd: Option<f64>,
    liquidity_usd: Option<f64>,
    net_apy: Option<f64>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct VaultAsset {
    decimals: Option<u32>,
}

// ---------------------------------------------------------------------------
// GQL Monitor
// ---------------------------------------------------------------------------

/// Polls Morpho's GraphQL API for market + position data.
/// Lower real-time fidelity than direct RPC, but requires zero node config.
pub struct GqlMonitor {
    pub gql_url: String,
    pub polling_interval_secs: u64,
    pub batch_size: usize,
    http: reqwest::Client,
    last_admin_alert: std::sync::Mutex<i64>,
    gql_alert_active: std::sync::Mutex<bool>,
}

/// Info for one aliased sub-query in a batch GQL request.
struct BatchItem {
    alias: String,
    gql_fragment: String,
    order_index: usize, // index into the active_orders vec
    is_market: bool,
    chain: String,
    market_id: String,
}

impl GqlMonitor {
    pub fn new(gql_url: &str, polling_interval_secs: u64, batch_size: usize) -> Self {
        Self {
            gql_url: gql_url.to_string(),
            polling_interval_secs,
            batch_size,
            http: reqwest::Client::new(),
            last_admin_alert: std::sync::Mutex::new(0),
            gql_alert_active: std::sync::Mutex::new(false),
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
                .filter(|o| {
                    matches!(
                        o.status,
                        OrderStatus::Editing | OrderStatus::Monitoring | OrderStatus::Alerting
                    )
                })
                .cloned()
                .collect()
        };

        if active_orders.is_empty() {
            self.maybe_recover_gql_alert(alert_manager, state).await;
            return Ok(());
        }

        // Build deduplicated query items — same (chain, market_id, type) only queried once
        let mut seen = std::collections::HashSet::new();
        let mut batch_items: Vec<BatchItem> = Vec::new();
        let mut order_data: Vec<(Option<MarketInfo>, Option<VaultInfo>)> =
            vec![(None, None); active_orders.len()];

        for (idx, order) in active_orders.iter().enumerate() {
            let is_market = order.order_type == "market";
            let dedup_key = format!("{}:{}:{}", order.chain, order.market_id, order.order_type);
            if !seen.insert(dedup_key) {
                continue; // dedup: this (chain, id, type) already queued
            }
            let cid = chain_id(&order.chain);
            let fragment = if is_market {
                format!("marketById(chainId: {cid}, marketId: \"{mid}\") {{ id loanAsset {{ decimals }} state {{ supplyAssets supplyShares borrowAssets borrowShares supplyApy }} }}",
                    cid = cid, mid = order.market_id)
            } else {
                format!("vaultV2ByAddress(address: \"{vid}\", chainId: {cid}) {{ id name asset {{ decimals }} totalAssetsUsd liquidityUsd netApy }}",
                    vid = order.market_id, cid = cid)
            };
            batch_items.push(BatchItem {
                alias: format!("q{}", batch_items.len()),
                gql_fragment: fragment,
                order_index: idx,
                is_market,
                chain: order.chain.clone(),
                market_id: order.market_id.clone(),
            });
        }

        debug!(
            "GQL batch: {} active orders → {} unique queries (batch_size={})",
            active_orders.len(),
            batch_items.len(),
            self.batch_size
        );

        let mut had_transient_error = false;

        // Process in batches
        for chunk in batch_items.chunks(self.batch_size) {
            let (batch_result, batch_err) = self.execute_batch(chunk).await;
            let mut batch_had_error = false;

            for item in chunk {
                let (mi_opt, vi_opt, err_msg) = if let Some(data) = batch_result.get(&item.alias) {
                    if item.is_market {
                        match serde_json::from_value::<MarketInfo>(data.clone()) {
                            Ok(mi) => {
                                if !mi.id.is_empty() || mi.state.is_some() {
                                    cache_market_data(state, &item.market_id, &mi).await;
                                    (Some(mi), None, None)
                                } else {
                                    (None, None, Some("Market not found".into()))
                                }
                            }
                            Err(e) => {
                                warn!(
                                    "GQL batch parse market {}: {} data={}",
                                    item.market_id, e, data
                                );
                                batch_had_error = true;
                                (None, None, Some(format!("parse error: {}", e)))
                            }
                        }
                    } else {
                        trace!("VaultInfo data={:?}", data);
                        match serde_json::from_value::<VaultInfo>(data.clone()) {
                            Ok(vi) => {
                                if vi.total_assets_usd.is_some() {
                                    cache_vault_data(state, &item.market_id, &vi).await;
                                    (None, Some(vi), None)
                                } else {
                                    (None, None, Some("Vault not found".into()))
                                }
                            }
                            Err(e) => {
                                warn!(
                                    "GQL batch parse vault {}: {} data={}",
                                    item.market_id, e, data
                                );
                                batch_had_error = true;
                                (None, None, Some(format!("parse error: {}", e)))
                            }
                        }
                    }
                } else {
                    // Alias missing from response entirely — transient error
                    batch_had_error = true;
                    (
                        None,
                        None,
                        Some(
                            batch_err
                                .clone()
                                .unwrap_or_else(|| "missing alias in GQL response".into()),
                        ),
                    )
                };

                // Store the results for all orders that share this (chain, market_id, type)
                for (oi, order) in active_orders.iter().enumerate() {
                    if order.chain == item.chain
                        && order.market_id == item.market_id
                        && order.order_type == (if item.is_market { "market" } else { "vault" })
                    {
                        if let Some(ref mi) = mi_opt {
                            order_data[oi].0 = Some(mi.clone());
                        }
                        if let Some(ref vi) = vi_opt {
                            order_data[oi].1 = Some(vi.clone());
                        }
                    }
                }

                // Handle errors for ALL orders affected by this query failure
                if let Some(ref err_msg) = err_msg {
                    for (_oi, order) in active_orders.iter().enumerate() {
                        if order.chain == item.chain
                            && order.market_id == item.market_id
                            && order.order_type == (if item.is_market { "market" } else { "vault" })
                        {
                            warn!(
                                "GQL error for order {} ({}): {}",
                                order.id, order.market_id, err_msg
                            );
                            if err_msg.contains("not found") {
                                self.mark_invalid(order, state, alert_manager).await;
                            } else {
                                had_transient_error = true;
                                self.maybe_alert_admin(
                                    state,
                                    alert_manager,
                                    &format!(
                                        "⚠️ GQL查询异常\n链: {}\n目标: {}\n类型: {}\n错误: {}",
                                        order.chain, order.market_id, order.order_type, err_msg
                                    ),
                                )
                                .await;
                            }
                        }
                    }
                }
            }

            if batch_had_error {
                had_transient_error = true;
            }
        }

        // Now evaluate conditions and transition state for each order
        for (idx, order) in active_orders.iter().enumerate() {
            // Skip orders that were already marked invalid/ended
            {
                let orders = state.orders.read().await;
                if let Some(o) = orders.get(&order.id) {
                    if o.status == OrderStatus::Ended {
                        continue;
                    }
                }
            }

            let (mi_opt, vi_opt) = (&order_data[idx].0, &order_data[idx].1);

            let alert_reasons = match order.order_type.as_str() {
                "market" => mi_opt.as_ref().map_or(Vec::new(), |mi| {
                    evaluate_market_conditions(&order.alert_conditions, mi)
                }),
                "vault" => vi_opt.as_ref().map_or(Vec::new(), |vi| {
                    evaluate_vault_conditions(&order.alert_conditions, vi)
                }),
                _ => continue,
            };
            let alert_triggered = !alert_reasons.is_empty();

            let liq_reasons = order
                .liquidation
                .as_ref()
                .map(|lc| match order.order_type.as_str() {
                    "market" => mi_opt.as_ref().map_or(Vec::new(), |mi| {
                        evaluate_market_conditions(&lc.conditions, mi)
                    }),
                    "vault" => vi_opt.as_ref().map_or(Vec::new(), |vi| {
                        evaluate_vault_conditions(&lc.conditions, vi)
                    }),
                    _ => Vec::new(),
                })
                .unwrap_or_default();
            let liquidation_triggered = !liq_reasons.is_empty();

            self.update_monitor_state(state, order, now).await;
            self.transition_state(
                order,
                alert_triggered,
                &alert_reasons,
                liquidation_triggered,
                &liq_reasons,
                state,
                alert_manager,
            )
            .await;
        }

        if !had_transient_error {
            self.maybe_recover_gql_alert(alert_manager, state).await;
        }
        Ok(())
    }

    /// Execute one batch of GQL sub-queries and return parsed JSON data per alias.
    /// Returns (alias → serde_json::Value, optional overall error message).
    async fn execute_batch(
        &self,
        items: &[BatchItem],
    ) -> (HashMap<String, serde_json::Value>, Option<String>) {
        let mut result = HashMap::new();
        if items.is_empty() {
            return (result, None);
        }

        // Build the GQL query with aliases
        let fragments: Vec<String> = items
            .iter()
            .map(|it| format!("{}: {}", it.alias, it.gql_fragment))
            .collect();
        let gql = format!("{{ {} }}", fragments.join(" "));
        let body = serde_json::json!({"query": gql}).to_string();

        debug!(
            "GQL batch request: {} queries, body_len={}",
            items.len(),
            body.len()
        );

        let resp = match self
            .http
            .post(&self.gql_url)
            .header("Content-Type", "application/json")
            .body(body.clone())
            .send()
            .await
        {
            Ok(r) => r,
            Err(e) => {
                let msg = format!("GQL batch HTTP request failed: {}", e);
                warn!("{} body={}", msg, &body[..body.len().min(1000)]);
                return (result, Some(msg));
            }
        };

        let status = resp.status().as_u16();
        let resp_text = match resp.text().await {
            Ok(t) => t,
            Err(e) => {
                let msg = format!("GQL batch read body failed: {}", e);
                warn!("{} body={}", msg, &body[..body.len().min(1000)]);
                return (result, Some(msg));
            }
        };

        if status < 200 || status >= 300 {
            let msg = format!(
                "GQL batch HTTP {} body={:.500}",
                status,
                &resp_text[..resp_text.len().min(500)]
            );
            warn!("{} request_body={}", msg, &body[..body.len().min(1000)]);
            return (result, Some(msg));
        }

        // Parse as generic JSON and extract each alias
        let parsed: serde_json::Value = match serde_json::from_str(&resp_text) {
            Ok(v) => v,
            Err(e) => {
                let msg = format!(
                    "GQL batch JSON parse error: {} body={:.500}",
                    e,
                    &resp_text[..resp_text.len().min(500)]
                );
                warn!("{}", msg);
                return (result, Some(msg));
            }
        };

        // Log GQL-level errors with full detail
        if let Some(errors) = parsed.get("errors") {
            warn!(
                "GQL batch response errors: {}",
                serde_json::to_string_pretty(errors).unwrap_or_else(|_| format!("{:?}", errors))
            );
        }

        // Extract data per alias
        if let Some(data) = parsed.get("data") {
            for item in items {
                if let Some(val) = data.get(&item.alias) {
                    if val.is_null() {
                        // null means GQL resolved the query but returned null (not found or error)
                        continue;
                    }
                    result.insert(item.alias.clone(), val.clone());
                }
            }
        }

        (result, None)
    }

    /// If GQL alert was active and queries now succeed, send recovery.
    async fn maybe_recover_gql_alert(&self, alert_manager: &AlertManager, state: &AppState) {
        let was_active = {
            let mut active = self.gql_alert_active.lock().unwrap();
            if *active {
                *active = false;
                true
            } else {
                false
            }
        };
        if was_active {
            info!("GQL queries recovered, sending admin notification");
            alert_manager
                .notify_admin(state, "✅ GQL查询已恢复正常")
                .await;
        }
    }

    /// Notify admin with 5-minute debounce to avoid spamming.
    async fn maybe_alert_admin(
        &self,
        state: &AppState,
        alert_manager: &AlertManager,
        content: &str,
    ) {
        let now = Utc::now().timestamp();
        let should_alert = {
            let mut last = self.last_admin_alert.lock().unwrap();
            if now - *last >= 300 {
                *last = now;
                true
            } else {
                false
            }
        }; // drop guard before .await
        if should_alert {
            *self.gql_alert_active.lock().unwrap() = true;
            alert_manager.notify_admin(state, content).await;
        }
    }

    /// Mark an order as Ended because the market/vault genuinely doesn't exist.
    async fn mark_invalid(&self, order: &Order, state: &AppState, alert_manager: &AlertManager) {
        {
            let mut orders = state.orders.write().await;
            if let Some(o) = orders.get_mut(&order.id) {
                info!(
                    "Order {} status: {:?} → Ended (not found)",
                    order.id, o.status
                );
                o.status = OrderStatus::Ended;
                o.updated_at = Utc::now().timestamp();
            }
        }
        let _ = crate::api::orders::persist_orders(state).await;
        alert_manager
            .notify_user(
                state,
                &order.user_address,
                &format!(
                    "⚠️ 订单自动作废\n订单 {} 的市场/金库 {} 在 GraphQL 中未找到，可能已过期。",
                    order.id, order.market_id
                ),
            )
            .await;
    }

    async fn update_monitor_state(&self, state: &AppState, order: &Order, now: i64) {
        let key = AlertManager::state_key(&order.chain, &order.market_id, &order.user_address);
        let mut states = state.monitor_states.write().await;
        let entry = states.entry(key).or_insert(MonitorState {
            chain: order.chain.clone(),
            market_id: order.market_id.clone(),
            user_address: order.user_address.clone(),
            collateral_amount: U256::ZERO,
            borrow_amount: U256::ZERO,
            health_factor: U256::ZERO,
            last_updated: now,
        });
        entry.last_updated = now;
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
            // Monitoring → Alerting on alert trigger — seed alert state for recovery
            (OrderStatus::Monitoring, true, false) => {
                alert_manager
                    .evaluate_risk(&order.chain, &order.market_id, &order.user_address, true)
                    .await;
                Some(OrderStatus::Alerting)
            }
            // Monitoring → Liquidating on liquidation trigger — seed alert state
            (OrderStatus::Monitoring, _, true) => {
                alert_manager
                    .evaluate_risk(&order.chain, &order.market_id, &order.user_address, true)
                    .await;
                Some(OrderStatus::Liquidating)
            }
            // Alerting → stays Alerting, but reset recovery streak on re-trigger
            (OrderStatus::Alerting, true, false) => {
                alert_manager
                    .evaluate_risk(&order.chain, &order.market_id, &order.user_address, true)
                    .await;
                None
            }
            // Alerting → Monitoring on recovery
            (OrderStatus::Alerting, false, false) => {
                // After server restart, in-memory alert state is lost. If the state
                // was never seeded (in_alert=false), seed it now and stay Alerting.
                // Recovery will be checked on subsequent polls (3 normal rounds needed).
                let key =
                    AlertManager::state_key(&order.chain, &order.market_id, &order.user_address);
                if !alert_manager.get_state(&key).await.in_alert {
                    alert_manager
                        .evaluate_risk(&order.chain, &order.market_id, &order.user_address, true)
                        .await;
                    None
                } else {
                    let decision = alert_manager
                        .evaluate_risk(&order.chain, &order.market_id, &order.user_address, false)
                        .await;
                    if decision == AlertDecision::Recovered {
                        Some(OrderStatus::Monitoring)
                    } else {
                        None
                    }
                }
            }
            // Alerting → Liquidating
            (OrderStatus::Alerting, _, true) => {
                alert_manager
                    .evaluate_risk(&order.chain, &order.market_id, &order.user_address, true)
                    .await;
                Some(OrderStatus::Liquidating)
            }
            _ => None,
        };

        if let Some(new) = new_status {
            let old = order.status.clone();
            info!("Order {} status: {:?} → {:?}", order.id, old, new);
            let mut orders = state.orders.write().await;
            if let Some(o) = orders.get_mut(&order.id) {
                o.status = new.clone();
                o.updated_at = Utc::now().timestamp();
                drop(orders);
                let _ = crate::api::orders::persist_orders(state).await;

                let reasons_text = |reasons: &[String]| {
                    if reasons.is_empty() {
                        String::new()
                    } else {
                        format!(
                            "\n触发条件:\n{}",
                            reasons
                                .iter()
                                .map(|r| format!("  • {}", r))
                                .collect::<Vec<_>>()
                                .join("\n")
                        )
                    }
                };
                let tlabel = if order.order_type == "vault" {
                    "Vault"
                } else {
                    "Market"
                };
                // Only notify on meaningful transitions (skip Editing→Monitoring)
                match (&old, &new) {
                    (_, OrderStatus::Alerting) => {
                        info!(
                            "ALERT triggered for order {} (chain={}, type={}, target={})",
                            order.id, order.chain, order.order_type, order.market_id
                        );
                        let msg = format!(
                            "🚨 预警已触发\n名称: {}\n链: {}\n类型: {}\n目标: {}{}",
                            order.name,
                            order.chain,
                            tlabel,
                            order.market_id,
                            reasons_text(alert_reasons)
                        );
                        alert_manager
                            .notify_user(state, &order.user_address, &msg)
                            .await;
                    }
                    (_, OrderStatus::Liquidating) => {
                        info!(
                            "LIQUIDATION triggered for order {} (chain={}, type={}, target={})",
                            order.id, order.chain, order.order_type, order.market_id
                        );
                        let msg = format!(
                            "🔥 强平已触发\n名称: {}\n链: {}\n类型: {}\n目标: {}{}{}",
                            order.name,
                            order.chain,
                            tlabel,
                            order.market_id,
                            reasons_text(alert_reasons),
                            reasons_text(liq_reasons)
                        );
                        alert_manager
                            .notify_user(state, &order.user_address, &msg)
                            .await;
                        if let Some(ref lc) = order.liquidation {
                            self.spawn_liquidation_task(
                                order.clone(),
                                lc.clone(),
                                state.clone(),
                                alert_manager.clone(),
                            )
                            .await;
                        }
                    }
                    (OrderStatus::Alerting, OrderStatus::Monitoring) => {
                        info!("Recovery confirmed for order {}", order.id);
                        let msg = format!(
                            "✅ 预警已解除\n名称: {}\n链: {}\n类型: {}\n目标: {}",
                            order.name, order.chain, tlabel, order.market_id
                        );
                        alert_manager
                            .notify_user(state, &order.user_address, &msg)
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
        alert_manager: AlertManager,
    ) {
        let order_id = order.id.clone();
        info!("Spawning liquidation task for order {}", order_id);

        tokio::spawn(async move {
            // Build an executor from app config
            let private_key = &state.config.hot_wallet.private_key;
            if private_key.is_empty() {
                warn!(
                    "Cannot execute liquidation for order {}: no hot wallet key configured",
                    order_id
                );
                return;
            }

            let morpho_addr = crate::monitor::morpho_address(&order.chain);
            let rpc_url = state
                .config
                .chains
                .chain_rpc_http(&order.chain)
                .unwrap_or_default();

            if rpc_url.is_empty() {
                warn!(
                    "Cannot execute liquidation for order {}: no RPC for chain {}",
                    order_id, order.chain
                );
                return;
            }

            let gas_min_u256 = parse_eth_to_wei(&state.config.hot_wallet.gas_min_balance)
                .unwrap_or(U256::from(100_000_000_000_000_000u128)); // 0.1 ETH default

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
                            info!(
                                "Order {} status: {:?} → Ended (liquidation ok)",
                                order_id, o.status
                            );
                            o.status = OrderStatus::Ended;
                            o.updated_at = Utc::now().timestamp();
                        }
                    }
                    let _ = crate::api::orders::persist_orders(&state).await;
                    alert_manager
                        .notify_user(
                            &state,
                            &order.user_address,
                            &format!(
                                "✅ 强平已执行\n链: {}\n目标: {}\n交易: {}",
                                order.chain, order.market_id, tx_hash
                            ),
                        )
                        .await;
                }
                Err(e) => {
                    warn!("Liquidation failed for order {}: {}", order_id, e);
                    {
                        let mut orders = state.orders.write().await;
                        if let Some(o) = orders.get_mut(&order_id) {
                            info!(
                                "Order {} status: {:?} → Ended (liquidation failed)",
                                order_id, o.status
                            );
                            o.status = OrderStatus::Ended;
                            o.updated_at = Utc::now().timestamp();
                        }
                    }
                    let _ = crate::api::orders::persist_orders(&state).await;
                    alert_manager
                        .notify_user(
                            &state,
                            &order.user_address,
                            &format!(
                                "❌ 强平执行失败\n链: {}\n目标: {}\n原因: {}",
                                order.chain, order.market_id, e
                            ),
                        )
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
                crate::error::AppError::RpcError(format!("GQL HTTP request failed: {}", e))
            })?;
        let status = resp_raw.status().as_u16();
        let resp_text = resp_raw.text().await.map_err(|e| {
            crate::error::AppError::RpcError(format!("GQL read body failed: {}", e))
        })?;
        if status < 200 || status >= 300 {
            return Err(crate::error::AppError::RpcError(format!(
                "GQL HTTP {} body={:.200}",
                status, resp_text
            )));
        }
        let resp: GqlResponse<MarketData> = serde_json::from_str(&resp_text).map_err(|e| {
            crate::error::AppError::RpcError(format!(
                "GQL JSON parse: {} body={:.300}",
                e, resp_text
            ))
        })?;

        // Check GQL errors first — only "NOT_FOUND" means the market genuinely doesn't exist
        if let Some(ref errors) = resp.errors {
            if !errors.is_empty() {
                let is_not_found = errors
                    .iter()
                    .any(|e| e.get("status").and_then(|s| s.as_str()) == Some("NOT_FOUND"));
                if is_not_found {
                    return Err(crate::error::AppError::RpcError("Market not found".into()));
                }
                // Other GQL errors: treat as transient, log the details
                let msg = errors
                    .iter()
                    .find_map(|e| e.get("message").and_then(|m| m.as_str()))
                    .unwrap_or("unknown");
                return Err(crate::error::AppError::RpcError(format!(
                    "GQL error: {}",
                    msg
                )));
            }
        }

        resp.data
            .and_then(|d| d.market_by_id)
            .ok_or_else(|| crate::error::AppError::RpcError("Market not found".into()))
    }

    /// Query vault state from Morpho GraphQL.
    async fn query_vault(&self, chain: &str, vault_id: &str) -> AppResult<VaultInfo> {
        let cid = chain_id(chain);
        let gql = format!(
            "{{ vaultV2ByAddress(address: \"{vid}\", chainId: {cid}) {{ id name asset {{ decimals }} totalAssetsUsd liquidityUsd netApy }} }}",
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
                crate::error::AppError::RpcError(format!("GQL HTTP request failed: {}", e))
            })?;
        let status = resp_raw.status().as_u16();
        let resp_text = resp_raw.text().await.map_err(|e| {
            crate::error::AppError::RpcError(format!("GQL read body failed: {}", e))
        })?;
        if status < 200 || status >= 300 {
            return Err(crate::error::AppError::RpcError(format!(
                "GQL HTTP {} body={:.200}",
                status, resp_text
            )));
        }
        let resp: GqlResponse<VaultData> = serde_json::from_str(&resp_text).map_err(|e| {
            crate::error::AppError::RpcError(format!(
                "GQL JSON parse: {} body={:.300}",
                e, resp_text
            ))
        })?;

        if let Some(ref errors) = resp.errors {
            if !errors.is_empty() {
                let is_not_found = errors
                    .iter()
                    .any(|e| e.get("status").and_then(|s| s.as_str()) == Some("NOT_FOUND"));
                if is_not_found {
                    return Err(crate::error::AppError::RpcError("Vault not found".into()));
                }
                let msg = errors
                    .iter()
                    .find_map(|e| e.get("message").and_then(|m| m.as_str()))
                    .unwrap_or("unknown");
                return Err(crate::error::AppError::RpcError(format!(
                    "GQL error: {}",
                    msg
                )));
            }
        }

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

        Ok(resp.data.and_then(|d| d.position).unwrap_or(PositionInfo {
            supply_shares: None,
            borrow_shares: None,
            collateral: None,
        }))
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Parse a decimal ETH string (e.g. "0.1") into wei as U256, without going through f64.
fn parse_eth_to_wei(s: &str) -> Option<U256> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let (int_part, frac_part) = match s.split_once('.') {
        Some((i, f)) => (i, f),
        None => (s, ""),
    };
    let integer = U256::from_str(int_part).ok()?;
    let wei_per_eth = U256::from(10).pow(U256::from(18));
    let whole = integer.checked_mul(wei_per_eth)?;
    if frac_part.is_empty() {
        return Some(whole);
    }
    let frac = &frac_part[..frac_part.len().min(18)];
    let frac_padded = format!("{:0<18}", frac);
    let fractional = U256::from_str(&frac_padded).ok()?;
    whole.checked_add(fractional)
}

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
        other => {
            warn!(
                "Unknown chain name '{}', falling back to Ethereum chain ID 1 — GQL query may fail",
                other
            );
            1
        }
    }
}

// ---------------------------------------------------------------------------
// Cache
// ---------------------------------------------------------------------------

async fn cache_market_data(state: &AppState, market_id: &str, mi: &MarketInfo) {
    trace!("MarketInfo: {:?}", mi);
    if let Some(ref st) = mi.state {
        let total = st
            .supply_assets
            .as_deref()
            .and_then(|s| U256::from_str(s).ok())
            .unwrap_or(U256::ZERO);
        let borrow = st
            .borrow_assets
            .as_deref()
            .and_then(|s| U256::from_str(s).ok())
            .unwrap_or(U256::ZERO);
        let liq = total.checked_sub(borrow).unwrap_or(U256::ZERO);
        let apy = st.supply_apy.unwrap_or(0.0);
        let decimals = mi
            .loan_asset
            .as_ref()
            .and_then(|a| a.decimals)
            .unwrap_or(18);
        let mut cache = state.market_cache.write().await;
        cache.insert(
            market_id.to_string(),
            CachedData::Market {
                total: total.to_string(),
                liquidity: liq.to_string(),
                apy: format!("{:.4}", apy),
                decimals,
                updated_at: Utc::now().timestamp(),
            },
        );
    }
}

async fn cache_vault_data(state: &AppState, vault_id: &str, vi: &VaultInfo) {
    let mut cache = state.market_cache.write().await;
    cache.insert(
        vault_id.to_string(),
        CachedData::Vault {
            name: vi.name.clone().unwrap_or_default(),
            deposits: format!("{:.2}", vi.total_assets_usd.unwrap_or(0.0)),
            liquidity: format!("{:.2}", vi.liquidity_usd.unwrap_or(0.0)),
            apy: format!("{:.4}", vi.net_apy.unwrap_or(0.0)),
            decimals: vi.asset.as_ref().and_then(|a| a.decimals).unwrap_or(0),
            updated_at: Utc::now().timestamp(),
        },
    );
}

// ---------------------------------------------------------------------------
// Condition evaluation
// ---------------------------------------------------------------------------

/// Evaluate a U256 metric condition, return triggered reasons.
/// `decimals` scales the user's threshold to match raw token units.
fn evaluate_u256_condition(
    name: &str,
    condition: &MetricCondition,
    current: U256,
    decimals: u32,
) -> Vec<String> {
    let mut reasons = Vec::new();
    if !condition.is_active() {
        return reasons;
    }
    let scale = U256::from(10).pow(U256::from(decimals));
    if let Some(ref upper) = condition.upper {
        if let Ok(limit) = U256::from_str(upper) {
            let scaled = limit.checked_mul(scale).unwrap_or(limit);
            if current > scaled {
                reasons.push(format!("{} 当前值 {} > 上限 {}", name, current, scaled));
            }
        }
    }
    if let Some(ref lower) = condition.lower {
        if let Ok(limit) = U256::from_str(lower) {
            let scaled = limit.checked_mul(scale).unwrap_or(limit);
            if current < scaled {
                reasons.push(format!("{} 当前值 {} < 下限 {}", name, current, scaled));
            }
        }
    }
    reasons
}

/// Evaluate an f64 metric condition, return triggered reasons.
fn evaluate_f64_condition(name: &str, condition: &MetricCondition, current: f64) -> Vec<String> {
    let mut reasons = Vec::new();
    if !condition.is_active() {
        return reasons;
    }
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
    let decimals = market
        .loan_asset
        .as_ref()
        .and_then(|a| a.decimals)
        .unwrap_or(18);
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

    let mut reasons = Vec::new();
    reasons.extend(evaluate_u256_condition(
        "Total Market",
        &cond.total_market,
        total_assets,
        decimals,
    ));
    reasons.extend(evaluate_u256_condition(
        "Liquidity",
        &cond.liquidity,
        liquidity,
        decimals,
    ));
    reasons.extend(evaluate_f64_condition("Supply APY", &cond.supply_apy, apy));
    reasons
}

/// Evaluate vault conditions, return triggered metric details.
fn evaluate_vault_conditions(cond: &ConditionGroup, vault: &VaultInfo) -> Vec<String> {
    let total = vault.total_assets_usd.unwrap_or(0.0);
    let liq = vault.liquidity_usd.unwrap_or(0.0);
    let apy = vault.net_apy.unwrap_or(0.0);

    let mut reasons = Vec::new();
    reasons.extend(evaluate_f64_condition(
        "Total Deposits",
        &cond.total_deposits,
        total,
    ));
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
    use std::collections::HashMap;
    use std::sync::Arc;
    use tokio::sync::RwLock;

    // -----------------------------------------------------------------------
    // Condition evaluation tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_evaluate_u256_condition() {
        let c = MetricCondition {
            enabled: true,
            upper: Some("100".into()),
            lower: Some("10".into()),
        };
        assert!(!evaluate_u256_condition("test", &c, U256::from(200), 0).is_empty());
        assert!(!evaluate_u256_condition("test", &c, U256::from(5), 0).is_empty());
        assert!(evaluate_u256_condition("test", &c, U256::from(50), 0).is_empty());
        let c = MetricCondition {
            enabled: false,
            upper: Some("100".into()),
            lower: None,
        };
        assert!(evaluate_u256_condition("test", &c, U256::from(200), 0).is_empty());
    }

    #[test]
    fn test_evaluate_u256_with_decimals() {
        // User enters "1" meaning 1 token, decimals=6 means raw threshold = 1_000_000
        let c = MetricCondition {
            enabled: true,
            upper: None,
            lower: Some("1".into()),
        };
        // raw value 500_000 < 1_000_000 → triggered
        assert!(!evaluate_u256_condition("test", &c, U256::from(500_000u64), 6).is_empty());
        // raw value 2_000_000 > 1_000_000 → not triggered (lower means "below")
        assert!(evaluate_u256_condition("test", &c, U256::from(2_000_000u64), 6).is_empty());
    }

    #[test]
    fn test_evaluate_f64_condition() {
        let c = MetricCondition {
            enabled: true,
            upper: Some("5.0".into()),
            lower: Some("0.5".into()),
        };
        assert!(!evaluate_f64_condition("test", &c, 10.0).is_empty());
        assert!(!evaluate_f64_condition("test", &c, 0.1).is_empty());
        assert!(evaluate_f64_condition("test", &c, 1.0).is_empty());
        let c = MetricCondition {
            enabled: false,
            upper: Some("5.0".into()),
            lower: None,
        };
        assert!(evaluate_f64_condition("test", &c, 10.0).is_empty());
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
            loan_asset: None,
            state: Some(MarketState {
                supply_assets: Some("2000".into()),
                supply_shares: Some("1000".into()),
                borrow_assets: Some("1500".into()),
                borrow_shares: Some("800".into()),
                supply_apy: Some(0.05),
            }),
        };
        assert!(!evaluate_market_conditions(&cond, &market).is_empty());
    }

    // -----------------------------------------------------------------------
    // Alert state machine tests — recovery logic
    // -----------------------------------------------------------------------

    fn make_alert_manager() -> AlertManager {
        AlertManager::new()
    }

    fn test_key() -> String {
        AlertManager::state_key("ethereum", "0xMarket", "0xuser")
    }

    #[tokio::test]
    async fn test_alert_state_first_trigger_seeds_state() {
        let mgr = make_alert_manager();
        // First risky signal → TriggerAlert, seeds in_alert=true, backoff_level=1
        let d = mgr
            .evaluate_risk("ethereum", "0xMarket", "0xuser", true)
            .await;
        assert_eq!(d, AlertDecision::TriggerAlert);

        let state = mgr.get_state(&test_key()).await;
        assert!(state.in_alert);
        assert_eq!(state.backoff_level, 1);
        assert_eq!(state.normal_streak, 0);
    }

    #[tokio::test]
    async fn test_alert_state_recovery_needs_3_normal_rounds() {
        let mgr = make_alert_manager();
        // Trigger alert
        mgr.evaluate_risk("ethereum", "0xMarket", "0xuser", true)
            .await;

        // Round 1 normal → Suppress, normal_streak=1
        let d = mgr
            .evaluate_risk("ethereum", "0xMarket", "0xuser", false)
            .await;
        assert_eq!(d, AlertDecision::Suppress);
        let state = mgr.get_state(&test_key()).await;
        assert_eq!(state.normal_streak, 1);

        // Round 2 normal → Suppress, normal_streak=2
        let d = mgr
            .evaluate_risk("ethereum", "0xMarket", "0xuser", false)
            .await;
        assert_eq!(d, AlertDecision::Suppress);
        assert_eq!(mgr.get_state(&test_key()).await.normal_streak, 2);

        // Round 3 normal → Recovered!
        let d = mgr
            .evaluate_risk("ethereum", "0xMarket", "0xuser", false)
            .await;
        assert_eq!(d, AlertDecision::Recovered);
        let state = mgr.get_state(&test_key()).await;
        assert!(!state.in_alert);
        assert_eq!(state.normal_streak, 0);
        assert_eq!(state.backoff_level, 0);
    }

    #[tokio::test]
    async fn test_alert_state_flapping_resets_streak() {
        let mgr = make_alert_manager();
        mgr.evaluate_risk("ethereum", "0xMarket", "0xuser", true)
            .await;
        // 2 normal rounds
        mgr.evaluate_risk("ethereum", "0xMarket", "0xuser", false)
            .await;
        mgr.evaluate_risk("ethereum", "0xMarket", "0xuser", false)
            .await;
        assert_eq!(mgr.get_state(&test_key()).await.normal_streak, 2);

        // Risky again → streak resets to 0
        mgr.evaluate_risk("ethereum", "0xMarket", "0xuser", true)
            .await;
        assert_eq!(mgr.get_state(&test_key()).await.normal_streak, 0);
        assert!(mgr.get_state(&test_key()).await.in_alert);
    }

    // -----------------------------------------------------------------------
    // State transition matrix tests
    // -----------------------------------------------------------------------

    fn make_test_order(status: OrderStatus) -> Order {
        Order {
            id: "test-id".into(),
            user_address: "0xuser".into(),
            name: "Test".into(),
            chain: "ethereum".into(),
            order_type: "market".into(),
            market_id: "0xMarket".into(),
            alert_conditions: ConditionGroup::default(),
            liquidation: None,
            status,
            created_at: 1000,
            updated_at: 2000,
        }
    }

    /// Helper: build the expected transition for each (old_status, alert, liquidation) tuple.
    /// Returns the expected new status and whether a notification should be sent.
    fn expected_transition(old: &OrderStatus, alert: bool, liq: bool) -> Option<OrderStatus> {
        match (old, alert, liq) {
            (OrderStatus::Editing, _, _) => Some(OrderStatus::Monitoring),
            (OrderStatus::Monitoring, true, false) => Some(OrderStatus::Alerting),
            (OrderStatus::Monitoring, _, true) => Some(OrderStatus::Liquidating),
            // Monitoring with no triggers → stays Monitoring (None = no change)
            (OrderStatus::Monitoring, false, false) => None,
            // Alerting with no triggers → recovery path (needs 3 rounds, tested separately)
            // Alerting with liquidation → Liquidating
            (OrderStatus::Alerting, _, true) => Some(OrderStatus::Liquidating),
            // Ended, Liquidating → no transitions
            _ => None,
        }
    }

    #[test]
    fn test_transition_editing_to_monitoring() {
        assert_eq!(
            expected_transition(&OrderStatus::Editing, false, false),
            Some(OrderStatus::Monitoring)
        );
        assert_eq!(
            expected_transition(&OrderStatus::Editing, true, false),
            Some(OrderStatus::Monitoring)
        );
        assert_eq!(
            expected_transition(&OrderStatus::Editing, true, true),
            Some(OrderStatus::Monitoring)
        );
    }

    #[test]
    fn test_transition_monitoring_to_alerting() {
        assert_eq!(
            expected_transition(&OrderStatus::Monitoring, true, false),
            Some(OrderStatus::Alerting)
        );
    }

    #[test]
    fn test_transition_monitoring_to_liquidating() {
        assert_eq!(
            expected_transition(&OrderStatus::Monitoring, false, true),
            Some(OrderStatus::Liquidating)
        );
        assert_eq!(
            expected_transition(&OrderStatus::Monitoring, true, true),
            Some(OrderStatus::Liquidating)
        );
    }

    #[test]
    fn test_transition_monitoring_stays() {
        assert_eq!(
            expected_transition(&OrderStatus::Monitoring, false, false),
            None
        );
    }

    #[test]
    fn test_transition_alerting_to_liquidating() {
        assert_eq!(
            expected_transition(&OrderStatus::Alerting, false, true),
            Some(OrderStatus::Liquidating)
        );
        assert_eq!(
            expected_transition(&OrderStatus::Alerting, true, true),
            Some(OrderStatus::Liquidating)
        );
    }

    #[test]
    fn test_transition_ended_no_transition() {
        assert_eq!(expected_transition(&OrderStatus::Ended, true, false), None);
        assert_eq!(expected_transition(&OrderStatus::Ended, true, true), None);
        assert_eq!(expected_transition(&OrderStatus::Ended, false, false), None);
    }

    #[test]
    fn test_transition_liquidating_no_transition() {
        assert_eq!(
            expected_transition(&OrderStatus::Liquidating, true, false),
            None
        );
        assert_eq!(
            expected_transition(&OrderStatus::Liquidating, false, true),
            None
        );
        assert_eq!(
            expected_transition(&OrderStatus::Liquidating, false, false),
            None
        );
    }

    // -----------------------------------------------------------------------
    // Full transition_state integration tests (with AlertManager)
    // -----------------------------------------------------------------------

    fn make_test_app_state() -> AppState {
        // Use a temp data_dir so persist doesn't fail
        std::fs::create_dir_all("data").ok();
        AppState {
            orders: Arc::new(RwLock::new(HashMap::new())),
            whitelist: Arc::new(RwLock::new(HashMap::new())),
            alert_configs: Arc::new(RwLock::new(HashMap::new())),
            monitor_states: Arc::new(RwLock::new(HashMap::new())),
            nonce_store: Arc::new(RwLock::new(HashMap::new())),
            market_cache: Arc::new(RwLock::new(HashMap::new())),
            config: Arc::new(crate::config::AppConfig {
                server: crate::config::ServerConfig {
                    host: "0.0.0.0".into(),
                    port: 16800,
                    data_dir: "data".into(),
                },
                admin: crate::config::AdminConfig {
                    address: "0xAdmin".into(),
                },
                hot_wallet: crate::config::HotWalletConfig {
                    private_key: String::new(),
                    gas_min_balance: "0.1".into(),
                },
                gql_url: "https://api.morpho.org/graphql".into(),
                gql_polling_interval_secs: 12,
                gql_batch_size: 100,
                chains: Default::default(),
                flashbots: None,
            }),
            jwt_secret: "test".into(),
            data_dir: "data".into(),
        }
    }

    #[tokio::test]
    async fn test_alert_state_seeded_on_alert_trigger() {
        let mgr = make_alert_manager();
        // First call to seed: Monitoring → Alerting path
        mgr.evaluate_risk("ethereum", "0xMarket", "0xuser", true)
            .await;

        let alert_state = mgr.get_state(&test_key()).await;
        assert!(
            alert_state.in_alert,
            "Alert state must be seeded on first risky signal"
        );
        assert_eq!(alert_state.backoff_level, 1);
    }

    #[tokio::test]
    async fn test_recovery_after_seeded_alert() {
        let mgr = make_alert_manager();

        // Seed alert
        mgr.evaluate_risk("ethereum", "0xMarket", "0xuser", true)
            .await;
        assert!(mgr.get_state(&test_key()).await.in_alert);

        // 3 normal rounds
        assert_eq!(
            mgr.evaluate_risk("ethereum", "0xMarket", "0xuser", false)
                .await,
            AlertDecision::Suppress
        );
        assert_eq!(
            mgr.evaluate_risk("ethereum", "0xMarket", "0xuser", false)
                .await,
            AlertDecision::Suppress
        );
        assert_eq!(
            mgr.evaluate_risk("ethereum", "0xMarket", "0xuser", false)
                .await,
            AlertDecision::Recovered
        );

        // After recovery, state is reset
        let state = mgr.get_state(&test_key()).await;
        assert!(!state.in_alert);
        assert_eq!(state.backoff_level, 0);
        assert_eq!(state.normal_streak, 0);
    }

    #[tokio::test]
    async fn test_backoff_progression() {
        let mgr = make_alert_manager();
        // First trigger
        mgr.evaluate_risk("ethereum", "0xMarket", "0xuser", true)
            .await;
        assert_eq!(mgr.get_state(&test_key()).await.backoff_level, 1);

        // Second trigger (immediate, within backoff) → suppressed
        let d = mgr
            .evaluate_risk("ethereum", "0xMarket", "0xuser", true)
            .await;
        assert_eq!(d, AlertDecision::Suppress);

        // After backoff elapsed, re-trigger → backoff_level advances
        // We can't easily manipulate time, but the logic is tested in alert.rs tests
    }

    // -----------------------------------------------------------------------
    // Integration tests — transition_state() with real GqlMonitor + AlertManager
    // -----------------------------------------------------------------------

    async fn assert_order_status(state: &AppState, order_id: &str, expected: OrderStatus) {
        let orders = state.orders.read().await;
        let actual = &orders.get(order_id).unwrap().status;
        assert_eq!(
            *actual, expected,
            "order {} expected {:?} but was {:?}",
            order_id, expected, actual
        );
    }

    async fn insert_test_order(state: &AppState, order: Order) {
        state.orders.write().await.insert(order.id.clone(), order);
    }

    #[tokio::test]
    async fn test_integration_editing_to_monitoring() {
        let monitor = GqlMonitor::new("https://test", 60, 100);
        let state = make_test_app_state();
        let alert_mgr = make_alert_manager();
        let order = make_test_order(OrderStatus::Editing);
        insert_test_order(&state, order.clone()).await;

        monitor
            .transition_state(&order, false, &[], false, &[], &state, &alert_mgr)
            .await;
        assert_order_status(&state, "test-id", OrderStatus::Monitoring).await;
    }

    #[tokio::test]
    async fn test_integration_editing_to_monitoring_no_false_recovery() {
        // Bug regression: Editing→Monitoring must NOT trigger recovery notification.
        // We can't easily assert "no notification" here, but the transition must not crash
        // and the status must be Monitoring, not any other state.
        let monitor = GqlMonitor::new("https://test", 60, 100);
        let state = make_test_app_state();
        let alert_mgr = make_alert_manager();
        let order = make_test_order(OrderStatus::Editing);
        insert_test_order(&state, order.clone()).await;

        monitor
            .transition_state(
                &order,
                true,
                &["test".into()],
                false,
                &[],
                &state,
                &alert_mgr,
            )
            .await;
        assert_order_status(&state, "test-id", OrderStatus::Monitoring).await;
        // Should NOT have seeded alert state (no evaluate_risk called)
        let alert_state = alert_mgr.get_state(&test_key()).await;
        assert!(!alert_state.in_alert);
    }

    #[tokio::test]
    async fn test_integration_monitoring_to_alerting_seeds_state() {
        let monitor = GqlMonitor::new("https://test", 60, 100);
        let state = make_test_app_state();
        let alert_mgr = make_alert_manager();
        let order = make_test_order(OrderStatus::Monitoring);
        insert_test_order(&state, order.clone()).await;

        // Precondition: no alert state yet
        assert!(!alert_mgr.get_state(&test_key()).await.in_alert);

        monitor
            .transition_state(
                &order,
                true,
                &["liquidity below".into()],
                false,
                &[],
                &state,
                &alert_mgr,
            )
            .await;
        assert_order_status(&state, "test-id", OrderStatus::Alerting).await;

        // MUST have seeded alert state
        let alert_state = alert_mgr.get_state(&test_key()).await;
        assert!(
            alert_state.in_alert,
            "BUG REGRESSION: Alerting transition must seed alert state for recovery"
        );
        assert_eq!(alert_state.backoff_level, 1);
    }

    #[tokio::test]
    async fn test_integration_monitoring_to_liquidating_seeds_state() {
        let monitor = GqlMonitor::new("https://test", 60, 100);
        let state = make_test_app_state();
        let alert_mgr = make_alert_manager();
        let order = make_test_order(OrderStatus::Monitoring);
        insert_test_order(&state, order.clone()).await;

        monitor
            .transition_state(
                &order,
                false,
                &[],
                true,
                &["liquidity below".into()],
                &state,
                &alert_mgr,
            )
            .await;
        assert_order_status(&state, "test-id", OrderStatus::Liquidating).await;

        // MUST have seeded alert state (for admin awareness)
        let alert_state = alert_mgr.get_state(&test_key()).await;
        assert!(
            alert_state.in_alert,
            "BUG REGRESSION: Liquidating transition must seed alert state"
        );
    }

    #[tokio::test]
    async fn test_integration_alerting_recovery_after_3_normal() {
        let monitor = GqlMonitor::new("https://test", 60, 100);
        let state = make_test_app_state();
        let alert_mgr = make_alert_manager();

        // Seed the alert state as if Monitoring→Alerting already happened
        alert_mgr
            .evaluate_risk("ethereum", "0xMarket", "0xuser", true)
            .await;
        assert!(alert_mgr.get_state(&test_key()).await.in_alert);

        let order = make_test_order(OrderStatus::Alerting);
        insert_test_order(&state, order.clone()).await;

        // Poll 1: normal, alert state normal_streak=1 → suppress
        monitor
            .transition_state(&order, false, &[], false, &[], &state, &alert_mgr)
            .await;
        assert_order_status(&state, "test-id", OrderStatus::Alerting).await; // still Alerting
        assert_eq!(alert_mgr.get_state(&test_key()).await.normal_streak, 1);

        // Poll 2: normal, normal_streak=2 → suppress
        monitor
            .transition_state(&order, false, &[], false, &[], &state, &alert_mgr)
            .await;
        assert_order_status(&state, "test-id", OrderStatus::Alerting).await;
        assert_eq!(alert_mgr.get_state(&test_key()).await.normal_streak, 2);

        // Poll 3: normal, normal_streak=3 → Recovered!
        monitor
            .transition_state(&order, false, &[], false, &[], &state, &alert_mgr)
            .await;
        assert_order_status(&state, "test-id", OrderStatus::Monitoring).await;
        assert!(!alert_mgr.get_state(&test_key()).await.in_alert);
        assert_eq!(alert_mgr.get_state(&test_key()).await.normal_streak, 0);
    }

    #[tokio::test]
    async fn test_integration_restart_unseeded_alert_seeds_and_stays() {
        // After server restart, AlertManager state is lost but orders.json remembers
        // the order was Alerting. First poll with normal conditions should seed the
        // state (so recovery can work) and stay Alerting — NOT recover immediately.
        let monitor = GqlMonitor::new("https://test", 60, 100);
        let state = make_test_app_state();
        let alert_mgr = make_alert_manager();

        // Simulate restart: order is Alerting, alert state is fresh (in_alert=false)
        assert!(!alert_mgr.get_state(&test_key()).await.in_alert);
        let order = make_test_order(OrderStatus::Alerting);
        insert_test_order(&state, order.clone()).await;

        // Poll: conditions normal, state unseeded → seed and stay Alerting
        monitor
            .transition_state(&order, false, &[], false, &[], &state, &alert_mgr)
            .await;
        assert_order_status(&state, "test-id", OrderStatus::Alerting).await;
        assert!(
            alert_mgr.get_state(&test_key()).await.in_alert,
            "Restart: must seed alert state so recovery can work"
        );
        assert_eq!(alert_mgr.get_state(&test_key()).await.backoff_level, 1);
        assert_eq!(alert_mgr.get_state(&test_key()).await.normal_streak, 0);
    }

    #[tokio::test]
    async fn test_integration_restart_full_recovery_after_seed() {
        // After restart + seed, 3 consecutive normal rounds → recovery
        let monitor = GqlMonitor::new("https://test", 60, 100);
        let state = make_test_app_state();
        let alert_mgr = make_alert_manager();
        let order = make_test_order(OrderStatus::Alerting);
        insert_test_order(&state, order.clone()).await;

        // Poll 1: restart seed
        monitor
            .transition_state(&order, false, &[], false, &[], &state, &alert_mgr)
            .await;
        assert_order_status(&state, "test-id", OrderStatus::Alerting).await;
        assert!(alert_mgr.get_state(&test_key()).await.in_alert);

        // Polls 2-4: 3 normal rounds
        monitor
            .transition_state(&order, false, &[], false, &[], &state, &alert_mgr)
            .await;
        assert_eq!(alert_mgr.get_state(&test_key()).await.normal_streak, 1);
        monitor
            .transition_state(&order, false, &[], false, &[], &state, &alert_mgr)
            .await;
        assert_eq!(alert_mgr.get_state(&test_key()).await.normal_streak, 2);
        monitor
            .transition_state(&order, false, &[], false, &[], &state, &alert_mgr)
            .await;
        assert_order_status(&state, "test-id", OrderStatus::Monitoring).await;
        assert!(!alert_mgr.get_state(&test_key()).await.in_alert);
    }

    #[tokio::test]
    async fn test_integration_restart_with_risk_triggers_immediately() {
        // After restart, if conditions are STILL triggered, seed + trigger alert
        let monitor = GqlMonitor::new("https://test", 60, 100);
        let state = make_test_app_state();
        let alert_mgr = make_alert_manager();
        let order = make_test_order(OrderStatus::Alerting);
        insert_test_order(&state, order.clone()).await;

        // (Alerting, true, false) → calls evaluate_risk(true) directly, seeds and stays
        monitor
            .transition_state(
                &order,
                true,
                &["below".into()],
                false,
                &[],
                &state,
                &alert_mgr,
            )
            .await;
        assert_order_status(&state, "test-id", OrderStatus::Alerting).await;
        assert!(
            alert_mgr.get_state(&test_key()).await.in_alert,
            "Re-trigger must also seed state on fresh start"
        );
    }

    #[tokio::test]
    async fn test_integration_alerting_to_liquidating() {
        let monitor = GqlMonitor::new("https://test", 60, 100);
        let state = make_test_app_state();
        let alert_mgr = make_alert_manager();

        // Seed alert state
        alert_mgr
            .evaluate_risk("ethereum", "0xMarket", "0xuser", true)
            .await;

        let order = make_test_order(OrderStatus::Alerting);
        insert_test_order(&state, order.clone()).await;

        monitor
            .transition_state(
                &order,
                false,
                &[],
                true,
                &["liquidity below".into()],
                &state,
                &alert_mgr,
            )
            .await;
        assert_order_status(&state, "test-id", OrderStatus::Liquidating).await;
    }

    #[tokio::test]
    async fn test_integration_alerting_stays_on_interrupted_recovery() {
        // If alert is in recovery (streak=1 or 2) and a risky signal returns,
        // the order stays Alerting — but in our transition_state, only
        // (Alerting, false, false) runs the recovery path; (Alerting, true, false)
        // would be caught by (Alerting, _, true) for liquidation... wait:
        // true,false matches (Alerting, _, true)? No, that's only for liquidation.
        // (Alerting, true, false) hits the _ catch-all and stays None → no change.
        // But we need to test flapping: risk returns during recovery streak.

        let monitor = GqlMonitor::new("https://test", 60, 100);
        let state = make_test_app_state();
        let alert_mgr = make_alert_manager();
        let key = test_key();

        // Seed alert and get 2 normal rounds
        alert_mgr
            .evaluate_risk("ethereum", "0xMarket", "0xuser", true)
            .await;
        alert_mgr
            .evaluate_risk("ethereum", "0xMarket", "0xuser", false)
            .await;
        alert_mgr
            .evaluate_risk("ethereum", "0xMarket", "0xuser", false)
            .await;
        assert_eq!(alert_mgr.get_state(&key).await.normal_streak, 2);

        let order = make_test_order(OrderStatus::Alerting);
        insert_test_order(&state, order.clone()).await;

        // Now risk returns — alert triggered during recovery
        // (Alerting, true, false) → None (no status change), but alert state streak should reset
        alert_mgr
            .evaluate_risk("ethereum", "0xMarket", "0xuser", true)
            .await;
        assert_eq!(
            alert_mgr.get_state(&key).await.normal_streak,
            0,
            "Streak should reset on risky signal"
        );

        // Order stays Alerting (the transition returns None for this case)
        monitor
            .transition_state(
                &order,
                true,
                &["below".into()],
                false,
                &[],
                &state,
                &alert_mgr,
            )
            .await;
        assert_order_status(&state, "test-id", OrderStatus::Alerting).await;
    }

    #[tokio::test]
    async fn test_integration_monitoring_stays_monitoring() {
        let monitor = GqlMonitor::new("https://test", 60, 100);
        let state = make_test_app_state();
        let alert_mgr = make_alert_manager();
        let order = make_test_order(OrderStatus::Monitoring);
        insert_test_order(&state, order.clone()).await;

        monitor
            .transition_state(&order, false, &[], false, &[], &state, &alert_mgr)
            .await;
        assert_order_status(&state, "test-id", OrderStatus::Monitoring).await;
    }

    #[tokio::test]
    async fn test_integration_liquidation_priority_over_alert() {
        // Monitoring with BOTH alert and liquidation triggers → Liquidating
        let monitor = GqlMonitor::new("https://test", 60, 100);
        let state = make_test_app_state();
        let alert_mgr = make_alert_manager();
        let order = make_test_order(OrderStatus::Monitoring);
        insert_test_order(&state, order.clone()).await;

        monitor
            .transition_state(
                &order,
                true,
                &["alert below".into()],
                true,
                &["liq below".into()],
                &state,
                &alert_mgr,
            )
            .await;
        assert_order_status(&state, "test-id", OrderStatus::Liquidating).await;
        assert!(alert_mgr.get_state(&test_key()).await.in_alert);
    }

    #[tokio::test]
    async fn test_integration_ended_no_transition() {
        let monitor = GqlMonitor::new("https://test", 60, 100);
        let state = make_test_app_state();
        let alert_mgr = make_alert_manager();

        for initial in &[OrderStatus::Ended, OrderStatus::Liquidating] {
            let status = initial.clone();
            let mut order = make_test_order(status);
            order.id = format!("test-{:?}", initial);
            insert_test_order(&state, order.clone()).await;

            // Try every trigger combination — nothing should change
            for (alert, liq) in &[(false, false), (true, false), (false, true), (true, true)] {
                monitor
                    .transition_state(&order, *alert, &[], *liq, &[], &state, &alert_mgr)
                    .await;
                assert_order_status(&state, &order.id, initial.clone()).await;
            }
        }
    }

    #[test]
    fn test_gql_monitor_new() {
        let m = GqlMonitor::new("https://api.morpho.org/graphql", 60, 100);
        assert_eq!(m.gql_url, "https://api.morpho.org/graphql");
        assert_eq!(m.polling_interval_secs, 60);
    }
}
