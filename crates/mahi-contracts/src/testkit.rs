//! Test doubles for use by dependent crates (behind the `testkit` feature):
//! a deterministic mock inference provider and a fully in-memory [`DataStore`].

use crate::compute::{
    CanHandleResult, FinishReason, InferenceChunk, InferenceProvider, InferenceRequest,
    InferenceStream,
};
use crate::data::*;
use crate::error::ContractError;
use crate::types::{
    CapabilitySet, ComputeMode, LimitationLabel, ModelDescriptor, ModelSource, PerfProfile,
};
use async_trait::async_trait;
use futures::stream;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

/// A deterministic provider that streams a canned reply word-by-word.
///
/// Always reports `ComputeMode::OnDevice` so it works fully offline.
pub struct MockInferenceProvider {
    pub reply: String,
}

impl Default for MockInferenceProvider {
    fn default() -> Self {
        Self {
            reply: "Hello from Mahi. This is the on-device mock model.".to_string(),
        }
    }
}

impl MockInferenceProvider {
    /// A provider that echoes the user's last message back.
    pub fn echo() -> Self {
        Self {
            reply: String::new(),
        }
    }
}

#[async_trait]
impl InferenceProvider for MockInferenceProvider {
    fn descriptor(&self) -> ModelDescriptor {
        ModelDescriptor {
            id: "mock-on-device".to_string(),
            display_name: "Mahi Mock (on-device)".to_string(),
            context_window: 4096,
            capabilities: CapabilitySet {
                vision: false,
                tool_calling: false,
                min_context_window: 0,
                code_gen: false,
            },
            limitations: vec![
                LimitationLabel::NoComplexCodeGen,
                LimitationLabel::MaxContextWindow(4096),
            ],
            size_bytes: None,
            quantization: None,
            source: ModelSource::OnDevice,
            perf_profile: PerfProfile {
                ttft_ms: 20,
                tok_per_sec: 40.0,
            },
        }
    }

    async fn can_handle(&self, _req: &InferenceRequest) -> CanHandleResult {
        CanHandleResult::capable()
    }

    async fn generate(
        &self,
        req: InferenceRequest,
        _cancel: CancellationToken,
    ) -> Result<InferenceStream, ContractError> {
        let reply = if self.reply.is_empty() {
            let last = req
                .messages
                .last()
                .map(|m| m.text_content())
                .unwrap_or_default();
            format!("You said: {last}")
        } else {
            self.reply.clone()
        };

        let mut chunks: Vec<Result<InferenceChunk, ContractError>> = reply
            .split_inclusive(' ')
            .map(|w| Ok(InferenceChunk::text(w.to_string(), ComputeMode::OnDevice)))
            .collect();
        chunks.push(Ok(InferenceChunk::finish(
            FinishReason::Stop,
            ComputeMode::OnDevice,
        )));

        Ok(Box::pin(stream::iter(chunks)))
    }
}

/// A fully in-memory backend implementing every store trait. Useful for wiring
/// a working [`DataStore`] in tests without a database.
#[derive(Default)]
pub struct MemBackend {
    conversations: Mutex<HashMap<Uuid, Conversation>>,
    messages: Mutex<Vec<Message>>,
    memory: Mutex<HashMap<Uuid, MemoryRecord>>,
    skills: Mutex<HashMap<Uuid, Skill>>,
    tasks: Mutex<HashMap<Uuid, TaskItem>>,
    artifacts: Mutex<HashMap<Uuid, Artifact>>,
    settings: Mutex<HashMap<String, Setting>>,
    checkpoints: Mutex<Vec<Checkpoint>>,
    audit: Mutex<Vec<AuditEvent>>,
    grants: Mutex<Vec<PermissionGrant>>,
}

/// Build a working in-memory [`DataStore`] backed by a single [`MemBackend`].
pub fn in_memory_datastore() -> DataStore {
    let b = Arc::new(MemBackend::default());
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

#[async_trait]
impl ConversationStore for MemBackend {
    async fn get(&self, id: Uuid) -> Result<Option<Conversation>, ContractError> {
        Ok(self.conversations.lock().unwrap().get(&id).cloned())
    }
    async fn upsert(&self, conv: Conversation) -> Result<(), ContractError> {
        self.conversations.lock().unwrap().insert(conv.id, conv);
        Ok(())
    }
    async fn delete(&self, id: Uuid) -> Result<(), ContractError> {
        self.conversations.lock().unwrap().remove(&id);
        Ok(())
    }
    async fn list(&self, limit: usize) -> Result<Vec<Conversation>, ContractError> {
        let mut v: Vec<_> = self
            .conversations
            .lock()
            .unwrap()
            .values()
            .cloned()
            .collect();
        v.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
        v.truncate(limit);
        Ok(v)
    }
}

#[async_trait]
impl MessageStore for MemBackend {
    async fn append(&self, msg: Message) -> Result<(), ContractError> {
        self.messages.lock().unwrap().push(msg);
        Ok(())
    }
    async fn range(
        &self,
        conversation_id: Uuid,
        limit: usize,
    ) -> Result<Vec<Message>, ContractError> {
        let mut v: Vec<_> = self
            .messages
            .lock()
            .unwrap()
            .iter()
            .filter(|m| m.conversation_id == conversation_id)
            .cloned()
            .collect();
        v.sort_by_key(|m| m.sequence_num);
        v.truncate(limit);
        Ok(v)
    }
    async fn next_sequence(&self, conversation_id: Uuid) -> Result<i64, ContractError> {
        let max = self
            .messages
            .lock()
            .unwrap()
            .iter()
            .filter(|m| m.conversation_id == conversation_id)
            .map(|m| m.sequence_num)
            .max();
        Ok(max.map(|m| m + 1).unwrap_or(0))
    }
}

#[async_trait]
impl MemoryStore for MemBackend {
    async fn get(&self, id: Uuid) -> Result<Option<MemoryRecord>, ContractError> {
        Ok(self.memory.lock().unwrap().get(&id).cloned())
    }
    async fn upsert(&self, record: MemoryRecord) -> Result<(), ContractError> {
        self.memory.lock().unwrap().insert(record.id, record);
        Ok(())
    }
    async fn delete(&self, id: Uuid) -> Result<(), ContractError> {
        self.memory.lock().unwrap().remove(&id);
        Ok(())
    }
    async fn semantic_search(
        &self,
        query: &str,
        space_id: Option<Uuid>,
        limit: usize,
    ) -> Result<Vec<MemoryRecord>, ContractError> {
        let q = query.to_lowercase();
        let mut v: Vec<_> = self
            .memory
            .lock()
            .unwrap()
            .values()
            .filter(|r| space_id.is_none() || r.space_id == space_id)
            .filter(|r| q.is_empty() || r.content.to_lowercase().contains(&q))
            .cloned()
            .collect();
        v.truncate(limit);
        Ok(v)
    }
}

#[async_trait]
impl SkillStore for MemBackend {
    async fn get(&self, id: Uuid) -> Result<Option<Skill>, ContractError> {
        Ok(self.skills.lock().unwrap().get(&id).cloned())
    }
    async fn list_by_scope(&self, scope: SkillScope) -> Result<Vec<Skill>, ContractError> {
        Ok(self
            .skills
            .lock()
            .unwrap()
            .values()
            .filter(|s| s.scope == scope)
            .cloned()
            .collect())
    }
    async fn upsert(&self, skill: Skill) -> Result<(), ContractError> {
        self.skills.lock().unwrap().insert(skill.id, skill);
        Ok(())
    }
    async fn delete(&self, id: Uuid) -> Result<(), ContractError> {
        self.skills.lock().unwrap().remove(&id);
        Ok(())
    }
}

#[async_trait]
impl TaskStore for MemBackend {
    async fn get(&self, id: Uuid) -> Result<Option<TaskItem>, ContractError> {
        Ok(self.tasks.lock().unwrap().get(&id).cloned())
    }
    async fn upsert(&self, task: TaskItem) -> Result<(), ContractError> {
        self.tasks.lock().unwrap().insert(task.task_id, task);
        Ok(())
    }
    async fn delete(&self, id: Uuid) -> Result<(), ContractError> {
        self.tasks.lock().unwrap().remove(&id);
        Ok(())
    }
    async fn list(&self, limit: usize) -> Result<Vec<TaskItem>, ContractError> {
        let mut v: Vec<_> = self.tasks.lock().unwrap().values().cloned().collect();
        v.truncate(limit);
        Ok(v)
    }
}

#[async_trait]
impl ArtifactStore for MemBackend {
    async fn write(&self, artifact: Artifact) -> Result<(), ContractError> {
        self.artifacts.lock().unwrap().insert(artifact.id, artifact);
        Ok(())
    }
    async fn read(&self, id: Uuid) -> Result<Option<Artifact>, ContractError> {
        Ok(self.artifacts.lock().unwrap().get(&id).cloned())
    }
}

#[async_trait]
impl SettingsStore for MemBackend {
    async fn get(&self, key: &str) -> Result<Option<Setting>, ContractError> {
        Ok(self.settings.lock().unwrap().get(key).cloned())
    }
    async fn set(&self, setting: Setting) -> Result<(), ContractError> {
        self.settings
            .lock()
            .unwrap()
            .insert(setting.key.clone(), setting);
        Ok(())
    }
}

#[async_trait]
impl CheckpointStore for MemBackend {
    async fn write(&self, checkpoint: Checkpoint) -> Result<(), ContractError> {
        self.checkpoints.lock().unwrap().push(checkpoint);
        Ok(())
    }
    async fn latest(&self, conversation_id: Uuid) -> Result<Option<Checkpoint>, ContractError> {
        Ok(self
            .checkpoints
            .lock()
            .unwrap()
            .iter()
            .filter(|c| c.conversation_id == conversation_id)
            .max_by_key(|c| c.captured_at)
            .cloned())
    }
}

#[async_trait]
impl AuditLog for MemBackend {
    async fn append(&self, event: AuditEvent) -> Result<(), ContractError> {
        self.audit.lock().unwrap().push(event);
        Ok(())
    }
    async fn query(
        &self,
        session_id: Option<Uuid>,
        limit: usize,
    ) -> Result<Vec<AuditEvent>, ContractError> {
        let mut v: Vec<_> = self
            .audit
            .lock()
            .unwrap()
            .iter()
            .filter(|e| session_id.is_none() || e.session_id == session_id)
            .cloned()
            .collect();
        v.truncate(limit);
        Ok(v)
    }
}

#[async_trait]
impl PermissionService for MemBackend {
    async fn check(
        &self,
        subject: &PermissionSubject,
        _scope: &[String],
    ) -> Result<Option<PermissionGrant>, ContractError> {
        Ok(self
            .grants
            .lock()
            .unwrap()
            .iter()
            .find(|g| &g.subject == subject && g.revoked_at.is_none())
            .cloned())
    }
    async fn grant(&self, grant: PermissionGrant) -> Result<(), ContractError> {
        self.grants.lock().unwrap().push(grant);
        Ok(())
    }
    async fn revoke(&self, grant_id: Uuid) -> Result<(), ContractError> {
        if let Some(g) = self
            .grants
            .lock()
            .unwrap()
            .iter_mut()
            .find(|g| g.grant_id == grant_id)
        {
            g.revoked_at = Some(chrono::Utc::now());
        }
        Ok(())
    }
    async fn list(&self) -> Result<Vec<PermissionGrant>, ContractError> {
        Ok(self.grants.lock().unwrap().clone())
    }
}
