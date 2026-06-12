//! # mahi-data
//!
//! The local-first trust backbone (Phase 0): a SQLite-backed implementation of
//! the full `mahi-contracts` [`DataStore`] trait family, with per-record
//! AES-256-GCM payload encryption (keys derived from a device root key in a
//! platform keystore) and a hash-chained audit log.
//!
//! See `docs/backend/phase-0/02-trust-backbone.md`.
//!
//! ```
//! # async fn demo() -> Result<(), mahi_contracts::ContractError> {
//! let store = mahi_data::open_in_memory()?;
//! let conv = mahi_contracts::data::Conversation::new(mahi_contracts::ComputeMode::OnDevice);
//! store.conversations.upsert(conv.clone()).await?;
//! assert!(store.conversations.get(conv.id).await?.is_some());
//! # Ok(()) }
//! ```

mod crypto;
mod keystore;
mod store;

pub use keystore::{FileKeystore, PlatformKeystore};
pub use store::SqliteBackend;

use mahi_contracts::data::DataStore;
use mahi_contracts::error::{ContractError, StoreError};
use std::path::Path;
use std::sync::Arc;

fn backend_err<E: std::fmt::Display>(e: E) -> ContractError {
    ContractError::Store(StoreError::Backend {
        message: e.to_string(),
    })
}

fn wire(b: Arc<SqliteBackend>) -> DataStore {
    DataStore {
        conversations: b.clone(),
        messages: b.clone(),
        memory: b.clone(),
        skills: b.clone(),
        tasks: b.clone(),
        artifacts: b.clone(),
        settings: b.clone(),
        checkpoints: b.clone(),
        audit: b.clone(),
        permissions: b,
    }
}

/// Open (or create) the encrypted on-disk store in directory `dir`.
///
/// `dir` holds `mahi.db` (the SQLite database) and `root.key` (the device root
/// key, mode `0600`). Both are created on first use.
pub fn open_store(dir: &Path) -> Result<DataStore, ContractError> {
    std::fs::create_dir_all(dir).map_err(backend_err)?;
    let keystore = FileKeystore::load_or_create(&dir.join("root.key")).map_err(backend_err)?;
    let conn = rusqlite::Connection::open(dir.join("mahi.db")).map_err(backend_err)?;
    let backend = SqliteBackend::new(conn, keystore.root_key())?;
    Ok(wire(backend))
}

/// Open an ephemeral in-memory store (tests, `--store memory`).
pub fn open_in_memory() -> Result<DataStore, ContractError> {
    let conn = rusqlite::Connection::open_in_memory().map_err(backend_err)?;
    let backend = SqliteBackend::new(conn, FileKeystore::ephemeral().root_key())?;
    Ok(wire(backend))
}
