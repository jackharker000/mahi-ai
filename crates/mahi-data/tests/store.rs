//! Integration tests for the mahi-data trust backbone.

use mahi_contracts::data::*;
use mahi_contracts::ComputeMode;
use uuid::Uuid;

#[tokio::test]
async fn conversation_and_message_round_trip() {
    let store = mahi_data::open_in_memory().expect("open");
    let conv = Conversation::new(ComputeMode::OnDevice);
    store
        .conversations
        .upsert(conv.clone())
        .await
        .expect("upsert conv");

    let seq = store.messages.next_sequence(conv.id).await.expect("seq");
    assert_eq!(seq, 0);
    let msg = Message::text(
        conv.id,
        MessageRole::User,
        "hello",
        ComputeMode::OnDevice,
        seq,
    );
    store.messages.append(msg.clone()).await.expect("append");

    let got = store
        .conversations
        .get(conv.id)
        .await
        .expect("get")
        .expect("present");
    assert_eq!(got.id, conv.id);

    let msgs = store.messages.range(conv.id, 10).await.expect("range");
    assert_eq!(msgs.len(), 1);
    assert_eq!(msgs[0].text_content(), "hello");
    assert_eq!(store.messages.next_sequence(conv.id).await.unwrap(), 1);

    let list = store.conversations.list(10).await.expect("list");
    assert_eq!(list.len(), 1);
}

#[tokio::test]
async fn memory_search_and_persistence_via_disk() {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = mahi_data::open_store(dir.path()).expect("open_store");
    let now = chrono::Utc::now();
    let rec = MemoryRecord {
        id: Uuid::new_v4(),
        space_id: None,
        kind: MemoryKind::Fact,
        content: "The user prefers dark mode".to_string(),
        embedding_ref: None,
        source_message_id: None,
        created_at: now,
        updated_at: now,
        user_visible: true,
        user_confirmed: true,
    };
    store.memory.upsert(rec.clone()).await.expect("upsert");

    let hits = store
        .memory
        .semantic_search("dark", None, 10)
        .await
        .expect("search");
    assert_eq!(hits.len(), 1);
    let miss = store
        .memory
        .semantic_search("zzz", None, 10)
        .await
        .expect("search");
    assert!(miss.is_empty());

    // Re-open: the key file persists, so the encrypted record decrypts again.
    drop(store);
    let reopened = mahi_data::open_store(dir.path()).expect("reopen");
    assert!(reopened.memory.get(rec.id).await.expect("get").is_some());
}

#[tokio::test]
async fn audit_chain_is_tamper_evident() {
    use mahi_data::SqliteBackend;
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    let backend = SqliteBackend::new(conn, [7u8; 32]).expect("backend");

    for i in 0..5 {
        let ev = AuditEvent {
            event_id: Uuid::new_v4(),
            device_id: Uuid::nil(),
            session_id: None,
            event_type: AuditEventType::ToolCall,
            actor: AuditActor::Agent,
            resource_ref: Some(format!("tool-{i}")),
            outcome: AuditOutcome::Allowed,
            metadata: serde_json::json!({ "i": i }),
            timestamp: chrono::Utc::now(),
            prev_hash: String::new(),
        };
        AuditLog::append(&*backend, ev).await.expect("append");
    }
    assert!(backend.verify_chain().expect("verify"));

    let events = AuditLog::query(&*backend, None, 10).await.expect("query");
    assert_eq!(events.len(), 5);
    // Each event carries the previous event's hash as prev_hash (genesis = "").
    assert_eq!(events[0].prev_hash, "");
    assert!(!events[1].prev_hash.is_empty());
}

#[tokio::test]
async fn permission_grant_check_revoke() {
    let store = mahi_data::open_in_memory().expect("open");
    let subject = PermissionSubject::Tool("shell_exec".to_string());
    assert!(store
        .permissions
        .check(&subject, &[])
        .await
        .expect("check")
        .is_none());

    let grant = PermissionGrant {
        grant_id: Uuid::new_v4(),
        subject: subject.clone(),
        scope: vec!["cwd".to_string()],
        tier: PermissionTier::AllowAlways,
        access_level: AccessLevel::Full,
        device_id: Uuid::nil(),
        prompt_text: "Allow shell?".to_string(),
        granted_at: chrono::Utc::now(),
        expires_at: None,
        revoked_at: None,
    };
    let gid = grant.grant_id;
    store.permissions.grant(grant).await.expect("grant");
    assert!(store
        .permissions
        .check(&subject, &[])
        .await
        .expect("check")
        .is_some());

    store.permissions.revoke(gid).await.expect("revoke");
    assert!(store
        .permissions
        .check(&subject, &[])
        .await
        .expect("check")
        .is_none());
}
