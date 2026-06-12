//! Session lifecycle: token issuance/validation, idle timeout, kill-switch,
//! and the "phone is controlling" indicator flag.
//!
//! Phase 0 keeps everything in-memory. Tokens are HMAC-SHA256 signed; in
//! production the signing key is derived from the pairing key so a peer can
//! verify tokens offline (design doc §3) — here it is a per-manager random
//! key, which is enough for single-process validation.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use hmac::{Hmac, Mac};
use mahi_contracts::connectivity::{ChannelId, MultiplexedEnvelope};
use mahi_contracts::error::ConnectivityError;
use rand::RngCore;
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use uuid::Uuid;

use crate::bus::{EnvelopeStream, MultiplexedBus};

type HmacSha256 = Hmac<Sha256>;

/// What a session is allowed to do (design doc: `inference|takeover|file_xfr`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Capability {
    Inference,
    Takeover,
    FileXfr,
}

/// A per-session bearer token.
///
/// `hmac` is HMAC-SHA256 over the other fields, keyed with the issuing
/// manager's signing key (pairing-key-derived in production).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionToken {
    pub session_id: Uuid,
    pub peer_id: Uuid,
    pub issued_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    pub capabilities: Vec<Capability>,
    /// HMAC-SHA256 signature over the fields above.
    pub hmac: Vec<u8>,
}

impl SessionToken {
    pub fn has_capability(&self, cap: Capability) -> bool {
        self.capabilities.contains(&cap)
    }
}

/// Internal per-session bookkeeping.
struct SessionEntry {
    token: SessionToken,
    last_activity: Instant,
    killed: bool,
    /// Drives the Mac-side "phone is controlling" takeover indicator.
    controlling: bool,
}

/// Issues, validates, and revokes [`SessionToken`]s.
///
/// - **expiry**: tokens carry an absolute `expires_at`; expired tokens are
///   rejected and not resumable (reconnect requires a fresh token).
/// - **idle timeout**: a session with no recorded activity for longer than
///   `idle_timeout` is rejected on next use.
/// - **kill-switch**: [`SessionManager::kill`] invalidates a session from
///   either side; downstream sends through a [`SessionBoundBus`] then fail.
pub struct SessionManager {
    signing_key: [u8; 32],
    idle_timeout: Duration,
    default_ttl: chrono::Duration,
    sessions: Mutex<HashMap<Uuid, SessionEntry>>,
}

impl SessionManager {
    /// Defaults: 15 min idle timeout, 8 h token lifetime.
    pub fn new() -> Self {
        Self::with_config(Duration::from_secs(15 * 60), chrono::Duration::hours(8))
    }

    pub fn with_config(idle_timeout: Duration, default_ttl: chrono::Duration) -> Self {
        let mut signing_key = [0u8; 32];
        rand::thread_rng().fill_bytes(&mut signing_key);
        Self {
            signing_key,
            idle_timeout,
            default_ttl,
            sessions: Mutex::new(HashMap::new()),
        }
    }

    /// Issue a token for `peer_id` with the manager's default TTL.
    pub fn issue(&self, peer_id: Uuid, capabilities: Vec<Capability>) -> SessionToken {
        self.issue_with_ttl(peer_id, capabilities, self.default_ttl)
    }

    /// Issue a token with an explicit TTL (a non-positive TTL produces an
    /// already-expired token, which is handy in tests).
    pub fn issue_with_ttl(
        &self,
        peer_id: Uuid,
        capabilities: Vec<Capability>,
        ttl: chrono::Duration,
    ) -> SessionToken {
        let issued_at = Utc::now();
        let mut token = SessionToken {
            session_id: Uuid::new_v4(),
            peer_id,
            issued_at,
            expires_at: issued_at + ttl,
            capabilities,
            hmac: Vec::new(),
        };
        token.hmac = self.sign(&token);

        self.sessions.lock().expect("session lock poisoned").insert(
            token.session_id,
            SessionEntry {
                token: token.clone(),
                last_activity: Instant::now(),
                killed: false,
                controlling: false,
            },
        );
        token
    }

    /// Validate a token: signature, expiry, kill state, idle timeout.
    // TODO(contracts): ConnectivityError has no session-specific variant;
    // Transport { message } is the closest frozen fit for rejection reasons.
    pub fn validate(&self, token: &SessionToken) -> Result<(), ConnectivityError> {
        // Constant-time signature check via the hmac crate.
        let mut mac =
            HmacSha256::new_from_slice(&self.signing_key).expect("hmac accepts any key length");
        mac.update(&Self::canonical_bytes(token));
        if mac.verify_slice(&token.hmac).is_err() {
            return Err(session_err(token.session_id, "invalid token signature"));
        }
        self.ensure_active(token.session_id)
    }

    /// Check the live state of a session (existence, kill, expiry, idle).
    pub fn ensure_active(&self, session_id: Uuid) -> Result<(), ConnectivityError> {
        let sessions = self.sessions.lock().expect("session lock poisoned");
        let entry = sessions
            .get(&session_id)
            .ok_or_else(|| session_err(session_id, "unknown session"))?;
        if entry.killed {
            return Err(session_err(session_id, "session was killed"));
        }
        if Utc::now() >= entry.token.expires_at {
            return Err(session_err(session_id, "session token expired"));
        }
        if entry.last_activity.elapsed() > self.idle_timeout {
            return Err(session_err(session_id, "session idle timeout exceeded"));
        }
        Ok(())
    }

    /// Record traffic on the session, resetting the idle clock.
    pub fn record_activity(&self, session_id: Uuid) {
        if let Some(entry) = self
            .sessions
            .lock()
            .expect("session lock poisoned")
            .get_mut(&session_id)
        {
            entry.last_activity = Instant::now();
        }
    }

    /// Kill-switch: immediately invalidate the session (from either side).
    /// Returns `true` if the session existed. Killed sessions are not
    /// resumable; reconnecting requires a freshly issued token.
    pub fn kill(&self, session_id: Uuid) -> bool {
        let mut sessions = self.sessions.lock().expect("session lock poisoned");
        match sessions.get_mut(&session_id) {
            Some(entry) => {
                entry.killed = true;
                entry.controlling = false;
                true
            }
            None => false,
        }
    }

    /// Set the takeover indicator ("phone is controlling this Mac").
    /// Fails if the session is not active or lacks the `Takeover` capability.
    pub fn set_controlling(
        &self,
        session_id: Uuid,
        controlling: bool,
    ) -> Result<(), ConnectivityError> {
        self.ensure_active(session_id)?;
        let mut sessions = self.sessions.lock().expect("session lock poisoned");
        let entry = sessions
            .get_mut(&session_id)
            .ok_or_else(|| session_err(session_id, "unknown session"))?;
        if controlling && !entry.token.has_capability(Capability::Takeover) {
            return Err(session_err(session_id, "session lacks takeover capability"));
        }
        entry.controlling = controlling;
        Ok(())
    }

    /// Whether the session is currently controlling this device (drives the
    /// on-screen takeover indicator). Killed/unknown sessions are `false`.
    pub fn is_controlling(&self, session_id: Uuid) -> bool {
        self.sessions
            .lock()
            .expect("session lock poisoned")
            .get(&session_id)
            .map(|e| e.controlling && !e.killed)
            .unwrap_or(false)
    }

    /// Session ids that are currently active (not killed/expired/idle).
    pub fn active_sessions(&self) -> Vec<Uuid> {
        let ids: Vec<Uuid> = self
            .sessions
            .lock()
            .expect("session lock poisoned")
            .keys()
            .copied()
            .collect();
        ids.into_iter()
            .filter(|id| self.ensure_active(*id).is_ok())
            .collect()
    }

    fn sign(&self, token: &SessionToken) -> Vec<u8> {
        let mut mac =
            HmacSha256::new_from_slice(&self.signing_key).expect("hmac accepts any key length");
        mac.update(&Self::canonical_bytes(token));
        mac.finalize().into_bytes().to_vec()
    }

    /// Deterministic byte encoding of the signed fields (everything but `hmac`).
    fn canonical_bytes(token: &SessionToken) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(64);
        bytes.extend_from_slice(token.session_id.as_bytes());
        bytes.extend_from_slice(token.peer_id.as_bytes());
        bytes.extend_from_slice(&token.issued_at.timestamp_millis().to_be_bytes());
        bytes.extend_from_slice(&token.expires_at.timestamp_millis().to_be_bytes());
        for cap in &token.capabilities {
            bytes.push(match cap {
                Capability::Inference => 1,
                Capability::Takeover => 2,
                Capability::FileXfr => 3,
            });
        }
        bytes
    }
}

impl Default for SessionManager {
    fn default() -> Self {
        Self::new()
    }
}

fn session_err(session_id: Uuid, reason: &str) -> ConnectivityError {
    ConnectivityError::Transport {
        message: format!("session {session_id}: {reason}"),
    }
}

/// A bus scoped to one session: every `send` is gated on the session still
/// being active, so [`SessionManager::kill`], expiry, and idle timeout all
/// make downstream sends fail with a [`ConnectivityError`].
pub struct SessionBoundBus<B> {
    inner: B,
    manager: std::sync::Arc<SessionManager>,
    session_id: Uuid,
}

impl<B: MultiplexedBus> SessionBoundBus<B> {
    pub fn new(inner: B, manager: std::sync::Arc<SessionManager>, session_id: Uuid) -> Self {
        Self {
            inner,
            manager,
            session_id,
        }
    }

    pub fn session_id(&self) -> Uuid {
        self.session_id
    }

    /// Access the wrapped transport-level bus.
    pub fn inner(&self) -> &B {
        &self.inner
    }
}

#[async_trait]
impl<B: MultiplexedBus> MultiplexedBus for SessionBoundBus<B> {
    async fn send(&self, mut envelope: MultiplexedEnvelope) -> Result<(), ConnectivityError> {
        self.manager.ensure_active(self.session_id)?;
        // Stamp the envelope with the bound session; outbound traffic counts
        // as activity for the idle clock.
        envelope.session_id = self.session_id;
        self.manager.record_activity(self.session_id);
        self.inner.send(envelope).await
    }

    fn subscribe(&self, channel: ChannelId) -> EnvelopeStream {
        self.inner.subscribe(channel)
    }
}
