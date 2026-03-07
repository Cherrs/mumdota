use anyhow::{Context, Result};
use futures::{SinkExt, StreamExt};
use mumble_protocol::control::{msgs, ClientControlCodec, ControlPacket};
use mumble_protocol::crypt::{ClientCryptState, BLOCK_SIZE, KEY_SIZE};
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
            let (mut sink, mut stream) = ClientControlCodec::new().framed(tls_stream).split();

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

            let mut ping_interval = tokio::time::interval(std::time::Duration::from_secs(15));

            loop {
                tokio::select! {
                    packet = stream.next() => {
                        match packet {
                            Some(Ok(packet)) => {
                                if !handle_control_packet(
                                    packet,
                                    &event_tx,
                                    &mut crypt_state,
                                    &mut crypt_tx,
                                    &mut channels,
                                    &mut users,
                                    &mut our_session_id,
                                    &mut connected,
                                ) {
                                    break;
                                }
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

fn fixed_size_bytes<const N: usize>(
    bytes: &[u8],
    field_name: &str,
) -> std::result::Result<[u8; N], String> {
    bytes.try_into().map_err(|_| {
        format!(
            "Invalid {field_name} size from server: expected {N} bytes, got {}",
            bytes.len()
        )
    })
}

fn crypt_state_from_message(
    msg: &mumble_protocol::control::msgs::CryptSetup,
) -> std::result::Result<ClientCryptState, String> {
    let key = fixed_size_bytes::<KEY_SIZE>(msg.get_key(), "key")?;
    let client_nonce = fixed_size_bytes::<BLOCK_SIZE>(msg.get_client_nonce(), "client nonce")?;
    let server_nonce = fixed_size_bytes::<BLOCK_SIZE>(msg.get_server_nonce(), "server nonce")?;

    Ok(ClientCryptState::new_from(key, client_nonce, server_nonce))
}

fn merge_user_state(existing: &UserInfo, msg: &msgs::UserState) -> UserInfo {
    UserInfo {
        session_id: existing.session_id,
        name: if msg.has_name() {
            msg.get_name().to_string()
        } else {
            existing.name.clone()
        },
        channel_id: if msg.has_channel_id() {
            msg.get_channel_id()
        } else {
            existing.channel_id
        },
        mute: if msg.has_mute() {
            msg.get_mute()
        } else {
            existing.mute
        },
        deaf: if msg.has_deaf() {
            msg.get_deaf()
        } else {
            existing.deaf
        },
        self_mute: if msg.has_self_mute() {
            msg.get_self_mute()
        } else {
            existing.self_mute
        },
        self_deaf: if msg.has_self_deaf() {
            msg.get_self_deaf()
        } else {
            existing.self_deaf
        },
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
) -> bool {
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
            } else if let Some(existing) = users.iter_mut().find(|u| u.session_id == session_id) {
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
                *existing = merge_user_state(existing, msg.as_ref());
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
        ControlPacket::CryptSetup(msg) => match crypt_state_from_message(msg.as_ref()) {
            Ok(cs) => {
                *crypt_state = Some(cs);
                debug!("CryptSetup received");
            }
            Err(reason) => {
                error!("{reason}");
                let _ = event_tx.send(MumbleEvent::Disconnected(reason));
                return false;
            }
        },
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
            let _ = event_tx.send(MumbleEvent::Disconnected(format!("Rejected: {}", reason)));
        }
        _ => {
            // Ignore other packets (Ping, ServerConfig, CodecVersion, etc.)
        }
    }

    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use mumble_protocol::crypt::BLOCK_SIZE;
    use std::env;
    use std::net::{SocketAddr, ToSocketAddrs};
    use tokio::sync::{mpsc, oneshot};
    use tokio::time::{timeout, Duration};

    fn expect_event<T>(
        event_rx: &mut mpsc::UnboundedReceiver<MumbleEvent>,
        matcher: impl Fn(MumbleEvent) -> Option<T>,
    ) -> T {
        matcher(event_rx.try_recv().expect("expected event")).expect("unexpected event")
    }

    fn resolve_test_server_addr(addr: &str) -> Result<SocketAddr, String> {
        addr.to_socket_addrs()
            .map_err(|err| format!("Failed to resolve {addr}: {err}"))?
            .next()
            .ok_or_else(|| format!("Failed to resolve {addr}: no socket addresses returned"))
    }

    async fn wait_for_connected(
        client: &mut MumbleClient,
    ) -> (u32, Option<u32>, Vec<u32>) {
        timeout(Duration::from_secs(10), async {
            loop {
                match client.event_rx.recv().await {
                    Some(MumbleEvent::Connected {
                        session_id,
                        channels,
                        users,
                    }) => {
                        let current_channel_id = users
                            .iter()
                            .find(|user| user.session_id == session_id)
                            .map(|user| user.channel_id);
                        let channel_ids = channels.into_iter().map(|channel| channel.id).collect();
                        break (session_id, current_channel_id, channel_ids);
                    }
                    Some(MumbleEvent::Disconnected(reason)) => {
                        panic!("Mumble disconnected during smoke test: {reason}");
                    }
                    Some(_) => {}
                    None => panic!("Mumble event stream closed before Connected"),
                }
            }
        })
        .await
        .expect("timed out waiting for Mumble Connected event")
    }

    async fn wait_for_channel_join(
        client: &mut MumbleClient,
        session_id: u32,
        target_channel_id: u32,
        context: &str,
    ) {
        timeout(Duration::from_secs(10), async {
            loop {
                match client.event_rx.recv().await {
                    Some(MumbleEvent::UserStateChanged(state))
                        if state.session_id == session_id
                            && state.channel_id == Some(target_channel_id) =>
                    {
                        break;
                    }
                    Some(MumbleEvent::Disconnected(reason)) => {
                        panic!("Mumble disconnected {context}: {reason}");
                    }
                    Some(_) => {}
                    None => panic!("Mumble event stream closed {context}"),
                }
            }
        })
        .await
        .expect("timed out waiting for channel join confirmation");
    }

    async fn wait_for_chat_message(
        client: &mut MumbleClient,
        sender_session: u32,
        target_channel_id: u32,
        expected_message: &str,
        context: &str,
    ) {
        timeout(Duration::from_secs(10), async {
            loop {
                match client.event_rx.recv().await {
                    Some(MumbleEvent::ChatMessage {
                        sender_session: actor,
                        channel_id,
                        message,
                    }) if actor == sender_session
                        && channel_id == target_channel_id
                        && message == expected_message =>
                    {
                        break;
                    }
                    Some(MumbleEvent::Disconnected(reason)) => {
                        panic!("Mumble disconnected {context}: {reason}");
                    }
                    Some(_) => {}
                    None => panic!("Mumble event stream closed {context}"),
                }
            }
        })
        .await
        .expect("timed out waiting for chat delivery");
    }

    #[test]
    fn invalid_cryptsetup_disconnects_instead_of_panicking() {
        let (event_tx, mut event_rx) = mpsc::unbounded_channel();
        let (crypt_tx, _crypt_rx) = oneshot::channel();
        let mut crypt_state = None;
        let mut crypt_tx = Some(crypt_tx);
        let mut channels = Vec::new();
        let mut users = Vec::new();
        let mut our_session_id = None;
        let mut connected = false;

        let mut msg = msgs::CryptSetup::new();
        msg.set_key(vec![0; 8]);
        msg.set_client_nonce(vec![0; BLOCK_SIZE]);
        msg.set_server_nonce(vec![0; BLOCK_SIZE]);

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            handle_control_packet(
                ControlPacket::CryptSetup(Box::new(msg)),
                &event_tx,
                &mut crypt_state,
                &mut crypt_tx,
                &mut channels,
                &mut users,
                &mut our_session_id,
                &mut connected,
            );
        }));

        assert!(result.is_ok(), "CryptSetup handler should not panic");
        assert!(crypt_state.is_none());

        match event_rx.try_recv() {
            Ok(MumbleEvent::Disconnected(reason)) => {
                assert!(reason.contains("Invalid key size"));
            }
            other => panic!("expected disconnect event, got {other:?}"),
        }
    }

    #[test]
    fn connected_user_channel_move_preserves_cached_user_details() {
        let (event_tx, mut event_rx) = mpsc::unbounded_channel();
        let mut crypt_state = None;
        let mut crypt_tx = None;
        let mut channels = Vec::new();
        let mut users = vec![UserInfo {
            session_id: 42,
            name: "alice".to_string(),
            channel_id: 1,
            mute: true,
            deaf: false,
            self_mute: true,
            self_deaf: false,
        }];
        let mut our_session_id = Some(42);
        let mut connected = true;

        let mut msg = msgs::UserState::new();
        msg.set_session(42);
        msg.set_channel_id(9);

        let keep_running = handle_control_packet(
            ControlPacket::UserState(Box::new(msg)),
            &event_tx,
            &mut crypt_state,
            &mut crypt_tx,
            &mut channels,
            &mut users,
            &mut our_session_id,
            &mut connected,
        );

        assert!(keep_running);
        let state = expect_event(&mut event_rx, |event| match event {
            MumbleEvent::UserStateChanged(state) => Some(state),
            _ => None,
        });
        assert_eq!(state.session_id, 42);
        assert_eq!(state.channel_id, Some(9));
        assert_eq!(state.name, None);
        assert_eq!(state.mute, None);
        assert_eq!(state.self_mute, None);

        assert_eq!(users[0].session_id, 42);
        assert_eq!(users[0].name, "alice");
        assert_eq!(users[0].channel_id, 9);
        assert!(users[0].mute);
        assert!(users[0].self_mute);
    }

    #[test]
    fn text_message_emits_chat_event_for_target_channel() {
        let (event_tx, mut event_rx) = mpsc::unbounded_channel();
        let mut crypt_state = None;
        let mut crypt_tx = None;
        let mut channels = Vec::new();
        let mut users = Vec::new();
        let mut our_session_id = Some(7);
        let mut connected = true;

        let mut msg = msgs::TextMessage::new();
        msg.set_actor(7);
        msg.mut_channel_id().push(12);
        msg.set_message("hello from test".to_string());

        let keep_running = handle_control_packet(
            ControlPacket::TextMessage(Box::new(msg)),
            &event_tx,
            &mut crypt_state,
            &mut crypt_tx,
            &mut channels,
            &mut users,
            &mut our_session_id,
            &mut connected,
        );

        assert!(keep_running);
        let chat = expect_event(&mut event_rx, |event| match event {
            MumbleEvent::ChatMessage {
                sender_session,
                channel_id,
                message,
            } => Some((sender_session, channel_id, message)),
            _ => None,
        });
        assert_eq!(chat.0, 7);
        assert_eq!(chat.1, 12);
        assert_eq!(chat.2, "hello from test");
    }

    #[test]
    fn resolve_test_server_addr_accepts_hostname_with_port() {
        let addr = resolve_test_server_addr("localhost:64738")
            .expect("localhost with port should resolve for smoke tests");

        assert_eq!(addr.port(), 64738);
    }

    #[test]
    fn resolve_test_server_addr_rejects_missing_port() {
        let err = resolve_test_server_addr("localhost").expect_err("missing port should fail");

        assert!(err.contains("Failed to resolve"));
    }

    // Opt-in smoke test env vars:
    // - required: MUMDOTA_TEST_MUMBLE_ADDR, MUMDOTA_TEST_MUMBLE_HOST
    // - optional: MUMDOTA_TEST_USERNAME, MUMDOTA_TEST_ALLOW_INVALID_CERTS,
    //   MUMDOTA_TEST_CHANNEL_ID, MUMDOTA_TEST_VERIFY_CHAT_ECHO,
    //   MUMDOTA_TEST_OBSERVER_USERNAME
    #[tokio::test]
    #[ignore = "requires a reachable Mumble server configured via MUMDOTA_TEST_MUMBLE_* env vars"]
    async fn real_mumble_smoke_connects_joins_channel_and_sends_chat() {
        let server_addr = resolve_test_server_addr(&read_env("MUMDOTA_TEST_MUMBLE_ADDR"))
            .expect("MUMDOTA_TEST_MUMBLE_ADDR must resolve to a socket address");
        let server_host = read_env("MUMDOTA_TEST_MUMBLE_HOST");
        let username = env::var("MUMDOTA_TEST_USERNAME")
            .unwrap_or_else(|_| format!("mumdota-test-{}", std::process::id()));
        let observer_username = env::var("MUMDOTA_TEST_OBSERVER_USERNAME")
            .unwrap_or_else(|_| format!("{username}-observer"));
        let accept_invalid_certs = env::var("MUMDOTA_TEST_ALLOW_INVALID_CERTS")
            .map(|value| value == "1" || value.eq_ignore_ascii_case("true"))
            .unwrap_or(true);
        let target_channel_id = env::var("MUMDOTA_TEST_CHANNEL_ID")
            .unwrap_or_else(|_| "0".to_string())
            .parse::<u32>()
            .expect("MUMDOTA_TEST_CHANNEL_ID must be a u32");
        let verify_chat_echo = env::var("MUMDOTA_TEST_VERIFY_CHAT_ECHO")
            .map(|value| value == "1" || value.eq_ignore_ascii_case("true"))
            .unwrap_or(false);
        let message = format!(
            "mumdota smoke {}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system clock should be after unix epoch")
                .as_nanos()
        );

        let mut observer = MumbleClient::connect(
            server_addr,
            server_host.clone(),
            observer_username,
            accept_invalid_certs,
        )
        .await
        .expect("observer should connect to the configured Mumble server");
        let (observer_session_id, observer_channel_id, observer_channels) =
            wait_for_connected(&mut observer).await;

        assert!(
            observer_channels.contains(&target_channel_id),
            "target channel {target_channel_id} was not advertised to observer"
        );

        if observer_channel_id != Some(target_channel_id) {
            observer
                .send_command(MumbleCommand::JoinChannel {
                    channel_id: target_channel_id,
                })
                .expect("observer join channel command should be accepted");
            wait_for_channel_join(
                &mut observer,
                observer_session_id,
                target_channel_id,
                "while waiting for observer join",
            )
            .await;
        }

        let mut client =
            MumbleClient::connect(server_addr, server_host, username, accept_invalid_certs)
                .await
                .expect("smoke test should connect to the configured Mumble server");

        let (session_id, current_channel_id, channel_ids) = wait_for_connected(&mut client).await;

        assert!(
            channel_ids.contains(&target_channel_id),
            "target channel {target_channel_id} was not advertised by the server"
        );

        client
            .send_command(MumbleCommand::JoinChannel {
                channel_id: target_channel_id,
            })
            .expect("join channel command should be accepted");

        if current_channel_id != Some(target_channel_id) {
            wait_for_channel_join(
                &mut client,
                session_id,
                target_channel_id,
                "after join request",
            )
            .await;
        }

        client
            .send_command(MumbleCommand::SendChat {
                channel_id: target_channel_id,
                message: message.clone(),
            })
            .expect("chat command should be accepted");

        wait_for_chat_message(
            &mut observer,
            session_id,
            target_channel_id,
            &message,
            "while waiting for observer chat delivery",
        )
        .await;

        if verify_chat_echo {
            wait_for_chat_message(
                &mut client,
                session_id,
                target_channel_id,
                &message,
                "while waiting for sender chat echo",
            )
            .await;
        }

        client
            .send_command(MumbleCommand::Disconnect)
            .expect("disconnect command should be accepted");
        observer
            .send_command(MumbleCommand::Disconnect)
            .expect("observer disconnect command should be accepted");
    }

    fn read_env(name: &str) -> String {
        env::var(name).unwrap_or_else(|_| panic!("{name} must be set for the smoke test"))
    }
}
