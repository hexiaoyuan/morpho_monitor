pub mod alert;
pub mod api;
pub mod auth;
pub mod config;
pub mod error;
pub mod executor;
pub mod gql_monitor;
pub mod models;
pub mod monitor;

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use tokio::sync::RwLock;

pub use error::{AppError, AppResult};
pub use models::AppState;

/// Initialize the application state from a config file and data directory.
pub async fn init_app_state(config_path: &Path, jwt_secret: &str) -> AppResult<AppState> {
    let config = Arc::new(config::AppConfig::load(config_path)?);

    // Load persisted data
    let orders = load_json_map("data/orders.json").unwrap_or_default();
    let whitelist = load_json_map("data/whitelist.json").unwrap_or_default();
    let alert_configs = load_json_map("data/alerts.json").unwrap_or_default();

    Ok(AppState {
        orders: Arc::new(RwLock::new(orders)),
        whitelist: Arc::new(RwLock::new(whitelist)),
        alert_configs: Arc::new(RwLock::new(alert_configs)),
        monitor_states: Arc::new(RwLock::new(HashMap::new())),
        nonce_store: Arc::new(RwLock::new(HashMap::new())),
        config,
        jwt_secret: jwt_secret.to_string(),
    })
}

fn load_json_map<T: serde::de::DeserializeOwned>(
    path: &str,
) -> Option<HashMap<String, T>> {
    let content = std::fs::read_to_string(path).ok()?;
    if content.trim().is_empty() {
        return Some(HashMap::new());
    }
    serde_json::from_str(&content).ok()
}
