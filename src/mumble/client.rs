use anyhow::{Context, Result};
use futures::{SinkExt, StreamExt};
use mumble_protocol::control::{ClientControlCodec, ControlPacket};
use mumble_protocol::crypt::ClientCryptState;
use mumble_protocol::Clientbound;
use std::convert::TryInto;
use std::net::SocketAddr;
use tokio::net::TcpStream;
use tokio::sync::{mpsc, oneshot};
use tokio_native_tls::TlsConnector;
use tokio_util::codec::Decoder;
use tracing::{debug, error, info, warn};

use super::proto;
use crate::ws::messages::{ChannelInfo, UserInfo, UserStateData};

/// Events emitted by the Mumble client connection
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub enum MumbleEvent {
    Connected {
        session_id: u32,
        channels: Vec<ChannelInfo>,
        users: Vec<UserInfo>,
    },
    UserJoined(UserInfo),
    UserLeft {
        session_id: u32,
    },
    UserStateChanged(UserStateData),
    ChannelAdded(ChannelInfo),
    ChannelUpdated(ChannelInfo),
    ChannelRemoved {
        channel_id: u32,
    },
    ChatMessage {
        sender_session: u32,
        channel_id: u32,
        message: String,
    },
    Disconnected(String),
}

/// Commands sent to the Mumble client
#[derive(Debug)]
pub enum MumbleCommand {
    SendChat { channel_id: u32, message: String },
    JoinChannel { channel_id: u32 },
    SetMute(bool),
    SetDeaf(bool),
    Disconnect,
}

/// Manages a single TCP+TLS connection to a Mumble server
pub struct MumbleClient {
    pub event_rx: mpsc::UnboundedReceiver<MumbleEvent>,
    pub command_tx: mpsc::UnboundedSender<MumbleCommand>,
    pub crypt_state_rx: oneshot::Receiver<ClientCryptState>,
}

impl MumbleClient {
    pub async fn connect(
        server_addr: SocketAddr,
        server_host: String,
        username: String,
        accept_invalid_certs: bool,
    ) -> Result<Self> {
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let (command_tx, command_rx) = mpsc::unbounded_channel();

        let stream = TcpStream::connect(&server_addr)
            .await
            .context("Failed to connect to Mumble server")?;
        debug!("TCP connected to {}", server_addr);

        let mut builder = native_tls::TlsConnector::builder();
        builder.danger_accept_invalid_certs(accept_invalid_certs);
        let connector: TlsConnector = builder
            .build()
            .context("Failed to create TLS connector")?
            .into();
        let tls_stream = connector
            .connect(&server_host, stream)
            .await
            .context("TLS handshake failed")?;
        debug!("TLS connected");

        let (crypt_tx, crypt_rx) = oneshot::channel();

        // Spawn the connection loop. We move everything into an async block
        // to avoid specifying the complex Framed codec types explicitly.
        tokio::spawn(async move {
            let (mut sink, mut stream) =
                ClientControlCodec::new().framed(tls_stream).split();

            // Send Version
            let version = proto::build_version_message();
            if let Err(e) = sink.send(version.into()).await {
                error!("Failed to send Version: {}", e);
                let _ = event_tx.send(MumbleEvent::Disconnected(e.to_string()));
                return;
            }

            // Send Authenticate
            let auth = proto::build_authenticate_message(&username);
            if let Err(e) = sink.send(auth.into()).await {
                error!("Failed to send Authenticate: {}", e);
                let _ = event_tx.send(MumbleEvent::Disconnected(e.to_string()));
                return;
            }
            info!("Authenticating as '{}'...", username);

            let mut crypt_state: Option<ClientCryptState> = None;
            let mut crypt_tx = Some(crypt_tx);
            let mut channels: Vec<ChannelInfo> = Vec::new();
            let mut users: Vec<UserInfo> = Vec::new();
            let mut our_session_id: Option<u32> = None;
            let mut connected = false;
            let mut command_rx = command_rx;

            let mut ping_interval =
                tokio::time::interval(std::time::Duration::from_secs(15));

            loop {
                tokio::select! {
                    packet = stream.next() => {
                        match packet {
                            Some(Ok(packet)) => {
                                handle_control_packet(
                                    packet,
                                    &event_tx,
                                    &mut crypt_state,
                                    &mut crypt_tx,
                                    &mut channels,
                                    &mut users,
                                    &mut our_session_id,
                                    &mut connected,
                                );
                            }
                            Some(Err(e)) => {
                                error!("Mumble stream error: {}", e);
                                let _ = event_tx.send(MumbleEvent::Disconnected(e.to_string()));
                                break;
                            }
                            None => {
                                info!("Mumble connection closed");
                                let _ = event_tx.send(MumbleEvent::Disconnected(
                                    "Connection closed".to_string(),
                                ));
                                break;
                            }
                        }
                    }
                    cmd = command_rx.recv() => {
                        match cmd {
                            Some(MumbleCommand::SendChat { channel_id, message }) => {
                                let msg = proto::build_text_message(channel_id, &message);
                                if let Err(e) = sink.send(msg.into()).await {
                                    warn!("Failed to send chat: {}", e);
                                }
                            }
                            Some(MumbleCommand::JoinChannel { channel_id }) => {
                                let msg = proto::build_user_state_channel(channel_id);
                                if let Err(e) = sink.send(msg.into()).await {
                                    warn!("Failed to join channel: {}", e);
                                }
                            }
                            Some(MumbleCommand::SetMute(muted)) => {
                                let msg = proto::build_user_state_mute(muted);
                                if let Err(e) = sink.send(msg.into()).await {
                                    warn!("Failed to set mute: {}", e);
                                }
                            }
                            Some(MumbleCommand::SetDeaf(deafened)) => {
                                let msg = proto::build_user_state_deaf(deafened);
                                if let Err(e) = sink.send(msg.into()).await {
                                    warn!("Failed to set deaf: {}", e);
                                }
                            }
                            Some(MumbleCommand::Disconnect) | None => {
                                info!("Disconnecting from Mumble");
                                break;
                            }
                        }
                    }
                    _ = ping_interval.tick() => {
                        let ping = proto::build_ping();
                        if let Err(e) = sink.send(ping.into()).await {
                            warn!("Failed to send ping: {}", e);
                        }
                    }
                }
            }
        });

        Ok(MumbleClient {
            event_rx,
            command_tx,
            crypt_state_rx: crypt_rx,
        })
    }

    pub fn send_command(&self, cmd: MumbleCommand) -> Result<()> {
        self.command_tx
            .send(cmd)
            .map_err(|_| anyhow::anyhow!("Mumble client disconnected"))
    }
}

fn handle_control_packet(
    packet: ControlPacket<Clientbound>,
    event_tx: &mpsc::UnboundedSender<MumbleEvent>,
    crypt_state: &mut Option<ClientCryptState>,
    crypt_tx: &mut Option<oneshot::Sender<ClientCryptState>>,
    channels: &mut Vec<ChannelInfo>,
    users: &mut Vec<UserInfo>,
    our_session_id: &mut Option<u32>,
    connected: &mut bool,
) {
    match packet {
        ControlPacket::ChannelState(msg) => {
            let info = ChannelInfo {
                id: msg.get_channel_id(),
                name: msg.get_name().to_string(),
                parent_id: msg.get_parent(),
                description: msg.get_description().to_string(),
            };
            if !*connected {
                channels.push(info);
            } else if let Some(existing) = channels.iter_mut().find(|c| c.id == info.id) {
                *existing = info.clone();
                let _ = event_tx.send(MumbleEvent::ChannelUpdated(info));
            } else {
                channels.push(info.clone());
                let _ = event_tx.send(MumbleEvent::ChannelAdded(info));
            }
        }
        ControlPacket::ChannelRemove(msg) => {
            let channel_id = msg.get_channel_id();
            channels.retain(|c| c.id != channel_id);
            if *connected {
                let _ = event_tx.send(MumbleEvent::ChannelRemoved { channel_id });
            }
        }
        ControlPacket::UserState(msg) => {
            let session_id = msg.get_session();
            let info = UserInfo {
                session_id,
                name: msg.get_name().to_string(),
                channel_id: msg.get_channel_id(),
                mute: msg.get_mute(),
                deaf: msg.get_deaf(),
                self_mute: msg.get_self_mute(),
                self_deaf: msg.get_self_deaf(),
            };
            if !*connected {
                users.push(info);
            } else if let Some(existing) =
                users.iter_mut().find(|u| u.session_id == session_id)
            {
                let state = UserStateData {
                    session_id,
                    channel_id: if msg.has_channel_id() {
                        Some(msg.get_channel_id())
                    } else {
                        None
                    },
                    name: if msg.has_name() {
                        Some(msg.get_name().to_string())
                    } else {
                        None
                    },
                    mute: if msg.has_mute() {
                        Some(msg.get_mute())
                    } else {
                        None
                    },
                    deaf: if msg.has_deaf() {
                        Some(msg.get_deaf())
                    } else {
                        None
                    },
                    self_mute: if msg.has_self_mute() {
                        Some(msg.get_self_mute())
                    } else {
                        None
                    },
                    self_deaf: if msg.has_self_deaf() {
                        Some(msg.get_self_deaf())
                    } else {
                        None
                    },
                };
                *existing = info;
                let _ = event_tx.send(MumbleEvent::UserStateChanged(state));
            } else {
                users.push(info.clone());
                let _ = event_tx.send(MumbleEvent::UserJoined(info));
            }
        }
        ControlPacket::UserRemove(msg) => {
            let session_id = msg.get_session();
            users.retain(|u| u.session_id != session_id);
            if *connected {
                let _ = event_tx.send(MumbleEvent::UserLeft { session_id });
            }
        }
        ControlPacket::CryptSetup(msg) => {
            let cs = ClientCryptState::new_from(
                msg.get_key()
                    .try_into()
                    .expect("Invalid key size from server"),
                msg.get_client_nonce()
                    .try_into()
                    .expect("Invalid client nonce size"),
                msg.get_server_nonce()
                    .try_into()
                    .expect("Invalid server nonce size"),
            );
            *crypt_state = Some(cs);
            debug!("CryptSetup received");
        }
        ControlPacket::ServerSync(msg) => {
            let session_id = msg.get_session();
            *our_session_id = Some(session_id);
            *connected = true;
            info!("Logged in with session_id={}", session_id);

            if let Some(cs) = crypt_state.take() {
                if let Some(tx) = crypt_tx.take() {
                    let _ = tx.send(cs);
                }
            }
            let _ = event_tx.send(MumbleEvent::Connected {
                session_id,
                channels: channels.clone(),
                users: users.clone(),
            });
        }
        ControlPacket::TextMessage(msg) => {
            let sender_session = msg.get_actor();
            let channel_ids = msg.get_channel_id();
            let channel_id = channel_ids.first().copied().unwrap_or(0);
            let message = msg.get_message().to_string();
            let _ = event_tx.send(MumbleEvent::ChatMessage {
                sender_session,
                channel_id,
                message,
            });
        }
        ControlPacket::Reject(msg) => {
            let reason = format!("{:?}: {}", msg.get_field_type(), msg.get_reason());
            error!("Login rejected: {}", reason);
            let _ = event_tx.send(MumbleEvent::Disconnected(format!(
                "Rejected: {}",
                reason
            )));
        }
        _ => {
            // Ignore other packets (Ping, ServerConfig, CodecVersion, etc.)
        }
    }
}
