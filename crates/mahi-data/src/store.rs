//! `SqliteBackend` — a single type implementing every `mahi-contracts` store
//! trait over rusqlite. Each row stores plaintext index columns plus an
//! AES-256-GCM-sealed payload (the record's JSON). The audit log is hash-chained.

use crate::crypto;
use async_trait::async_trait;
use mahi_contracts::data::*;
use mahi_contracts::error::{ContractError, StoreError};
use rusqlite::{params, Connection, OptionalExtension};
use serde::de::DeserializeOwned;
use serde::Serialize;
use std::sync::{Arc, Mutex};
use uuid::Uuid;

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS conversations (id TEXT PRIMARY KEY, updated_at INTEGER NOT NULL, enc BLOB NOT NULL, nonce BLOB NOT NULL);
CREATE TABLE IF NOT EXISTS messages (id TEXT PRIMARY KEY, conversation_id TEXT NOT NULL, sequence_num INTEGER NOT NULL, enc BLOB NOT NULL, nonce BLOB NOT NULL);
CREATE INDEX IF NOT EXISTS idx_messages_conv ON messages(conversation_id, sequence_num);
CREATE TABLE IF NOT EXISTS memory (id TEXT PRIMARY KEY, space_id TEXT, enc BLOB NOT NULL, nonce BLOB NOT NULL);
CREATE TABLE IF NOT EXISTS skills (id TEXT PRIMARY KEY, scope TEXT NOT NULL, enc BLOB NOT NULL, nonce BLOB NOT NULL);
CREATE TABLE IF NOT EXISTS settings (key TEXT PRIMARY KEY, enc BLOB NOT NULL, nonce BLOB NOT NULL);
CREATE TABLE IF NOT EXISTS tasks (id TEXT PRIMARY KEY, enc BLOB NOT NULL, nonce BLOB NOT NULL);
CREATE TABLE IF NOT EXISTS artifacts (id TEXT PRIMARY KEY, enc BLOB NOT NULL, nonce BLOB NOT NULL);
CREATE TABLE IF NOT EXISTS checkpoints (id TEXT PRIMARY KEY, conversation_id TEXT NOT NULL, captured_at INTEGER NOT NULL, enc BLOB NOT NULL, nonce BLOB NOT NULL);
CREATE TABLE IF NOT EXISTS audit (seq INTEGER PRIMARY KEY AUTOINCREMENT, event_id TEXT NOT NULL UNIQUE, session_id TEXT, prev_hash TEXT NOT NULL, hash TEXT NOT NULL, enc BLOB NOT NULL, nonce BLOB NOT NULL);
CREATE TABLE IF NOT EXISTS grants (grant_id TEXT PRIMARY KEY, subject TEXT NOT NULL, revoked INTEGER NOT NULL DEFAULT 0, enc BLOB NOT NULL, nonce BLOB NOT NULL);
"#;

/// `(event_id, prev_hash, hash, ciphertext, nonce)` row of the audit table.
type AuditRow = (String, String, String, Vec<u8>, Vec<u8>);

/// SQLite-backed implementation of the full `DataStore` trait family.
pub struct SqliteBackend {
    conn: Arc<Mutex<Connection>>,
    root: [u8; 32],
}

fn be<E: std::fmt::Display>(e: E) -> ContractError {
    ContractError::Store(StoreError::Backend {
        message: e.to_string(),
    })
}
fn ser<E: std::fmt::Display>(e: E) -> ContractError {
    ContractError::Store(StoreError::Serialization {
        message: e.to_string(),
    })
}
fn enc_err() -> ContractError {
    ContractError::Store(StoreError::Encryption {
        message: "AEAD failure".into(),
    })
}

fn scope_key(scope: &SkillScope) -> String {
    match scope {
        SkillScope::Global => "global".to_string(),
        SkillScope::Space(id) => format!("space:{id}"),
        SkillScope::User(id) => format!("user:{id}"),
    }
}

fn subject_key(subject: &PermissionSubject) -> String {
    match subject {
        PermissionSubject::Tool(s) => format!("tool:{s}"),
        PermissionSubject::Feature(s) => format!("feature:{s}"),
    }
}

impl SqliteBackend {
    /// Wrap an open connection, applying migrations.
    pub fn new(conn: Connection, root: [u8; 32]) -> Result<Arc<Self>, ContractError> {
        conn.execute_batch(SCHEMA).map_err(be)?;
        Ok(Arc::new(Self {
            conn: Arc::new(Mutex::new(conn)),
            root,
        }))
    }

    fn seal<T: Serialize>(
        &self,
        collection: &str,
        id: &str,
        rec: &T,
    ) -> Result<(Vec<u8>, Vec<u8>), ContractError> {
        let json = serde_json::to_vec(rec).map_err(ser)?;
        let key = crypto::record_key(&self.root, collection, id.as_bytes());
        let (ct, nonce) = crypto::encrypt(&key, &json, id.as_bytes()).map_err(|_| enc_err())?;
        Ok((ct, nonce.to_vec()))
    }

    fn unseal<T: DeserializeOwned>(
        &self,
        collection: &str,
        id: &str,
        ct: &[u8],
        nonce: &[u8],
    ) -> Result<T, ContractError> {
        let key = crypto::record_key(&self.root, collection, id.as_bytes());
        let pt = crypto::decrypt(&key, ct, nonce, id.as_bytes()).map_err(|_| enc_err())?;
        serde_json::from_slice(&pt).map_err(ser)
    }

    /// Verify the audit hash chain. Returns `true` if intact.
    pub fn verify_chain(&self) -> Result<bool, ContractError> {
        let rows: Vec<AuditRow> = {
            let conn = self.conn.lock().unwrap();
            let mut stmt = conn
                .prepare("SELECT event_id, prev_hash, hash, enc, nonce FROM audit ORDER BY seq ASC")
                .map_err(be)?;
            let it = stmt
                .query_map([], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, Vec<u8>>(3)?,
                        r.get::<_, Vec<u8>>(4)?,
                    ))
                })
                .map_err(be)?;
            let mut v = Vec::new();
            for row in it {
                v.push(row.map_err(be)?);
            }
            v
        };
        let mut expected_prev = String::new();
        for (id, prev_hash, hash, ct, nonce) in rows {
            if prev_hash != expected_prev {
                return Ok(false);
            }
            let event: AuditEvent = self.unseal("audit", &id, &ct, &nonce)?;
            let recomputed = crypto::sha256_hex(&serde_json::to_vec(&event).map_err(ser)?);
            if recomputed != hash {
                return Ok(false);
            }
            expected_prev = hash;
        }
        Ok(true)
    }
}

// ─────────────────────────── Conversations ───────────────────────────

#[async_trait]
impl ConversationStore for SqliteBackend {
    async fn get(&self, id: Uuid) -> Result<Option<Conversation>, ContractError> {
        let key = id.to_string();
        let row = {
            let conn = self.conn.lock().unwrap();
            conn.query_row(
                "SELECT enc, nonce FROM conversations WHERE id = ?1",
                params![key],
                |r| Ok((r.get::<_, Vec<u8>>(0)?, r.get::<_, Vec<u8>>(1)?)),
            )
            .optional()
            .map_err(be)?
        };
        match row {
            Some((ct, nonce)) => Ok(Some(self.unseal("conversations", &key, &ct, &nonce)?)),
            None => Ok(None),
        }
    }

    async fn upsert(&self, conv: Conversation) -> Result<(), ContractError> {
        let id = conv.id.to_string();
        let updated = conv.updated_at.timestamp_millis();
        let (ct, nonce) = self.seal("conversations", &id, &conv)?;
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO conversations (id, updated_at, enc, nonce) VALUES (?1, ?2, ?3, ?4)",
            params![id, updated, ct, nonce],
        )
        .map_err(be)?;
        Ok(())
    }

    async fn delete(&self, id: Uuid) -> Result<(), ContractError> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "DELETE FROM conversations WHERE id = ?1",
            params![id.to_string()],
        )
        .map_err(be)?;
        Ok(())
    }

    async fn list(&self, limit: usize) -> Result<Vec<Conversation>, ContractError> {
        let rows: Vec<(String, Vec<u8>, Vec<u8>)> = {
            let conn = self.conn.lock().unwrap();
            let mut stmt = conn
                .prepare(
                    "SELECT id, enc, nonce FROM conversations ORDER BY updated_at DESC LIMIT ?1",
                )
                .map_err(be)?;
            let it = stmt
                .query_map(params![limit as i64], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, Vec<u8>>(1)?,
                        r.get::<_, Vec<u8>>(2)?,
                    ))
                })
                .map_err(be)?;
            let mut v = Vec::new();
            for row in it {
                v.push(row.map_err(be)?);
            }
            v
        };
        let mut out = Vec::with_capacity(rows.len());
        for (id, ct, nonce) in rows {
            out.push(self.unseal("conversations", &id, &ct, &nonce)?);
        }
        Ok(out)
    }
}

// ─────────────────────────── Messages ───────────────────────────

#[async_trait]
impl MessageStore for SqliteBackend {
    async fn append(&self, msg: Message) -> Result<(), ContractError> {
        let id = msg.id.to_string();
        let conv = msg.conversation_id.to_string();
        let seq = msg.sequence_num;
        let (ct, nonce) = self.seal("messages", &id, &msg)?;
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO messages (id, conversation_id, sequence_num, enc, nonce) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![id, conv, seq, ct, nonce],
        )
        .map_err(be)?;
        Ok(())
    }

    async fn range(
        &self,
        conversation_id: Uuid,
        limit: usize,
    ) -> Result<Vec<Message>, ContractError> {
        let rows: Vec<(String, Vec<u8>, Vec<u8>)> = {
            let conn = self.conn.lock().unwrap();
            let mut stmt = conn
                .prepare("SELECT id, enc, nonce FROM messages WHERE conversation_id = ?1 ORDER BY sequence_num ASC LIMIT ?2")
                .map_err(be)?;
            let it = stmt
                .query_map(params![conversation_id.to_string(), limit as i64], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, Vec<u8>>(1)?,
                        r.get::<_, Vec<u8>>(2)?,
                    ))
                })
                .map_err(be)?;
            let mut v = Vec::new();
            for row in it {
                v.push(row.map_err(be)?);
            }
            v
        };
        let mut out = Vec::with_capacity(rows.len());
        for (id, ct, nonce) in rows {
            out.push(self.unseal("messages", &id, &ct, &nonce)?);
        }
        Ok(out)
    }

    async fn next_sequence(&self, conversation_id: Uuid) -> Result<i64, ContractError> {
        let conn = self.conn.lock().unwrap();
        let max: Option<i64> = conn
            .query_row(
                "SELECT MAX(sequence_num) FROM messages WHERE conversation_id = ?1",
                params![conversation_id.to_string()],
                |r| r.get(0),
            )
            .optional()
            .map_err(be)?
            .flatten();
        Ok(max.map(|m| m + 1).unwrap_or(0))
    }
}

// ─────────────────────────── Memory ───────────────────────────

#[async_trait]
impl MemoryStore for SqliteBackend {
    async fn get(&self, id: Uuid) -> Result<Option<MemoryRecord>, ContractError> {
        let key = id.to_string();
        let row = {
            let conn = self.conn.lock().unwrap();
            conn.query_row(
                "SELECT enc, nonce FROM memory WHERE id = ?1",
                params![key],
                |r| Ok((r.get::<_, Vec<u8>>(0)?, r.get::<_, Vec<u8>>(1)?)),
            )
            .optional()
            .map_err(be)?
        };
        match row {
            Some((ct, nonce)) => Ok(Some(self.unseal("memory", &key, &ct, &nonce)?)),
            None => Ok(None),
        }
    }

    async fn upsert(&self, record: MemoryRecord) -> Result<(), ContractError> {
        let id = record.id.to_string();
        let space = record.space_id.map(|s| s.to_string());
        let (ct, nonce) = self.seal("memory", &id, &record)?;
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO memory (id, space_id, enc, nonce) VALUES (?1, ?2, ?3, ?4)",
            params![id, space, ct, nonce],
        )
        .map_err(be)?;
        Ok(())
    }

    async fn delete(&self, id: Uuid) -> Result<(), ContractError> {
        let conn = self.conn.lock().unwrap();
        conn.execute("DELETE FROM memory WHERE id = ?1", params![id.to_string()])
            .map_err(be)?;
        Ok(())
    }

    async fn semantic_search(
        &self,
        query: &str,
        space_id: Option<Uuid>,
        limit: usize,
    ) -> Result<Vec<MemoryRecord>, ContractError> {
        // Phase 0: load candidate rows (decrypting in-process) and substring-match.
        let rows: Vec<(String, Vec<u8>, Vec<u8>)> = {
            let conn = self.conn.lock().unwrap();
            let (sql, want_space) = match space_id {
                Some(_) => (
                    "SELECT id, enc, nonce FROM memory WHERE space_id = ?1",
                    true,
                ),
                None => ("SELECT id, enc, nonce FROM memory", false),
            };
            let mut stmt = conn.prepare(sql).map_err(be)?;
            let map = |r: &rusqlite::Row| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, Vec<u8>>(1)?,
                    r.get::<_, Vec<u8>>(2)?,
                ))
            };
            let collected: rusqlite::Result<Vec<_>> = if want_space {
                stmt.query_map(params![space_id.unwrap().to_string()], map)
                    .map_err(be)?
                    .collect()
            } else {
                stmt.query_map([], map).map_err(be)?.collect()
            };
            collected.map_err(be)?
        };
        let needle = query.to_lowercase();
        let mut out = Vec::new();
        for (id, ct, nonce) in rows {
            let rec: MemoryRecord = self.unseal("memory", &id, &ct, &nonce)?;
            if needle.is_empty() || rec.content.to_lowercase().contains(&needle) {
                out.push(rec);
                if out.len() >= limit {
                    break;
                }
            }
        }
        Ok(out)
    }
}

// ─────────────────────────── Skills ───────────────────────────

#[async_trait]
impl SkillStore for SqliteBackend {
    async fn get(&self, id: Uuid) -> Result<Option<Skill>, ContractError> {
        let key = id.to_string();
        let row = {
            let conn = self.conn.lock().unwrap();
            conn.query_row(
                "SELECT enc, nonce FROM skills WHERE id = ?1",
                params![key],
                |r| Ok((r.get::<_, Vec<u8>>(0)?, r.get::<_, Vec<u8>>(1)?)),
            )
            .optional()
            .map_err(be)?
        };
        match row {
            Some((ct, nonce)) => Ok(Some(self.unseal("skills", &key, &ct, &nonce)?)),
            None => Ok(None),
        }
    }

    async fn list_by_scope(&self, scope: SkillScope) -> Result<Vec<Skill>, ContractError> {
        let rows: Vec<(String, Vec<u8>, Vec<u8>)> = {
            let conn = self.conn.lock().unwrap();
            let mut stmt = conn
                .prepare("SELECT id, enc, nonce FROM skills WHERE scope = ?1")
                .map_err(be)?;
            let it = stmt
                .query_map(params![scope_key(&scope)], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, Vec<u8>>(1)?,
                        r.get::<_, Vec<u8>>(2)?,
                    ))
                })
                .map_err(be)?;
            let mut v = Vec::new();
            for row in it {
                v.push(row.map_err(be)?);
            }
            v
        };
        let mut out = Vec::with_capacity(rows.len());
        for (id, ct, nonce) in rows {
            out.push(self.unseal("skills", &id, &ct, &nonce)?);
        }
        Ok(out)
    }

    async fn upsert(&self, skill: Skill) -> Result<(), ContractError> {
        let id = skill.id.to_string();
        let scope = scope_key(&skill.scope);
        let (ct, nonce) = self.seal("skills", &id, &skill)?;
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO skills (id, scope, enc, nonce) VALUES (?1, ?2, ?3, ?4)",
            params![id, scope, ct, nonce],
        )
        .map_err(be)?;
        Ok(())
    }

    async fn delete(&self, id: Uuid) -> Result<(), ContractError> {
        let conn = self.conn.lock().unwrap();
        conn.execute("DELETE FROM skills WHERE id = ?1", params![id.to_string()])
            .map_err(be)?;
        Ok(())
    }
}

// ─────────────────────────── Tasks ───────────────────────────

#[async_trait]
impl TaskStore for SqliteBackend {
    async fn get(&self, id: Uuid) -> Result<Option<TaskItem>, ContractError> {
        let key = id.to_string();
        let row = {
            let conn = self.conn.lock().unwrap();
            conn.query_row(
                "SELECT enc, nonce FROM tasks WHERE id = ?1",
                params![key],
                |r| Ok((r.get::<_, Vec<u8>>(0)?, r.get::<_, Vec<u8>>(1)?)),
            )
            .optional()
            .map_err(be)?
        };
        match row {
            Some((ct, nonce)) => Ok(Some(self.unseal("tasks", &key, &ct, &nonce)?)),
            None => Ok(None),
        }
    }

    async fn upsert(&self, task: TaskItem) -> Result<(), ContractError> {
        let id = task.task_id.to_string();
        let (ct, nonce) = self.seal("tasks", &id, &task)?;
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO tasks (id, enc, nonce) VALUES (?1, ?2, ?3)",
            params![id, ct, nonce],
        )
        .map_err(be)?;
        Ok(())
    }

    async fn delete(&self, id: Uuid) -> Result<(), ContractError> {
        let conn = self.conn.lock().unwrap();
        conn.execute("DELETE FROM tasks WHERE id = ?1", params![id.to_string()])
            .map_err(be)?;
        Ok(())
    }

    async fn list(&self, limit: usize) -> Result<Vec<TaskItem>, ContractError> {
        let rows: Vec<(String, Vec<u8>, Vec<u8>)> = {
            let conn = self.conn.lock().unwrap();
            let mut stmt = conn
                .prepare("SELECT id, enc, nonce FROM tasks LIMIT ?1")
                .map_err(be)?;
            let it = stmt
                .query_map(params![limit as i64], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, Vec<u8>>(1)?,
                        r.get::<_, Vec<u8>>(2)?,
                    ))
                })
                .map_err(be)?;
            let mut v = Vec::new();
            for row in it {
                v.push(row.map_err(be)?);
            }
            v
        };
        let mut out = Vec::with_capacity(rows.len());
        for (id, ct, nonce) in rows {
            out.push(self.unseal("tasks", &id, &ct, &nonce)?);
        }
        Ok(out)
    }
}

// ─────────────────────────── Artifacts ───────────────────────────

#[async_trait]
impl ArtifactStore for SqliteBackend {
    async fn write(&self, artifact: Artifact) -> Result<(), ContractError> {
        let id = artifact.id.to_string();
        let (ct, nonce) = self.seal("artifacts", &id, &artifact)?;
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO artifacts (id, enc, nonce) VALUES (?1, ?2, ?3)",
            params![id, ct, nonce],
        )
        .map_err(be)?;
        Ok(())
    }

    async fn read(&self, id: Uuid) -> Result<Option<Artifact>, ContractError> {
        let key = id.to_string();
        let row = {
            let conn = self.conn.lock().unwrap();
            conn.query_row(
                "SELECT enc, nonce FROM artifacts WHERE id = ?1",
                params![key],
                |r| Ok((r.get::<_, Vec<u8>>(0)?, r.get::<_, Vec<u8>>(1)?)),
            )
            .optional()
            .map_err(be)?
        };
        match row {
            Some((ct, nonce)) => Ok(Some(self.unseal("artifacts", &key, &ct, &nonce)?)),
            None => Ok(None),
        }
    }
}

// ─────────────────────────── Settings ───────────────────────────

#[async_trait]
impl SettingsStore for SqliteBackend {
    async fn get(&self, key: &str) -> Result<Option<Setting>, ContractError> {
        let row = {
            let conn = self.conn.lock().unwrap();
            conn.query_row(
                "SELECT enc, nonce FROM settings WHERE key = ?1",
                params![key],
                |r| Ok((r.get::<_, Vec<u8>>(0)?, r.get::<_, Vec<u8>>(1)?)),
            )
            .optional()
            .map_err(be)?
        };
        match row {
            Some((ct, nonce)) => Ok(Some(self.unseal("settings", key, &ct, &nonce)?)),
            None => Ok(None),
        }
    }

    async fn set(&self, setting: Setting) -> Result<(), ContractError> {
        let key = setting.key.clone();
        let (ct, nonce) = self.seal("settings", &key, &setting)?;
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO settings (key, enc, nonce) VALUES (?1, ?2, ?3)",
            params![key, ct, nonce],
        )
        .map_err(be)?;
        Ok(())
    }
}

// ─────────────────────────── Checkpoints ───────────────────────────

#[async_trait]
impl CheckpointStore for SqliteBackend {
    async fn write(&self, checkpoint: Checkpoint) -> Result<(), ContractError> {
        let id = checkpoint.id.to_string();
        let conv = checkpoint.conversation_id.to_string();
        let captured = checkpoint.captured_at.timestamp_millis();
        let (ct, nonce) = self.seal("checkpoints", &id, &checkpoint)?;
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO checkpoints (id, conversation_id, captured_at, enc, nonce) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![id, conv, captured, ct, nonce],
        )
        .map_err(be)?;
        Ok(())
    }

    async fn latest(&self, conversation_id: Uuid) -> Result<Option<Checkpoint>, ContractError> {
        let row = {
            let conn = self.conn.lock().unwrap();
            conn.query_row(
                "SELECT id, enc, nonce FROM checkpoints WHERE conversation_id = ?1 ORDER BY captured_at DESC LIMIT 1",
                params![conversation_id.to_string()],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, Vec<u8>>(1)?, r.get::<_, Vec<u8>>(2)?)),
            )
            .optional()
            .map_err(be)?
        };
        match row {
            Some((id, ct, nonce)) => Ok(Some(self.unseal("checkpoints", &id, &ct, &nonce)?)),
            None => Ok(None),
        }
    }
}

// ─────────────────────────── Audit ───────────────────────────

#[async_trait]
impl AuditLog for SqliteBackend {
    async fn append(&self, mut event: AuditEvent) -> Result<(), ContractError> {
        let id = event.event_id.to_string();
        let session = event.session_id.map(|s| s.to_string());
        let conn = self.conn.lock().unwrap();
        let prev_hash: String = conn
            .query_row(
                "SELECT hash FROM audit ORDER BY seq DESC LIMIT 1",
                [],
                |r| r.get(0),
            )
            .optional()
            .map_err(be)?
            .unwrap_or_default();
        event.prev_hash = prev_hash.clone();
        let json = serde_json::to_vec(&event).map_err(ser)?;
        let hash = crypto::sha256_hex(&json);
        let key = crypto::record_key(&self.root, "audit", id.as_bytes());
        let (ct, nonce) = crypto::encrypt(&key, &json, id.as_bytes()).map_err(|_| enc_err())?;
        conn.execute(
            "INSERT INTO audit (event_id, session_id, prev_hash, hash, enc, nonce) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![id, session, prev_hash, hash, ct, nonce.to_vec()],
        )
        .map_err(be)?;
        Ok(())
    }

    async fn query(
        &self,
        session_id: Option<Uuid>,
        limit: usize,
    ) -> Result<Vec<AuditEvent>, ContractError> {
        let rows: Vec<(String, Vec<u8>, Vec<u8>)> = {
            let conn = self.conn.lock().unwrap();
            let map = |r: &rusqlite::Row| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, Vec<u8>>(1)?,
                    r.get::<_, Vec<u8>>(2)?,
                ))
            };
            let collected: rusqlite::Result<Vec<_>> = match session_id {
                Some(s) => {
                    let mut stmt = conn
                        .prepare("SELECT event_id, enc, nonce FROM audit WHERE session_id = ?1 ORDER BY seq ASC LIMIT ?2")
                        .map_err(be)?;
                    let out: rusqlite::Result<Vec<(String, Vec<u8>, Vec<u8>)>> = stmt
                        .query_map(params![s.to_string(), limit as i64], map)
                        .map_err(be)?
                        .collect();
                    out
                }
                None => {
                    let mut stmt = conn
                        .prepare("SELECT event_id, enc, nonce FROM audit ORDER BY seq ASC LIMIT ?1")
                        .map_err(be)?;
                    let out: rusqlite::Result<Vec<(String, Vec<u8>, Vec<u8>)>> = stmt
                        .query_map(params![limit as i64], map)
                        .map_err(be)?
                        .collect();
                    out
                }
            };
            collected.map_err(be)?
        };
        let mut out = Vec::with_capacity(rows.len());
        for (id, ct, nonce) in rows {
            out.push(self.unseal("audit", &id, &ct, &nonce)?);
        }
        Ok(out)
    }
}

// ─────────────────────────── Permissions ───────────────────────────

#[async_trait]
impl PermissionService for SqliteBackend {
    async fn check(
        &self,
        subject: &PermissionSubject,
        _scope: &[String],
    ) -> Result<Option<PermissionGrant>, ContractError> {
        let subj = subject_key(subject);
        let row = {
            let conn = self.conn.lock().unwrap();
            conn.query_row(
                "SELECT grant_id, enc, nonce FROM grants WHERE subject = ?1 AND revoked = 0 ORDER BY rowid DESC LIMIT 1",
                params![subj],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, Vec<u8>>(1)?, r.get::<_, Vec<u8>>(2)?)),
            )
            .optional()
            .map_err(be)?
        };
        match row {
            Some((id, ct, nonce)) => Ok(Some(self.unseal("grants", &id, &ct, &nonce)?)),
            None => Ok(None),
        }
    }

    async fn grant(&self, grant: PermissionGrant) -> Result<(), ContractError> {
        let id = grant.grant_id.to_string();
        let subj = subject_key(&grant.subject);
        let revoked = i64::from(grant.revoked_at.is_some());
        let (ct, nonce) = self.seal("grants", &id, &grant)?;
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO grants (grant_id, subject, revoked, enc, nonce) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![id, subj, revoked, ct, nonce],
        )
        .map_err(be)?;
        Ok(())
    }

    async fn revoke(&self, grant_id: Uuid) -> Result<(), ContractError> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE grants SET revoked = 1 WHERE grant_id = ?1",
            params![grant_id.to_string()],
        )
        .map_err(be)?;
        Ok(())
    }

    async fn list(&self) -> Result<Vec<PermissionGrant>, ContractError> {
        let rows: Vec<(String, Vec<u8>, Vec<u8>)> = {
            let conn = self.conn.lock().unwrap();
            let mut stmt = conn
                .prepare("SELECT grant_id, enc, nonce FROM grants")
                .map_err(be)?;
            let it = stmt
                .query_map([], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, Vec<u8>>(1)?,
                        r.get::<_, Vec<u8>>(2)?,
                    ))
                })
                .map_err(be)?;
            let mut v = Vec::new();
            for row in it {
                v.push(row.map_err(be)?);
            }
            v
        };
        let mut out = Vec::with_capacity(rows.len());
        for (id, ct, nonce) in rows {
            out.push(self.unseal("grants", &id, &ct, &nonce)?);
        }
        Ok(out)
    }
}
