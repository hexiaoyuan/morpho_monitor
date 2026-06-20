use serde::Deserialize;
use std::fs;
use std::path::Path;

use crate::error::{AppError, AppResult};

// ---------------------------------------------------------------------------
// Configuration structs (mapped from config.toml)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
pub struct AppConfig {
    pub server: ServerConfig,
    pub admin: AdminConfig,
    pub hot_wallet: HotWalletConfig,
    #[serde(default = "default_gql_url")]
    pub gql_url: String,
    #[serde(default = "default_gql_polling_interval")]
    pub gql_polling_interval_secs: u64,
    #[serde(default = "default_gql_batch_size")]
    pub gql_batch_size: usize,
    #[serde(default)]
    pub chains: ChainsConfig,
    pub flashbots: Option<FlashbotsConfig>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ServerConfig {
    #[serde(default = "default_host")]
    pub host: String,
    #[serde(default = "default_port")]
    pub port: u16,
    #[serde(default = "default_data_dir")]
    pub data_dir: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AdminConfig {
    #[serde(default)]
    pub address: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct HotWalletConfig {
    #[serde(default)]
    pub private_key: String,
    #[serde(default = "default_gas_min")]
    pub gas_min_balance: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ChainsConfig {
    #[serde(default)]
    pub ethereum: Option<ChainConfig>,
    #[serde(default)]
    pub base: Option<ChainConfig>,
    #[serde(default)]
    pub optimism: Option<ChainConfig>,
    #[serde(default)]
    pub arbitrum: Option<ChainConfig>,
    #[serde(default)]
    pub unichain: Option<ChainConfig>,
    #[serde(default)]
    pub hyperevm: Option<ChainConfig>,
    #[serde(default)]
    pub monad: Option<ChainConfig>,
    #[serde(default)]
    pub katana: Option<ChainConfig>,
    #[serde(default)]
    pub polygon: Option<ChainConfig>,
    #[serde(default)]
    pub stable: Option<ChainConfig>,
    #[serde(default)]
    pub tempo: Option<ChainConfig>,
    #[serde(default)]
    pub worldchain: Option<ChainConfig>,
}

impl ChainsConfig {
    /// Get the RPC HTTP URL for a chain by name.
    pub fn chain_rpc_http(&self, chain: &str) -> Option<String> {
        let cc = match chain {
            "ethereum" => self.ethereum.as_ref(),
            "base" => self.base.as_ref(),
            "optimism" => self.optimism.as_ref(),
            "arbitrum" => self.arbitrum.as_ref(),
            "unichain" => self.unichain.as_ref(),
            "hyperevm" => self.hyperevm.as_ref(),
            "monad" => self.monad.as_ref(),
            "katana" => self.katana.as_ref(),
            "polygon" => self.polygon.as_ref(),
            "stable" => self.stable.as_ref(),
            "tempo" => self.tempo.as_ref(),
            "worldchain" => self.worldchain.as_ref(),
            _ => return None,
        };
        cc.and_then(|c| c.rpc_http.clone()).filter(|u| !u.is_empty())
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct ChainConfig {
    #[serde(default)]
    pub rpc_ws: Option<String>,
    #[serde(default)]
    pub rpc_http: Option<String>,
    #[serde(default = "default_polling_interval")]
    pub polling_interval_secs: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct FlashbotsConfig {
    pub rpc_url: String,
}

// ---------------------------------------------------------------------------
// Default values
// ---------------------------------------------------------------------------

fn default_host() -> String {
    "0.0.0.0".into()
}

fn default_port() -> u16 {
    16800
}

fn default_data_dir() -> String {
    "data".into()
}

fn default_gas_min() -> String {
    "0.1".into()
}

fn default_polling_interval() -> u64 {
    12
}

fn default_gql_polling_interval() -> u64 {
    12
}

fn default_gql_batch_size() -> usize {
    100
}

fn default_gql_url() -> String {
    "https://api.morpho.org/graphql".into()
}

// ---------------------------------------------------------------------------
// Config loading
// ---------------------------------------------------------------------------

impl AppConfig {
    /// Load configuration: config.toml (optional) + environment variable overrides.
    /// Without any config file, only `MORPHO_ADMIN_ADDRESS` env var is required.
    pub fn load(path: &Path) -> AppResult<Self> {
        // Try config.toml, fall back to defaults if missing
        let mut config: AppConfig = match fs::read_to_string(path) {
            Ok(content) => toml::from_str(&content).map_err(|e| {
                AppError::Config(format!("Failed to parse config file: {}", e))
            })?,
            Err(_) => AppConfig::default(),
        };

        // Environment variable overrides
        if let Ok(v) = std::env::var("MORPHO_ADMIN_ADDRESS") { config.admin.address = v; }
        if let Ok(v) = std::env::var("MORPHO_HOT_WALLET_KEY") { config.hot_wallet.private_key = v; }
        if let Ok(v) = std::env::var("MORPHO_GQL_URL") { config.gql_url = v; }
        if let Ok(v) = std::env::var("MORPHO_DATA_DIR") { config.server.data_dir = v; }
        if let Ok(v) = std::env::var("MORPHO_SERVER_PORT") {
            if let Ok(p) = v.parse() { config.server.port = p; }
        }

        // RPC env var overrides
        macro_rules! env_override {
            ($config:expr, $chain:ident, $env_http:expr, $env_ws:expr) => {
                if let Some(ref mut c) = $config.chains.$chain {
                    if let Ok(url) = std::env::var($env_ws) { c.rpc_ws = Some(url); }
                    if let Ok(url) = std::env::var($env_http) { c.rpc_http = Some(url); }
                }
            };
            ($config:expr, $chain:ident, $env_http:expr) => {
                if let Some(ref mut c) = $config.chains.$chain {
                    if let Ok(url) = std::env::var($env_http) { c.rpc_http = Some(url); }
                }
            };
        }
        env_override!(config, ethereum, "RPC_ETH_HTTP", "RPC_ETH_WS");
        env_override!(config, base, "RPC_BASE_HTTP", "RPC_BASE_WS");
        env_override!(config, optimism, "RPC_OPTIMISM_HTTP", "RPC_OPTIMISM_WS");
        env_override!(config, arbitrum, "RPC_ARBITRUM_HTTP", "RPC_ARBITRUM_WS");
        env_override!(config, unichain, "RPC_UNICHAIN_HTTP", "RPC_UNICHAIN_WS");
        env_override!(config, hyperevm, "RPC_HYPEREVM_HTTP");
        env_override!(config, monad, "RPC_MONAD_HTTP");
        env_override!(config, katana, "RPC_KATANA_HTTP");
        env_override!(config, polygon, "RPC_POLYGON_HTTP");
        env_override!(config, stable, "RPC_STABLE_HTTP");
        env_override!(config, tempo, "RPC_TEMPO_HTTP");
        env_override!(config, worldchain, "RPC_WORLDCHAIN_HTTP");

        // Validate minimum requirements
        if config.admin.address.is_empty() {
            return Err(AppError::Config(
                "Admin address is required. Set MORPHO_ADMIN_ADDRESS env var or [admin] in config.toml".into()
            ));
        }

        Ok(config)
    }
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            server: ServerConfig { host: default_host(), port: default_port(), data_dir: default_data_dir() },
            admin: AdminConfig { address: String::new() },
            hot_wallet: HotWalletConfig { private_key: String::new(), gas_min_balance: default_gas_min() },
            gql_url: default_gql_url(),
            gql_polling_interval_secs: default_gql_polling_interval(),
            gql_batch_size: default_gql_batch_size(),
            chains: ChainsConfig::default(),
            flashbots: None,
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
    fn test_load_config_minimal() {
        let config_str = r#"
[server]
host = "127.0.0.1"
port = 8080

[admin]
address = "0xAdmin00000000000000000000000000000000000000"

[hot_wallet]
private_key = "0xdeadbeef"

[chains.ethereum]
rpc_http = "https://eth.example.com"
"#;
        let config: AppConfig = toml::from_str(config_str).unwrap();
        assert_eq!(config.server.port, 8080);
        assert_eq!(config.admin.address, "0xAdmin00000000000000000000000000000000000000");
        assert_eq!(config.hot_wallet.private_key, "0xdeadbeef");
        assert_eq!(config.chains.ethereum.as_ref().unwrap().rpc_http.as_deref(), Some("https://eth.example.com"));
    }

    #[test]
    fn test_default_values() {
        let config_str = r#"
[server]
[admin]
address = "0xAdmin00000000000000000000000000000000000000"
[hot_wallet]
private_key = "0xdeadbeef"

[chains.ethereum]
rpc_http = "https://eth.example.com"
"#;
        let config: AppConfig = toml::from_str(config_str).unwrap();
        assert_eq!(config.server.host, "0.0.0.0");
        assert_eq!(config.server.port, 16800);
        assert_eq!(config.hot_wallet.gas_min_balance, "0.1");
        assert_eq!(
            config.chains.ethereum.as_ref().unwrap().polling_interval_secs,
            12
        );
    }

    #[test]
    fn test_load_config_file() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        let config_content = r#"
[server]
host = "0.0.0.0"
port = 3000

[admin]
address = "0xAdmin00000000000000000000000000000000000000"

[hot_wallet]
private_key = "0xabcdef"

[chains.ethereum]
rpc_http = "https://eth.example.com"
"#;
        std::fs::write(&config_path, config_content).unwrap();
        let config = AppConfig::load(&config_path).unwrap();
        assert_eq!(config.server.port, 3000);
        assert_eq!(config.admin.address, "0xAdmin00000000000000000000000000000000000000");
    }

    #[test]
    fn test_load_missing_file_falls_back_to_defaults() {
        // When config.toml is missing, load() uses defaults + env vars.
        // But admin address is empty by default → should fail validation.
        let result = AppConfig::load(Path::new("/nonexistent/config.toml"));
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Admin address is required"));
    }

    #[test]
    fn test_load_invalid_toml() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("bad.toml");
        std::fs::write(&config_path, "this is not valid {{{ toml").unwrap();
        let result = AppConfig::load(&config_path);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Failed to parse"));
    }
}
