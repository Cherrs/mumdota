use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::{debug, error};
use webrtc::peer_connection::OnTrackHdlrFn;
use webrtc::rtp_transceiver::rtp_receiver::RTCRtpReceiver;
use webrtc::rtp_transceiver::RTCRtpTransceiver;
use webrtc::track::track_remote::TrackRemote;

use bytes::Bytes;

/// Opus RTP packet received from browser → to be sent to Mumble
#[derive(Debug)]
#[allow(dead_code)]
pub struct IncomingAudioPacket {
    pub opus_data: Bytes,
    pub seq_num: u16,
    pub timestamp: u32,
    pub received_at: std::time::Instant,
}

/// Set up handler for receiving audio from the browser via WebRTC
pub fn setup_incoming_audio_handler(audio_tx: mpsc::Sender<IncomingAudioPacket>) -> OnTrackHdlrFn {
    Box::new(
        move |track: Arc<TrackRemote>,
              _receiver: Arc<RTCRtpReceiver>,
              _transceiver: Arc<RTCRtpTransceiver>| {
            let tx = audio_tx.clone();
            debug!(
                "Incoming track: kind={}, codec={}",
                track.kind(),
                track.codec().capability.mime_type
            );

            Box::pin(async move {
                // Only handle audio tracks
                if track.kind() != webrtc::rtp_transceiver::rtp_codec::RTPCodecType::Audio
                    || !track
                        .codec()
                        .capability
                        .mime_type
                        .eq_ignore_ascii_case("audio/opus")
                {
                    return;
                }

                tokio::spawn(async move {
                    loop {
                        match track.read_rtp().await {
                            Ok((rtp_packet, _attrs)) => {
                                let packet = IncomingAudioPacket {
                                    opus_data: rtp_packet.payload,
                                    seq_num: rtp_packet.header.sequence_number,
                                    timestamp: rtp_packet.header.timestamp,
                                    received_at: std::time::Instant::now(),
                                };
                                match tx.try_send(packet) {
                                    Ok(()) | Err(mpsc::error::TrySendError::Full(_)) => {}
                                    Err(mpsc::error::TrySendError::Closed(_)) => break,
                                }
                            }
                            Err(e) => {
                                error!("Error reading RTP: {}", e);
                                break;
                            }
                        }
                    }
                });
            })
        },
    )
}
