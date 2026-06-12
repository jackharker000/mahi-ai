//! Phase 0 pairing scaffolding.
//!
//! ## What this is (and is not)
//!
//! Real pairing (design doc §2) is a QR/proximity ceremony with an X25519 key
//! exchange and a Noise handshake, with private keys living in the device
//! keychain. **Phase 0 stands all of that in with placeholders:**
//!
//! - the "X25519-style" keypair is 32 random secret bytes whose "public" half
//!   is just SHA-256 of the secret — it performs **no** Diffie-Hellman and has
//!   **no** cryptographic pairing security;
//! - mutual proof of the pairing code is a plain HMAC-SHA256 tag keyed by the
//!   6-digit code (a PAKE in production — a 6-digit HMAC key is brute-forceable
//!   offline, so do not ship this);
//! - the resulting [`TrustRecord`] keeps the private key inline instead of a
//!   keychain `SecretRef`.
//!
//! The shapes (`PairingChallenge` -> code shown out-of-band -> confirmation
//! tag -> `TrustRecord`) match the real flow so call sites won't churn when
//! the crypto lands in Phase 2.

use std::collections::HashMap;
use std::sync::Mutex;

use chrono::{DateTime, Utc};
use hmac::{Hmac, Mac};
use mahi_contracts::error::ConnectivityError;
use rand::{Rng, RngCore};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

type HmacSha256 = Hmac<Sha256>;

/// How two devices were introduced (design doc: `qr | proximity`; `Code` is
/// the Phase 0 manual 6-digit flow).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PairingMethod {
    Qr,
    Proximity,
    Code,
}

/// Placeholder "X25519-style" keypair.
///
/// PHASE-0 STAND-IN: random secret bytes with `public_key = SHA-256(secret)`.
/// There is no curve arithmetic and no shared-secret derivation. Replaced by
/// a real X25519 keypair held in the keychain in Phase 2.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlaceholderKeypair {
    pub public_key: [u8; 32],
    // TODO(contracts): production wants a keychain SecretRef here, not bytes.
    pub secret_key: [u8; 32],
}

impl PlaceholderKeypair {
    pub fn generate() -> Self {
        let mut secret_key = [0u8; 32];
        rand::thread_rng().fill_bytes(&mut secret_key);
        let public_key = Sha256::digest(secret_key).into();
        Self {
            public_key,
            secret_key,
        }
    }
}

/// One side's view of an in-flight pairing attempt.
#[derive(Debug, Clone)]
pub struct PairingChallenge {
    pub challenge_id: Uuid,
    /// 6-digit code, shown to the user out-of-band (QR/screen) and entered or
    /// scanned on the peer.
    pub code: String,
    pub local_keypair: PlaceholderKeypair,
    pub created_at: DateTime<Utc>,
}

/// The durable outcome of pairing: "this peer is allowed".
/// Stored in the device keychain in production; in-memory in Phase 0.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrustRecord {
    pub peer_id: Uuid,
    pub pairing_date: DateTime<Utc>,
    /// PHASE-0 STAND-IN: inline private key. Production stores a keychain
    /// `SecretRef` and the bytes never leave the secure enclave/keychain.
    pub local_private_key: [u8; 32],
    pub peer_public_key: [u8; 32],
    pub pairing_method: PairingMethod,
    pub revoked: bool,
    pub revoked_at: Option<DateTime<Utc>>,
}

/// Drives one-time enrollment and keeps the resulting trust records.
pub struct PairingManager {
    pending: Mutex<HashMap<Uuid, PairingChallenge>>,
    trusted: Mutex<HashMap<Uuid, TrustRecord>>,
}

impl PairingManager {
    pub fn new() -> Self {
        Self {
            pending: Mutex::new(HashMap::new()),
            trusted: Mutex::new(HashMap::new()),
        }
    }

    /// Start pairing: generate a fresh 6-digit code and a placeholder keypair.
    /// The code is displayed out-of-band; the keypair's public half is sent to
    /// the peer alongside the challenge id.
    pub fn begin_pairing(&self) -> PairingChallenge {
        let challenge = PairingChallenge {
            challenge_id: Uuid::new_v4(),
            code: format!("{:06}", rand::thread_rng().gen_range(0..1_000_000u32)),
            local_keypair: PlaceholderKeypair::generate(),
            created_at: Utc::now(),
        };
        self.pending
            .lock()
            .expect("pending lock poisoned")
            .insert(challenge.challenge_id, challenge.clone());
        challenge
    }

    /// PHASE-0 STAND-IN handshake proof: HMAC-SHA256 keyed by the 6-digit
    /// code over both public keys (initiator first). Both sides compute it
    /// from the code the user transferred; matching tags "prove" the peer saw
    /// the same code. Production replaces this with a PAKE/Noise handshake —
    /// an HMAC keyed by a 6-digit code is trivially brute-forceable offline.
    pub fn confirmation_tag(
        code: &str,
        initiator_public: &[u8; 32],
        responder_public: &[u8; 32],
    ) -> [u8; 32] {
        let mut mac =
            HmacSha256::new_from_slice(code.as_bytes()).expect("hmac accepts any key length");
        mac.update(initiator_public);
        mac.update(responder_public);
        mac.finalize().into_bytes().into()
    }

    /// Finish pairing on the initiating side: verify the responder's
    /// confirmation tag and mint a [`TrustRecord`] for the peer.
    pub fn complete_pairing(
        &self,
        challenge_id: Uuid,
        peer_id: Uuid,
        peer_public_key: [u8; 32],
        peer_tag: &[u8; 32],
        method: PairingMethod,
    ) -> Result<TrustRecord, ConnectivityError> {
        let challenge = self
            .pending
            .lock()
            .expect("pending lock poisoned")
            .remove(&challenge_id)
            .ok_or_else(|| ConnectivityError::Transport {
                message: format!("pairing: unknown or already-used challenge {challenge_id}"),
            })?;

        let expected = Self::confirmation_tag(
            &challenge.code,
            &challenge.local_keypair.public_key,
            &peer_public_key,
        );
        // Constant-time comparison.
        let mut mac = HmacSha256::new_from_slice(challenge.code.as_bytes())
            .expect("hmac accepts any key length");
        mac.update(&challenge.local_keypair.public_key);
        mac.update(&peer_public_key);
        if mac.verify_slice(peer_tag).is_err() {
            debug_assert_ne!(&expected, peer_tag);
            return Err(ConnectivityError::Transport {
                message: "pairing: confirmation tag mismatch (wrong code?)".into(),
            });
        }

        let record = TrustRecord {
            peer_id,
            pairing_date: Utc::now(),
            local_private_key: challenge.local_keypair.secret_key,
            peer_public_key,
            pairing_method: method,
            revoked: false,
            revoked_at: None,
        };
        self.trusted
            .lock()
            .expect("trusted lock poisoned")
            .insert(peer_id, record.clone());
        Ok(record)
    }

    /// Look up the trust record for a peer (revoked records are returned too;
    /// check `revoked`).
    pub fn trust_record(&self, peer_id: Uuid) -> Option<TrustRecord> {
        self.trusted
            .lock()
            .expect("trusted lock poisoned")
            .get(&peer_id)
            .cloned()
    }

    /// Is this peer currently trusted (paired and not revoked)?
    pub fn is_trusted(&self, peer_id: Uuid) -> bool {
        self.trust_record(peer_id)
            .map(|r| !r.revoked)
            .unwrap_or(false)
    }

    /// Revoke trust in a peer. Returns `true` if a record existed.
    pub fn revoke(&self, peer_id: Uuid) -> bool {
        let mut trusted = self.trusted.lock().expect("trusted lock poisoned");
        match trusted.get_mut(&peer_id) {
            Some(record) => {
                record.revoked = true;
                record.revoked_at = Some(Utc::now());
                true
            }
            None => false,
        }
    }
}

impl Default for PairingManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pairing_code_is_six_digits() {
        let manager = PairingManager::new();
        for _ in 0..32 {
            let challenge = manager.begin_pairing();
            assert_eq!(challenge.code.len(), 6);
            assert!(challenge.code.chars().all(|c| c.is_ascii_digit()));
        }
    }

    #[test]
    fn complete_pairing_with_matching_tag_produces_trust_record() {
        let initiator = PairingManager::new();
        let challenge = initiator.begin_pairing();

        // Responder side: sees the code out-of-band, has its own keypair.
        let responder_keys = PlaceholderKeypair::generate();
        let responder_id = Uuid::new_v4();
        let tag = PairingManager::confirmation_tag(
            &challenge.code,
            &challenge.local_keypair.public_key,
            &responder_keys.public_key,
        );

        let record = initiator
            .complete_pairing(
                challenge.challenge_id,
                responder_id,
                responder_keys.public_key,
                &tag,
                PairingMethod::Code,
            )
            .expect("pairing should succeed with the right code");

        assert_eq!(record.peer_id, responder_id);
        assert_eq!(record.peer_public_key, responder_keys.public_key);
        assert!(!record.revoked);
        assert!(initiator.is_trusted(responder_id));

        // Revocation flips the trust answer.
        assert!(initiator.revoke(responder_id));
        assert!(!initiator.is_trusted(responder_id));
        assert!(initiator
            .trust_record(responder_id)
            .unwrap()
            .revoked_at
            .is_some());
    }

    #[test]
    fn complete_pairing_rejects_wrong_code() {
        let initiator = PairingManager::new();
        let challenge = initiator.begin_pairing();
        let responder_keys = PlaceholderKeypair::generate();

        let wrong_code = if challenge.code == "000000" {
            "000001"
        } else {
            "000000"
        };
        let bad_tag = PairingManager::confirmation_tag(
            wrong_code,
            &challenge.local_keypair.public_key,
            &responder_keys.public_key,
        );

        let err = initiator
            .complete_pairing(
                challenge.challenge_id,
                Uuid::new_v4(),
                responder_keys.public_key,
                &bad_tag,
                PairingMethod::Code,
            )
            .unwrap_err();
        assert!(matches!(err, ConnectivityError::Transport { .. }));

        // The challenge is consumed either way: no second guess.
        let retry = initiator.complete_pairing(
            challenge.challenge_id,
            Uuid::new_v4(),
            responder_keys.public_key,
            &bad_tag,
            PairingMethod::Code,
        );
        assert!(retry.is_err());
    }
}
