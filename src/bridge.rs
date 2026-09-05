use crate::mumble::voice::{MumbleVoiceData, WebrtcVoiceData};
use crate::webrtc::{audio::IncomingAudioPacket, speakers::SpeakerTracks};
use anyhow::{anyhow, bail, Result};
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::Duration;
use tracing::{debug, warn};

const OPUS_SAMPLE_RATE: u32 = 48_000;
const MAX_OPUS_PACKET_SAMPLES: u32 = (OPUS_SAMPLE_RATE / 1000) * 120;

#[cfg(test)]
fn opus_packet_duration(packet: &[u8]) -> Result<Duration> {
    let total_samples = opus_packet_total_samples(packet)?;
    Ok(Duration::from_nanos(
        (1_000_000_000u64 * total_samples as u64) / OPUS_SAMPLE_RATE as u64,
    ))
}

pub(crate) fn opus_packet_total_samples(packet: &[u8]) -> Result<u32> {
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

#[derive(Default)]
struct UplinkClock {
    last_timestamp: Option<u32>,
    last_sequence: Option<u16>,
    frame: u64,
    samples: u32,
}
impl UplinkClock {
    fn packet(&mut self, timestamp: u32, sequence: u16, samples: u32) -> Option<u64> {
        if let Some(last) = self.last_sequence {
            let delta = sequence.wrapping_sub(last);
            if delta == 0 || delta >= 0x8000 {
                return None;
            }
        }
        if let Some(last) = self.last_timestamp {
            let delta = timestamp.wrapping_sub(last);
            if delta == 0 || delta >= 0x8000_0000 || delta % 480 != 0 {
                return None;
            }
            self.frame += u64::from(delta / 480);
        }
        self.last_timestamp = Some(timestamp);
        self.last_sequence = Some(sequence);
        self.samples = samples;
        Some(self.frame)
    }
    fn end_frame(&self) -> u64 {
        self.frame + u64::from(self.samples / 480)
    }
}

pub struct AudioBridge {
    tasks: Vec<JoinHandle<()>>,
}
impl AudioBridge {
    pub fn start(
        mut mumble_voice_rx: mpsc::Receiver<MumbleVoiceData>,
        mumble_voice_tx: mpsc::Sender<WebrtcVoiceData>,
        mut webrtc_audio_rx: mpsc::Receiver<IncomingAudioPacket>,
        speakers: Arc<SpeakerTracks>,
    ) -> Self {
        let downlink = tokio::spawn(async move {
            while let Some(data) = mumble_voice_rx.recv().await {
                if let Err(error) = speakers.write(data).await {
                    warn!("Dropping invalid Mumble audio: {}", error);
                }
            }
        });
        let uplink = tokio::spawn(async move {
            let mut clock = UplinkClock::default();
            let mut last_packet = None;
            let mut tick = tokio::time::interval(Duration::from_millis(50));
            loop {
                tokio::select! {
                    packet = webrtc_audio_rx.recv() => {
                        let Some(packet) = packet else { break; };
                        if packet.received_at.elapsed().as_millis() > 120 { continue; }
                        let samples = match opus_packet_total_samples(&packet.opus_data) {
                            Ok(samples) if samples % 480 == 0 => samples,
                            _ => continue,
                        };
                        let Some(seq_num) = clock.packet(packet.timestamp, packet.seq_num, samples) else { continue; };
                        last_packet = Some(tokio::time::Instant::now());
                        let frame = WebrtcVoiceData { seq_num, opus_data: packet.opus_data, last_frame: false };
                        if let Err(mpsc::error::TrySendError::Closed(_)) = mumble_voice_tx.try_send(frame) { break; }
                    }
                    _ = tick.tick() => {
                        if last_packet.is_some_and(|last: tokio::time::Instant| last.elapsed() >= Duration::from_millis(200)) {
                            // End a talk spurt during DTX or an interrupted microphone stream.
                            let _ = mumble_voice_tx.try_send(WebrtcVoiceData { seq_num: clock.end_frame(), opus_data: bytes::Bytes::new(), last_frame: true });
                            last_packet = None;
                        }
                    }
                }
            }
            debug!("Audio uplink stopped");
        });
        Self {
            tasks: vec![downlink, uplink],
        }
    }
    pub async fn shutdown(mut self) {
        for task in &self.tasks {
            task.abort();
        }
        for task in self.tasks.drain(..) {
            let _ = task.await;
        }
    }
}
impl Drop for AudioBridge {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
    fn uplink_counts_ten_ms_frames_and_preserves_loss_dtx_and_wraparound() {
        let mut clock = UplinkClock::default();
        let start = u32::MAX - 479;
        assert_eq!(clock.packet(start, 65534, 960), Some(0));
        assert_eq!(clock.packet(start.wrapping_add(960), 65535, 1920), Some(2));
        assert_eq!(clock.packet(start.wrapping_add(2880), 0, 960), Some(6));
        assert_eq!(clock.packet(start.wrapping_add(5760), 3, 960), Some(12));
        assert_eq!(clock.packet(start.wrapping_add(4800), 2, 960), None);
        assert_eq!(clock.packet(start.wrapping_add(53760), 4, 960), Some(112));
        assert_eq!(clock.end_frame(), 114);
    }
}
