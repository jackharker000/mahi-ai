//! The device root key, held by a platform keystore.
//!
//! Phase 0 ships a file-backed keystore (root key in a `0600` file) used on
//! Linux/CI and as a fallback elsewhere. On macOS/iOS this is where a Keychain /
//! Secure Enclave-backed implementation slots in without changing callers.

use rand::rngs::OsRng;
use rand::RngCore;
use std::fs;
use std::io::Write;
use std::path::Path;

/// Source of the 32-byte device root key from which all record keys derive.
pub trait PlatformKeystore: Send + Sync {
    /// The device root key.
    fn root_key(&self) -> [u8; 32];
}

/// A file-backed keystore. The key never leaves this process except as the file.
pub struct FileKeystore {
    key: [u8; 32],
}

impl FileKeystore {
    /// Load the root key from `path`, creating a fresh random one (mode `0600`)
    /// if the file is absent or malformed.
    pub fn load_or_create(path: &Path) -> std::io::Result<Self> {
        if let Ok(bytes) = fs::read(path) {
            if bytes.len() == 32 {
                let mut key = [0u8; 32];
                key.copy_from_slice(&bytes);
                return Ok(Self { key });
            }
        }
        let mut key = [0u8; 32];
        OsRng.fill_bytes(&mut key);
        let mut f = fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(path)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let perms = fs::Permissions::from_mode(0o600);
            let _ = fs::set_permissions(path, perms);
        }
        f.write_all(&key)?;
        Ok(Self { key })
    }

    /// An ephemeral in-memory key (for `open_in_memory` / tests).
    pub fn ephemeral() -> Self {
        let mut key = [0u8; 32];
        OsRng.fill_bytes(&mut key);
        Self { key }
    }
}

impl PlatformKeystore for FileKeystore {
    fn root_key(&self) -> [u8; 32] {
        self.key
    }
}
