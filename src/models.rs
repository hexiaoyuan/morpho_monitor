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
// Order (Conditional Order)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum OrderStatus {
    Active,
    Triggered,
    Executed,
    Failed,
    Invalid,
    Cancelled,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum TriggerType {
    /// Trigger when health factor drops below threshold
    HealthFactorBelow,
    /// Trigger when LLTV reaches a certain percentage
    LltvAbove,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ActionType {
    /// Withdraw collateral to user's wallet
    WithdrawCollateral,
    /// Repay borrow on behalf of user
    RepayBorrow,
    /// Close position entirely
    ClosePosition,
}

/// A conditional order placed by a user. Triggers when chain state
/// meets the specified conditions.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Order {
    /// Unique order ID (UUID v4)
    pub id: String,
    /// User's cold wallet address (lowercase)
    pub user_address: String,
    /// Chain identifier (e.g. "ethereum", "base", "hyperevm")
    pub chain: String,
    /// Morpho market ID being monitored
    pub market_id: String,
    /// Type of trigger condition
    pub trigger_type: TriggerType,
    /// Threshold value (e.g. health factor 1.05 = 5% above liquidation)
    pub trigger_threshold: String,
    /// Action to take
    pub action: ActionType,
    /// EIP-712 Authorization struct (for Morpho permission)
    pub authorization: Authorization,
    /// The user's EIP-712 signature over the Authorization
    pub signature: String,
    /// Current order status
    pub status: OrderStatus,
    /// Unix timestamp when the order was created
    pub created_at: i64,
    /// Unix timestamp of last update
    pub updated_at: i64,
    /// Optional: Feishu user open_id or webhook for notifications
    pub feishu_target: Option<String>,
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
    pub market_id: String,
    pub trigger_type: TriggerType,
    pub trigger_threshold: String,
    pub action: ActionType,
    pub authorization: Authorization,
    pub signature: String,
    pub feishu_target: Option<String>,
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
    fn test_order_serialization() {
        let order = Order {
            id: "test-id".into(),
            user_address: "0x123".into(),
            chain: "ethereum".into(),
            market_id: "0xabc".into(),
            trigger_type: TriggerType::HealthFactorBelow,
            trigger_threshold: "1.05".into(),
            action: ActionType::ClosePosition,
            authorization: Authorization {
                authorizer: Address::ZERO,
                authorized: Address::ZERO,
                is_authorized: true,
                nonce: U256::ZERO,
                deadline: U256::ZERO,
            },
            signature: "0xdead".into(),
            status: OrderStatus::Active,
            created_at: 1000,
            updated_at: 1000,
            feishu_target: None,
        };

        let json = serde_json::to_string(&order).unwrap();
        let parsed: Order = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.id, "test-id");
        assert_eq!(parsed.status, OrderStatus::Active);
    }
}
