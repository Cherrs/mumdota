use std::sync::Arc;

use axum::extract::ws::{Message, WebSocket};
use futures::{SinkExt, StreamExt};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use crate::session::SessionManager;
use crate::ws::messages::{ClientMessage, ServerMessage};

/// Handle a single WebSocket connection
pub async fn handle_ws_connection(socket: WebSocket, session_manager: Arc<SessionManager>) {
    let conn_id = uuid::Uuid::new_v4().to_string();
    info!("New WebSocket connection: {}", conn_id);

    let (mut ws_sender, mut ws_receiver) = socket.split();

    // Channel for sending messages back to the WebSocket
    let (ws_tx, mut ws_rx) = mpsc::unbounded_channel::<ServerMessage>();

    // Spawn task to forward server messages to WebSocket
    let send_task = tokio::spawn(async move {
        while let Some(msg) = ws_rx.recv().await {
            let json = msg.to_json();
            if ws_sender
                .send(Message::Text(json.into()))
                .await
                .is_err()
            {
                break;
            }
        }
    });

    // Process incoming WebSocket messages
    while let Some(msg) = ws_receiver.next().await {
        let msg = match msg {
            Ok(msg) => msg,
            Err(e) => {
                warn!("WebSocket error for {}: {}", conn_id, e);
                break;
            }
        };

        match msg {
            Message::Text(text) => {
                let text_str: &str = &text;
                match serde_json::from_str::<ClientMessage>(text_str) {
                    Ok(client_msg) => {
                        handle_client_message(
                            &conn_id,
                            client_msg,
                            &session_manager,
                            &ws_tx,
                        )
                        .await;
                    }
                    Err(e) => {
                        warn!("Invalid message from {}: {}", conn_id, e);
                        let _ = ws_tx.send(ServerMessage::error(
                            "invalid_message",
                            format!("Failed to parse message: {}", e),
                        ));
                    }
                }
            }
            Message::Close(_) => {
                info!("WebSocket closed by client: {}", conn_id);
                break;
            }
            Message::Ping(_data) => {
                // axum handles pong automatically
                debug!("Ping from {}", conn_id);
            }
            _ => {}
        }
    }

    // Clean up on disconnect
    session_manager.disconnect_user(&conn_id).await;
    send_task.abort();
    info!("WebSocket connection ended: {}", conn_id);
}

async fn handle_client_message(
    conn_id: &str,
    msg: ClientMessage,
    session_manager: &Arc<SessionManager>,
    ws_tx: &mpsc::UnboundedSender<ServerMessage>,
) {
    match msg {
        ClientMessage::Connect(data) => {
            match session_manager
                .connect_user(conn_id, &data.username, ws_tx.clone())
                .await
            {
                Ok(()) => {
                    debug!("User '{}' connect initiated", data.username);
                }
                Err(e) => {
                    let _ = ws_tx.send(ServerMessage::error("connect_failed", e));
                }
            }
        }
        ClientMessage::Disconnect => {
            session_manager.disconnect_user(conn_id).await;
        }
        ClientMessage::Offer(data) => {
            match session_manager.handle_offer(conn_id, &data.sdp).await {
                Ok(answer_sdp) => {
                    let _ = ws_tx.send(ServerMessage::Answer(
                        crate::ws::messages::AnswerData { sdp: answer_sdp },
                    ));
                }
                Err(e) => {
                    let _ = ws_tx.send(ServerMessage::error("offer_failed", e));
                }
            }
        }
        ClientMessage::IceCandidate(data) => {
            if let Err(e) = session_manager
                .add_ice_candidate(
                    conn_id,
                    &data.candidate,
                    data.sdp_mid,
                    data.sdp_mline_index,
                )
                .await
            {
                let _ = ws_tx.send(ServerMessage::error("ice_failed", e));
            }
        }
        ClientMessage::ChatSend(data) => {
            if let Err(e) = session_manager
                .send_chat(conn_id, data.channel_id, &data.message)
                .await
            {
                let _ = ws_tx.send(ServerMessage::error("chat_failed", e));
            }
        }
        ClientMessage::ChannelJoin(data) => {
            if let Err(e) = session_manager
                .join_channel(conn_id, data.channel_id)
                .await
            {
                let _ = ws_tx.send(ServerMessage::error("channel_join_failed", e));
            }
        }
        ClientMessage::Mute(data) => {
            if let Err(e) = session_manager.set_mute(conn_id, data.muted).await {
                let _ = ws_tx.send(ServerMessage::error("mute_failed", e));
            }
        }
        ClientMessage::Deafen(data) => {
            if let Err(e) = session_manager.set_deaf(conn_id, data.deafened).await {
                let _ = ws_tx.send(ServerMessage::error("deafen_failed", e));
            }
        }
    }
}
