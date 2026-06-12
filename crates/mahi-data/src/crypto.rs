//! App-layer payload cryptography: HKDF key derivation + AES-256-GCM.
//!
//! Each record is sealed under a per-record key derived from the device root
//! key, with the record id bound in as additional authenticated data (AAD).

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Key, Nonce};
use hkdf::Hkdf;
use rand::rngs::OsRng;
use rand::RngCore;
use sha2::{Digest, Sha256};

/// HKDF-SHA256 expand of `root` into a 32-byte key for `info`.
pub fn derive_key(root: &[u8; 32], info: &[u8]) -> [u8; 32] {
    let hk = Hkdf::<Sha256>::new(None, root);
    let mut okm = [0u8; 32];
    hk.expand(info, &mut okm)
        .expect("32 bytes is a valid OKM length");
    okm
}

/// Per-record key = HKDF(root, collection ‖ 0x00 ‖ record_id).
pub fn record_key(root: &[u8; 32], collection: &str, id: &[u8]) -> [u8; 32] {
    let mut info = Vec::with_capacity(collection.len() + 1 + id.len());
    info.extend_from_slice(collection.as_bytes());
    info.push(0);
    info.extend_from_slice(id);
    derive_key(root, &info)
}

/// Encrypt `plaintext` with a fresh random nonce; returns `(ciphertext, nonce)`.
pub fn encrypt(key: &[u8; 32], plaintext: &[u8], aad: &[u8]) -> Result<(Vec<u8>, [u8; 12]), ()> {
    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(key));
    let mut nonce = [0u8; 12];
    OsRng.fill_bytes(&mut nonce);
    let ct = cipher
        .encrypt(
            Nonce::from_slice(&nonce),
            Payload {
                msg: plaintext,
                aad,
            },
        )
        .map_err(|_| ())?;
    Ok((ct, nonce))
}

/// Decrypt; fails (`Err(())`) on a wrong key, tampered ciphertext, or bad AAD.
pub fn decrypt(key: &[u8; 32], ciphertext: &[u8], nonce: &[u8], aad: &[u8]) -> Result<Vec<u8>, ()> {
    if nonce.len() != 12 {
        return Err(());
    }
    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(key));
    cipher
        .decrypt(
            Nonce::from_slice(nonce),
            Payload {
                msg: ciphertext,
                aad,
            },
        )
        .map_err(|_| ())
}

/// Lowercase hex SHA-256 of `data`.
pub fn sha256_hex(data: &[u8]) -> String {
    let digest = Sha256::digest(data);
    let mut s = String::with_capacity(digest.len() * 2);
    for b in digest {
        s.push_str(&format!("{b:02x}"));
    }
    s
}
