use super::*;
use crate::{config::Config, mumble::voice::MumbleVoiceData, turn::TurnService};
use bytes::Bytes;
use std::{collections::HashMap, net::Ipv4Addr, time::Instant};
use tokio::{
    net::{TcpListener, UdpSocket},
    time::{timeout, Duration},
};
use webrtc::{
    ice_transport::ice_server::RTCIceServer,
    peer_connection::policy::ice_transport_policy::RTCIceTransportPolicy,
    rtp::{header::Header, packet::Packet},
    rtp_transceiver::rtp_codec::RTCRtpCodecCapability,
    track::track_local::{track_local_static_rtp::TrackLocalStaticRTP, TrackLocalWriter},
};

async fn negotiate(server: &mut WebrtcSession, browser: &RTCPeerConnection, restart: bool) {
    // Production trickles these candidates over WS. Gather them in SDP here so
    // the test exercises real ICE/DTLS/SRTP without a fake network transport.
    server.offer(restart).await.unwrap().unwrap();
    let mut complete = server.peer_connection.gathering_complete_promise().await;
    complete.recv().await;
    browser
        .set_remote_description(server.peer_connection.local_description().await.unwrap())
        .await
        .unwrap();
    let answer = browser.create_answer(None).await.unwrap();
    let mut complete = browser.gathering_complete_promise().await;
    browser.set_local_description(answer).await.unwrap();
    complete.recv().await;
    server
        .answer(&browser.local_description().await.unwrap().sdp)
        .await
        .unwrap();
}

#[tokio::test]
async fn two_speakers_and_uplink_work_through_builtin_turn_without_public_hairpin() {
    timeout(Duration::from_secs(25), async {
        let mut config = Config::load(concat!(env!("CARGO_MANIFEST_DIR"), "/config.toml")).unwrap();
        let media = UdpSocket::bind("0.0.0.0:0").await.unwrap();
        config.webrtc.udp_port = media.local_addr().unwrap().port();
        drop(media);
        // This address is deliberately not assigned locally. TURN's loopback
        // mapping must work without a router hairpinning the public address.
        config.webrtc.public_ip = Some(Ipv4Addr::new(198, 51, 100, 9));
        config.turn.enabled = true;
        config.turn.public_ip = config.webrtc.public_ip;
        config.turn.public_host = "127.0.0.1".into();
        let port = TcpListener::bind("127.0.0.1:0").await.unwrap();
        config.turn.port = port.local_addr().unwrap().port();
        drop(port);
        config.turn.relay_min_port = 57000;
        config.turn.relay_max_port = 57999;
        if (57000..=57999).contains(&config.webrtc.udp_port)
            || (57000..=57999).contains(&config.turn.port)
        {
            config.turn.relay_min_port = 58000;
            config.turn.relay_max_port = 58999;
        }
        let turn = TurnService::start(&config).await.unwrap();
        // Supply interface metadata because some CI sandboxes forbid interface
        // enumeration. The UDP mux, TURN and all media sockets remain real.
        let interfaces = Arc::new(webrtc_util::vnet::net::Net::new(Some(Default::default())));
        let media_api = create_api_with_network(&config.webrtc, Some(interfaces))
            .await
            .unwrap();
        let mut server = WebrtcSession::new(&media_api.api).await.unwrap();
        server.speakers.add(10).await.unwrap();
        server.speakers.add(20).await.unwrap();
        server.started = true;
        let mut media = MediaEngine::default();
        media.register_default_codecs().unwrap();
        let api = APIBuilder::new().with_media_engine(media).build();
        let credentials = turn.ice_config("browser");
        let ice = &credentials.ice_servers[1];
        let browser = api
            .new_peer_connection(RTCConfiguration {
                ice_servers: vec![RTCIceServer {
                    urls: vec![ice.urls.clone()],
                    username: ice.username.clone().unwrap(),
                    credential: ice.credential.clone().unwrap(),
                }],
                ice_transport_policy: RTCIceTransportPolicy::Relay,
                ..Default::default()
            })
            .await
            .unwrap();
        let uplink = Arc::new(TrackLocalStaticRTP::new(
            RTCRtpCodecCapability {
                mime_type: "audio/opus".into(),
                clock_rate: 48000,
                channels: 2,
                ..Default::default()
            },
            "mic".into(),
            "mic-stream".into(),
        ));
        browser.add_track(uplink.clone()).await.unwrap();
        let (tx, mut rx) = mpsc::unbounded_channel();
        browser.on_track(Box::new(move |track, _, _| {
            let tx = tx.clone();
            Box::pin(async move {
                tokio::spawn(async move {
                    for _ in 0..2 {
                        let (packet, _) = track.read_rtp().await.unwrap();
                        tx.send((track.stream_id(), packet)).unwrap();
                    }
                });
            })
        }));
        negotiate(&mut server, &browser, false).await;
        let mut connected = tokio::time::interval(Duration::from_millis(10));
        while browser.connection_state() != RTCPeerConnectionState::Connected {
            connected.tick().await;
        }
        assert!(browser
            .local_description()
            .await
            .unwrap()
            .sdp
            .contains("typ relay"));
        // Let SRTP bindings start before sending the short test burst.
        let (ready_tx, mut ready_rx) = mpsc::unbounded_channel();
        let send_uplink = uplink.clone();
        let pump = tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_millis(20));
            let mut seq = 1u16;
            loop {
                tick.tick().await;
                send_uplink
                    .write_rtp(&Packet {
                        header: Header {
                            version: 2,
                            sequence_number: seq,
                            timestamp: u32::from(seq) * 960,
                            ..Default::default()
                        },
                        payload: Bytes::from_static(&[0xf8, 0xff, 0xfe]),
                    })
                    .await
                    .unwrap();
                seq = seq.wrapping_add(1);
                let _ = ready_tx.send(());
            }
        });
        let incoming = server.audio_rx.recv().await.unwrap();
        assert_eq!(incoming.opus_data, Bytes::from_static(&[0xf8, 0xff, 0xfe]));
        ready_rx.recv().await.unwrap();
        for offset in [0, 2] {
            for (session_id, base) in [(10, 100), (20, 500)] {
                server
                    .speakers
                    .write(MumbleVoiceData {
                        session_id,
                        seq_num: base + offset,
                        opus_data: Bytes::from_static(&[0xf8, 0xff, 0xfe]),
                        last_frame: false,
                        received_at: Instant::now(),
                    })
                    .await
                    .unwrap();
            }
        }
        let mut streams: HashMap<String, Vec<Packet>> = HashMap::new();
        for _ in 0..4 {
            let (stream, packet) = rx.recv().await.unwrap();
            streams.entry(stream).or_default().push(packet);
        }
        assert_eq!(streams.len(), 2);
        for name in ["mumble-stream-10", "mumble-stream-20"] {
            let packets = &streams[name];
            assert_eq!(packets.len(), 2);
            assert_eq!(
                packets[1].header.timestamp - packets[0].header.timestamp,
                960
            );
        }
        assert_ne!(
            streams["mumble-stream-10"][0].header.ssrc,
            streams["mumble-stream-20"][0].header.ssrc
        );
        // A new speaker needs another offer; ICE restart must preserve audio.
        server.speakers.add(30).await.unwrap();
        negotiate(&mut server, &browser, false).await;
        negotiate(&mut server, &browser, true).await;
        while browser.connection_state() != RTCPeerConnectionState::Connected {
            connected.tick().await;
        }
        while server.audio_rx.try_recv().is_ok() {}
        server.audio_rx.recv().await.unwrap();
        pump.abort();
        browser.close().await.unwrap();
        server.close().await.unwrap();
        media_api.close().await;
        turn.close().await;
    })
    .await
    .expect("WebRTC TURN media test timed out");
}
