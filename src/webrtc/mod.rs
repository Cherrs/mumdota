pub mod audio;
pub mod speakers;

use crate::config::WebrtcConfig;
use anyhow::{Context, Result};
pub use audio::IncomingAudioPacket;
use speakers::SpeakerTracks;
use std::sync::Arc;
use tokio::sync::mpsc;
use webrtc::api::{
    interceptor_registry::register_default_interceptors, media_engine::MediaEngine,
    setting_engine::SettingEngine, APIBuilder, API,
};
use webrtc::ice::{
    network_type::NetworkType,
    udp_mux::{UDPMux, UDPMuxDefault, UDPMuxParams},
    udp_network::UDPNetwork,
};
use webrtc::ice_transport::{
    ice_candidate::RTCIceCandidateInit, ice_candidate_type::RTCIceCandidateType,
};
use webrtc::interceptor::registry::Registry;
use webrtc::peer_connection::{
    configuration::RTCConfiguration, offer_answer_options::RTCOfferOptions,
    peer_connection_state::RTCPeerConnectionState, sdp::session_description::RTCSessionDescription,
    signaling_state::RTCSignalingState, RTCPeerConnection,
};
use webrtc::rtp_transceiver::{
    rtp_codec::RTPCodecType, rtp_transceiver_direction::RTCRtpTransceiverDirection,
    RTCRtpTransceiverInit,
};

#[derive(Debug)]
pub enum WebrtcEvent {
    ConnectionStateChanged(RTCPeerConnectionState),
    IceCandidate(RTCIceCandidateInit),
    NegotiationNeeded,
}

pub struct MediaApi {
    pub api: API,
    mux: Arc<UDPMuxDefault>,
}
impl MediaApi {
    pub async fn close(&self) {
        let _ = self.mux.close().await;
    }
}

pub async fn create_api(config: &WebrtcConfig) -> Result<MediaApi> {
    create_api_with_network(config, None).await
}

async fn create_api_with_network(
    config: &WebrtcConfig,
    network: Option<Arc<webrtc_util::vnet::net::Net>>,
) -> Result<MediaApi> {
    let mut media = MediaEngine::default();
    media.register_default_codecs()?;
    let registry = register_default_interceptors(Registry::new(), &mut media)?;
    let socket = tokio::net::UdpSocket::bind((std::net::Ipv4Addr::UNSPECIFIED, config.udp_port))
        .await
        .context("bind WebRTC UDP media port")?;
    let mut settings = SettingEngine::default();
    settings.set_network_types(vec![NetworkType::Udp4]);
    if network.is_some() {
        settings.set_include_loopback_candidate(true);
    }
    settings.set_vnet(network);
    settings.set_ice_multicast_dns_mode(webrtc::ice::mdns::MulticastDnsMode::Disabled);
    let mux = UDPMuxDefault::new(UDPMuxParams::new(socket));
    settings.set_udp_network(UDPNetwork::Muxed(mux.clone()));
    if config.public_ip.is_some_and(|ip| ip.is_loopback()) {
        settings.set_include_loopback_candidate(true);
    }
    if let Some(ip) = config.public_ip {
        settings.set_nat_1to1_ips(vec![ip.to_string()], RTCIceCandidateType::Host);
    }
    Ok(MediaApi {
        api: APIBuilder::new()
            .with_media_engine(media)
            .with_interceptor_registry(registry)
            .with_setting_engine(settings)
            .build(),
        mux,
    })
}

pub struct WebrtcSession {
    pub peer_connection: Arc<RTCPeerConnection>,
    pub speakers: Arc<SpeakerTracks>,
    pub audio_rx: mpsc::Receiver<IncomingAudioPacket>,
    pub event_rx: mpsc::UnboundedReceiver<WebrtcEvent>,
    pub started: bool,
    needs_offer: bool,
    restart_pending: bool,
    pending_candidates: Vec<RTCIceCandidateInit>,
}

impl WebrtcSession {
    pub async fn new(api: &API) -> Result<Self> {
        let peer = Arc::new(api.new_peer_connection(RTCConfiguration::default()).await?);
        let (audio_tx, audio_rx) = mpsc::channel(6);
        peer.on_track(audio::setup_incoming_audio_handler(audio_tx));
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let tx = event_tx.clone();
        peer.on_ice_candidate(Box::new(move |candidate| {
            let tx = tx.clone();
            Box::pin(async move {
                if let Some(candidate) = candidate {
                    if let Ok(init) = candidate.to_json() {
                        let _ = tx.send(WebrtcEvent::IceCandidate(init));
                    }
                }
            })
        }));
        let tx = event_tx.clone();
        peer.on_negotiation_needed(Box::new(move || {
            let tx = tx.clone();
            Box::pin(async move {
                let _ = tx.send(WebrtcEvent::NegotiationNeeded);
            })
        }));
        peer.on_peer_connection_state_change(Box::new(move |state| {
            let tx = event_tx.clone();
            Box::pin(async move {
                let _ = tx.send(WebrtcEvent::ConnectionStateChanged(state));
            })
        }));
        peer.add_transceiver_from_kind(
            RTPCodecType::Audio,
            Some(RTCRtpTransceiverInit {
                direction: RTCRtpTransceiverDirection::Recvonly,
                send_encodings: Vec::new(),
            }),
        )
        .await?;
        Ok(Self {
            speakers: Arc::new(SpeakerTracks::new(peer.clone())),
            peer_connection: peer,
            audio_rx,
            event_rx,
            started: false,
            needs_offer: false,
            restart_pending: false,
            pending_candidates: Vec::new(),
        })
    }

    pub async fn offer(&mut self, restart: bool) -> Result<Option<String>> {
        self.restart_pending |= restart;
        if !self.started {
            return Ok(None);
        }
        if self.peer_connection.signaling_state() != RTCSignalingState::Stable {
            self.needs_offer = true;
            return Ok(None);
        }
        let offer = self
            .peer_connection
            .create_offer(Some(RTCOfferOptions {
                ice_restart: self.restart_pending,
                ..Default::default()
            }))
            .await?;
        self.restart_pending = false;
        self.needs_offer = false;
        let sdp = offer.sdp.clone();
        self.peer_connection.set_local_description(offer).await?;
        Ok(Some(sdp))
    }

    pub async fn answer(&mut self, sdp: &str) -> Result<Option<String>> {
        self.peer_connection
            .set_remote_description(RTCSessionDescription::answer(sdp.to_string())?)
            .await?;
        for candidate in self.pending_candidates.drain(..) {
            self.peer_connection.add_ice_candidate(candidate).await?;
        }
        if self.needs_offer {
            self.offer(false).await
        } else {
            Ok(None)
        }
    }

    pub async fn add_ice_candidate(
        &mut self,
        candidate: &str,
        sdp_mid: Option<String>,
        sdp_mline_index: Option<u16>,
    ) -> Result<()> {
        let init = RTCIceCandidateInit {
            candidate: candidate.into(),
            sdp_mid,
            sdp_mline_index,
            ..Default::default()
        };
        if self.peer_connection.remote_description().await.is_none()
            || self.peer_connection.signaling_state() == RTCSignalingState::HaveLocalOffer
        {
            anyhow::ensure!(
                self.pending_candidates.len() < 128,
                "too many pending ICE candidates"
            );
            self.pending_candidates.push(init);
        } else {
            self.peer_connection.add_ice_candidate(init).await?;
        }
        Ok(())
    }

    pub async fn close(&self) -> Result<()> {
        self.peer_connection.close().await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests;
