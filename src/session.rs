use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex, Notify, OwnedSemaphorePermit, RwLock, Semaphore};
use tokio::task::JoinHandle;
use tracing::{error, info, warn};

use crate::bridge::AudioBridge;
use crate::config::Config;
use crate::mumble::{MumbleClient, MumbleCommand, MumbleEvent, MumbleVoice};
use crate::webrtc::{WebrtcEvent, WebrtcSession};
use crate::ws::messages::*;
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
    voice_setup_task: Option<JoinHandle<()>>,
    slot: Option<OwnedSemaphorePermit>,
    closed: bool,
    close_notify: Arc<Notify>,
}

fn abort_background_task(task: &mut Option<JoinHandle<()>>) {
    if let Some(task) = task.take() {
        task.abort();
    }
}

async fn shutdown_user_session(session: &mut UserSession) {
    if session.slot.is_none() {
        return;
    }
    session.closed = true;
    session.close_notify.notify_one();
    let _ = session
        .mumble_client
        .send_command(MumbleCommand::Disconnect);
    abort_background_task(&mut session.voice_setup_task);

    if let Some(bridge) = session.bridge.take() {
        bridge.shutdown().await;
    }

    if let Some(mut voice) = session.mumble_voice.take() {
        voice.shutdown();
    }

    if let Err(err) = session.webrtc_session.close().await {
        warn!(
            "Failed to close WebRTC session for '{}': {}",
            session.username, err
        );
    }
    session.slot.take();
}

/// Manages all active user sessions
type Sessions = RwLock<HashMap<String, Arc<Mutex<UserSession>>>>;

pub struct SessionManager {
    sessions: Arc<Sessions>,
    slots: Arc<Semaphore>,
    config: Config,
}

impl SessionManager {
    pub fn new(config: Config) -> Self {
        SessionManager {
            sessions: Arc::new(RwLock::new(HashMap::new())),
            slots: Arc::new(Semaphore::new(config.server.max_connections)),
            config,
        }
    }

    #[allow(dead_code)]
    pub async fn connection_count(&self) -> usize {
        self.sessions.read().await.len()
    }

    async fn resolve_mumble_addr(&self) -> Result<SocketAddr, String> {
        tokio::net::lookup_host(self.config.mumble_addr())
            .await
            .map_err(|e| format!("Failed to resolve Mumble server: {}", e))?
            .next()
            .ok_or_else(|| "Failed to resolve Mumble server address".to_string())
    }

    /// Connect a new user to Mumble and set up WebRTC
    pub async fn connect_user(
        &self,
        conn_id: &str,
        username: &str,
        ws_tx: mpsc::UnboundedSender<ServerMessage>,
    ) -> Result<(), String> {
        if self.sessions.read().await.contains_key(conn_id) {
            return Err("Already connected".to_string());
        }
        // Reserve capacity before DNS/TLS/WebRTC awaits. Dropping the permit on
        // setup failure or cancellation automatically makes the slot available.
        let slot = self
            .slots
            .clone()
            .try_acquire_owned()
            .map_err(|_| "Server full".to_string())?;

        // Async DNS resolution — does not block tokio worker threads
        let addr = self.resolve_mumble_addr().await?;

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
            .map_err(|e| format!("WebRTC setup failed: {:#}", e))?;

        let session = Arc::new(Mutex::new(UserSession {
            mumble_client,
            mumble_voice: None,
            webrtc_session,
            bridge: None,
            ws_tx: ws_tx.clone(),
            username: username.to_string(),
            session_id: None,
            voice_setup_task: None,
            slot: Some(slot),
            closed: false,
            close_notify: Arc::new(Notify::new()),
        }));

        {
            let mut sessions = self.sessions.write().await;
            // The WebSocket handler serializes messages, but also protect direct
            // callers from replacing a session with another in-flight connect.
            if sessions.contains_key(conn_id) {
                drop(sessions);
                shutdown_user_session(&mut *session.lock().await).await;
                return Err("Already connected".to_string());
            }
            sessions.insert(conn_id.to_string(), session.clone());
        }

        let conn_id_owned = conn_id.to_string();
        let config = self.config.clone();
        let sessions = self.sessions.clone();
        tokio::spawn(async move {
            Self::process_events(conn_id_owned, session, config, sessions).await;
        });

        info!("User '{}' connecting (conn_id={})", username, conn_id);
        Ok(())
    }

    /// Look up a session by conn_id, cloning the Arc so the RwLock is released immediately.
    async fn get_session(&self, conn_id: &str) -> Result<Arc<Mutex<UserSession>>, String> {
        self.sessions
            .read()
            .await
            .get(conn_id)
            .cloned()
            .ok_or_else(|| "Session not found".to_string())
    }

    /// Handle SDP offer from browser
    pub async fn handle_offer(&self, conn_id: &str, sdp: &str) -> Result<String, String> {
        let session = self.get_session(conn_id).await?;
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
        let session = self.get_session(conn_id).await?;
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
        let session = self.get_session(conn_id).await?;
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
        let session = self.get_session(conn_id).await?;
        let session = session.lock().await;
        session
            .mumble_client
            .send_command(MumbleCommand::JoinChannel { channel_id })
            .map_err(|e| e.to_string())
    }

    /// Set mute state
    pub async fn set_mute(&self, conn_id: &str, muted: bool) -> Result<(), String> {
        let session = self.get_session(conn_id).await?;
        let session = session.lock().await;
        session
            .mumble_client
            .send_command(MumbleCommand::SetMute(muted))
            .map_err(|e| e.to_string())
    }

    /// Set deaf state
    pub async fn set_deaf(&self, conn_id: &str, deafened: bool) -> Result<(), String> {
        let session = self.get_session(conn_id).await?;
        let session = session.lock().await;
        session
            .mumble_client
            .send_command(MumbleCommand::SetDeaf(deafened))
            .map_err(|e| e.to_string())
    }

    /// Disconnect user without holding the global map lock during teardown.
    pub async fn disconnect_user(&self, conn_id: &str) {
        let session = self.sessions.write().await.remove(conn_id);
        if let Some(session) = session {
            shutdown_user_session(&mut *session.lock().await).await;
            info!("User disconnected (conn_id={})", conn_id);
        }
    }

    /// Remove only this generation: an old event task must not remove a reconnect.
    async fn finish_session(
        sessions: &Sessions,
        conn_id: &str,
        session: &Arc<Mutex<UserSession>>,
        terminal_error: Option<ServerMessage>,
    ) -> bool {
        let ws_tx = {
            let mut session = session.lock().await;
            shutdown_user_session(&mut session).await;
            session.ws_tx.clone()
        };
        let mut sessions = sessions.write().await;
        if sessions
            .get(conn_id)
            .is_some_and(|current| Arc::ptr_eq(current, session))
        {
            sessions.remove(conn_id);
            // Removal and notification are synchronous under the map lock, so a
            // reconnect cannot be inserted between them and get a stale error.
            if let Some(error) = terminal_error {
                let _ = ws_tx.send(error);
            }
            true
        } else {
            false
        }
    }

    /// Process Mumble events and WebRTC events for a session.
    async fn process_events(
        conn_id: String,
        session: Arc<Mutex<UserSession>>,
        config: Config,
        sessions: Arc<Sessions>,
    ) {
        // Initialization and storing the task handle share the session lock with
        // teardown, so disconnect cannot race with spawning a new voice task.
        let mut s = session.lock().await;
        if s.closed {
            return;
        }
        let close_notify = s.close_notify.clone();
        // Take event receivers out of the session to avoid holding the lock
        let (mut mumble_event_rx, mut webrtc_event_rx, ws_tx, crypt_state_rx) = {
            let mumble_rx =
                std::mem::replace(&mut s.mumble_client.event_rx, mpsc::unbounded_channel().1);
            let webrtc_rx =
                std::mem::replace(&mut s.webrtc_session.event_rx, mpsc::unbounded_channel().1);
            let crypt_rx =
                std::mem::replace(&mut s.mumble_client.crypt_state_rx, oneshot::channel().1);
            (mumble_rx, webrtc_rx, s.ws_tx.clone(), crypt_rx)
        };

        // Track known users for name lookup in chat
        let mut user_map: HashMap<u32, String> = HashMap::new();

        // Spawn a task to wait for CryptState and start voice
        let session_clone = session.clone();
        let config_clone = config.clone();
        let conn_id_clone = conn_id.clone();
        let ws_tx_clone = ws_tx.clone();
        let voice_setup_task = tokio::spawn(async move {
            match crypt_state_rx.await {
                Ok(crypt_state) => {
                    let addr = tokio::net::lookup_host(config_clone.mumble_addr())
                        .await
                        .ok()
                        .and_then(|mut a| a.next());

                    let Some(addr) = addr else {
                        error!("Failed to resolve voice address for {}", conn_id_clone);
                        let _ = ws_tx_clone.send(ServerMessage::error(
                            "voice_error",
                            "Voice setup failed: could not resolve Mumble server address",
                        ));
                        return;
                    };

                    match MumbleVoice::start(addr, crypt_state).await {
                        Ok(mut voice) => {
                            let mut s = session_clone.lock().await;
                            let webrtc_audio_rx = std::mem::replace(
                                &mut s.webrtc_session.audio_rx,
                                mpsc::unbounded_channel().1,
                            );
                            match voice.take_channels() {
                                Ok((voice_rx, voice_tx)) => {
                                    let bridge = AudioBridge::start(
                                        voice_rx,
                                        voice_tx,
                                        webrtc_audio_rx,
                                        s.webrtc_session.outgoing_track.clone(),
                                    );
                                    s.mumble_voice = Some(voice);
                                    s.bridge = Some(bridge);
                                    info!("Audio bridge started for {}", conn_id_clone);
                                }
                                Err(e) => {
                                    voice.shutdown();
                                    error!("Failed to attach voice channels: {}", e);
                                    let _ = ws_tx_clone.send(ServerMessage::error(
                                        "voice_error",
                                        format!("Voice setup failed: {}", e),
                                    ));
                                }
                            }
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
                Err(_) => {
                    warn!("CryptState channel closed before receiving state");
                }
            }
        });

        s.voice_setup_task = Some(voice_setup_task);
        drop(s);

        let mut terminal_error = None;
        loop {
            tokio::select! {
                biased;
                _ = close_notify.notified() => break,
                _ = ws_tx.closed() => break,
                event = mumble_event_rx.recv() => {
                    match event {
                        Some(MumbleEvent::Connected { session_id, channels, users }) => {
                            for u in &users {
                                user_map.insert(u.session_id, u.name.clone());
                            }
                            {
                                let mut s = session.lock().await;
                                if s.closed {
                                    break;
                                }
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
                            terminal_error = Some(ServerMessage::error("mumble_disconnected", reason));
                            break;
                        }
                        None => {
                            break;
                        }
                    }
                }
                event = webrtc_event_rx.recv() => {
                    match event {
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
        // Release capacity before telling the browser it can reconnect. If this
        // session was explicitly replaced, do not send a stale terminal error.
        Self::finish_session(&sessions, &conn_id, &session, terminal_error).await;
    }
}

#[cfg(test)]
mod tests;
