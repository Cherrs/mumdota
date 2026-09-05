//! TURN over TCP/TLS uses STUN length framing and 32-bit padded ChannelData.
use async_trait::async_trait;
use std::any::Any;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadHalf, WriteHalf};
use tokio::sync::{watch, Mutex};
use webrtc_util::{Conn, Result};

pub trait IoStream: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> IoStream for T {}
type Stream = Box<dyn IoStream>;

pub struct StreamConn {
    reader: Mutex<ReadHalf<Stream>>,
    writer: Mutex<WriteHalf<Stream>>,
    local: SocketAddr,
    remote: SocketAddr,
    closed: watch::Sender<bool>,
}

fn invalid(message: &str) -> webrtc_util::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, message).into()
}

impl StreamConn {
    pub fn new(stream: Stream, local: SocketAddr, remote: SocketAddr) -> Arc<Self> {
        let (reader, writer) = tokio::io::split(stream);
        Arc::new(Self {
            reader: Mutex::new(reader),
            writer: Mutex::new(writer),
            local,
            remote,
            closed: watch::channel(false).0,
        })
    }

    pub async fn closed(&self) {
        let mut closed = self.closed.subscribe();
        let _ = closed.wait_for(|closed| *closed).await;
    }

    async fn read_frame(&self, buf: &mut [u8]) -> Result<usize> {
        let mut reader = self.reader.lock().await;
        let mut header = [0; 4];
        reader.read_exact(&mut header).await?;
        let payload_len = u16::from_be_bytes([header[2], header[3]]) as usize;
        let (len, padded_len) = match header[0] >> 6 {
            0 => {
                if !payload_len.is_multiple_of(4) {
                    return Err(invalid("unaligned STUN length"));
                }
                (20 + payload_len, 20 + payload_len)
            }
            1 => (4 + payload_len, (4 + payload_len + 3) & !3),
            _ => return Err(invalid("invalid TURN frame type")),
        };
        if len > buf.len() {
            return Err(invalid("TURN frame exceeds MTU"));
        }
        buf[..4].copy_from_slice(&header);
        reader.read_exact(&mut buf[4..len]).await?;
        let mut padding = [0; 3];
        reader.read_exact(&mut padding[..padded_len - len]).await?;
        Ok(len)
    }
}

#[async_trait]
impl Conn for StreamConn {
    async fn connect(&self, addr: SocketAddr) -> Result<()> {
        if addr != self.remote {
            return Err(invalid("connected TURN stream cannot change peer"));
        }
        Ok(())
    }
    async fn recv(&self, buf: &mut [u8]) -> Result<usize> {
        let result = tokio::select! { result = tokio::time::timeout(std::time::Duration::from_secs(90), self.read_frame(buf)) => result.unwrap_or_else(|_| Err(invalid("idle TURN stream"))), _ = self.closed() => Err(invalid("closed")) };
        if result.is_err() {
            self.closed.send_replace(true);
        }
        result
    }
    async fn recv_from(&self, buf: &mut [u8]) -> Result<(usize, SocketAddr)> {
        Ok((self.recv(buf).await?, self.remote))
    }
    async fn send(&self, buf: &[u8]) -> Result<usize> {
        let write = async {
            let mut writer = self.writer.lock().await;
            writer.write_all(buf).await?;
            if buf.first().is_some_and(|b| b >> 6 == 1) {
                let padding = (4 - buf.len() % 4) % 4;
                writer.write_all(&[0; 3][..padding]).await?;
            }
            writer.flush().await?;
            Ok(buf.len())
        };
        let result = tokio::time::timeout(std::time::Duration::from_secs(5), write)
            .await
            .unwrap_or_else(|_| Err(invalid("TURN write timed out")));
        if result.is_err() {
            self.closed.send_replace(true);
        }
        result
    }

    async fn send_to(&self, buf: &[u8], target: SocketAddr) -> Result<usize> {
        self.connect(target).await?;
        self.send(buf).await
    }
    fn local_addr(&self) -> Result<SocketAddr> {
        Ok(self.local)
    }
    fn remote_addr(&self) -> Option<SocketAddr> {
        Some(self.remote)
    }
    async fn close(&self) -> Result<()> {
        self.closed.send_replace(true);
        let _ = tokio::time::timeout(std::time::Duration::from_secs(1), async {
            self.writer.lock().await.shutdown().await
        })
        .await;
        Ok(())
    }
    fn as_any(&self) -> &(dyn Any + Send + Sync) {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn stream_handles_fragmented_and_coalesced_frames_with_channel_padding() {
        let (client, mut server) = tokio::io::duplex(256);
        let addr = "127.0.0.1:1".parse().unwrap();
        let conn = StreamConn::new(Box::new(client), addr, addr);
        let writer = tokio::spawn(async move {
            server.write_all(&[0x40, 0]).await.unwrap();
            server.write_all(&[0, 1, 7, 0, 0, 0]).await.unwrap();
            let mut stun = [0u8; 20];
            stun[1] = 1;
            server.write_all(&stun).await.unwrap();
        });
        let mut buf = [0; 1500];
        assert_eq!(conn.recv(&mut buf).await.unwrap(), 5);
        assert_eq!(&buf[..5], &[0x40, 0, 0, 1, 7]);
        assert_eq!(conn.recv(&mut buf).await.unwrap(), 20);
        writer.await.unwrap();
        assert!(conn.recv(&mut buf).await.is_err());
        conn.closed().await;
    }
}
