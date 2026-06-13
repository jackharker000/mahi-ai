//! Integration tests for the FFI engine handle, exercised exactly as the
//! Swift bridge calls it (synchronous, offline — the placeholder provider
//! answers when no real model is configured).

use mahi_ffi::{AgentEventFfi, HostedConfigFfi, MahiEngineHandle, ModelStateFfi, RuntimeStatusFfi};

#[test]
fn in_memory_engine_lists_the_full_catalog_all_not_installed() {
    let handle = MahiEngineHandle::in_memory().expect("engine builds");
    let catalog = handle.model_catalog().expect("catalog");
    assert_eq!(catalog.len(), 9, "the curated catalog has 9 models");
    assert!(
        catalog
            .iter()
            .all(|m| matches!(m.state, ModelStateFfi::NotInstalled)),
        "nothing is installed in a fresh in-memory engine"
    );
    assert!(matches!(handle.runtime_status(), RuntimeStatusFfi::NoModel));
}

#[test]
fn placeholder_turn_streams_a_setup_hint_and_finishes() {
    let handle = MahiEngineHandle::in_memory().unwrap();
    let conversation = handle.create_conversation().unwrap();
    let turn = handle
        .start_turn(conversation, "hello".to_string())
        .unwrap();

    let mut text = String::new();
    let mut finished = false;
    loop {
        let batch = turn.poll_batch(32);
        if batch.is_empty() {
            break; // the stream ended
        }
        for event in batch {
            match event {
                AgentEventFfi::TextDelta { text: delta } => text.push_str(&delta),
                AgentEventFfi::TurnFinished { .. } => finished = true,
                _ => {}
            }
        }
    }
    assert!(finished, "the turn must finish");
    assert!(
        text.contains("No model is loaded"),
        "placeholder guidance streamed, got: {text:?}"
    );
}

#[test]
fn setting_and_clearing_hosted_config_moves_runtime_status() {
    let handle = MahiEngineHandle::in_memory().unwrap();
    handle
        .set_hosted_config(Some(HostedConfigFfi {
            provider: "openai".to_string(),
            api_key: "test-key".to_string(),
            model: "test-model".to_string(),
            base_url: Some("http://127.0.0.1:9".to_string()),
        }))
        .unwrap();
    match handle.runtime_status() {
        RuntimeStatusFfi::Running { model_id } => assert_eq!(model_id, "test-model"),
        other => panic!("expected Running, got {other:?}"),
    }

    handle.set_hosted_config(None).unwrap();
    assert!(matches!(handle.runtime_status(), RuntimeStatusFfi::NoModel));
}

#[test]
fn spawn_subagents_returns_one_summary_per_goal() {
    let handle = MahiEngineHandle::in_memory().unwrap();
    let summaries = handle
        .spawn_subagents(vec!["research X".to_string(), "draft Y".to_string()])
        .expect("subagents run");
    assert_eq!(summaries.len(), 2);
}

#[test]
fn downloads_and_unknown_models_are_handled_gracefully() {
    let handle = MahiEngineHandle::in_memory().unwrap();
    // Cancelling/deleting a model that was never downloaded must not panic.
    handle
        .cancel_download("qwen2.5-0.5b-instruct".to_string())
        .unwrap();
    assert!(handle
        .delete_model("qwen2.5-0.5b-instruct".to_string())
        .is_err());
    // Activating a model that isn't downloaded is a clean error.
    assert!(handle
        .activate_model("llama-3.1-8b-instruct".to_string(), 8192)
        .is_err());
    // An unknown id is rejected.
    assert!(handle
        .start_download("not-a-real-model".to_string())
        .is_err());
}
