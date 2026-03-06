pub mod audio;

use std::sync::Arc;
use anyhow::{Context, Result};
use tokio::sync::mpsc;
use tracing::{debug, info};
use webrtc::api::interceptor_registry::register_default_interceptors;
use webrtc::api::media_engine::MediaEngine;
use webrtc::api::APIBuilder;
use webrtc::ice_transport::ice_candidate::{RTCIceCandidate, RTCIceCandidateInit};
use webrtc::ice_transport::ice_server::RTCIceServer;
use webrtc::interceptor::registry::Registry;
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;
use webrtc::peer_connection::RTCPeerConnection;
use webrtc::track::track_local::track_local_static_rtp::TrackLocalStaticRTP;
use webrtc::track::track_local::TrackLocal;

use crate::config::WebrtcConfig;
use crate::ws::messages::IceCandidateResponse;

pub use audio::IncomingAudioPacket;

/// Events from WebRTC layer
#[derive(Debug)]
pub enum WebrtcEvent {
    IceCandidate(IceCandidateResponse),
    ConnectionStateChanged(RTCPeerConnectionState),
}

/// Manages a WebRTC PeerConnection for one user
pub struct WebrtcSession {
    pub peer_connection: Arc<RTCPeerConnection>,
    pub outgoing_track: Arc<TrackLocalStaticRTP>,
    pub audio_rx: mpsc::UnboundedReceiver<IncomingAudioPacket>,
    pub event_rx: mpsc::UnboundedReceiver<WebrtcEvent>,
}

impl WebrtcSession {
    pub async fn new(config: &WebrtcConfig) -> Result<Self> {
        let mut m = MediaEngine::default();
        m.register_default_codecs()
            .context("Failed to register codecs")?;

        let mut registry = Registry::new();
        registry = register_default_interceptors(registry, &mut m)
            .context("Failed to register interceptors")?;

        let api = APIBuilder::new()
            .with_media_engine(m)
            .with_interceptor_registry(registry)
            .build();

        let ice_servers: Vec<RTCIceServer> = config
            .stun_servers
            .iter()
            .map(|url| RTCIceServer {
                urls: vec![url.clone()],
                ..Default::default()
            })
            .collect();

        let rtc_config = RTCConfiguration {
            ice_servers,
            ..Default::default()
        };

        let peer_connection = Arc::new(
            api.new_peer_connection(rtc_config)
                .await
                .context("Failed to create PeerConnection")?,
        );

        // Create outgoing audio track (proxy → browser)
        let outgoing_track = audio::create_opus_track("mumble-audio", "mumble-stream");
        peer_connection
            .add_track(Arc::clone(&outgoing_track) as Arc<dyn TrackLocal + Send + Sync>)
            .await
            .context("Failed to add audio track")?;

        // Set up incoming audio handler (browser → proxy)
        let (audio_tx, audio_rx) = mpsc::unbounded_channel();
        peer_connection.on_track(audio::setup_incoming_audio_handler(audio_tx));

        // Set up ICE candidate and connection state event forwarding
        let (event_tx, event_rx) = mpsc::unbounded_channel();

        let event_tx_ice = event_tx.clone();
        peer_connection.on_ice_candidate(Box::new(move |candidate: Option<RTCIceCandidate>| {
            let tx = event_tx_ice.clone();
            Box::pin(async move {
                if let Some(c) = candidate {
                    if let Ok(json) = c.to_json() {
                        let _ = tx.send(WebrtcEvent::IceCandidate(IceCandidateResponse {
                            candidate: json.candidate,
                            sdp_mid: json.sdp_mid,
                            sdp_mline_index: json.sdp_mline_index,
                        }));
                    }
                }
            })
        }));

        let event_tx_state = event_tx.clone();
        peer_connection.on_peer_connection_state_change(Box::new(
            move |state: RTCPeerConnectionState| {
                let tx = event_tx_state.clone();
                info!("WebRTC connection state: {}", state);
                Box::pin(async move {
                    let _ = tx.send(WebrtcEvent::ConnectionStateChanged(state));
                })
            },
        ));

        Ok(WebrtcSession {
            peer_connection,
            outgoing_track,
            audio_rx,
            event_rx,
        })
    }

    /// Process an SDP offer from the browser and return an answer
    pub async fn handle_offer(&self, sdp_offer: &str) -> Result<String> {
        let offer = RTCSessionDescription::offer(sdp_offer.to_string())
            .context("Invalid SDP offer")?;

        self.peer_connection
            .set_remote_description(offer)
            .await
            .context("Failed to set remote description")?;

        let answer = self
            .peer_connection
            .create_answer(None)
            .await
            .context("Failed to create answer")?;

        let mut gather_complete = self.peer_connection.gathering_complete_promise().await;

        self.peer_connection
            .set_local_description(answer)
            .await
            .context("Failed to set local description")?;

        let _ = gather_complete.recv().await;

        let local_desc = self
            .peer_connection
            .local_description()
            .await
            .ok_or_else(|| anyhow::anyhow!("No local description"))?;

        debug!("SDP answer created");
        Ok(local_desc.sdp)
    }

    /// Add a remote ICE candidate from the browser
    pub async fn add_ice_candidate(
        &self,
        candidate: &str,
        sdp_mid: Option<String>,
        sdp_mline_index: Option<u16>,
    ) -> Result<()> {
        let init = RTCIceCandidateInit {
            candidate: candidate.to_string(),
            sdp_mid,
            sdp_mline_index,
            ..Default::default()
        };
        self.peer_connection
            .add_ice_candidate(init)
            .await
            .context("Failed to add ICE candidate")?;
        Ok(())
    }

    pub async fn close(&self) -> Result<()> {
        self.peer_connection.close().await?;
        Ok(())
    }
}
