//! The data seam: records and the store trait family the agent core persists through.

use crate::error::ContractError;
use crate::tooling::DestructiveLevel;
use crate::types::ComputeMode;
use async_trait::async_trait;
use futures::Stream;
use serde::{Deserialize, Serialize};
use std::pin::Pin;
use uuid::Uuid;

// ─────────────────────────── Records ───────────────────────────

/// A conversation/session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Conversation {
    pub id: Uuid,
    pub space_id: Option<Uuid>,
    pub title: Option<String>,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
    pub mode_at_creation: ComputeMode,
    pub system_prompt: Option<String>,
    pub persona_id: Option<Uuid>,
    pub active_checkpoint_id: Option<Uuid>,
}

impl Conversation {
    /// Create a new empty conversation.
    pub fn new(mode: ComputeMode) -> Self {
        let now = chrono::Utc::now();
        Self {
            id: Uuid::new_v4(),
            space_id: None,
            title: None,
            created_at: now,
            updated_at: now,
            mode_at_creation: mode,
            system_prompt: None,
            persona_id: None,
            active_checkpoint_id: None,
        }
    }
}

/// One message in a conversation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub id: Uuid,
    pub conversation_id: Uuid,
    pub role: MessageRole,
    pub content: Vec<ContentBlock>,
    pub model_id: Option<String>,
    pub mode: ComputeMode,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub sequence_num: i64,
    pub approval_refs: Vec<Uuid>,
}

impl Message {
    /// Build a plain-text message.
    pub fn text(
        conversation_id: Uuid,
        role: MessageRole,
        text: impl Into<String>,
        mode: ComputeMode,
        sequence_num: i64,
    ) -> Self {
        Self {
            id: Uuid::new_v4(),
            conversation_id,
            role,
            content: vec![ContentBlock::Text { text: text.into() }],
            model_id: None,
            mode,
            created_at: chrono::Utc::now(),
            sequence_num,
            approval_refs: Vec::new(),
        }
    }

    /// Concatenate all text blocks into a single string.
    pub fn text_content(&self) -> String {
        self.content
            .iter()
            .filter_map(|b| match b {
                ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MessageRole {
    User,
    Assistant,
    Tool,
    System,
}

/// A unit of message content.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum ContentBlock {
    Text {
        text: String,
    },
    ToolCall {
        call_id: String,
        tool_id: String,
        args: serde_json::Value,
    },
    ToolResult {
        call_id: String,
        output: serde_json::Value,
    },
    ArtifactRef {
        artifact_id: Uuid,
    },
}

/// A persistent memory fact about the user/project.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryRecord {
    pub id: Uuid,
    pub space_id: Option<Uuid>,
    pub kind: MemoryKind,
    pub content: String,
    pub embedding_ref: Option<Uuid>,
    pub source_message_id: Option<Uuid>,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
    pub user_visible: bool,
    pub user_confirmed: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MemoryKind {
    Fact,
    Preference,
    ProjectNote,
    System,
}

/// An installable, markdown-defined task playbook.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Skill {
    pub id: Uuid,
    pub slug: String,
    pub scope: SkillScope,
    pub source_markdown: String,
    pub installed_by: Uuid,
    pub version: String,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SkillScope {
    Global,
    Space(Uuid),
    User(Uuid),
}

/// A unit of work on the cross-device task queue.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskItem {
    pub task_id: Uuid,
    pub origin_device: Uuid,
    pub assigned_device: Option<Uuid>,
    pub status: TaskStatus,
    pub summary: String,
    pub capabilities_required: Vec<String>,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
    pub expires_at: Option<chrono::DateTime<chrono::Utc>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TaskStatus {
    Pending,
    Running,
    AwaitingApproval,
    Completed,
    Cancelled,
    Failed,
}

/// A rendered artifact (HTML/SVG/chart/doc/image).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Artifact {
    pub id: Uuid,
    pub conversation_id: Option<Uuid>,
    pub kind: String,
    pub content_type: String,
    pub bytes: Vec<u8>,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

/// A namespaced setting value.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Setting {
    pub key: String,
    pub value: serde_json::Value,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

/// A serialized conversation checkpoint for cross-mode handoff.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Checkpoint {
    pub id: Uuid,
    pub conversation_id: Uuid,
    pub captured_at: chrono::DateTime<chrono::Utc>,
    pub mode_at_capture: ComputeMode,
    /// Opaque serialized engine state.
    pub state: serde_json::Value,
}

// ─────────────────────────── Security records ───────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditEvent {
    pub event_id: Uuid,
    pub device_id: Uuid,
    pub session_id: Option<Uuid>,
    pub event_type: AuditEventType,
    pub actor: AuditActor,
    pub resource_ref: Option<String>,
    pub outcome: AuditOutcome,
    pub metadata: serde_json::Value,
    pub timestamp: chrono::DateTime<chrono::Utc>,
    /// Hash-chain link to the previous event (hex). Empty for the genesis event.
    pub prev_hash: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AuditEventType {
    ToolCall,
    FileAccess,
    UiAction,
    SyncOp,
    PermissionChange,
    HardRuleBlock,
    ApprovalWall,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AuditActor {
    User,
    Agent,
    System,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AuditOutcome {
    Allowed,
    Denied,
    Pending,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PermissionGrant {
    pub grant_id: Uuid,
    pub subject: PermissionSubject,
    pub scope: Vec<String>,
    pub tier: PermissionTier,
    pub access_level: AccessLevel,
    pub device_id: Uuid,
    pub prompt_text: String,
    pub granted_at: chrono::DateTime<chrono::Utc>,
    pub expires_at: Option<chrono::DateTime<chrono::Utc>>,
    pub revoked_at: Option<chrono::DateTime<chrono::Utc>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PermissionSubject {
    Tool(String),
    Feature(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PermissionTier {
    AllowOnce,
    AllowAlways,
    Deny,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AccessLevel {
    ReadOnly,
    Click,
    Full,
}

/// A pending approval request surfaced to the user.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApprovalRequest {
    pub id: Uuid,
    pub conversation_id: Uuid,
    pub message_id: Uuid,
    pub tool_invocation_id: Uuid,
    pub action_summary: String,
    pub destructive_level: DestructiveLevel,
    pub expires_at: Option<chrono::DateTime<chrono::Utc>>,
    pub status: ApprovalStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ApprovalStatus {
    Pending,
    Approved,
    Denied,
    TimedOut,
}

/// A user's response to an [`ApprovalRequest`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApprovalResponse {
    pub request_id: Uuid,
    pub decision: ApprovalDecision,
    pub responded_at: chrono::DateTime<chrono::Utc>,
    pub device_id: Uuid,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ApprovalDecision {
    Approve,
    Deny,
    AllowAlways,
}

// ─────────────────────────── Store traits ───────────────────────────

/// A stream of change notifications for a subscribed collection.
pub type SubscriptionStream<T> = Pin<Box<dyn Stream<Item = Result<T, ContractError>> + Send>>;

#[async_trait]
pub trait ConversationStore: Send + Sync {
    async fn get(&self, id: Uuid) -> Result<Option<Conversation>, ContractError>;
    async fn upsert(&self, conv: Conversation) -> Result<(), ContractError>;
    async fn delete(&self, id: Uuid) -> Result<(), ContractError>;
    async fn list(&self, limit: usize) -> Result<Vec<Conversation>, ContractError>;
}

#[async_trait]
pub trait MessageStore: Send + Sync {
    async fn append(&self, msg: Message) -> Result<(), ContractError>;
    async fn range(
        &self,
        conversation_id: Uuid,
        limit: usize,
    ) -> Result<Vec<Message>, ContractError>;
    async fn next_sequence(&self, conversation_id: Uuid) -> Result<i64, ContractError>;
}

#[async_trait]
pub trait MemoryStore: Send + Sync {
    async fn get(&self, id: Uuid) -> Result<Option<MemoryRecord>, ContractError>;
    async fn upsert(&self, record: MemoryRecord) -> Result<(), ContractError>;
    async fn delete(&self, id: Uuid) -> Result<(), ContractError>;
    async fn semantic_search(
        &self,
        query: &str,
        space_id: Option<Uuid>,
        limit: usize,
    ) -> Result<Vec<MemoryRecord>, ContractError>;
}

#[async_trait]
pub trait SkillStore: Send + Sync {
    async fn get(&self, id: Uuid) -> Result<Option<Skill>, ContractError>;
    async fn list_by_scope(&self, scope: SkillScope) -> Result<Vec<Skill>, ContractError>;
    async fn upsert(&self, skill: Skill) -> Result<(), ContractError>;
    async fn delete(&self, id: Uuid) -> Result<(), ContractError>;
}

#[async_trait]
pub trait TaskStore: Send + Sync {
    async fn get(&self, id: Uuid) -> Result<Option<TaskItem>, ContractError>;
    async fn upsert(&self, task: TaskItem) -> Result<(), ContractError>;
    async fn delete(&self, id: Uuid) -> Result<(), ContractError>;
    async fn list(&self, limit: usize) -> Result<Vec<TaskItem>, ContractError>;
}

#[async_trait]
pub trait ArtifactStore: Send + Sync {
    async fn write(&self, artifact: Artifact) -> Result<(), ContractError>;
    async fn read(&self, id: Uuid) -> Result<Option<Artifact>, ContractError>;
}

#[async_trait]
pub trait SettingsStore: Send + Sync {
    async fn get(&self, key: &str) -> Result<Option<Setting>, ContractError>;
    async fn set(&self, setting: Setting) -> Result<(), ContractError>;
}

#[async_trait]
pub trait CheckpointStore: Send + Sync {
    async fn write(&self, checkpoint: Checkpoint) -> Result<(), ContractError>;
    async fn latest(&self, conversation_id: Uuid) -> Result<Option<Checkpoint>, ContractError>;
}

#[async_trait]
pub trait AuditLog: Send + Sync {
    async fn append(&self, event: AuditEvent) -> Result<(), ContractError>;
    async fn query(
        &self,
        session_id: Option<Uuid>,
        limit: usize,
    ) -> Result<Vec<AuditEvent>, ContractError>;
}

#[async_trait]
pub trait PermissionService: Send + Sync {
    async fn check(
        &self,
        subject: &PermissionSubject,
        scope: &[String],
    ) -> Result<Option<PermissionGrant>, ContractError>;
    async fn grant(&self, grant: PermissionGrant) -> Result<(), ContractError>;
    async fn revoke(&self, grant_id: Uuid) -> Result<(), ContractError>;
    async fn list(&self) -> Result<Vec<PermissionGrant>, ContractError>;
}

/// A composite handle bundling every store facet the agent core needs.
#[derive(Clone)]
pub struct DataStore {
    pub conversations: std::sync::Arc<dyn ConversationStore>,
    pub messages: std::sync::Arc<dyn MessageStore>,
    pub memory: std::sync::Arc<dyn MemoryStore>,
    pub skills: std::sync::Arc<dyn SkillStore>,
    pub tasks: std::sync::Arc<dyn TaskStore>,
    pub artifacts: std::sync::Arc<dyn ArtifactStore>,
    pub settings: std::sync::Arc<dyn SettingsStore>,
    pub checkpoints: std::sync::Arc<dyn CheckpointStore>,
    pub audit: std::sync::Arc<dyn AuditLog>,
    pub permissions: std::sync::Arc<dyn PermissionService>,
}
