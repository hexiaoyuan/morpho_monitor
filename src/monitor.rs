use alloy::primitives::Address;
use alloy::providers::{Provider, ProviderBuilder};
use alloy::rpc::types::Filter;
use alloy::sol_types::SolEvent;
use chrono::Utc;
use std::str::FromStr;
use tracing::{error, info, warn};

use crate::alert::AlertManager;
use crate::error::AppResult;
use crate::models::{AppState, Order, OrderStatus};

// ---------------------------------------------------------------------------
// Morpho Blue Event Definitions
// ---------------------------------------------------------------------------

alloy::sol! {
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

    /// Execute one polling cycle: check nonce validity for active orders on this chain.
    /// Condition evaluation is handled by the GQL monitor.
    async fn poll_chain(
        &self,
        provider: &impl Provider,
        state: &AppState,
        alert_manager: &AlertManager,
    ) -> AppResult<()> {
        // Get monitoring / alerting orders for this chain
        let active_orders: Vec<Order> = {
            let orders = state.orders.read().await;
            orders
                .values()
                .filter(|o| {
                    o.chain == self.chain_name
                        && matches!(o.status, OrderStatus::Monitoring | OrderStatus::Alerting)
                })
                .cloned()
                .collect()
        };

        if active_orders.is_empty() {
            return Ok(());
        }

        for order in &active_orders {
            // Check if the authorization nonce is still valid
            if let Err(e) = self
                .check_nonce_validity(provider, order, state, alert_manager)
                .await
            {
                warn!("Nonce check failed for order {}: {}", order.id, e);
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
        // Only check if order has liquidation config with an authorization
        let auth_nonce = match order.liquidation.as_ref() {
            Some(lc) => lc.authorization.nonce,
            None => return Ok(()),
        };

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
                if event.newNonce > auth_nonce {
                    warn!(
                        "Nonce for {} incremented from {} to {} — invalidating order {}",
                        order.user_address, auth_nonce, event.newNonce, order.id
                    );

                    // Mark order ended
                    {
                        let mut orders = state.orders.write().await;
                        if let Some(o) = orders.get_mut(&order.id) {
                            o.status = OrderStatus::Ended;
                            o.updated_at = Utc::now().timestamp();
                        }
                    }
                    let _ = crate::api::orders::persist_orders(state).await;

                    // Reset alert state
                    alert_manager
                        .reset_state(&self.chain_name, &order.market_id, &order.user_address)
                        .await;

                    // Send notification
                    alert_manager
                        .notify_user(state, &order.user_address, &format!(
                            "⚠️ 授权已失效\n订单 {} 因 Nonce 变更自动作废。\n请重新签署授权并创建新订单。",
                            order.id
                        ))
                        .await;
                }
            }
        }

        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Multi-chain monitor orchestrator
// ---------------------------------------------------------------------------

/// Morpho Blue addresses per chain (source: https://docs.morpho.org/get-started/resources/addresses/)
pub fn morpho_address(chain: &str) -> Address {
    match chain {
        "ethereum" => "0xBBBBBbBBb9cC5e90e3b3Af64bdAF62C37EEFFCb",
        "base" => "0xBBBBBbBBb9cC5e90e3b3Af64bdAF62C37EEFFCb",
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
        ("ethereum", state.config.chains.ethereum.as_ref()),
        ("base", state.config.chains.base.as_ref()),
        ("optimism", state.config.chains.optimism.as_ref()),
        ("arbitrum", state.config.chains.arbitrum.as_ref()),
        ("unichain", state.config.chains.unichain.as_ref()),
        ("hyperevm", state.config.chains.hyperevm.as_ref()),
    ];

    for (name, chain_config) in chains {
        if let Some(cc) = chain_config {
            let rpc_http = match &cc.rpc_http {
                Some(url) if !url.is_empty() => url.clone(),
                _ => {
                    info!(
                        "Skipping RPC monitor for '{}': no rpc_http configured (GQL fallback covers it)",
                        name
                    );
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
        let monitor = ChainMonitor::new("ethereum", "https://eth.example.com", None, 12, Address::ZERO);
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
