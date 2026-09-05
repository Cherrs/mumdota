use crate::bridge::opus_packet_total_samples;
use crate::mumble::voice::MumbleVoiceData;
use anyhow::Result;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use webrtc::peer_connection::RTCPeerConnection;
use webrtc::rtp::{header::Header, packet::Packet};
use webrtc::rtp_transceiver::rtp_codec::RTCRtpCodecCapability;
use webrtc::rtp_transceiver::rtp_sender::RTCRtpSender;
use webrtc::track::track_local::{
    track_local_static_rtp::TrackLocalStaticRTP, TrackLocal, TrackLocalWriter,
};

#[derive(Default)]
struct MediaClock {
    last_frame: Option<u64>,
    timestamp: u32,
    sequence: u16,
    samples: u32,
    ended: bool,
    received_at: Option<Instant>,
}

impl MediaClock {
    fn packet(
        &mut self,
        frame: u64,
        samples: u32,
        ended: bool,
        now: Instant,
    ) -> Option<(u16, u32, bool)> {
        let new_talk = self.last_frame.is_none()
            || self.ended
            || self
                .received_at
                .is_some_and(|last| now.duration_since(last).as_millis() > 500);
        if let Some(last) = self.last_frame {
            if new_talk {
                let elapsed = self.received_at.map_or(0, |t| {
                    (now.duration_since(t).as_secs_f64() * 48000.0) as u32
                });
                self.timestamp = self.timestamp.wrapping_add(elapsed.max(self.samples));
            } else {
                if frame <= last {
                    return None;
                }
                self.timestamp = self
                    .timestamp
                    .wrapping_add(frame.wrapping_sub(last).wrapping_mul(480) as u32);
            }
        }
        self.last_frame = Some(frame);
        self.samples = samples;
        self.ended = ended;
        self.received_at = Some(now);
        self.sequence = self.sequence.wrapping_add(1);
        Some((self.sequence, self.timestamp, new_talk))
    }
}

struct Speaker {
    track: Arc<TrackLocalStaticRTP>,
    sender: Arc<RTCRtpSender>,
    rtcp: JoinHandle<()>,
    clock: MediaClock,
}
impl Drop for Speaker {
    fn drop(&mut self) {
        self.rtcp.abort();
    }
}

/// A distinct RTP track and media clock for each Mumble speaker.
pub struct SpeakerTracks {
    peer: Arc<RTCPeerConnection>,
    speakers: Mutex<HashMap<u32, Speaker>>,
}

impl SpeakerTracks {
    pub fn new(peer: Arc<RTCPeerConnection>) -> Self {
        Self {
            peer,
            speakers: Mutex::new(HashMap::new()),
        }
    }
    pub async fn add(&self, session: u32) -> Result<()> {
        let mut speakers = self.speakers.lock().await;
        if speakers.contains_key(&session) {
            return Ok(());
        }
        let track = Arc::new(TrackLocalStaticRTP::new(
            RTCRtpCodecCapability {
                mime_type: "audio/opus".into(),
                clock_rate: 48000,
                channels: 2,
                sdp_fmtp_line: "minptime=10;useinbandfec=1".into(),
                ..Default::default()
            },
            format!("mumble-user-{session}"),
            format!("mumble-stream-{session}"),
        ));
        let sender = self
            .peer
            .add_track(track.clone() as Arc<dyn TrackLocal + Send + Sync>)
            .await?;
        let read_sender = sender.clone();
        let rtcp = tokio::spawn(async move { while read_sender.read_rtcp().await.is_ok() {} });
        speakers.insert(
            session,
            Speaker {
                track,
                sender,
                rtcp,
                clock: MediaClock::default(),
            },
        );
        Ok(())
    }
    pub async fn remove(&self, session: u32) -> Result<()> {
        if let Some(speaker) = self.speakers.lock().await.remove(&session) {
            self.peer.remove_track(&speaker.sender).await?;
        }
        Ok(())
    }
    pub async fn write(&self, data: MumbleVoiceData) -> Result<()> {
        if data.received_at.elapsed().as_millis() > 120 {
            return Ok(());
        }
        let mut speakers = self.speakers.lock().await;
        let Some(speaker) = speakers.get_mut(&data.session_id) else {
            return Ok(());
        };
        if data.opus_data.is_empty() {
            speaker.clock.ended |= data.last_frame;
            return Ok(());
        }
        let samples = opus_packet_total_samples(&data.opus_data)?;
        let Some((sequence_number, timestamp, marker)) =
            speaker
                .clock
                .packet(data.seq_num, samples, data.last_frame, Instant::now())
        else {
            return Ok(());
        };
        let track = speaker.track.clone();
        drop(speakers);
        track
            .write_rtp(&Packet {
                header: Header {
                    version: 2,
                    sequence_number,
                    timestamp,
                    marker,
                    ..Default::default()
                },
                payload: data.opus_data,
            })
            .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    #[test]
    fn speakers_keep_independent_clocks_and_preserve_gaps() {
        let now = Instant::now();
        let mut a = MediaClock::default();
        let mut b = MediaClock::default();
        assert_eq!(a.packet(100, 960, false, now).unwrap().1, 0);
        assert_eq!(b.packet(5, 960, false, now).unwrap().1, 0);
        assert_eq!(
            a.packet(102, 960, false, now + Duration::from_millis(20))
                .unwrap()
                .1,
            960
        );
        assert_eq!(
            b.packet(9, 960, false, now + Duration::from_millis(40))
                .unwrap()
                .1,
            1920
        );
        assert!(a
            .packet(101, 960, false, now + Duration::from_millis(30))
            .is_none());
    }
    #[test]
    fn talk_restart_uses_a_continuous_rtp_clock() {
        let now = Instant::now();
        let mut clock = MediaClock::default();
        clock.packet(10, 960, true, now).unwrap();
        let (_, timestamp, marker) = clock
            .packet(0, 960, false, now + Duration::from_secs(1))
            .unwrap();
        assert_eq!(timestamp, 48000);
        assert!(marker);
    }
}
