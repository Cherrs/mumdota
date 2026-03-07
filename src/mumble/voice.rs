use anyhow::Result;
use bytes::Bytes;
use futures::{SinkExt, StreamExt};
use mumble_protocol::crypt::ClientCryptState;
use mumble_protocol::voice::{VoicePacket, VoicePacketPayload};
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::udp::UdpFramed;
use tracing::{debug, error, warn};

/// Incoming voice data from Mumble server
#[derive(Debug)]
#[allow(dead_code)]
pub struct MumbleVoiceData {
    pub session_id: u32,
    pub seq_num: u64,
    pub opus_data: Bytes,
    pub last_frame: bool,
}

/// Outgoing voice data to Mumble server
#[derive(Debug)]
pub struct WebrtcVoiceData {
    pub seq_num: u64,
    pub opus_data: Bytes,
    pub last_frame: bool,
}

pub struct MumbleVoice {
    task: Option<JoinHandle<()>>,
    voice_rx: Option<mpsc::UnboundedReceiver<MumbleVoiceData>>,
    voice_tx: Option<mpsc::UnboundedSender<WebrtcVoiceData>>,
}

impl MumbleVoice {
    pub async fn start(server_addr: SocketAddr, crypt_state: ClientCryptState) -> Result<Self> {
        let (incoming_tx, voice_rx) = mpsc::unbounded_channel();
        let (voice_tx, outgoing_rx) = mpsc::unbounded_channel();

        let bind_addr: SocketAddr = if server_addr.is_ipv6() {
            (Ipv6Addr::UNSPECIFIED, 0u16).into()
        } else {
            (Ipv4Addr::UNSPECIFIED, 0u16).into()
        };
        let udp_socket = UdpSocket::bind(bind_addr)
            .await
            .map_err(|e| anyhow::anyhow!("Failed to bind UDP socket: {}", e))?;

        debug!("UDP voice socket bound to {}", udp_socket.local_addr()?);

        let task = tokio::spawn(Self::run(
            udp_socket,
            server_addr,
            crypt_state,
            incoming_tx,
            outgoing_rx,
        ));

        Ok(MumbleVoice {
            task: Some(task),
            voice_rx: Some(voice_rx),
            voice_tx: Some(voice_tx),
        })
    }

    pub fn take_channels(
        &mut self,
    ) -> Result<(
        mpsc::UnboundedReceiver<MumbleVoiceData>,
        mpsc::UnboundedSender<WebrtcVoiceData>,
    )> {
        let voice_rx = self
            .voice_rx
            .take()
            .ok_or_else(|| anyhow::anyhow!("Voice receiver already taken"))?;
        let voice_tx = self
            .voice_tx
            .take()
            .ok_or_else(|| anyhow::anyhow!("Voice sender already taken"))?;

        Ok((voice_rx, voice_tx))
    }

    pub fn shutdown(&mut self) {
        self.voice_rx.take();
        self.voice_tx.take();

        if let Some(task) = self.task.take() {
            task.abort();
        }
    }

    #[cfg(test)]
    fn from_parts_for_test(
        task: JoinHandle<()>,
        voice_rx: mpsc::UnboundedReceiver<MumbleVoiceData>,
        voice_tx: mpsc::UnboundedSender<WebrtcVoiceData>,
    ) -> Self {
        Self {
            task: Some(task),
            voice_rx: Some(voice_rx),
            voice_tx: Some(voice_tx),
        }
    }

    async fn run(
        udp_socket: UdpSocket,
        server_addr: SocketAddr,
        crypt_state: ClientCryptState,
        incoming_tx: mpsc::UnboundedSender<MumbleVoiceData>,
        mut outgoing_rx: mpsc::UnboundedReceiver<WebrtcVoiceData>,
    ) {
        let (mut sink, mut source) = UdpFramed::new(udp_socket, crypt_state).split();

        // Send an initial dummy packet to make the server accept our UDP
        let init_packet = VoicePacket::Audio {
            _dst: std::marker::PhantomData,
            target: 0,
            session_id: (),
            seq_num: 0,
            payload: VoicePacketPayload::Opus(Bytes::from_static(&[0u8; 16]), true),
            position_info: None,
        };
        if let Err(e) = sink.send((init_packet, server_addr)).await {
            error!("Failed to send initial UDP packet: {}", e);
            return;
        }
        debug!("Initial UDP voice packet sent");

        loop {
            tokio::select! {
                packet = source.next() => {
                    match packet {
                        Some(Ok((voice_packet, _src_addr))) => {
                            match voice_packet {
                                VoicePacket::Ping { .. } => {
                                    // Respond to voice pings
                                    continue;
                                }
                                VoicePacket::Audio {
                                    session_id,
                                    seq_num,
                                    payload,
                                    ..
                                } => {
                                    if let VoicePacketPayload::Opus(data, last) = payload {
                                        let _ = incoming_tx.send(MumbleVoiceData {
                                            session_id,
                                            seq_num,
                                            opus_data: data,
                                            last_frame: last,
                                        });
                                    }
                                }
                            }
                        }
                        Some(Err(e)) => {
                            warn!("Invalid UDP packet: {}", e);
                            continue;
                        }
                        None => {
                            debug!("UDP stream ended");
                            break;
                        }
                    }
                }
                outgoing = outgoing_rx.recv() => {
                    match outgoing {
                        Some(data) => {
                            let packet = VoicePacket::Audio {
                                _dst: std::marker::PhantomData,
                                target: 0, // normal speech
                                session_id: (),
                                seq_num: data.seq_num,
                                payload: VoicePacketPayload::Opus(data.opus_data, data.last_frame),
                                position_info: None,
                            };
                            if let Err(e) = sink.send((packet, server_addr)).await {
                                warn!("Failed to send voice packet: {}", e);
                            }
                        }
                        None => {
                            debug!("Voice outgoing channel closed");
                            break;
                        }
                    }
                }
            }
        }
    }
}

impl Drop for MumbleVoice {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::oneshot;
    use tokio::time::{timeout, Duration};

    struct DropSignal(Option<oneshot::Sender<()>>);

    impl Drop for DropSignal {
        fn drop(&mut self) {
            if let Some(tx) = self.0.take() {
                let _ = tx.send(());
            }
        }
    }

    #[tokio::test]
    async fn shutdown_aborts_background_voice_task() {
        let (drop_tx, drop_rx) = oneshot::channel();
        let task = tokio::spawn(async move {
            let _signal = DropSignal(Some(drop_tx));
            std::future::pending::<()>().await;
        });
        let (_incoming_tx, voice_rx) = mpsc::unbounded_channel();
        let (voice_tx, _outgoing_rx) = mpsc::unbounded_channel();
        tokio::task::yield_now().await;

        let mut voice = MumbleVoice::from_parts_for_test(task, voice_rx, voice_tx);
        voice.shutdown();

        timeout(Duration::from_secs(1), drop_rx)
            .await
            .expect("voice task should be aborted")
            .expect("drop signal should be delivered");
    }
}
