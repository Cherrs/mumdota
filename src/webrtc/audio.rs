use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::{debug, error};
use webrtc::rtp_transceiver::rtp_codec::RTCRtpCodecCapability;
use webrtc::rtp_transceiver::rtp_receiver::RTCRtpReceiver;
use webrtc::rtp_transceiver::RTCRtpTransceiver;
use webrtc::track::track_local::track_local_static_sample::TrackLocalStaticSample;
use webrtc::track::track_remote::TrackRemote;

use bytes::Bytes;

/// Opus RTP packet received from browser → to be sent to Mumble
#[derive(Debug)]
#[allow(dead_code)]
pub struct IncomingAudioPacket {
    pub opus_data: Bytes,
    pub seq_num: u16,
    pub timestamp: u32,
}

/// Create an Opus audio track for sending audio to the browser
pub fn create_opus_track(track_id: &str, stream_id: &str) -> Arc<TrackLocalStaticSample> {
    Arc::new(TrackLocalStaticSample::new(
        RTCRtpCodecCapability {
            mime_type: "audio/opus".to_owned(),
            clock_rate: 48000,
            channels: 2,
            sdp_fmtp_line: "minptime=10;useinbandfec=1".to_owned(),
            ..Default::default()
        },
        track_id.to_owned(),
        stream_id.to_owned(),
    ))
}

/// Set up handler for receiving audio from the browser via WebRTC
pub fn setup_incoming_audio_handler(
    audio_tx: mpsc::UnboundedSender<IncomingAudioPacket>,
) -> Box<
    dyn FnMut(
            Arc<TrackRemote>,
            Arc<RTCRtpReceiver>,
            Arc<RTCRtpTransceiver>,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>
        + Send
        + Sync,
> {
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
                if track.kind() != webrtc::rtp_transceiver::rtp_codec::RTPCodecType::Audio {
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
                                };
                                if tx.send(packet).is_err() {
                                    debug!("Audio receiver closed");
                                    break;
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
