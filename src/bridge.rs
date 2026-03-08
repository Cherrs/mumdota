use anyhow::{anyhow, bail, Result};
use std::sync::Arc;
use std::time::SystemTime;
use tokio::sync::mpsc;
use tokio::time::{sleep_until, Duration, Instant};
use tracing::{debug, warn};
use webrtc::track::track_local::track_local_static_sample::TrackLocalStaticSample;
use webrtc_media::Sample;

use crate::mumble::voice::{MumbleVoiceData, WebrtcVoiceData};
use crate::webrtc::audio::IncomingAudioPacket;

const OPUS_SAMPLE_RATE: u32 = 48_000;
const MAX_OPUS_PACKET_SAMPLES: u32 = (OPUS_SAMPLE_RATE / 1000) * 120;

#[derive(Default)]
struct SendPacer {
    next_send_at: Option<Instant>,
}

impl SendPacer {
    fn schedule(&mut self, now: Instant, packet_duration: Duration) -> Instant {
        let send_at = match self.next_send_at {
            Some(next_send_at) if next_send_at > now => next_send_at,
            _ => now,
        };
        self.next_send_at = Some(send_at + packet_duration);
        send_at
    }
}

fn opus_packet_duration(packet: &[u8]) -> Result<Duration> {
    let total_samples = opus_packet_total_samples(packet)?;
    Ok(Duration::from_nanos(
        (1_000_000_000u64 * total_samples as u64) / OPUS_SAMPLE_RATE as u64,
    ))
}

fn opus_packet_total_samples(packet: &[u8]) -> Result<u32> {
    let frame_count = opus_packet_frame_count(packet)? as u32;
    let samples_per_frame = opus_packet_samples_per_frame(packet)?;
    let total_samples = frame_count * samples_per_frame;
    if total_samples > MAX_OPUS_PACKET_SAMPLES {
        bail!("Opus packet duration exceeds 120ms");
    }
    Ok(total_samples)
}

fn opus_packet_frame_count(packet: &[u8]) -> Result<u8> {
    let Some(&toc) = packet.first() else {
        bail!("empty Opus packet");
    };

    match toc & 0x03 {
        0 => Ok(1),
        1 | 2 => Ok(2),
        3 => {
            let Some(&count_byte) = packet.get(1) else {
                bail!("Opus packet with VBR/CBR frame count is missing the count byte");
            };
            let frame_count = count_byte & 0x3F;
            if frame_count == 0 {
                bail!("Opus packet declares zero frames");
            }
            Ok(frame_count)
        }
        _ => Err(anyhow!("invalid Opus frame count code")),
    }
}

fn opus_packet_samples_per_frame(packet: &[u8]) -> Result<u32> {
    let Some(&toc) = packet.first() else {
        bail!("empty Opus packet");
    };

    let config = toc >> 3;
    let samples_per_frame = if config >= 16 {
        (OPUS_SAMPLE_RATE << (config & 0x03)) / 400
    } else if config >= 12 {
        (OPUS_SAMPLE_RATE << (config & 0x01)) / 100
    } else if (config & 0x03) == 0x03 {
        (OPUS_SAMPLE_RATE * 60) / 1000
    } else {
        (OPUS_SAMPLE_RATE << (config & 0x03)) / 100
    };

    Ok(samples_per_frame)
}

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
        mut mumble_voice_rx: mpsc::Receiver<MumbleVoiceData>,
        mumble_voice_tx: mpsc::Sender<WebrtcVoiceData>,
        mut webrtc_audio_rx: mpsc::UnboundedReceiver<IncomingAudioPacket>,
        webrtc_track: Arc<TrackLocalStaticSample>,
    ) -> Self {
        let (shutdown_tx, mut shutdown_rx) = mpsc::channel::<()>(1);

        // Mumble → WebRTC: forward Opus from Mumble to browser
        let track = webrtc_track.clone();
        let _shutdown_rx2 = shutdown_tx.clone();
        tokio::spawn(async move {
            let mut pacer = SendPacer::default();

            loop {
                tokio::select! {
                    voice = mumble_voice_rx.recv() => {
                        match voice {
                            Some(data) => {
                                let packet_duration = match opus_packet_duration(&data.opus_data) {
                                    Ok(packet_duration) => packet_duration,
                                    Err(err) => {
                                        warn!("Dropping invalid Opus packet from Mumble: {}", err);
                                        continue;
                                    }
                                };

                                let send_at = pacer.schedule(Instant::now(), packet_duration);
                                let sleep = sleep_until(send_at);
                                tokio::pin!(sleep);

                                tokio::select! {
                                    _ = &mut sleep => {}
                                    _ = shutdown_rx.recv() => {
                                        debug!("Bridge shutdown (mumble→webrtc)");
                                        break;
                                    }
                                }

                                let sample = Sample {
                                    data: data.opus_data,
                                    duration: packet_duration,
                                    timestamp: SystemTime::now(),
                                    ..Default::default()
                                };

                                if let Err(e) = track.write_sample(&sample).await {
                                    warn!("Failed to write Opus sample to WebRTC: {}", e);
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
                        // Use try_send: drop packets rather than blocking
                        // when the Mumble UDP sender can't keep up
                        match mumble_voice_tx.try_send(voice_data) {
                            Ok(()) => {}
                            Err(mpsc::error::TrySendError::Closed(_)) => {
                                debug!("Mumble voice tx closed");
                                break;
                            }
                            Err(mpsc::error::TrySendError::Full(_)) => {
                                debug!("Mumble voice buffer full, dropping outgoing packet");
                            }
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

#[cfg(test)]
mod tests {
    use super::{opus_packet_duration, SendPacer};
    use tokio::time::{Duration, Instant};

    #[test]
    fn parses_single_frame_opus_duration() {
        let packet = [0xF8];

        let duration = opus_packet_duration(&packet).expect("duration should parse");

        assert_eq!(duration, Duration::from_millis(20));
    }

    #[test]
    fn parses_two_frame_opus_duration() {
        let packet = [0xF9];

        let duration = opus_packet_duration(&packet).expect("duration should parse");

        assert_eq!(duration, Duration::from_millis(40));
    }

    #[test]
    fn parses_code_three_opus_duration() {
        let packet = [0xF3, 0x03];

        let duration = opus_packet_duration(&packet).expect("duration should parse");

        assert_eq!(duration, Duration::from_millis(30));
    }

    #[test]
    fn rejects_empty_opus_packets() {
        let error = opus_packet_duration(&[]).expect_err("empty packet should be invalid");

        assert!(error.to_string().contains("empty"));
    }

    #[test]
    fn rejects_opus_packets_longer_than_120ms() {
        let packet = [0xF3, 0x3F];

        let error = opus_packet_duration(&packet).expect_err("oversized packet should be invalid");

        assert!(error.to_string().contains("120ms"));
    }

    #[test]
    fn pacer_keeps_bursty_packets_on_media_clock() {
        let mut pacer = SendPacer::default();
        let start = Instant::now();

        let first = pacer.schedule(start, Duration::from_millis(20));
        let second = pacer.schedule(start + Duration::from_millis(1), Duration::from_millis(40));

        assert_eq!(first, start);
        assert_eq!(second, start + Duration::from_millis(20));
    }

    #[test]
    fn pacer_resets_when_stream_falls_behind() {
        let mut pacer = SendPacer::default();
        let start = Instant::now();

        let _ = pacer.schedule(start, Duration::from_millis(20));
        let scheduled = pacer.schedule(start + Duration::from_millis(80), Duration::from_millis(20));

        assert_eq!(scheduled, start + Duration::from_millis(80));
    }
}
