use std::sync::Arc;

use axum::extract::ws::WebSocketUpgrade;
use axum::extract::State;
use axum::response::Response;
use axum::routing::get;
use axum::Router;
use axum::{
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    Json,
};
use tower_http::cors::CorsLayer;
use tracing::info;

use crate::config::Config;
use crate::session::SessionManager;
use crate::turn::TurnService;
use crate::ws::handler::handle_ws_connection;
use tokio::sync::Semaphore;

#[derive(Clone)]
#[allow(dead_code)]
pub struct AppState {
    pub session_manager: Arc<SessionManager>,
    pub config: Config,
    pub websocket_slots: Arc<Semaphore>,
}

async fn ws_upgrade(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Response {
    if !state.config.server.allowed_origins.is_empty() {
        if let Some(origin) = headers.get("origin") {
            if !state
                .config
                .server
                .allowed_origins
                .iter()
                .any(|allowed| origin == allowed.as_str())
            {
                return StatusCode::FORBIDDEN.into_response();
            }
        }
    }
    let Ok(slot) = state.websocket_slots.clone().try_acquire_owned() else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    ws.max_message_size(128 * 1024)
        .max_frame_size(128 * 1024)
        .on_upgrade(move |socket| async move {
            let _slot = slot;
            handle_ws_connection(socket, state.session_manager).await;
        })
}

async fn health_check() -> &'static str {
    "ok"
}

pub fn create_router(config: Config, session_manager: Arc<SessionManager>) -> Router {
    let state = AppState {
        session_manager,
        websocket_slots: Arc::new(Semaphore::new(config.server.max_connections)),
        config,
    };

    Router::new()
        .route("/ws", get(ws_upgrade))
        .route("/health", get(health_check))
        .route("/ready", get(readiness))
        .layer(CorsLayer::permissive())
        .with_state(state)
}

async fn readiness(State(state): State<AppState>) -> Response {
    let reachable = matches!(
        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            tokio::net::TcpStream::connect(state.config.mumble_addr())
        )
        .await,
        Ok(Ok(_))
    );
    let status = if reachable {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    (status, Json(serde_json::json!({"upstream_tcp_reachable": reachable, "sessions": state.session_manager.connection_count().await,
        "builtin_turn": state.config.turn.enabled, "media_udp_port": state.config.webrtc.udp_port}))).into_response()
}

pub async fn run(config: Config) -> anyhow::Result<()> {
    let listen_addr = config.listen_addr();
    let listener = tokio::net::TcpListener::bind(&listen_addr).await?;
    let turn = if config.turn.enabled {
        Some(TurnService::start(&config).await?)
    } else {
        None
    };
    let manager = Arc::new(SessionManager::new(config.clone()).with_turn(turn.clone()));
    if let Err(error) = manager.initialize_media().await {
        if let Some(turn) = &turn {
            turn.close().await;
        }
        return Err(error);
    }
    let router = create_router(config, manager.clone());

    info!("Starting server on {}", listen_addr);
    let shutdown_manager = manager.clone();
    let result = axum::serve(listener, router)
        .with_graceful_shutdown(async move {
            let mut terminate =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                    .expect("SIGTERM handler");
            tokio::select! { _ = tokio::signal::ctrl_c() => {}, _ = terminate.recv() => {} }
            shutdown_manager.begin_shutdown();
        })
        .await;
    manager.close().await;
    if let Some(turn) = turn {
        turn.close().await;
    }
    result?;
    Ok(())
}
