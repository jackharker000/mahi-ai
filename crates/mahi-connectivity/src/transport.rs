//! Phase 0 TCP transport: length-prefixed JSON framing of `MultiplexedEnvelope`.
//!
//! Wire format per frame: `u32` big-endian payload length, followed by exactly
//! that many bytes of `serde_json`-encoded [`MultiplexedEnvelope`].
//!
//! This is deliberately plain TCP with no encryption: the Phase 0 loopback /
//! LAN-dev transport. The production transport is QUIC + Noise over the
//! pairing key (Phase 2); the `MultiplexedBus` API is identical so downstream
//! code is transport-agnostic.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use mahi_contracts::connectivity::{ChannelId, MultiplexedEnvelope};
use mahi_contracts::error::ConnectivityError;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream, ToSocketAddrs};
use tokio::sync::mpsc;

use crate::bus::{
    pick_sender, ChannelRouter, EnvelopeStream, MultiplexedBus, SendQueues,
    DEFAULT_CHANNEL_CAPACITY, DEFAULT_QUEUE_DEPTH,
};

/// Refuse frames larger than this (a corrupt/hostile length prefix would
/// otherwise make us allocate gigabytes).
pub const MAX_FRAME_LEN: usize = 16 * 1024 * 1024;

fn transport_err(message: impl std::fmt::Display) -> ConnectivityError {
    ConnectivityError::Transport {
        message: message.to_string(),
    }
}

/// Write one length-prefixed JSON frame.
pub(crate) async fn write_frame<W: AsyncWrite + Unpin>(
    writer: &mut W,
    envelope: &MultiplexedEnvelope,
) -> std::io::Result<()> {
    let bytes = serde_json::to_vec(envelope)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    if bytes.len() > MAX_FRAME_LEN {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("frame of {} bytes exceeds MAX_FRAME_LEN", bytes.len()),
        ));
    }
    // tokio's write_u32 is big-endian.
    writer.write_u32(bytes.len() as u32).await?;
    writer.write_all(&bytes).await?;
    writer.flush().await
}

/// Read one length-prefixed JSON frame. `Ok(None)` signals a clean EOF at a
/// frame boundary (the peer hung up).
pub(crate) async fn read_frame<R: AsyncRead + Unpin>(
    reader: &mut R,
) -> std::io::Result<Option<MultiplexedEnvelope>> {
    let len = match reader.read_u32().await {
        Ok(len) => len as usize,
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    };
    if len > MAX_FRAME_LEN {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("incoming frame of {len} bytes exceeds MAX_FRAME_LEN"),
        ));
    }
    let mut buf = vec![0u8; len];
    reader.read_exact(&mut buf).await?;
    let envelope = serde_json::from_slice(&buf)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    Ok(Some(envelope))
}

/// Entry points for the Phase 0 TCP transport.
pub struct TcpTransport;

impl TcpTransport {
    /// Bind `addr` and return a bus wired to the **first** inbound connection.
    ///
    /// The bus is usable immediately: `subscribe` works right away and `send`s
    /// are queued until a peer connects. Phase 0 is single-peer; later
    /// connections are not accepted.
    pub async fn listen(addr: impl ToSocketAddrs) -> Result<TcpBus, ConnectivityError> {
        let listener = TcpListener::bind(addr).await.map_err(transport_err)?;
        let local_addr = listener.local_addr().map_err(transport_err)?;
        Ok(TcpBus::start(local_addr, move || async move {
            listener
                .accept()
                .await
                .map(|(stream, peer)| {
                    tracing::debug!(%peer, "tcp transport accepted connection");
                    stream
                })
                .map_err(|e| transport_err(format!("accept failed: {e}")))
        }))
    }

    /// Connect to a listening peer and return a bus over that connection.
    pub async fn connect(addr: impl ToSocketAddrs) -> Result<TcpBus, ConnectivityError> {
        let stream = TcpStream::connect(addr)
            .await
            .map_err(|e| transport_err(format!("connect failed: {e}")))?;
        let local_addr = stream.local_addr().map_err(transport_err)?;
        Ok(TcpBus::start(local_addr, move || async move { Ok(stream) }))
    }
}

/// A [`MultiplexedBus`] backed by one TCP connection.
///
/// `send` enqueues outbound envelopes (Control prioritized over everything
/// else); `subscribe` yields envelopes received **from the peer**. A pair of
/// background tasks pump frames both ways.
pub struct TcpBus {
    router: Arc<ChannelRouter>,
    queues: Mutex<Option<SendQueues>>,
    local_addr: SocketAddr,
}

impl TcpBus {
    /// The bound/connected local address. After `listen("127.0.0.1:0")` this
    /// carries the OS-assigned port to dial.
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// Close the outbound side: subsequent `send`s fail with
    /// [`ConnectivityError::ChannelClosed`] and the writer task exits (which
    /// closes the socket's write half once in-flight frames flush).
    pub fn close(&self) {
        self.queues.lock().expect("send-queue lock poisoned").take();
    }

    /// Build the bus and spawn the pump. `acquire` resolves to the connected
    /// stream (immediately for `connect`, on first accept for `listen`).
    fn start<F, Fut>(local_addr: SocketAddr, acquire: F) -> Self
    where
        F: FnOnce() -> Fut + Send + 'static,
        Fut: std::future::Future<Output = Result<TcpStream, ConnectivityError>> + Send,
    {
        let router = Arc::new(ChannelRouter::new(DEFAULT_CHANNEL_CAPACITY));
        let (control_tx, control_rx) = mpsc::channel(DEFAULT_QUEUE_DEPTH);
        let (normal_tx, normal_rx) = mpsc::channel(DEFAULT_QUEUE_DEPTH);

        let pump_router = Arc::clone(&router);
        tokio::spawn(async move {
            match acquire().await {
                Ok(stream) => pump(stream, pump_router, control_rx, normal_rx).await,
                Err(e) => tracing::warn!("tcp transport failed to acquire stream: {e}"),
            }
        });

        Self {
            router,
            queues: Mutex::new(Some(SendQueues::new(control_tx, normal_tx))),
            local_addr,
        }
    }
}

/// Pump frames both ways until either side of the connection goes away.
async fn pump(
    stream: TcpStream,
    router: Arc<ChannelRouter>,
    mut control_rx: mpsc::Receiver<MultiplexedEnvelope>,
    mut normal_rx: mpsc::Receiver<MultiplexedEnvelope>,
) {
    let (mut read_half, mut write_half) = stream.into_split();

    // Inbound: socket -> per-channel subscribers.
    let reader = tokio::spawn(async move {
        loop {
            match read_frame(&mut read_half).await {
                Ok(Some(envelope)) => router.publish(envelope),
                Ok(None) => {
                    tracing::debug!("tcp peer closed the connection");
                    break;
                }
                Err(e) => {
                    tracing::warn!("tcp reader stopped: {e}");
                    break;
                }
            }
        }
    });

    // Outbound: priority queues -> socket. Biased select keeps the Control
    // channel (kill-switch, heartbeat) ahead of saturated data channels.
    let mut control_open = true;
    let mut normal_open = true;
    loop {
        let envelope = tokio::select! {
            biased;
            maybe = control_rx.recv(), if control_open => match maybe {
                Some(envelope) => envelope,
                None => { control_open = false; continue; }
            },
            maybe = normal_rx.recv(), if normal_open => match maybe {
                Some(envelope) => envelope,
                None => { normal_open = false; continue; }
            },
            else => break,
        };
        if let Err(e) = write_frame(&mut write_half, &envelope).await {
            tracing::warn!("tcp writer stopped: {e}");
            break;
        }
    }
    drop(write_half); // half-close: peer's reader sees EOF
    let _ = reader.await;
    tracing::debug!("tcp pump finished");
}

#[async_trait]
impl MultiplexedBus for TcpBus {
    async fn send(&self, envelope: MultiplexedEnvelope) -> Result<(), ConnectivityError> {
        let tx = pick_sender(&self.queues, envelope.channel)?;
        tx.send(envelope)
            .await
            .map_err(|e| ConnectivityError::ChannelClosed {
                channel: e.0.channel,
            })
    }

    fn subscribe(&self, channel: ChannelId) -> EnvelopeStream {
        self.router.subscribe(channel)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    fn envelope(channel: ChannelId, payload: &[u8]) -> MultiplexedEnvelope {
        MultiplexedEnvelope {
            channel,
            seq_no: 7,
            session_id: Uuid::new_v4(),
            payload_type: "TestPayload".into(),
            payload_bytes: payload.to_vec(),
        }
    }

    #[tokio::test]
    async fn frame_roundtrip_and_length_prefix() {
        let env = envelope(ChannelId::Inference, b"{\"token\":\"hi\"}");

        let mut wire = Vec::new();
        write_frame(&mut wire, &env).await.unwrap();

        // u32 big-endian length prefix must match the JSON body length.
        let prefix = u32::from_be_bytes(wire[..4].try_into().unwrap()) as usize;
        assert_eq!(prefix, wire.len() - 4);
        let parsed: MultiplexedEnvelope = serde_json::from_slice(&wire[4..]).unwrap();
        assert_eq!(parsed.payload_bytes, env.payload_bytes);

        let mut cursor = std::io::Cursor::new(wire);
        let read = read_frame(&mut cursor).await.unwrap().unwrap();
        assert_eq!(read.channel, env.channel);
        assert_eq!(read.seq_no, env.seq_no);
        assert_eq!(read.session_id, env.session_id);
        assert_eq!(read.payload_type, env.payload_type);
        assert_eq!(read.payload_bytes, env.payload_bytes);
    }

    #[tokio::test]
    async fn clean_eof_yields_none() {
        let mut cursor = std::io::Cursor::new(Vec::<u8>::new());
        assert!(read_frame(&mut cursor).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn oversized_length_prefix_is_rejected() {
        let mut wire = ((MAX_FRAME_LEN + 1) as u32).to_be_bytes().to_vec();
        wire.extend_from_slice(b"junk");
        let mut cursor = std::io::Cursor::new(wire);
        let err = read_frame(&mut cursor).await.unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    }
}
