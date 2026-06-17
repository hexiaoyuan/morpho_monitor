use std::net::SocketAddr;
use std::path::PathBuf;
use tower_http::cors::{Any, CorsLayer};
use tower_http::services::ServeDir;
use tracing::info;

use morpho_monitor::alert::AlertManager;
use morpho_monitor::api;
use morpho_monitor::gql_monitor::GqlMonitor;
use morpho_monitor::monitor;

#[tokio::main]
async fn main() {
    // Load .env file (ignore if missing)
    let _ = dotenvy::dotenv();

    // Initialize tracing
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "morpho_monitor=info".into()),
        )
        .init();

    // Load configuration
    let config_path = std::env::var("MORPHO_CONFIG")
        .unwrap_or_else(|_| "config.toml".to_string());

    // JWT secret — env var takes priority, otherwise persisted to file
    let jwt_secret = std::env::var("MORPHO_JWT_SECRET").unwrap_or_else(|_| {
        let path = "data/jwt_secret";
        std::fs::read_to_string(path)
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| {
                let secret = uuid::Uuid::new_v4().to_string();
                std::fs::write(path, &secret).ok();
                secret
            })
    });

    // Ensure data directory exists
    std::fs::create_dir_all("data").ok();

    let state = morpho_monitor::init_app_state(
        &PathBuf::from(&config_path),
        &jwt_secret,
    )
    .await
    .expect("Failed to initialize application state — check config.toml");

    let server_config = state.config.server.clone();
    let alert_manager = AlertManager::new();

    // CORS — allow known frontend origins
    let cors = CorsLayer::new()
        .allow_origin([
            "https://hexiaoyuan.github.io".parse().unwrap(),
            "http://localhost:16800".parse().unwrap(),
            "http://127.0.0.1:16800".parse().unwrap(),
        ])
        .allow_methods(Any)
        .allow_headers(Any);

    // Build the API router
    let api_router = api::build_router(state.clone());

    // Merge API routes with static file serving
    let app = api_router
        .layer(cors)
        .fallback_service(ServeDir::new("static"));

    // Start GQL monitor (always on — zero-config fallback)
    let gql_monitor = GqlMonitor::new(&state.config.gql_url, 60);
    tokio::spawn({
        let s = state.clone();
        let am = alert_manager.clone();
        async move { gql_monitor.run(s, am).await }
    });

    // Start RPC monitors in the background (only for chains with rpc_http configured)
    monitor::start_monitors(state.clone(), alert_manager.clone()).await;

    // Start the HTTP server
    let addr: SocketAddr = format!("{}:{}", server_config.host, server_config.port)
        .parse()
        .expect("Invalid server address — check config.toml [server] host/port");

    info!("morpho_monitor starting on {}", addr);
    info!("Frontend: http://{}", addr);

    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .expect("Failed to bind TCP listener — port may be in use");

    axum::serve(listener, app)
        .await
        .expect("Server error");
}
