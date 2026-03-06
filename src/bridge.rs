use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::{debug, warn};
use webrtc::rtp::packet::Packet as RtpPacket;
use webrtc::track::track_local::track_local_static_rtp::TrackLocalStaticRTP;
use webrtc::track::track_local::TrackLocalWriter;

use crate::mumble::voice::{MumbleVoiceData, WebrtcVoiceData};
use crate::webrtc::audio::IncomingAudioPacket;

/// Bidirectional audio bridge between Mumble voice and WebRTC
pub struct AudioBridge {
    shutdown_tx: mpsc::Sender<()>,
}

impl AudioBridge {
    /// Start the audio bridge
    ///
    /// - `mumble_voice_rx`: incoming Opus from Mumble server
    /// - `mumble_voice_tx`: outgoing Opus to Mumble server
    /// - `webrtc_audio_rx`: incoming Opus from browser via WebRTC
    /// - `webrtc_track`: outgoing Opus track to browser via WebRTC
    pub fn start(
        mut mumble_voice_rx: mpsc::UnboundedReceiver<MumbleVoiceData>,
        mumble_voice_tx: mpsc::UnboundedSender<WebrtcVoiceData>,
        mut webrtc_audio_rx: mpsc::UnboundedReceiver<IncomingAudioPacket>,
        webrtc_track: Arc<TrackLocalStaticRTP>,
    ) -> Self {
        let (shutdown_tx, mut shutdown_rx) = mpsc::channel::<()>(1);

        // Mumble → WebRTC: forward Opus from Mumble to browser
        let track = webrtc_track.clone();
        let _shutdown_rx2 = shutdown_tx.clone();
        tokio::spawn(async move {
            let mut rtp_seq: u16 = 0;
            let mut rtp_timestamp: u32 = 0;
            // Opus at 48kHz, 20ms frames = 960 samples per frame
            let samples_per_frame: u32 = 960;

            loop {
                tokio::select! {
                    voice = mumble_voice_rx.recv() => {
                        match voice {
                            Some(data) => {
                                rtp_seq = rtp_seq.wrapping_add(1);
                                rtp_timestamp = rtp_timestamp.wrapping_add(samples_per_frame);

                                let rtp = RtpPacket {
                                    header: webrtc::rtp::header::Header {
                                        version: 2,
                                        padding: false,
                                        extension: false,
                                        marker: false,
                                        payload_type: 111, // Opus payload type
                                        sequence_number: rtp_seq,
                                        timestamp: rtp_timestamp,
                                        ssrc: 1,
                                        ..Default::default()
                                    },
                                    payload: data.opus_data,
                                };

                                if let Err(e) = track.write_rtp(&rtp).await {
                                    warn!("Failed to write RTP to WebRTC: {}", e);
                                }
                            }
                            None => {
                                debug!("Mumble voice channel closed");
                                break;
                            }
                        }
                    }
                    _ = shutdown_rx.recv() => {
                        debug!("Bridge shutdown (mumble→webrtc)");
                        break;
                    }
                }
            }
        });

        // WebRTC → Mumble: forward Opus from browser to Mumble
        tokio::spawn(async move {
            let mut mumble_seq: u64 = 0;

            loop {
                match webrtc_audio_rx.recv().await {
                    Some(packet) => {
                        mumble_seq += 1;
                        let voice_data = WebrtcVoiceData {
                            seq_num: mumble_seq,
                            opus_data: packet.opus_data,
                            last_frame: false,
                        };
                        if mumble_voice_tx.send(voice_data).is_err() {
                            debug!("Mumble voice tx closed");
                            break;
                        }
                    }
                    None => {
                        debug!("WebRTC audio channel closed");
                        break;
                    }
                }
            }
        });

        AudioBridge { shutdown_tx }
    }

    pub async fn shutdown(&self) {
        let _ = self.shutdown_tx.send(()).await;
    }
}
