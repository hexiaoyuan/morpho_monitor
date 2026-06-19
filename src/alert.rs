use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

use crate::error::{AppError, AppResult};

// ---------------------------------------------------------------------------
// Alert State Machine (per monitored position)
// ---------------------------------------------------------------------------

/// Tracks the alert debounce state for one (chain, market, user) tuple.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AlertState {
    /// Whether this position is currently in an abnormal (risk) condition.
    pub in_alert: bool,
    /// Exponential backoff level: 0 = instant, 1 = 1min, 2 = 2min, 3 = 4min, ...
    pub backoff_level: u32,
    /// Unix timestamp of the last alert sent.
    pub last_alert_at: i64,
    /// Consecutive normal rounds counted (for recovery confirmation).
    pub normal_streak: u32,
}

impl Default for AlertState {
    fn default() -> Self {
        Self {
            in_alert: false,
            backoff_level: 0,
            last_alert_at: 0,
            normal_streak: 0,
        }
    }
}

/// Result of evaluating whether an alert should be sent.
#[derive(Debug, PartialEq, Eq)]
pub enum AlertDecision {
    /// Send an alert now (first trigger or backoff interval elapsed).
    TriggerAlert,
    /// Send a recovery notification (normal for 3+ consecutive rounds).
    Recovered,
    /// Do nothing (within backoff cooldown or still accumulating normal_streak).
    Suppress,
}

impl AlertState {
    pub const MAX_BACKOFF_LEVEL: u32 = 7;
    pub const MAX_BACKOFF_MINUTES: u32 = 64;
    pub const RECOVERY_STREAK_REQUIRED: u32 = 3;

    /// Get the current backoff interval in minutes.
    pub fn backoff_minutes(&self) -> u32 {
        if self.backoff_level == 0 {
            return 0;
        }
        // Use checked shift to avoid overflow with high backoff levels
        let shift = (self.backoff_level - 1).min(31);
        let minutes = 1u32 << shift;
        minutes.min(Self::MAX_BACKOFF_MINUTES)
    }

    /// Check whether enough time has passed to send another alert.
    pub fn can_send_alert(&self, now_ts: i64) -> bool {
        let minutes = self.backoff_minutes();
        if minutes == 0 {
            return true;
        }
        let elapsed_secs = now_ts - self.last_alert_at;
        elapsed_secs >= (minutes as i64) * 60
    }

    /// Process a new risk evaluation and return the decision.
    ///
    /// - `is_risky`: true if the current health factor / LLTV exceeds the threshold.
    /// - `now_ts`: current unix timestamp.
    pub fn evaluate(&mut self, is_risky: bool, now_ts: i64) -> AlertDecision {
        if is_risky {
            // Enter/remain in abnormal branch
            self.normal_streak = 0;

            if !self.in_alert {
                // First trigger → instant alert + execute
                self.in_alert = true;
                self.backoff_level = 1;
                self.last_alert_at = now_ts;
                return AlertDecision::TriggerAlert;
            }

            // Already in alert — check backoff
            if self.can_send_alert(now_ts) {
                self.last_alert_at = now_ts;
                if self.backoff_level < Self::MAX_BACKOFF_LEVEL {
                    self.backoff_level += 1;
                }
                return AlertDecision::TriggerAlert;
            }

            return AlertDecision::Suppress;
        } else {
            // Normal state
            if self.in_alert {
                self.normal_streak += 1;
                if self.normal_streak >= Self::RECOVERY_STREAK_REQUIRED {
                    // Confirmed recovery
                    self.in_alert = false;
                    self.backoff_level = 0;
                    self.normal_streak = 0;
                    return AlertDecision::Recovered;
                }
                // Still accumulating streak
                return AlertDecision::Suppress;
            }
            // Already normal → nothing to do
            return AlertDecision::Suppress;
        }
    }

    /// Reset state completely (e.g. on order cancellation).
    pub fn reset(&mut self) {
        *self = Self::default();
    }
}

// ---------------------------------------------------------------------------
// AlertManager — manages alert states and notification dispatch
// ---------------------------------------------------------------------------

/// High-level alert manager holding per-position states and feishu integration.
#[derive(Clone)]
pub struct AlertManager {
    /// Per-key alert state: key format = "chain:market:user"
    states: Arc<RwLock<HashMap<String, AlertState>>>,
    /// Feishu token cache keyed by app_id
    feishu_tokens: Arc<RwLock<HashMap<String, (String, i64)>>>,
    /// HTTP client
    http_client: reqwest::Client,
}

impl AlertManager {
    pub fn new() -> Self {
        Self {
            states: Arc::new(RwLock::new(HashMap::new())),
            feishu_tokens: Arc::new(RwLock::new(HashMap::new())),
            http_client: reqwest::Client::new(),
        }
    }

    pub fn state_key(chain: &str, market_id: &str, user_address: &str) -> String {
        format!("{}:{}:{}", chain, market_id, user_address)
    }

    /// Get the current alert state for a position.
    pub async fn get_state(&self, key: &str) -> AlertState {
        let states = self.states.read().await;
        states.get(key).cloned().unwrap_or_default()
    }

    /// Evaluate risk for a position and decide what action to take.
    pub async fn evaluate_risk(
        &self,
        chain: &str,
        market_id: &str,
        user_address: &str,
        is_risky: bool,
    ) -> AlertDecision {
        let key = Self::state_key(chain, market_id, user_address);
        let mut states = self.states.write().await;
        let state = states.entry(key).or_default();
        let now = Utc::now().timestamp();
        state.evaluate(is_risky, now)
    }

    /// Reset alert state for a position (e.g. when an order is cancelled).
    pub async fn reset_state(&self, chain: &str, market_id: &str, user_address: &str) {
        let key = Self::state_key(chain, market_id, user_address);
        let mut states = self.states.write().await;
        if let Some(state) = states.get_mut(&key) {
            state.reset();
        }
    }

    /// Fetch or refresh Feishu tenant access token (cached per app_id).
    pub async fn get_feishu_token(&self, app_id: &str, app_secret: &str) -> AppResult<String> {
        {
            let cache = self.feishu_tokens.read().await;
            if let Some((token, expires_at)) = cache.get(app_id) {
                if Utc::now().timestamp() < expires_at - 300 {
                    return Ok(token.clone());
                }
            }
        }
        let resp: FeishuTokenResponse = self
            .http_client
            .post("https://open.feishu.cn/open-apis/auth/v3/tenant_access_token/internal")
            .json(&serde_json::json!({"app_id": app_id, "app_secret": app_secret}))
            .send().await.map_err(|e| AppError::Notification(format!("Feishu token request: {}", e)))?
            .json().await.map_err(|e| AppError::Notification(format!("Feishu token parse: {}", e)))?;

        if resp.code != 0 {
            return Err(AppError::Notification(format!("Feishu token error: code={}", resp.code)));
        }
        let token = resp.tenant_access_token.clone();
        let expires_at = Utc::now().timestamp() + resp.expire as i64;
        self.feishu_tokens.write().await.insert(app_id.to_string(), (token.clone(), expires_at));
        Ok(token)
    }

    /// Send a text message via Feishu using per-user AlertConfig.
    pub async fn send_to_user(&self, cfg: &crate::models::AlertConfig, content: &str) -> AppResult<()> {
        let token = self.get_feishu_token(&cfg.app_id, &cfg.app_secret).await?;
        let resp: FeishuApiResponse = self
            .http_client
            .post("https://open.feishu.cn/open-apis/im/v1/messages?receive_id_type=open_id")
            .header("Authorization", format!("Bearer {}", token))
            .json(&serde_json::json!({
                "receive_id": cfg.user_openid,
                "msg_type": "text",
                "content": serde_json::to_string(&serde_json::json!({"text": content})).unwrap_or_default(),
            }))
            .send().await.map_err(|e| AppError::Notification(format!("Feishu send: {}", e)))?
            .json().await.map_err(|e| AppError::Notification(format!("Feishu response: {}", e)))?;
        if resp.code != 0 {
            return Err(AppError::Notification(format!("Feishu send error: code={}", resp.code)));
        }
        Ok(())
    }

    /// Try to notify the admin. Reads admin AlertConfig, sends if configured.
    pub async fn notify_admin(&self, state: &crate::models::AppState, content: &str) {
        let admin = state.config.admin.address.to_lowercase();
        if admin.is_empty() { return; }
        self.notify_user(state, &admin, content).await;
    }

    /// Try to send to a user by their address. Reads AlertConfig, sends if configured, logs otherwise.
    pub async fn notify_user(&self, state: &crate::models::AppState, user_address: &str, content: &str) {
        let first_line = content.lines().next().unwrap_or("");
        tracing::info!("Feishu notify user={} msg=\"{}\"", user_address, first_line);
        let cfg = {
            let configs = state.alert_configs.read().await;
            configs.get(user_address).cloned()
        };
        match cfg {
            Some(c) if !c.app_id.is_empty() && !c.user_openid.is_empty() => {
                let label = format!("{} [{}]", c.nickname, user_address);
                let full = format!("{}\n👤 {}\n{}", content, label, Utc::now().format("%Y-%m-%d %H:%M:%S UTC"));
                if let Err(e) = self.send_to_user(&c, &full).await {
                    tracing::warn!("Feishu send failed for {}: {}", label, e);
                } else {
                    tracing::info!("Feishu sent to {} ({})", c.nickname, user_address);
                }
            }
            _ => {
                tracing::info!("No feishu config for {} — skipped notification", &user_address);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Feishu API response types
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct FeishuTokenResponse {
    code: i32,
    #[allow(unused)]
    msg: String,
    #[serde(default)]
    tenant_access_token: String,
    #[serde(default)]
    expire: i32,
}

#[derive(Debug, Deserialize)]
struct FeishuApiResponse {
    code: i32,
    #[allow(unused)]
    msg: String,
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_alert_state_first_trigger() {
        let mut state = AlertState::default();
        let decision = state.evaluate(true, 1000);
        assert_eq!(decision, AlertDecision::TriggerAlert);
        assert!(state.in_alert);
        assert_eq!(state.backoff_level, 1);
        assert_eq!(state.last_alert_at, 1000);
    }

    #[test]
    fn test_alert_state_backoff_suppress() {
        let mut state = AlertState {
            in_alert: true,
            backoff_level: 1,
            last_alert_at: 1000,
            normal_streak: 0,
        };

        // 30 seconds later — within 1-minute backoff
        let decision = state.evaluate(true, 1030);
        assert_eq!(decision, AlertDecision::Suppress);
        assert_eq!(state.backoff_level, 1); // unchanged

        // 65 seconds later — past 1-minute backoff
        let decision = state.evaluate(true, 1065);
        assert_eq!(decision, AlertDecision::TriggerAlert);
        assert_eq!(state.backoff_level, 2); // advanced
        assert_eq!(state.last_alert_at, 1065);
    }

    #[test]
    fn test_alert_state_recovery() {
        let mut state = AlertState {
            in_alert: true,
            backoff_level: 3,
            last_alert_at: 500,
            normal_streak: 0,
        };

        // 1st normal round
        let d = state.evaluate(false, 600);
        assert_eq!(d, AlertDecision::Suppress);
        assert_eq!(state.normal_streak, 1);

        // 2nd normal round
        let d = state.evaluate(false, 700);
        assert_eq!(d, AlertDecision::Suppress);
        assert_eq!(state.normal_streak, 2);

        // 3rd normal round → recovery!
        let d = state.evaluate(false, 800);
        assert_eq!(d, AlertDecision::Recovered);
        assert!(!state.in_alert);
        assert_eq!(state.backoff_level, 0);
        assert_eq!(state.normal_streak, 0);
    }

    #[test]
    fn test_alert_state_flapping() {
        let mut state = AlertState {
            in_alert: true,
            backoff_level: 2,
            last_alert_at: 100,
            normal_streak: 2, // almost recovered
        };

        // Risky again → streak resets
        let d = state.evaluate(true, 200);
        assert_eq!(d, AlertDecision::Suppress); // within backoff
        assert_eq!(state.normal_streak, 0);
        assert!(state.in_alert);
    }

    #[test]
    fn test_alert_state_already_normal() {
        let mut state = AlertState::default();
        // Already normal, still normal
        let d = state.evaluate(false, 100);
        assert_eq!(d, AlertDecision::Suppress);
    }

    #[test]
    fn test_backoff_minutes_table() {
        let test_cases = vec![
            (0, 0),
            (1, 1),
            (2, 2),
            (3, 4),
            (4, 8),
            (5, 16),
            (6, 32),
            (7, 64),
            (8, 64),  // capped
            (99, 64), // capped
        ];
        for (level, expected) in test_cases {
            let state = AlertState {
                backoff_level: level,
                ..Default::default()
            };
            assert_eq!(state.backoff_minutes(), expected, "level {}", level);
        }
    }

    #[test]
    fn test_state_key_format() {
        let key = AlertManager::state_key("ethereum", "0xMarket1", "0xUserA");
        assert_eq!(key, "ethereum:0xMarket1:0xUserA");
    }

    #[tokio::test]
    async fn test_alert_manager_evaluate_risk() {
        let mgr = AlertManager::new();

        // First trigger
        let d = mgr.evaluate_risk("eth", "m1", "u1", true).await;
        assert_eq!(d, AlertDecision::TriggerAlert);

        // Second within backoff → suppress
        let d = mgr.evaluate_risk("eth", "m1", "u1", true).await;
        assert_eq!(d, AlertDecision::Suppress);
    }

    #[tokio::test]
    async fn test_alert_manager_reset() {
        let mgr = AlertManager::new();
        mgr.evaluate_risk("eth", "m1", "u1", true).await;

        mgr.reset_state("eth", "m1", "u1").await;
        let state = mgr.get_state("eth:m1:u1").await;
        assert!(!state.in_alert);
        assert_eq!(state.backoff_level, 0);
    }
}
