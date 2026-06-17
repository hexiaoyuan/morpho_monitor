use alloy::primitives::{Address, U256};
use alloy::providers::{Provider, ProviderBuilder};
use alloy::rpc::types::Filter;
use alloy::sol_types::SolEvent;
use chrono::Utc;
use std::str::FromStr;
use tracing::{error, info, warn};

use crate::alert::{AlertDecision, AlertManager};
use crate::error::AppResult;
use crate::models::{AppState, MonitorState, Order, OrderStatus, TriggerType};

// ---------------------------------------------------------------------------
// Morpho Blue Event Definitions
// Note: field names with trailing underscore disambiguate indexed vs non-indexed
// parameters that share the same name in the Solidity ABI.
// ---------------------------------------------------------------------------

alloy::sol! {
    /// Emitted when supply collateral changes.
    #[allow(missing_docs)]
    event SupplyCollateral(
        bytes32 indexed id,
        address indexed caller,
        address indexed onBehalf,
        uint256 assets
    );

    /// Emitted when collateral is withdrawn.
    #[allow(missing_docs)]
    event WithdrawCollateral(
        bytes32 indexed id,
        address indexed caller,
        address indexed onBehalf,
        address receiver,
        uint256 assets
    );

    /// Emitted when a borrow is taken.
    #[allow(missing_docs)]
    event Borrow(
        bytes32 indexed id,
        address caller,
        address indexed onBehalf,
        address receiver,
        uint256 assets,
        uint256 shares
    );

    /// Emitted when a borrow is repaid.
    #[allow(missing_docs)]
    event Repay(
        bytes32 indexed id,
        address caller,
        address indexed onBehalf,
        uint256 assets,
        uint256 shares
    );

    /// Emitted when an authorization is set via signature.
    #[allow(missing_docs)]
    event AuthorizationSet(
        address indexed authorizer,
        address indexed authorized,
        bool isAuthorized,
        uint256 nonce,
        uint256 deadline
    );

    /// Emitted when an authorizer's nonce is incremented.
    #[allow(missing_docs)]
    event NonceIncremented(
        bytes32 indexed id,
        address indexed authorizer,
        uint256 newNonce
    );
}

// ---------------------------------------------------------------------------
// Chain Monitor
// ---------------------------------------------------------------------------

/// A monitor for a single chain.
pub struct ChainMonitor {
    pub chain_name: String,
    pub rpc_http: String,
    pub rpc_ws: Option<String>,
    pub polling_interval_secs: u64,
    pub morpho_blue_address: Address,
}

impl ChainMonitor {
    /// Create a new chain monitor.
    pub fn new(
        chain_name: &str,
        rpc_http: &str,
        rpc_ws: Option<&str>,
        polling_interval_secs: u64,
        morpho_blue_address: Address,
    ) -> Self {
        Self {
            chain_name: chain_name.to_string(),
            rpc_http: rpc_http.to_string(),
            rpc_ws: rpc_ws.map(|s| s.to_string()),
            polling_interval_secs,
            morpho_blue_address,
        }
    }

    /// Start the monitoring loop for this chain.
    pub async fn run(&self, state: AppState, alert_manager: AlertManager) {
        info!(
            "Starting monitor for chain '{}' (polling every {}s)",
            self.chain_name, self.polling_interval_secs
        );

        let url = match self.rpc_http.parse() {
            Ok(u) => u,
            Err(e) => {
                error!("Invalid RPC URL for {}: {} — {}", self.chain_name, self.rpc_http, e);
                return;
            }
        };
        let provider = ProviderBuilder::new().connect_http(url);

        let mut interval = tokio::time::interval(
            std::time::Duration::from_secs(self.polling_interval_secs),
        );

        loop {
            interval.tick().await;
            if let Err(e) = self.poll_chain(&provider, &state, &alert_manager).await {
                warn!("Poll error on {}: {}", self.chain_name, e);
            }
        }
    }

    /// Execute one polling cycle: fetch active orders for this chain,
    /// check on-chain state, and evaluate alerts.
    async fn poll_chain(
        &self,
        provider: &impl Provider,
        state: &AppState,
        alert_manager: &AlertManager,
    ) -> AppResult<()> {
        let now = Utc::now().timestamp();

        // Get active orders for this chain
        let active_orders: Vec<Order> = {
            let orders = state.orders.read().await;
            orders
                .values()
                .filter(|o| o.chain == self.chain_name && o.status == OrderStatus::Active)
                .cloned()
                .collect()
        };

        if active_orders.is_empty() {
            return Ok(());
        }

        // For each active order, evaluate risk
        for order in &active_orders {
            // Check if the authorization nonce is still valid
            if let Err(e) = self.check_nonce_validity(provider, order, state, alert_manager)
                .await
            {
                warn!("Nonce check failed for order {}: {}", order.id, e);
                continue;
            }

            // Evaluate position health
            if let Err(e) = self.evaluate_position_health(provider, order, state, alert_manager, now)
                .await
            {
                warn!("Health eval failed for order {}: {}", order.id, e);
                continue;
            }
        }

        Ok(())
    }

    /// Check whether the order's authorization nonce is still valid.
    async fn check_nonce_validity(
        &self,
        provider: &impl Provider,
        order: &Order,
        state: &AppState,
        alert_manager: &AlertManager,
    ) -> AppResult<()> {
        let authorizer = match Address::from_str(&order.user_address) {
            Ok(a) => a,
            Err(_) => {
                warn!("Invalid user address in order {}: {}", order.id, order.user_address);
                return Ok(());
            }
        };

        let filter = Filter::new()
            .address(self.morpho_blue_address)
            .event_signature(NonceIncremented::SIGNATURE_HASH)
            .topic2(authorizer.into_word());

        let logs = provider.get_logs(&filter).await.map_err(|e| {
            crate::error::AppError::RpcError(format!("Failed to fetch NonceIncremented logs: {}", e))
        })?;

        for log in &logs {
            if let Ok(event) = NonceIncremented::decode_log(&log.inner) {
                if event.newNonce > order.authorization.nonce {
                    warn!(
                        "Nonce for {} incremented from {} to {} — invalidating order {}",
                        order.user_address,
                        order.authorization.nonce,
                        event.newNonce,
                        order.id
                    );

                    // Mark order as invalid
                    {
                        let mut orders = state.orders.write().await;
                        if let Some(o) = orders.get_mut(&order.id) {
                            o.status = OrderStatus::Invalid;
                            o.updated_at = Utc::now().timestamp();
                        }
                    }

                    // Reset alert state
                    alert_manager
                        .reset_state(&self.chain_name, &order.market_id, &order.user_address)
                        .await;

                    // Send notification
                    alert_manager.notify_user(state, &order.user_address, &format!(
                        "⚠️ 授权已失效\n订单 {} 因 Nonce 变更自动作废。\n请重新签署授权并创建新订单。",
                        order.id
                    )).await;
                }
            }
        }

        Ok(())
    }

    /// Evaluate the position health for an order and trigger alerts/execution if needed.
    async fn evaluate_position_health(
        &self,
        _provider: &impl Provider,
        order: &Order,
        state: &AppState,
        alert_manager: &AlertManager,
        now: i64,
    ) -> AppResult<()> {
        // In production, this would query Morpho Blue market state and user position.
        // For now, we use the stored monitor state (updated via real RPC elsewhere).

        let state_key =
            AlertManager::state_key(&self.chain_name, &order.market_id, &order.user_address);

        let health_factor = {
            let monitor_states = state.monitor_states.read().await;
            monitor_states
                .get(&state_key)
                .map(|ms| ms.health_factor)
                .unwrap_or_default()
        };

        // Determine if risky
        let threshold = U256::from_str(&order.trigger_threshold).unwrap_or(U256::ZERO);
        let is_risky = match order.trigger_type {
            TriggerType::HealthFactorBelow => {
                health_factor < threshold && health_factor > U256::ZERO
            }
            TriggerType::LltvAbove => health_factor > threshold,
        };

        // Evaluate alert
        let decision = alert_manager
            .evaluate_risk(&self.chain_name, &order.market_id, &order.user_address, is_risky)
            .await;

        match decision {
            AlertDecision::TriggerAlert => {
                info!(
                    "ALERT triggered for order {} (chain={}, market={}, user={})",
                    order.id, self.chain_name, order.market_id, order.user_address
                );

                // Update monitor state
                {
                    let mut states = state.monitor_states.write().await;
                    if let Some(ms) = states.get_mut(&state_key) {
                        ms.last_updated = now;
                    } else {
                        states.insert(
                            state_key.clone(),
                            MonitorState {
                                chain: self.chain_name.clone(),
                                market_id: order.market_id.clone(),
                                user_address: order.user_address.clone(),
                                collateral_amount: U256::ZERO,
                                borrow_amount: U256::ZERO,
                                health_factor,
                                last_updated: now,
                            },
                        );
                    }
                }

                // Send feishu notification
                alert_manager.notify_user(state, &order.user_address, &format!(
                    "🚨 风险预警\n链: {}\n市场: {}\n当前健康因子: {}\n阈值: {}",
                    self.chain_name, order.market_id, health_factor, order.trigger_threshold
                )).await;
                // TODO: Trigger bot executor for this order
            }
            AlertDecision::Recovered => {
                info!("Recovery confirmed for order {} (chain={}, market={}, user={})", order.id, self.chain_name, order.market_id, order.user_address);
                alert_manager.notify_user(state, &order.user_address, &format!(
                    "✅ 风险已解除\n链: {}\n市场: {}",
                    self.chain_name, order.market_id
                )).await;
            }
            AlertDecision::Suppress => {
                // No action needed
            }
        }

        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Multi-chain monitor orchestrator
// ---------------------------------------------------------------------------

/// Morpho Blue addresses per chain (source: https://docs.morpho.org/get-started/resources/addresses/)
fn morpho_address(chain: &str) -> Address {
    match chain {
        "ethereum" => "0xBBBBBbBBb9cC5e90e3b3Af64bdAF62C37EEFFCb",
        "base"     => "0xBBBBBbBBb9cC5e90e3b3Af64bdAF62C37EEFFCb",
        "optimism" => "0xce95AfbB8EA029495c66020883F87aaE8864AF92",
        "arbitrum" => "0x6c247b1F6182318877311737BaC0844bAa518F5e",
        "unichain" => "0x8f5ae9CddB9f68de460C77730b018Ae7E04a140A",
        "hyperevm" => "0x68e37dE8d93d3496ae143F2E900490f6280C57cD",
        _ => "0xBBBBBbBBb9cC5e90e3b3Af64bdAF62C37EEFFCb",
    }
    .parse()
    .unwrap_or(Address::ZERO)
}

/// Spawn monitor tasks for all configured chains.
pub async fn start_monitors(state: AppState, alert_manager: AlertManager) {
    let chains = vec![
        ("ethereum",  state.config.chains.ethereum.as_ref()),
        ("base",      state.config.chains.base.as_ref()),
        ("optimism",  state.config.chains.optimism.as_ref()),
        ("arbitrum",  state.config.chains.arbitrum.as_ref()),
        ("unichain",  state.config.chains.unichain.as_ref()),
        ("hyperevm",  state.config.chains.hyperevm.as_ref()),
    ];

    for (name, chain_config) in chains {
        if let Some(cc) = chain_config {
            // Only start RPC monitor if an HTTP RPC endpoint is configured
            let rpc_http = match &cc.rpc_http {
                Some(url) if !url.is_empty() => url.clone(),
                _ => {
                    info!("Skipping RPC monitor for '{}': no rpc_http configured (GQL fallback covers it)", name);
                    continue;
                }
            };
            let monitor = ChainMonitor::new(
                name,
                &rpc_http,
                cc.rpc_ws.as_deref(),
                cc.polling_interval_secs,
                morpho_address(name),
            );

            let state_clone = state.clone();
            let am_clone = alert_manager.clone();

            tokio::spawn(async move {
                monitor.run(state_clone, am_clone).await;
            });

            info!("Spawned monitor for chain '{}'", name);
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_chain_monitor_creation() {
        let monitor = ChainMonitor::new(
            "ethereum",
            "https://eth.example.com",
            None,
            12,
            Address::ZERO,
        );
        assert_eq!(monitor.chain_name, "ethereum");
        assert_eq!(monitor.rpc_http, "https://eth.example.com");
        assert_eq!(monitor.polling_interval_secs, 12);
        assert!(monitor.rpc_ws.is_none());
    }

    #[test]
    fn test_chain_monitor_with_ws() {
        let monitor = ChainMonitor::new(
            "base",
            "https://base.example.com",
            Some("wss://base.example.com/ws"),
            6,
            Address::ZERO,
        );
        assert_eq!(monitor.rpc_ws, Some("wss://base.example.com/ws".into()));
    }

    #[test]
    fn test_alert_manager_state_key() {
        let key = AlertManager::state_key("ethereum", "0xMarket", "0xUser");
        assert_eq!(key, "ethereum:0xMarket:0xUser");
    }
}
