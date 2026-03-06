use std::sync::Arc;

use axum::extract::ws::WebSocketUpgrade;
use axum::extract::State;
use axum::response::Response;
use axum::routing::get;
use axum::Router;
use tower_http::cors::CorsLayer;
use tracing::info;

use crate::config::Config;
use crate::session::SessionManager;
use crate::ws::handler::handle_ws_connection;

#[derive(Clone)]
#[allow(dead_code)]
pub struct AppState {
    pub session_manager: Arc<SessionManager>,
    pub config: Config,
}

async fn ws_upgrade(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
) -> Response {
    ws.on_upgrade(move |socket| handle_ws_connection(socket, state.session_manager))
}

async fn health_check() -> &'static str {
    "ok"
}

pub fn create_router(config: Config) -> Router {
    let session_manager = Arc::new(SessionManager::new(config.clone()));

    let state = AppState {
        session_manager,
        config,
    };

    Router::new()
        .route("/ws", get(ws_upgrade))
        .route("/health", get(health_check))
        .layer(CorsLayer::permissive())
        .with_state(state)
}

pub async fn run(config: Config) -> anyhow::Result<()> {
    let listen_addr = config.listen_addr();
    let router = create_router(config);

    info!("Starting server on {}", listen_addr);
    let listener = tokio::net::TcpListener::bind(&listen_addr).await?;
    axum::serve(listener, router).await?;
    Ok(())
}
