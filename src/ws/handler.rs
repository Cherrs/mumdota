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
    let mut send_task = tokio::spawn(async move {
        let mut heartbeat = tokio::time::interval(std::time::Duration::from_secs(15));
        loop {
            let (message, terminal) = tokio::select! {
                _ = heartbeat.tick() => (Message::Ping(Default::default()), false),
                msg = ws_rx.recv() => {
                    let Some(msg) = msg else { break; };
                    let terminal = matches!(&msg, ServerMessage::Error(e) if e.code == "mumble_disconnected" || e.code == "connect_failed");
                    (Message::Text(msg.to_json().into()), terminal)
                }
            };
            if !matches!(
                tokio::time::timeout(std::time::Duration::from_secs(5), ws_sender.send(message))
                    .await,
                Ok(Ok(()))
            ) {
                break;
            }
            if terminal || ws_rx.len() > 256 {
                let _ = tokio::time::timeout(
                    std::time::Duration::from_secs(1),
                    ws_sender.send(Message::Close(None)),
                )
                .await;
                break;
            }
        }
    });

    // Process incoming WebSocket messages
    let mut first_message = true;
    let first_deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let deadline = if first_message {
            first_deadline
        } else {
            tokio::time::Instant::now() + std::time::Duration::from_secs(45)
        };
        let result = tokio::select! {
            _ = &mut send_task => break,
            _ = session_manager.stopping() => break,
            result = tokio::time::timeout_at(deadline, ws_receiver.next()) => result,
        };
        let Ok(Some(msg)) = result else {
            break;
        };
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
                        if first_message && !matches!(&client_msg, ClientMessage::Connect(_)) {
                            break;
                        }
                        first_message = false;
                        if matches!(&client_msg, ClientMessage::Disconnect) {
                            break;
                        }
                        if tokio::time::timeout(
                            std::time::Duration::from_secs(15),
                            handle_client_message(&conn_id, client_msg, &session_manager, &ws_tx),
                        )
                        .await
                        .is_err()
                        {
                            break;
                        }
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
    session_manager.revoke_turn(&conn_id).await;
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
            if data.username.trim().is_empty() || data.username.len() > 128 {
                let _ = ws_tx.send(ServerMessage::error("connect_failed", "invalid username"));
                return;
            }
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
            debug!(sdp_bytes = data.sdp.len(), "Legacy client requires upgrade");
            let _ = ws_tx.send(ServerMessage::error("upgrade_required", "MumDota protocol v2 requires start_voice and server SDP offers; refresh the web client"));
        }
        ClientMessage::Answer(data) => {
            if let Err(e) = session_manager.handle_answer(conn_id, &data.sdp).await {
                let _ = ws_tx.send(ServerMessage::error("voice_error", e));
            }
        }
        ClientMessage::StartVoice => {
            if let Err(e) = session_manager.start_voice(conn_id, false).await {
                let _ = ws_tx.send(ServerMessage::error("voice_error", e));
            }
        }
        ClientMessage::IceRestart => {
            if let Err(e) = session_manager.start_voice(conn_id, true).await {
                let _ = ws_tx.send(ServerMessage::error("voice_error", e));
            }
        }
        ClientMessage::IceRefresh => {
            if let Err(e) = session_manager.refresh_ice(conn_id).await {
                let _ = ws_tx.send(ServerMessage::error("voice_error", e));
            }
        }
        ClientMessage::IceCandidate(data) => {
            if let Err(e) = session_manager
                .add_ice_candidate(conn_id, &data.candidate, data.sdp_mid, data.sdp_mline_index)
                .await
            {
                let _ = ws_tx.send(ServerMessage::error("ice_failed", e));
            }
        }
        ClientMessage::ChatSend(data) => {
            if data.message.len() > 4096 {
                let _ = ws_tx.send(ServerMessage::error(
                    "chat_failed",
                    "message exceeds 4096 bytes",
                ));
                return;
            }
            if let Err(e) = session_manager
                .send_chat(conn_id, data.channel_id, &data.message)
                .await
            {
                let _ = ws_tx.send(ServerMessage::error("chat_failed", e));
            }
        }
        ClientMessage::ChannelJoin(data) => {
            if let Err(e) = session_manager.join_channel(conn_id, data.channel_id).await {
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
