//! mahi-connectivity — the multiplexed secure bus and pairing (in-process stub in Phase 0).
//!
//! Phase 0 scope (see `docs/backend/domains/04-connectivity-takeover.md`):
//!
//! - [`bus::MultiplexedBus`] — the transport-agnostic seam: `send` a
//!   [`mahi_contracts::connectivity::MultiplexedEnvelope`], `subscribe` to a
//!   [`mahi_contracts::connectivity::ChannelId`] as a stream. `Control` is
//!   scheduled with strict priority.
//! - [`bus::LocalBus`] — in-process implementation (broadcast fan-out).
//! - [`transport::TcpTransport`] — length-prefixed JSON frames over TCP;
//!   `listen`/`connect` return a [`transport::TcpBus`].
//! - [`session::SessionManager`] — token issue/validate, idle timeout,
//!   kill-switch, takeover ("controlling") indicator.
//! - [`pairing::PairingManager`] / [`peers::PeerRegistry`] — Phase 0 pairing
//!   scaffolding (placeholder crypto, clearly marked) and peer descriptors.
//!
//! QUIC/WireGuard/relay and screen streaming are Phase 2 (macOS) and are
//! intentionally absent here.

pub mod bus;
pub mod pairing;
pub mod peers;
pub mod session;
pub mod transport;

pub use bus::{EnvelopeStream, LocalBus, MultiplexedBus};
pub use pairing::{
    PairingChallenge, PairingManager, PairingMethod, PlaceholderKeypair, TrustRecord,
};
pub use peers::{
    PeerAddress, PeerDescriptor, PeerRegistry, PeerRegistrySnapshot, PeerTransport, Reachability,
};
pub use session::{Capability, SessionBoundBus, SessionManager, SessionToken};
pub use transport::{TcpBus, TcpTransport, MAX_FRAME_LEN};
