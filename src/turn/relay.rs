use async_trait::async_trait;
use std::any::Any;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use turn::relay::{relay_range::RelayAddressGeneratorRanges, RelayAddressGenerator};
use webrtc_util::{Conn, Result};

/// This TURN is dedicated to MumDota media, not a general-purpose UDP proxy.
/// Rewriting the advertised public media endpoint to loopback also avoids
/// depending on NAT hairpin support when TURN and WebRTC share a process.
pub struct MediaRelayGenerator {
    pub inner: RelayAddressGeneratorRanges,
    pub public_ip: Ipv4Addr,
    pub media_port: u16,
}

#[async_trait]
impl RelayAddressGenerator for MediaRelayGenerator {
    fn validate(&self) -> std::result::Result<(), turn::Error> {
        self.inner.validate()
    }
    async fn allocate_conn(
        &self,
        ipv4: bool,
        port: u16,
    ) -> std::result::Result<(Arc<dyn Conn + Send + Sync>, SocketAddr), turn::Error> {
        if !ipv4 || (port != 0 && !(self.inner.min_port..=self.inner.max_port).contains(&port)) {
            return Err(turn::Error::Other(
                "unsupported relay address or port".into(),
            ));
        }
        let (conn, addr) = self.inner.allocate_conn(ipv4, port).await?;
        Ok((
            Arc::new(MediaRelayConn {
                conn,
                public_ip: self.public_ip,
                media_port: self.media_port,
            }),
            addr,
        ))
    }
}

struct MediaRelayConn {
    conn: Arc<dyn Conn + Send + Sync>,
    public_ip: Ipv4Addr,
    media_port: u16,
}

impl MediaRelayConn {
    fn target(&self, target: SocketAddr) -> Result<SocketAddr> {
        if target.port() != self.media_port || target.ip() != IpAddr::V4(self.public_ip) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "TURN may relay only to MumDota media",
            )
            .into());
        }
        Ok(SocketAddr::from((Ipv4Addr::LOCALHOST, self.media_port)))
    }
}

#[async_trait]
impl Conn for MediaRelayConn {
    async fn connect(&self, addr: SocketAddr) -> Result<()> {
        self.conn.connect(self.target(addr)?).await
    }
    async fn recv(&self, buf: &mut [u8]) -> Result<usize> {
        Ok(self.recv_from(buf).await?.0)
    }
    async fn recv_from(&self, buf: &mut [u8]) -> Result<(usize, SocketAddr)> {
        loop {
            let (len, from) = self.conn.recv_from(buf).await?;
            if from == SocketAddr::from((Ipv4Addr::LOCALHOST, self.media_port)) {
                return Ok((len, SocketAddr::from((self.public_ip, self.media_port))));
            }
        }
    }
    async fn send(&self, buf: &[u8]) -> Result<usize> {
        self.conn.send(buf).await
    }
    async fn send_to(&self, buf: &[u8], addr: SocketAddr) -> Result<usize> {
        self.conn.send_to(buf, self.target(addr)?).await
    }
    fn local_addr(&self) -> Result<SocketAddr> {
        self.conn.local_addr()
    }
    fn remote_addr(&self) -> Option<SocketAddr> {
        self.conn.remote_addr()
    }
    async fn close(&self) -> Result<()> {
        self.conn.close().await
    }
    fn as_any(&self) -> &(dyn Any + Send + Sync) {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn relay_restricts_destination_and_maps_public_endpoint_to_local_media() {
        let media = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let conn = Arc::new(tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let relay = MediaRelayConn {
            conn,
            public_ip: Ipv4Addr::new(203, 0, 113, 7),
            media_port: media.local_addr().unwrap().port(),
        };
        assert!(relay.target("169.254.169.254:80".parse().unwrap()).is_err());
        assert!(relay.target("203.0.113.7:22".parse().unwrap()).is_err());
        let target = SocketAddr::from((relay.public_ip, relay.media_port));
        relay.send_to(b"voice", target).await.unwrap();
        let mut buf = [0; 32];
        let (n, from) = media.recv_from(&mut buf).await.unwrap();
        media.send_to(&buf[..n], from).await.unwrap();
        let (n, source) = relay.recv_from(&mut buf).await.unwrap();
        assert_eq!(&buf[..n], b"voice");
        assert_eq!(source, target);
    }
}
