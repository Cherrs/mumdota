pub mod credentials;
mod relay;
mod stream;

use crate::config::Config;
use anyhow::{Context, Result};
use credentials::{Credentials, IceConfig};
use relay::MediaRelayGenerator;
use std::net::IpAddr;
use std::sync::{Arc, Weak};
use stream::{IoStream, StreamConn};
use tokio::net::{TcpListener, UdpSocket};
use tokio::sync::{watch, Mutex, RwLock, Semaphore};
use tokio::task::{JoinHandle, JoinSet};
use tokio::time::{timeout, Duration};
use turn::relay::relay_range::RelayAddressGeneratorRanges;
use turn::server::{
    config::{ConnConfig, ServerConfig},
    Server,
};
use webrtc_util::{vnet::net::Net, Conn};

pub struct TurnService {
    config: Config,
    pub credentials: Arc<Credentials>,
    servers: RwLock<Vec<Weak<Server>>>,
    shutdown: watch::Sender<bool>,
    tasks: Mutex<Vec<JoinHandle<()>>>,
}

impl TurnService {
    pub async fn start(config: &Config) -> Result<Arc<Self>> {
        config.validate()?;
        anyhow::ensure!(config.turn.enabled, "TURN is disabled");
        // Bind every configured listener before starting any background task.
        let address = (config.turn.listen_addr, config.turn.port);
        let udp = UdpSocket::bind(address).await.context("bind TURN UDP")?;
        let tcp = TcpListener::bind(address).await.context("bind TURN TCP")?;
        let tls = match (&config.turn.tls_cert, &config.turn.tls_key) {
            (Some(cert), Some(key)) => {
                let identity =
                    native_tls::Identity::from_pkcs8(&std::fs::read(cert)?, &std::fs::read(key)?)
                        .context("TURN TLS needs a PEM certificate chain and PKCS#8 private key")?;
                let acceptor =
                    tokio_native_tls::TlsAcceptor::from(native_tls::TlsAcceptor::new(identity)?);
                Some((
                    TcpListener::bind((config.turn.listen_addr, config.turn.tls_port)).await?,
                    acceptor,
                ))
            }
            _ => None,
        };
        let service = Arc::new(Self {
            credentials: Arc::new(Credentials::new(
                config.turn.realm.clone(),
                config.turn.credential_ttl_secs,
            )),
            config: config.clone(),
            servers: RwLock::new(Vec::new()),
            shutdown: watch::channel(false).0,
            tasks: Mutex::new(Vec::new()),
        });
        let udp_server = service.make_server(Arc::new(udp)).await?;
        let mut closed = service.shutdown.subscribe();
        service.tasks.lock().await.push(tokio::spawn(async move {
            let _ = closed.wait_for(|v| *v).await;
            let _ = udp_server.close().await;
        }));
        let service_clone = service.clone();
        service.tasks.lock().await.push(tokio::spawn(async move {
            service_clone.accept_streams(tcp, None).await;
        }));
        if let Some((listener, acceptor)) = tls {
            let service_clone = service.clone();
            service.tasks.lock().await.push(tokio::spawn(async move {
                service_clone.accept_streams(listener, Some(acceptor)).await;
            }));
        }
        tracing::info!(
            port = config.turn.port,
            tls = config.turn.tls_cert.is_some(),
            "Built-in TURN started"
        );
        Ok(service)
    }

    async fn make_server(&self, conn: Arc<dyn Conn + Send + Sync>) -> Result<Arc<Server>> {
        let ip = self
            .config
            .turn
            .public_ip
            .context("TURN public IP is missing")?;
        let server = Arc::new(
            Server::new(ServerConfig {
                conn_configs: vec![ConnConfig {
                    conn,
                    relay_addr_generator: Box::new(MediaRelayGenerator {
                        inner: RelayAddressGeneratorRanges {
                            relay_address: IpAddr::V4(ip),
                            address: std::net::Ipv4Addr::LOCALHOST.to_string(),
                            min_port: self.config.turn.relay_min_port,
                            max_port: self.config.turn.relay_max_port,
                            max_retries: 100,
                            net: Arc::new(Net::new(None)),
                        },
                        public_ip: ip,
                        media_port: self.config.webrtc.udp_port,
                    }),
                }],
                realm: self.config.turn.realm.clone(),
                auth_handler: self.credentials.clone(),
                channel_bind_timeout: Duration::from_secs(600),
                alloc_close_notify: None,
            })
            .await?,
        );
        let mut servers = self.servers.write().await;
        servers.retain(|s| s.strong_count() > 0);
        servers.push(Arc::downgrade(&server));
        Ok(server)
    }

    async fn accept_streams(
        self: Arc<Self>,
        listener: TcpListener,
        tls: Option<tokio_native_tls::TlsAcceptor>,
    ) {
        let mut shutdown = self.shutdown.subscribe();
        let slots = Arc::new(Semaphore::new(self.config.server.max_connections * 2));
        let mut clients = JoinSet::new();
        loop {
            tokio::select! {
                _ = shutdown.wait_for(|v| *v) => break,
                _ = clients.join_next(), if !clients.is_empty() => {},
                result = listener.accept() => {
                    let Ok((socket, remote)) = result else { break; };
                    let Ok(slot) = slots.clone().try_acquire_owned() else { continue; };
                    let service = self.clone();
                    let tls = tls.clone();
                    clients.spawn(async move {
                        let _slot = slot;
                        let Ok(local) = socket.local_addr() else { return; };
                        let _ = socket.set_nodelay(true);
                        let stream: Box<dyn IoStream> = if let Some(tls) = tls {
                            match timeout(Duration::from_secs(10), tls.accept(socket)).await {
                                Ok(Ok(stream)) => Box::new(stream), _ => return,
                            }
                        } else { Box::new(socket) };
                        let conn = StreamConn::new(stream, local, remote);
                        let Ok(server) = service.make_server(conn.clone()).await else { return; };
                        let mut shutdown = service.shutdown.subscribe();
                        tokio::select! { _ = conn.closed() => {}, _ = shutdown.wait_for(|v| *v) => {} }
                        let _ = server.close().await;
                    });
                }
            }
        }
        while clients.join_next().await.is_some() {}
    }

    pub fn ice_config(&self, session: &str) -> IceConfig {
        let turn = &self.config.turn;
        let mut urls = vec![
            format!("stun:{}:{}", turn.public_host, turn.port),
            format!("turn:{}:{}?transport=udp", turn.public_host, turn.port),
            format!("turn:{}:{}?transport=tcp", turn.public_host, turn.port),
        ];
        if turn.tls_cert.is_some() {
            urls.push(format!(
                "turns:{}:{}?transport=tcp",
                turn.public_host, turn.tls_port
            ));
        }
        self.credentials.issue(session, &urls)
    }

    pub async fn revoke(&self, session: &str) {
        let usernames = self.credentials.revoke(session);
        let servers: Vec<_> = self
            .servers
            .read()
            .await
            .iter()
            .filter_map(Weak::upgrade)
            .collect();
        for username in usernames {
            futures::future::join_all(servers.iter().map(|server| {
                timeout(
                    Duration::from_secs(1),
                    server.delete_allocations_by_username(username.clone()),
                )
            }))
            .await;
        }
    }

    pub async fn close(&self) {
        self.shutdown.send_replace(true);
        for task in self.tasks.lock().await.drain(..) {
            let _ = task.await;
        }
    }
}

#[cfg(test)]
mod tests;
