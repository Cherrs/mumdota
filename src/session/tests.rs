use super::*;
use crate::config::{MumbleConfig, ServerConfig, WebrtcConfig};
use openssl::{asn1::Asn1Time, hash::MessageDigest, pkey::PKey, rsa::Rsa, x509::X509};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::task::JoinSet;
use tokio::time::{timeout, Duration};

fn config(addr: SocketAddr, max_connections: usize) -> Config {
    Config {
        server: ServerConfig {
            listen_addr: "127.0.0.1".into(),
            listen_port: 0,
            max_connections,
            allowed_origins: vec![],
        },
        mumble: MumbleConfig {
            host: addr.ip().to_string(),
            port: addr.port(),
            accept_invalid_certs: true,
        },
        webrtc: WebrtcConfig {
            stun_servers: vec![],
            udp_port: 0,
            public_ip: None,
        },
        turn: Default::default(),
    }
}

// This server only implements TLS and an authentication/ServerSync exchange.
// Each connection can be closed explicitly to simulate an upstream disconnect.
struct MockMumble {
    addr: SocketAddr,
    connections: mpsc::UnboundedReceiver<oneshot::Sender<()>>,
    task: JoinHandle<()>,
}

impl MockMumble {
    async fn start() -> Self {
        // Generate a fresh test key in memory; no checked-in private keys.
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
        let identity = native_tls::Identity::from_pkcs8(
            &cert.build().to_pem().unwrap(),
            &key.private_key_to_pem_pkcs8().unwrap(),
        )
        .unwrap();
        let acceptor =
            tokio_native_tls::TlsAcceptor::from(native_tls::TlsAcceptor::new(identity).unwrap());
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, connections) = mpsc::unbounded_channel();
        let task = tokio::spawn(async move {
            let mut clients = JoinSet::new();
            loop {
                tokio::select! {
                    accepted = listener.accept() => {
                        let (stream, _) = accepted.unwrap();
                        let acceptor = acceptor.clone();
                        let tx = tx.clone();
                        clients.spawn(async move {
                            let Ok(mut stream) = acceptor.accept(stream).await else { return };
                            let (close_tx, mut close_rx) = oneshot::channel();
                            let mut close_tx = Some(close_tx);
                            loop {
                                let mut header = [0; 6];
                                tokio::select! {
                                    result = stream.read_exact(&mut header) => {
                                        if result.is_err() { break; }
                                    }
                                    _ = &mut close_rx => break,
                                }
                                let len = u32::from_be_bytes(header[2..].try_into().unwrap()) as usize;
                                let mut body = vec![0; len];
                                if stream.read_exact(&mut body).await.is_err() { break; }
                                if u16::from_be_bytes(header[..2].try_into().unwrap()) == 2 {
                                    // ServerSync control packet (type 5), session = 1.
                                    if stream.write_all(&[0, 5, 0, 0, 0, 2, 8, 1]).await.is_err() { break; }
                                    if let Some(close_tx) = close_tx.take() {
                                        let _ = tx.send(close_tx);
                                    }
                                }
                            }
                        });
                    }
                    _ = clients.join_next(), if !clients.is_empty() => {}
                }
            }
        });
        Self {
            addr,
            connections,
            task,
        }
    }

    async fn connection(&mut self) -> oneshot::Sender<()> {
        timeout(Duration::from_secs(5), self.connections.recv())
            .await
            .unwrap()
            .unwrap()
    }
}

impl Drop for MockMumble {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn receive(rx: &mut mpsc::UnboundedReceiver<ServerMessage>) -> ServerMessage {
    timeout(Duration::from_secs(5), rx.recv())
        .await
        .unwrap()
        .unwrap()
}

#[tokio::test]
async fn pending_handshake_reserves_capacity_and_cancellation_releases_it() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let manager = Arc::new(SessionManager::new(config(
        listener.local_addr().unwrap(),
        1,
    )));
    let (tx, _rx) = mpsc::unbounded_channel();
    let pending_manager = manager.clone();
    let pending_tx = tx.clone();
    let pending = tokio::spawn(async move {
        pending_manager
            .connect_user("pending", "pending", pending_tx)
            .await
    });
    // Hold the TCP connection open without completing TLS.
    let (_stream, _) = timeout(Duration::from_secs(5), listener.accept())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        manager
            .connect_user("second", "second", tx)
            .await
            .unwrap_err(),
        "Server full"
    );
    pending.abort();
    assert!(pending.await.unwrap_err().is_cancelled());
    assert_eq!(manager.slots.available_permits(), 1);
    assert_eq!(manager.connection_count().await, 0);
}

#[tokio::test]
async fn failed_connection_releases_capacity() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let manager = SessionManager::new(config(listener.local_addr().unwrap(), 1));
    // Explicitly reject TLS locally instead of relying on OS port-zero behavior.
    let server = tokio::spawn(async move {
        for _ in 0..2 {
            let (stream, _) = listener.accept().await.unwrap();
            drop(stream);
        }
    });
    let (tx, _rx) = mpsc::unbounded_channel();
    for _ in 0..2 {
        let error = timeout(
            Duration::from_secs(5),
            manager.connect_user("client", "client", tx.clone()),
        )
        .await
        .unwrap()
        .unwrap_err();
        assert!(error.starts_with("Mumble connection failed:"), "{error}");
    }
    server.await.unwrap();
    assert_eq!(manager.slots.available_permits(), 1);
}

#[tokio::test]
async fn simultaneous_connections_cannot_exceed_limit() {
    let mut mock = MockMumble::start().await;
    let manager = SessionManager::new(config(mock.addr, 1));
    let (a_tx, mut a_rx) = mpsc::unbounded_channel();
    let (b_tx, mut b_rx) = mpsc::unbounded_channel();
    let (a, b) = tokio::join!(
        manager.connect_user("a", "a", a_tx),
        manager.connect_user("b", "b", b_tx),
    );
    assert_ne!(
        a.is_ok(),
        b.is_ok(),
        "exactly one connection should be accepted"
    );
    let _close = mock.connection().await;
    match (a, b) {
        (Ok(()), Err(error)) => {
            assert_eq!(error, "Server full");
            assert!(matches!(
                receive(&mut a_rx).await,
                ServerMessage::Connected(_)
            ));
            manager.disconnect_user("a").await;
        }
        (Err(error), Ok(())) => {
            assert_eq!(error, "Server full");
            assert!(matches!(
                receive(&mut b_rx).await,
                ServerMessage::Connected(_)
            ));
            manager.disconnect_user("b").await;
        }
        other => panic!("expected one accepted connection, got {other:?}"),
    }
    assert_eq!(manager.connection_count().await, 0);
    assert_eq!(manager.slots.available_permits(), 1);
}

#[tokio::test]
async fn upstream_disconnect_releases_capacity_before_reconnect_notification() {
    let mut mock = MockMumble::start().await;
    let manager = SessionManager::new(config(mock.addr, 1));
    let (tx, mut rx) = mpsc::unbounded_channel();
    manager
        .connect_user("client", "client", tx.clone())
        .await
        .unwrap();
    let close = mock.connection().await;
    assert!(matches!(
        receive(&mut rx).await,
        ServerMessage::Connected(_)
    ));
    let old = manager.get_session("client").await.unwrap();
    close.send(()).unwrap();
    assert!(
        matches!(receive(&mut rx).await, ServerMessage::Error(e) if e.code == "mumble_disconnected")
    );
    assert_eq!(manager.connection_count().await, 0);
    assert_eq!(manager.slots.available_permits(), 1);
    {
        let old = old.lock().await;
        assert!(old.closed);
        assert!(old.voice_setup_task.is_none());
    }
    // Keep the same message channel alive, as an open browser WebSocket would.
    manager
        .connect_user("client", "reconnected", tx)
        .await
        .unwrap();
    let _close = mock.connection().await;
    assert!(matches!(
        receive(&mut rx).await,
        ServerMessage::Connected(_)
    ));
    // A delayed cleanup from the old generation cannot remove the replacement.
    assert!(
        !SessionManager::finish_session(
            &manager.sessions,
            "client",
            &old,
            Some(ServerMessage::error("stale", "old session"))
        )
        .await
    );
    assert_eq!(manager.connection_count().await, 1);
    assert_eq!(manager.slots.available_permits(), 0);
    assert!(
        rx.try_recv().is_err(),
        "old cleanup must not send an error to the replacement"
    );
    manager.disconnect_user("client").await;
    assert_eq!(manager.slots.available_permits(), 1);
}

#[tokio::test]
async fn teardown_does_not_hold_the_global_session_map_lock() {
    let mut mock = MockMumble::start().await;
    let manager = Arc::new(SessionManager::new(config(mock.addr, 2)));
    let (tx, mut rx) = mpsc::unbounded_channel();
    manager.connect_user("client", "client", tx).await.unwrap();
    let _close = mock.connection().await;
    assert!(matches!(
        receive(&mut rx).await,
        ServerMessage::Connected(_)
    ));
    let session = manager.get_session("client").await.unwrap();
    let guard = session.lock().await;
    let disconnect_manager = manager.clone();
    let disconnect =
        tokio::spawn(async move { disconnect_manager.disconnect_user("client").await });
    tokio::task::yield_now().await;
    assert_eq!(
        timeout(Duration::from_secs(1), manager.connection_count())
            .await
            .unwrap(),
        0
    );
    drop(guard);
    timeout(Duration::from_secs(5), disconnect)
        .await
        .unwrap()
        .unwrap();
}

struct DropSignal(Option<oneshot::Sender<()>>);

impl Drop for DropSignal {
    fn drop(&mut self) {
        if let Some(tx) = self.0.take() {
            let _ = tx.send(());
        }
    }
}

#[tokio::test]
async fn abort_background_task_cancels_pending_work() {
    let (drop_tx, drop_rx) = oneshot::channel();
    let mut task = Some(tokio::spawn(async move {
        let _signal = DropSignal(Some(drop_tx));
        std::future::pending::<()>().await;
    }));
    tokio::task::yield_now().await;
    abort_background_task(&mut task);
    assert!(task.is_none());
    timeout(Duration::from_secs(1), drop_rx)
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn immediate_disconnect_does_not_start_voice_after_teardown() {
    let mock = MockMumble::start().await;
    let manager = SessionManager::new(config(mock.addr, 1));
    let (tx, _rx) = mpsc::unbounded_channel();
    manager.connect_user("client", "client", tx).await.unwrap();
    let old = manager.get_session("client").await.unwrap();
    manager.disconnect_user("client").await;
    tokio::task::yield_now().await;
    let old = old.lock().await;
    assert!(old.closed);
    assert!(old.voice_setup_task.is_none());
    assert_eq!(manager.connection_count().await, 0);
    assert_eq!(manager.slots.available_permits(), 1);
}
