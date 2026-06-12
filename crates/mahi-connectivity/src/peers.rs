//! In-memory registry of known peers, annotated with reachability, latency,
//! and load hints. Exported to Compute as a snapshot so multi-Mac tie-breaking
//! (latency vs load) stays Compute's call, not silently ours.

use std::collections::HashMap;
use std::sync::RwLock;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// How a peer address can be reached (design doc: `mdns | quic-direct | relay`;
/// `Tcp` is the Phase 0 loopback/LAN-dev transport).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PeerTransport {
    Mdns,
    QuicDirect,
    Relay,
    Tcp,
}

/// One way to reach a peer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeerAddress {
    pub transport: PeerTransport,
    /// Transport-specific address string (e.g. `"192.168.1.20:7411"`).
    pub addr: String,
}

/// Coarse reachability assessment for a peer right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Reachability {
    /// Never probed (e.g. loaded from a stored descriptor).
    #[default]
    Unknown,
    /// At least one address answered recently.
    Reachable,
    /// All known addresses failed recently.
    Unreachable,
}

/// Everything we know about a peer device.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PeerDescriptor {
    pub peer_id: Uuid,
    pub display_name: String,
    /// The peer's pairing public key (placeholder bytes in Phase 0).
    pub public_key: [u8; 32],
    pub addresses: Vec<PeerAddress>,
    pub reachability: Reachability,
    pub last_seen: DateTime<Utc>,
    /// Smoothed round-trip latency, if we have measured one.
    pub latency_ms: Option<u32>,
    /// Peer-reported load in `0.0..=1.0` (`None` until the peer reports).
    pub load_hint: Option<f32>,
}

impl PeerDescriptor {
    /// A minimal descriptor for a freshly discovered/paired peer.
    pub fn new(peer_id: Uuid, display_name: impl Into<String>, public_key: [u8; 32]) -> Self {
        Self {
            peer_id,
            display_name: display_name.into(),
            public_key,
            addresses: Vec::new(),
            reachability: Reachability::Unknown,
            last_seen: Utc::now(),
            latency_ms: None,
            load_hint: None,
        }
    }
}

/// A point-in-time copy of the registry, handed to Compute for routing.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PeerRegistrySnapshot {
    pub taken_at: DateTime<Utc>,
    pub peers: Vec<PeerDescriptor>,
}

/// Live, in-memory peer table. Discovery and transports feed it; Compute
/// consumes [`PeerRegistrySnapshot`]s.
#[derive(Default)]
pub struct PeerRegistry {
    peers: RwLock<HashMap<Uuid, PeerDescriptor>>,
}

impl PeerRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert or replace a peer descriptor.
    pub fn upsert(&self, descriptor: PeerDescriptor) {
        self.peers
            .write()
            .expect("peer lock poisoned")
            .insert(descriptor.peer_id, descriptor);
    }

    pub fn get(&self, peer_id: Uuid) -> Option<PeerDescriptor> {
        self.peers
            .read()
            .expect("peer lock poisoned")
            .get(&peer_id)
            .cloned()
    }

    pub fn remove(&self, peer_id: Uuid) -> Option<PeerDescriptor> {
        self.peers
            .write()
            .expect("peer lock poisoned")
            .remove(&peer_id)
    }

    /// Mark the peer alive now and optionally update its reachability.
    pub fn mark_seen(&self, peer_id: Uuid, reachability: Reachability) {
        if let Some(peer) = self
            .peers
            .write()
            .expect("peer lock poisoned")
            .get_mut(&peer_id)
        {
            peer.last_seen = Utc::now();
            peer.reachability = reachability;
        }
    }

    /// Record a measured round-trip latency for the peer.
    pub fn record_latency(&self, peer_id: Uuid, latency_ms: u32) {
        if let Some(peer) = self
            .peers
            .write()
            .expect("peer lock poisoned")
            .get_mut(&peer_id)
        {
            peer.latency_ms = Some(latency_ms);
        }
    }

    /// Record a peer-reported load hint, clamped to `0.0..=1.0`.
    pub fn record_load_hint(&self, peer_id: Uuid, load: f32) {
        if let Some(peer) = self
            .peers
            .write()
            .expect("peer lock poisoned")
            .get_mut(&peer_id)
        {
            peer.load_hint = Some(load.clamp(0.0, 1.0));
        }
    }

    /// Point-in-time copy for Compute (`PeerRegistrySnapshot` in the design doc).
    pub fn snapshot(&self) -> PeerRegistrySnapshot {
        let peers = self
            .peers
            .read()
            .expect("peer lock poisoned")
            .values()
            .cloned()
            .collect();
        PeerRegistrySnapshot {
            taken_at: Utc::now(),
            peers,
        }
    }

    pub fn len(&self) -> usize {
        self.peers.read().expect("peer lock poisoned").len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_upsert_hints_and_snapshot() {
        let registry = PeerRegistry::new();
        assert!(registry.is_empty());

        let peer_id = Uuid::new_v4();
        let mut descriptor = PeerDescriptor::new(peer_id, "Jack's Mac", [7u8; 32]);
        descriptor.addresses.push(PeerAddress {
            transport: PeerTransport::Tcp,
            addr: "127.0.0.1:7411".into(),
        });
        registry.upsert(descriptor);

        registry.mark_seen(peer_id, Reachability::Reachable);
        registry.record_latency(peer_id, 12);
        registry.record_load_hint(peer_id, 1.7); // clamped

        let peer = registry.get(peer_id).unwrap();
        assert_eq!(peer.reachability, Reachability::Reachable);
        assert_eq!(peer.latency_ms, Some(12));
        assert_eq!(peer.load_hint, Some(1.0));

        let snapshot = registry.snapshot();
        assert_eq!(snapshot.peers.len(), 1);
        assert_eq!(snapshot.peers[0].peer_id, peer_id);

        assert!(registry.remove(peer_id).is_some());
        assert!(registry.is_empty());
    }
}
