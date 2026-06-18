use alloy::primitives::{Address, U256};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

// ---------------------------------------------------------------------------
// EIP-712 Authorization
// ---------------------------------------------------------------------------

/// Represents an EIP-712 Authorization as expected by Morpho Blue's
/// `setAuthorizationWithSignature`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Authorization {
    /// The user's cold wallet address that signs the authorization.
    pub authorizer: Address,
    /// The server's hot wallet address that is granted permission.
    pub authorized: Address,
    /// Must be `true` to grant authorization.
    pub is_authorized: bool,
    /// The authorizer's current nonce in Morpho Blue.
    pub nonce: U256,
    /// Deadline after which the authorization is no longer valid (unix timestamp).
    pub deadline: U256,
}

impl Authorization {
    /// The EIP-712 type string.
    pub const TYPE_STR: &'static str =
        "Authorization(address authorizer,address authorized,bool isAuthorized,uint256 nonce,uint256 deadline)";
}

// ---------------------------------------------------------------------------
// Order Status
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum OrderStatus {
    /// Being created / edited by the user
    Editing,
    /// Actively monitoring
    Monitoring,
    /// Alert threshold triggered
    Alerting,
    /// Force liquidation triggered
    Liquidating,
    /// Completed or cancelled
    Ended,
}

// ---------------------------------------------------------------------------
// Metric Conditions
// ---------------------------------------------------------------------------

/// A single metric condition with optional upper/lower bounds.
/// When `enabled` is true and a bound is set, exceeding that bound
/// triggers the condition.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct MetricCondition {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upper: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lower: Option<String>,
}

impl MetricCondition {
    /// True when this metric is enabled and has at least one bound.
    pub fn is_active(&self) -> bool {
        self.enabled && (self.upper.is_some() || self.lower.is_some())
    }
}

/// Condition groups for market and vault metrics.
/// The order_type determines which subset is evaluated.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct ConditionGroup {
    // --- Market metrics ---
    #[serde(default)]
    pub total_market: MetricCondition,
    #[serde(default)]
    pub liquidity: MetricCondition,
    #[serde(default)]
    pub supply_apy: MetricCondition,

    // --- Vault metrics ---
    #[serde(default)]
    pub total_deposits: MetricCondition,
    #[serde(default)]
    pub net_apy: MetricCondition,
}

impl ConditionGroup {
    /// True if any individual metric is active.
    pub fn has_active_conditions(&self) -> bool {
        self.total_market.is_active()
            || self.liquidity.is_active()
            || self.supply_apy.is_active()
            || self.total_deposits.is_active()
            || self.net_apy.is_active()
    }
}

// ---------------------------------------------------------------------------
// Liquidation Config
// ---------------------------------------------------------------------------

/// Liquidation configuration — only present when the user enables
/// force liquidation. Contains the EIP-712 authorization needed
/// for the hot wallet to withdraw on the user's behalf.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LiquidationConfig {
    pub conditions: ConditionGroup,
    pub authorization: Authorization,
    pub signature: String,
}

// ---------------------------------------------------------------------------
// Order
// ---------------------------------------------------------------------------

/// A conditional order placed by a user. Supports alert monitoring
/// and optional force liquidation with EIP-712 authorization.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Order {
    /// Unique order ID (UUID v4)
    pub id: String,
    /// User's cold wallet address (lowercase)
    pub user_address: String,
    /// Human-readable display name
    pub name: String,
    /// Chain identifier (e.g. "ethereum", "base")
    pub chain: String,
    /// "market" or "vault"
    pub order_type: String,
    /// Market / vault ID
    pub market_id: String,
    /// Alert condition group (at least one metric must be active)
    pub alert_conditions: ConditionGroup,
    /// Optional liquidation config with its own conditions + EIP-712 sig
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub liquidation: Option<LiquidationConfig>,
    /// Current status
    pub status: OrderStatus,
    /// Unix timestamp when the order was created
    pub created_at: i64,
    /// Unix timestamp of last update
    pub updated_at: i64,
}

// ---------------------------------------------------------------------------
// Whitelist
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WhitelistEntry {
    /// Wallet address (lowercase hex)
    pub address: String,
    /// Human-readable nickname
    pub nickname: String,
    /// Unix timestamp when added
    pub added_at: i64,
}

// ---------------------------------------------------------------------------
// Feishu / Alert Configuration
// ---------------------------------------------------------------------------

/// Per-user Feishu notification configuration stored in alerts.json.
/// Each user brings their own Feishu app credentials.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AlertConfig {
    /// User's wallet address (lowercase)
    pub user_address: String,
    /// Human-readable nickname (from whitelist)
    pub nickname: String,
    /// Feishu app ID
    pub app_id: String,
    /// Feishu app secret
    pub app_secret: String,
    /// Feishu user open_id (ou_xxx) — the notification target
    pub user_openid: String,
    /// Unix timestamp of last update
    pub updated_at: i64,
}

// ---------------------------------------------------------------------------
// Monitor State Machine
// ---------------------------------------------------------------------------

/// Real-time monitoring state for one position/market.
/// Alert/backoff logic is handled by AlertState in the alert module.
#[derive(Debug, Clone)]
pub struct MonitorState {
    /// Chain identifier
    pub chain: String,
    /// Market ID
    pub market_id: String,
    /// User address being monitored
    pub user_address: String,
    /// Current collateral amount (in token decimals)
    pub collateral_amount: U256,
    /// Current borrow amount (in token decimals)
    pub borrow_amount: U256,
    /// Current health factor (scaled by 1e18)
    pub health_factor: U256,
    /// Last update timestamp
    pub last_updated: i64,
}

// ---------------------------------------------------------------------------
// Shared Application State
// ---------------------------------------------------------------------------

/// Thread-safe application state shared across all handlers and background tasks.
#[derive(Clone)]
pub struct AppState {
    /// In-memory order store: order_id -> Order
    pub orders: std::sync::Arc<tokio::sync::RwLock<HashMap<String, Order>>>,
    /// In-memory whitelist: address -> WhitelistEntry
    pub whitelist: std::sync::Arc<tokio::sync::RwLock<HashMap<String, WhitelistEntry>>>,
    /// In-memory alert configs: user_address -> AlertConfig
    pub alert_configs: std::sync::Arc<tokio::sync::RwLock<HashMap<String, AlertConfig>>>,
    /// Monitor states: "chain:market:user" -> MonitorState
    pub monitor_states: std::sync::Arc<tokio::sync::RwLock<HashMap<String, MonitorState>>>,
    /// Application config
    pub config: std::sync::Arc<AppConfig>,
    /// JWT encoding key
    pub jwt_secret: String,
    /// SIWE nonce store
    pub nonce_store: std::sync::Arc<tokio::sync::RwLock<HashMap<String, (String, i64)>>>,
    /// Data directory path
    pub data_dir: String,
}

// Re-export AppConfig from config
pub use crate::config::AppConfig;

// ---------------------------------------------------------------------------
// API Request / Response types
// ---------------------------------------------------------------------------

/// POST /api/auth/login request body
#[derive(Debug, Deserialize)]
pub struct LoginRequest {
    pub message: String,
    pub signature: String,
}

/// POST /api/auth/login response
#[derive(Debug, Serialize)]
pub struct LoginResponse {
    pub token: String,
    pub address: String,
    pub role: String,
}

/// POST /api/orders request body
#[derive(Debug, Deserialize)]
pub struct CreateOrderRequest {
    pub chain: String,
    pub name: String,
    pub order_type: String,
    pub market_id: String,
    pub alert_conditions: ConditionGroup,
    #[serde(default)]
    pub liquidation_conditions: Option<ConditionGroup>,
    #[serde(default)]
    pub authorization: Option<Authorization>,
    #[serde(default)]
    pub signature: Option<String>,
}

/// POST /api/admin/whitelist request body
#[derive(Debug, Deserialize)]
pub struct WhitelistRequest {
    pub address: String,
    pub nickname: String,
}

/// PUT /api/alerts request body
#[derive(Debug, Deserialize)]
pub struct AlertConfigRequest {
    pub nickname: String,
    pub app_id: String,
    pub app_secret: String,
    pub user_openid: String,
}

/// POST /api/alerts/test request body
#[derive(Debug, Deserialize)]
pub struct AlertTestRequest {
    pub text: String,
}

/// Standard API response wrapper
#[derive(Debug, Serialize)]
pub struct ApiResponse<T: Serialize> {
    pub success: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<T>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl<T: Serialize> ApiResponse<T> {
    pub fn ok(data: T) -> Self {
        Self {
            success: true,
            data: Some(data),
            error: None,
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
    fn test_authorization_type_str() {
        assert_eq!(
            Authorization::TYPE_STR,
            "Authorization(address authorizer,address authorized,bool isAuthorized,uint256 nonce,uint256 deadline)"
        );
    }

    #[test]
    fn test_monitor_state_creation() {
        let state = MonitorState {
            chain: "ethereum".into(),
            market_id: "test".into(),
            user_address: "0x0".into(),
            collateral_amount: U256::ZERO,
            borrow_amount: U256::ZERO,
            health_factor: U256::from(100),
            last_updated: 1234567890,
        };
        assert_eq!(state.chain, "ethereum");
        assert_eq!(state.health_factor, U256::from(100));
        assert_eq!(state.last_updated, 1234567890);
    }

    #[test]
    fn test_metric_condition_is_active() {
        let c = MetricCondition { enabled: true, upper: Some("100".into()), lower: None };
        assert!(c.is_active());

        let c = MetricCondition { enabled: true, upper: None, lower: Some("50".into()) };
        assert!(c.is_active());

        let c = MetricCondition { enabled: false, upper: Some("100".into()), lower: Some("50".into()) };
        assert!(!c.is_active());

        let c = MetricCondition { enabled: true, upper: None, lower: None };
        assert!(!c.is_active());
    }

    #[test]
    fn test_condition_group_has_active() {
        let mut cg = ConditionGroup::default();
        assert!(!cg.has_active_conditions());

        cg.liquidity = MetricCondition { enabled: true, upper: None, lower: Some("1.0".into()) };
        assert!(cg.has_active_conditions());
    }

    #[test]
    fn test_order_serialization() {
        let order = Order {
            id: "test-id".into(),
            user_address: "0x123".into(),
            name: "Test Order".into(),
            chain: "ethereum".into(),
            order_type: "market".into(),
            market_id: "0xabc".into(),
            alert_conditions: ConditionGroup {
                liquidity: MetricCondition { enabled: true, upper: None, lower: Some("1.0".into()) },
                ..Default::default()
            },
            liquidation: None,
            status: OrderStatus::Monitoring,
            created_at: 1000,
            updated_at: 1000,
        };

        let json = serde_json::to_string(&order).unwrap();
        let parsed: Order = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.id, "test-id");
        assert_eq!(parsed.status, OrderStatus::Monitoring);
        assert_eq!(parsed.order_type, "market");
    }

    #[test]
    fn test_order_serialization_with_liquidation() {
        let order = Order {
            id: "liq-id".into(),
            user_address: "0x456".into(),
            name: "Liq Order".into(),
            chain: "base".into(),
            order_type: "market".into(),
            market_id: "0xdef".into(),
            alert_conditions: ConditionGroup::default(),
            liquidation: Some(LiquidationConfig {
                conditions: ConditionGroup {
                    liquidity: MetricCondition { enabled: true, upper: Some("2.0".into()), lower: None },
                    ..Default::default()
                },
                authorization: Authorization {
                    authorizer: Address::ZERO,
                    authorized: Address::ZERO,
                    is_authorized: true,
                    nonce: U256::ZERO,
                    deadline: U256::ZERO,
                },
                signature: "0xdead".into(),
            }),
            status: OrderStatus::Monitoring,
            created_at: 2000,
            updated_at: 2000,
        };

        let json = serde_json::to_string(&order).unwrap();
        let parsed: Order = serde_json::from_str(&json).unwrap();
        assert!(parsed.liquidation.is_some());
        assert_eq!(parsed.liquidation.unwrap().signature, "0xdead");
    }

}
