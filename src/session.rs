use std::collections::HashMap;
use std::net::ToSocketAddrs;
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex, RwLock};
use tracing::{error, info, warn};

use crate::bridge::AudioBridge;
use crate::config::Config;
use crate::mumble::{MumbleClient, MumbleCommand, MumbleEvent, MumbleVoice};
use crate::webrtc::{WebrtcEvent, WebrtcSession};
use crate::ws::messages::*;
use mumble_protocol::crypt::ClientCryptState;
use tokio::sync::oneshot;

/// Represents a connected user's full session
#[allow(dead_code)]
struct UserSession {
    mumble_client: MumbleClient,
    mumble_voice: Option<MumbleVoice>,
    webrtc_session: WebrtcSession,
    bridge: Option<AudioBridge>,
    ws_tx: mpsc::UnboundedSender<ServerMessage>,
    username: String,
    session_id: Option<u32>,
    crypt_state_rx: Option<oneshot::Receiver<ClientCryptState>>,
}

/// Manages all active user sessions
pub struct SessionManager {
    sessions: RwLock<HashMap<String, Arc<Mutex<UserSession>>>>,
    config: Config,
}

impl SessionManager {
    pub fn new(config: Config) -> Self {
        SessionManager {
            sessions: RwLock::new(HashMap::new()),
            config,
        }
    }

    #[allow(dead_code)]
    pub async fn connection_count(&self) -> usize {
        self.sessions.read().await.len()
    }

    /// Connect a new user to Mumble and set up WebRTC
    pub async fn connect_user(
        &self,
        conn_id: &str,
        username: &str,
        ws_tx: mpsc::UnboundedSender<ServerMessage>,
    ) -> Result<(), String> {
        if self.sessions.read().await.len() >= self.config.server.max_connections {
            return Err("Server full".to_string());
        }

        if self.sessions.read().await.contains_key(conn_id) {
            return Err("Already connected".to_string());
        }

        // Resolve Mumble server address
        let addr = self
            .config
            .mumble_addr()
            .to_socket_addrs()
            .map_err(|e| format!("Failed to resolve Mumble server: {}", e))?
            .next()
            .ok_or("Failed to resolve Mumble server address")?;

        // Connect to Mumble
        let mumble_client = MumbleClient::connect(
            addr,
            self.config.mumble.host.clone(),
            username.to_string(),
            self.config.mumble.accept_invalid_certs,
        )
        .await
        .map_err(|e| format!("Mumble connection failed: {}", e))?;

        // Create WebRTC session
        let webrtc_session = WebrtcSession::new(&self.config.webrtc)
            .await
            .map_err(|e| format!("WebRTC setup failed: {}", e))?;

        let session = Arc::new(Mutex::new(UserSession {
            mumble_client,
            mumble_voice: None,
            webrtc_session,
            bridge: None,
            ws_tx: ws_tx.clone(),
            username: username.to_string(),
            session_id: None,
            crypt_state_rx: None, // will be set in process_events
        }));

        self.sessions
            .write()
            .await
            .insert(conn_id.to_string(), session.clone());

        // Spawn event processing loop
        let conn_id_owned = conn_id.to_string();
        let config = self.config.clone();
        tokio::spawn(async move {
            Self::process_events(conn_id_owned, session, config).await;
        });

        info!("User '{}' connecting (conn_id={})", username, conn_id);
        Ok(())
    }

    /// Handle SDP offer from browser
    pub async fn handle_offer(&self, conn_id: &str, sdp: &str) -> Result<String, String> {
        let sessions = self.sessions.read().await;
        let session = sessions
            .get(conn_id)
            .ok_or("Session not found")?;
        let session = session.lock().await;
        session
            .webrtc_session
            .handle_offer(sdp)
            .await
            .map_err(|e| e.to_string())
    }

    /// Add ICE candidate from browser
    pub async fn add_ice_candidate(
        &self,
        conn_id: &str,
        candidate: &str,
        sdp_mid: Option<String>,
        sdp_mline_index: Option<u16>,
    ) -> Result<(), String> {
        let sessions = self.sessions.read().await;
        let session = sessions
            .get(conn_id)
            .ok_or("Session not found")?;
        let session = session.lock().await;
        session
            .webrtc_session
            .add_ice_candidate(candidate, sdp_mid, sdp_mline_index)
            .await
            .map_err(|e| e.to_string())
    }

    /// Send chat message
    pub async fn send_chat(
        &self,
        conn_id: &str,
        channel_id: u32,
        message: &str,
    ) -> Result<(), String> {
        let sessions = self.sessions.read().await;
        let session = sessions
            .get(conn_id)
            .ok_or("Session not found")?;
        let session = session.lock().await;
        session
            .mumble_client
            .send_command(MumbleCommand::SendChat {
                channel_id,
                message: message.to_string(),
            })
            .map_err(|e| e.to_string())
    }

    /// Join channel
    pub async fn join_channel(&self, conn_id: &str, channel_id: u32) -> Result<(), String> {
        let sessions = self.sessions.read().await;
        let session = sessions
            .get(conn_id)
            .ok_or("Session not found")?;
        let session = session.lock().await;
        session
            .mumble_client
            .send_command(MumbleCommand::JoinChannel { channel_id })
            .map_err(|e| e.to_string())
    }

    /// Set mute state
    pub async fn set_mute(&self, conn_id: &str, muted: bool) -> Result<(), String> {
        let sessions = self.sessions.read().await;
        let session = sessions
            .get(conn_id)
            .ok_or("Session not found")?;
        let session = session.lock().await;
        session
            .mumble_client
            .send_command(MumbleCommand::SetMute(muted))
            .map_err(|e| e.to_string())
    }

    /// Set deaf state
    pub async fn set_deaf(&self, conn_id: &str, deafened: bool) -> Result<(), String> {
        let sessions = self.sessions.read().await;
        let session = sessions
            .get(conn_id)
            .ok_or("Session not found")?;
        let session = session.lock().await;
        session
            .mumble_client
            .send_command(MumbleCommand::SetDeaf(deafened))
            .map_err(|e| e.to_string())
    }

    /// Disconnect user
    pub async fn disconnect_user(&self, conn_id: &str) {
        if let Some(session) = self.sessions.write().await.remove(conn_id) {
            let session = session.lock().await;
            let _ = session
                .mumble_client
                .send_command(MumbleCommand::Disconnect);
            if let Some(bridge) = &session.bridge {
                bridge.shutdown().await;
            }
            let _ = session.webrtc_session.close().await;
            info!(
                "User '{}' disconnected (conn_id={})",
                session.username, conn_id
            );
        }
    }

    /// Process Mumble events and WebRTC events for a session
    async fn process_events(
        conn_id: String,
        session: Arc<Mutex<UserSession>>,
        config: Config,
    ) {
        // Take event receivers out of the session to avoid holding the lock
        let (mut mumble_event_rx, mut webrtc_event_rx, ws_tx, crypt_state_rx) = {
            let mut s = session.lock().await;
            let mumble_rx = std::mem::replace(
                &mut s.mumble_client.event_rx,
                mpsc::unbounded_channel().1,
            );
            let webrtc_rx = std::mem::replace(
                &mut s.webrtc_session.event_rx,
                mpsc::unbounded_channel().1,
            );
            let crypt_rx = std::mem::replace(
                &mut s.mumble_client.crypt_state_rx,
                oneshot::channel().1,
            );
            (mumble_rx, webrtc_rx, s.ws_tx.clone(), crypt_rx)
        };

        // Track known users for name lookup in chat
        let mut user_map: HashMap<u32, String> = HashMap::new();

        // Spawn a task to wait for CryptState and start voice
        let session_clone = session.clone();
        let config_clone = config.clone();
        let conn_id_clone = conn_id.clone();
        let ws_tx_clone = ws_tx.clone();
        tokio::spawn(async move {
            match crypt_state_rx.await {
                Ok(crypt_state) => {
                    let addr = config_clone
                        .mumble_addr()
                        .to_socket_addrs()
                        .ok()
                        .and_then(|mut a| a.next());

                    if let Some(addr) = addr {
                        match MumbleVoice::start(addr, crypt_state).await {
                            Ok(voice) => {
                                let mut s = session_clone.lock().await;
                                let webrtc_audio_rx = std::mem::replace(
                                    &mut s.webrtc_session.audio_rx,
                                    mpsc::unbounded_channel().1,
                                );
                                let bridge = AudioBridge::start(
                                    voice.voice_rx,
                                    voice.voice_tx,
                                    webrtc_audio_rx,
                                    s.webrtc_session.outgoing_track.clone(),
                                );
                                s.bridge = Some(bridge);
                                info!("Audio bridge started for {}", conn_id_clone);
                            }
                            Err(e) => {
                                error!("Failed to start voice: {}", e);
                                let _ = ws_tx_clone.send(ServerMessage::error(
                                    "voice_error",
                                    format!("Voice setup failed: {}", e),
                                ));
                            }
                        }
                    }
                }
                Err(_) => {
                    warn!("CryptState channel closed before receiving state");
                }
            }
        });

        loop {
            tokio::select! {
                event = mumble_event_rx.recv() => {
                    match event {
                        Some(MumbleEvent::Connected { session_id, channels, users }) => {
                            for u in &users {
                                user_map.insert(u.session_id, u.name.clone());
                            }
                            {
                                let mut s = session.lock().await;
                                s.session_id = Some(session_id);
                            }
                            let _ = ws_tx.send(ServerMessage::Connected(ConnectedData {
                                session_id,
                                channels,
                                users,
                            }));
                        }
                        Some(MumbleEvent::UserJoined(user)) => {
                            user_map.insert(user.session_id, user.name.clone());
                            let _ = ws_tx.send(ServerMessage::UserJoined(user));
                        }
                        Some(MumbleEvent::UserLeft { session_id }) => {
                            user_map.remove(&session_id);
                            let _ = ws_tx.send(ServerMessage::UserLeft(UserLeftData { session_id }));
                        }
                        Some(MumbleEvent::UserStateChanged(state)) => {
                            if let Some(name) = &state.name {
                                user_map.insert(state.session_id, name.clone());
                            }
                            let _ = ws_tx.send(ServerMessage::UserState(state));
                        }
                        Some(MumbleEvent::ChannelAdded(ch)) | Some(MumbleEvent::ChannelUpdated(ch)) => {
                            let _ = ws_tx.send(ServerMessage::ChannelUpdated(ChannelUpdatedData {
                                channels: vec![ch],
                            }));
                        }
                        Some(MumbleEvent::ChannelRemoved { .. }) => {
                            // Frontend should refetch channel list
                        }
                        Some(MumbleEvent::ChatMessage { sender_session, channel_id, message }) => {
                            let sender_name = user_map
                                .get(&sender_session)
                                .cloned()
                                .unwrap_or_else(|| format!("User#{}", sender_session));
                            let timestamp = std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .unwrap_or_default()
                                .as_secs();
                            let _ = ws_tx.send(ServerMessage::ChatReceived(ChatReceivedData {
                                sender_session,
                                sender_name,
                                channel_id,
                                message,
                                timestamp,
                            }));
                        }
                        Some(MumbleEvent::Disconnected(reason)) => {
                            warn!("Mumble disconnected for {}: {}", conn_id, reason);
                            let _ = ws_tx.send(ServerMessage::error("mumble_disconnected", reason));
                            break;
                        }
                        None => {
                            break;
                        }
                    }
                }
                event = webrtc_event_rx.recv() => {
                    match event {
                        Some(WebrtcEvent::IceCandidate(candidate)) => {
                            let _ = ws_tx.send(ServerMessage::IceCandidate(candidate));
                        }
                        Some(WebrtcEvent::ConnectionStateChanged(state)) => {
                            if state == webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState::Failed
                                || state == webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState::Disconnected
                            {
                                let _ = ws_tx.send(ServerMessage::error(
                                    "webrtc_disconnected",
                                    format!("WebRTC state: {}", state),
                                ));
                            }
                        }
                        None => {
                            break;
                        }
                    }
                }
            }
        }
    }
}
