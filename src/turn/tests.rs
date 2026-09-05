use super::*;
use openssl::{asn1::Asn1Time, hash::MessageDigest, pkey::PKey, rsa::Rsa, x509::X509};
use std::net::{Ipv4Addr, SocketAddr};
use turn::client::{Client, ClientConfig};

async fn free_tcp_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .await
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

// Real authenticated TURN allocations and bidirectional media over every
// supported listener, using ephemeral credentials, sockets and a test TLS key.
#[tokio::test]
async fn builtin_turn_relays_udp_tcp_and_tls_and_revokes_allocations() {
    timeout(Duration::from_secs(15), async {
        let media = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let target = media.local_addr().unwrap();
        let mut config = Config::load(concat!(env!("CARGO_MANIFEST_DIR"), "/config.toml")).unwrap();
        config.webrtc.public_ip = Some(Ipv4Addr::LOCALHOST);
        config.webrtc.udp_port = target.port();
        config.turn.enabled = true;
        config.turn.public_ip = Some(Ipv4Addr::LOCALHOST);
        config.turn.public_host = "127.0.0.1".into();
        config.turn.port = free_tcp_port().await;
        config.turn.tls_port = free_tcp_port().await;
        config.turn.relay_min_port = 55000;
        config.turn.relay_max_port = 55999;
        // Keep the range clear of randomly chosen test listeners.
        if (55000..=55999).contains(&target.port()) || (55000..=55999).contains(&config.turn.port) {
            config.turn.relay_min_port = 56000;
            config.turn.relay_max_port = 56999;
        }
        let key = PKey::from_rsa(Rsa::generate(2048).unwrap()).unwrap();
        let mut name = openssl::x509::X509NameBuilder::new().unwrap();
        name.append_entry_by_text("CN", "localhost").unwrap();
        let name = name.build();
        let mut cert = X509::builder().unwrap();
        cert.set_version(2).unwrap();
        cert.set_subject_name(&name).unwrap();
        cert.set_issuer_name(&name).unwrap();
        cert.set_pubkey(&key).unwrap();
        cert.set_not_before(&Asn1Time::days_from_now(0).unwrap())
            .unwrap();
        cert.set_not_after(&Asn1Time::days_from_now(1).unwrap())
            .unwrap();
        cert.sign(&key, MessageDigest::sha256()).unwrap();
        let directory =
            std::env::temp_dir().join(format!("mumdota-turn-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&directory).unwrap();
        let cert_path = directory.join("cert.pem");
        let key_path = directory.join("key.pem");
        std::fs::write(&cert_path, cert.build().to_pem().unwrap()).unwrap();
        std::fs::write(&key_path, key.private_key_to_pem_pkcs8().unwrap()).unwrap();
        config.turn.tls_cert = Some(cert_path.to_str().unwrap().into());
        config.turn.tls_key = Some(key_path.to_str().unwrap().into());
        let service = TurnService::start(&config).await.unwrap();
        std::fs::remove_dir_all(directory).unwrap();
        for transport in ["udp", "tcp", "tls"] {
            let port = if transport == "tls" {
                config.turn.tls_port
            } else {
                config.turn.port
            };
            let server_addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
            let conn: Arc<dyn Conn + Send + Sync> = if transport == "udp" {
                Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap())
            } else {
                let socket = tokio::net::TcpStream::connect(server_addr).await.unwrap();
                let local = socket.local_addr().unwrap();
                let io: Box<dyn IoStream> = if transport == "tls" {
                    let tls = native_tls::TlsConnector::builder()
                        .danger_accept_invalid_certs(true)
                        .build()
                        .unwrap();
                    Box::new(
                        tokio_native_tls::TlsConnector::from(tls)
                            .connect("localhost", socket)
                            .await
                            .unwrap(),
                    )
                } else {
                    Box::new(socket)
                };
                StreamConn::new(io, local, server_addr)
            };
            let ice = service.ice_config(transport);
            assert_eq!(ice.ice_servers.len(), 4);
            let credential = &ice.ice_servers[1];
            let client = Client::new(ClientConfig {
                stun_serv_addr: server_addr.to_string(),
                turn_serv_addr: server_addr.to_string(),
                username: credential.username.clone().unwrap(),
                password: credential.credential.clone().unwrap(),
                realm: String::new(),
                software: "mumdota-test".into(),
                rto_in_ms: 20,
                conn: conn.clone(),
                vnet: None,
            })
            .await
            .unwrap();
            client.listen().await.unwrap();
            let relay = client.allocate().await.unwrap();
            assert!((config.turn.relay_min_port..=config.turn.relay_max_port)
                .contains(&relay.local_addr().unwrap().port()));
            let mut buf = [0; 1500];
            // First send uses Send Indication; subsequent sends exercise ChannelData.
            for payload in [b"one".as_slice(), b"second-packet", b"third"] {
                relay.send_to(payload, target).await.unwrap();
                let (n, from) = media.recv_from(&mut buf).await.unwrap();
                assert_eq!(&buf[..n], payload, "{transport}");
                media.send_to(&buf[..n], from).await.unwrap();
                let (n, from) = relay.recv_from(&mut buf).await.unwrap();
                assert_eq!(&buf[..n], payload, "{transport}");
                assert_eq!(from, target);
            }
            service.revoke(transport).await;
            let released =
                UdpSocket::bind((Ipv4Addr::UNSPECIFIED, relay.local_addr().unwrap().port()))
                    .await
                    .unwrap();
            drop(released);
            client.close().await.unwrap();
            conn.close().await.unwrap();
            // A fresh client/socket rules out the client's one-allocation guard.
            let denied_conn = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
            let denied = Client::new(ClientConfig {
                stun_serv_addr: String::new(),
                turn_serv_addr: format!("127.0.0.1:{}", config.turn.port),
                username: credential.username.clone().unwrap(),
                password: credential.credential.clone().unwrap(),
                realm: String::new(),
                software: String::new(),
                rto_in_ms: 20,
                conn: denied_conn.clone(),
                vnet: None,
            })
            .await
            .unwrap();
            denied.listen().await.unwrap();
            assert!(
                denied.allocate().await.is_err(),
                "revoked {transport} credentials accepted"
            );
            denied.close().await.unwrap();
            denied_conn.close().await.unwrap();
        }
        service.close().await;
        // Closing must release both listener types, not leave background tasks alive.
        UdpSocket::bind((Ipv4Addr::UNSPECIFIED, config.turn.port))
            .await
            .unwrap();
        TcpListener::bind((Ipv4Addr::UNSPECIFIED, config.turn.port))
            .await
            .unwrap();
    })
    .await
    .expect("TURN integration test timed out");
}
