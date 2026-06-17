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
use std::sync::Arc;
use tokio::sync::RwLock;

pub use error::{AppError, AppResult};
pub use models::AppState;

/// Initialize the application state from an already-loaded config.
pub async fn init_app_state(config: Arc<config::AppConfig>, jwt_secret: &str) -> AppResult<AppState> {
    let data_dir = config.server.data_dir.clone();

    // Load persisted data
    let orders = load_json_map(&format!("{}/orders.json", data_dir)).unwrap_or_default();
    let whitelist = load_json_map(&format!("{}/whitelist.json", data_dir)).unwrap_or_default();
    let alert_configs = load_json_map(&format!("{}/alerts.json", data_dir)).unwrap_or_default();

    Ok(AppState {
        orders: Arc::new(RwLock::new(orders)),
        whitelist: Arc::new(RwLock::new(whitelist)),
        alert_configs: Arc::new(RwLock::new(alert_configs)),
        monitor_states: Arc::new(RwLock::new(HashMap::new())),
        nonce_store: Arc::new(RwLock::new(HashMap::new())),
        config,
        jwt_secret: jwt_secret.to_string(),
        data_dir,
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
