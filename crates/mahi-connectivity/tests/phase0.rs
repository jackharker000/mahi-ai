//! Phase 0 integration tests: LocalBus routing, TCP loopback framing, and
//! session lifecycle (issue / expire / kill).

use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use mahi_connectivity::{
    Capability, LocalBus, MultiplexedBus, SessionBoundBus, SessionManager, TcpTransport,
};
use mahi_contracts::connectivity::{ChannelId, MultiplexedEnvelope};
use mahi_contracts::error::ConnectivityError;
use serde_json::json;
use tokio::time::timeout;
use uuid::Uuid;

const RECV_TIMEOUT: Duration = Duration::from_secs(2);
const SILENCE_TIMEOUT: Duration = Duration::from_millis(150);

/// An envelope carrying an InferenceChunk-shaped JSON payload.
fn inference_chunk_envelope(session_id: Uuid, seq_no: u64) -> MultiplexedEnvelope {
    let payload = json!({
        "request_id": Uuid::new_v4(),
        "token": "Wel",
        "index": seq_no,
        "is_final": false,
    });
    MultiplexedEnvelope {
        channel: ChannelId::Inference,
        seq_no,
        session_id,
        payload_type: "InferenceChunk".to_string(),
        payload_bytes: serde_json::to_vec(&payload).expect("payload serializes"),
    }
}

#[tokio::test]
async fn local_bus_routes_inference_envelope_to_channel_subscribers_only() {
    let bus = LocalBus::new();
    let session_id = Uuid::new_v4();

    // Subscribe before sending: broadcast semantics deliver only to existing
    // subscribers.
    let mut inference_sub = bus.subscribe(ChannelId::Inference);
    let mut sync_sub = bus.subscribe(ChannelId::Sync);

    let sent = inference_chunk_envelope(session_id, 42);
    bus.send(sent.clone()).await.expect("send succeeds");

    let got = timeout(RECV_TIMEOUT, inference_sub.next())
        .await
        .expect("inference subscriber should receive within timeout")
        .expect("stream should not have ended");

    // Full payload integrity.
    assert_eq!(got.channel, ChannelId::Inference);
    assert_eq!(got.seq_no, 42);
    assert_eq!(got.session_id, session_id);
    assert_eq!(got.payload_type, "InferenceChunk");
    assert_eq!(got.payload_bytes, sent.payload_bytes);
    let decoded: serde_json::Value =
        serde_json::from_slice(&got.payload_bytes).expect("payload is valid JSON");
    assert_eq!(decoded["token"], "Wel");
    assert_eq!(decoded["index"], 42);

    // A subscriber on a *different* channel must not see it.
    let nothing = timeout(SILENCE_TIMEOUT, sync_sub.next()).await;
    assert!(
        nothing.is_err(),
        "Sync subscriber must not receive an Inference envelope, got {:?}",
        nothing
    );
}

#[tokio::test]
async fn local_bus_delivers_control_and_fails_after_close() {
    let bus = LocalBus::new();
    let mut control_sub = bus.subscribe(ChannelId::Control);

    let kill = MultiplexedEnvelope {
        channel: ChannelId::Control,
        seq_no: 1,
        session_id: Uuid::new_v4(),
        payload_type: "KillSwitch".to_string(),
        payload_bytes: b"{}".to_vec(),
    };
    bus.send(kill.clone()).await.expect("control send succeeds");

    let got = timeout(RECV_TIMEOUT, control_sub.next())
        .await
        .expect("control subscriber should receive")
        .expect("stream open");
    assert_eq!(got.payload_type, "KillSwitch");

    bus.close();
    assert!(bus.is_closed());
    let err = bus.send(kill).await.expect_err("send after close must fail");
    assert!(matches!(
        err,
        ConnectivityError::ChannelClosed { channel: ChannelId::Control }
    ));
}

#[tokio::test]
async fn tcp_loopback_delivers_framed_envelope() {
    // Bind an ephemeral port; the returned bus is usable before any peer
    // connects.
    let server = TcpTransport::listen("127.0.0.1:0")
        .await
        .expect("listen on loopback");
    let addr = server.local_addr();
    assert_ne!(addr.port(), 0, "OS must have assigned a real port");

    let mut server_inference = server.subscribe(ChannelId::Inference);
    let mut server_sync = server.subscribe(ChannelId::Sync);

    let client = TcpTransport::connect(addr).await.expect("connect to listener");

    let session_id = Uuid::new_v4();
    let sent = inference_chunk_envelope(session_id, 7);
    client.send(sent.clone()).await.expect("client send succeeds");

    let got = timeout(RECV_TIMEOUT, server_inference.next())
        .await
        .expect("server should receive the frame within timeout")
        .expect("stream open");

    // The frame survived encode -> u32-BE-length-prefix -> decode intact.
    assert_eq!(got.channel, ChannelId::Inference);
    assert_eq!(got.seq_no, 7);
    assert_eq!(got.session_id, session_id);
    assert_eq!(got.payload_type, "InferenceChunk");
    assert_eq!(got.payload_bytes, sent.payload_bytes);

    // Channel isolation holds across the wire too.
    assert!(
        timeout(SILENCE_TIMEOUT, server_sync.next()).await.is_err(),
        "Sync subscriber must not see an Inference frame"
    );
}

#[tokio::test]
async fn tcp_loopback_is_bidirectional() {
    let server = TcpTransport::listen("127.0.0.1:0").await.expect("listen");
    let addr = server.local_addr();
    let client = TcpTransport::connect(addr).await.expect("connect");

    let mut client_control = client.subscribe(ChannelId::Control);

    // Server -> client this time.
    let heartbeat = MultiplexedEnvelope {
        channel: ChannelId::Control,
        seq_no: 1,
        session_id: Uuid::new_v4(),
        payload_type: "Heartbeat".to_string(),
        payload_bytes: serde_json::to_vec(&json!({ "at": "2026-06-11T00:00:00Z" })).unwrap(),
    };
    server.send(heartbeat.clone()).await.expect("server send succeeds");

    let got = timeout(RECV_TIMEOUT, client_control.next())
        .await
        .expect("client should receive heartbeat")
        .expect("stream open");
    assert_eq!(got.payload_type, "Heartbeat");
    assert_eq!(got.payload_bytes, heartbeat.payload_bytes);
}

#[tokio::test]
async fn session_fresh_token_validates() {
    let manager = SessionManager::new();
    let peer = Uuid::new_v4();
    let token = manager.issue(peer, vec![Capability::Inference, Capability::Takeover]);

    assert_eq!(token.peer_id, peer);
    assert!(token.expires_at > token.issued_at);
    assert!(token.has_capability(Capability::Inference));
    assert!(!token.hmac.is_empty());

    manager.validate(&token).expect("fresh token must validate");
    assert_eq!(manager.active_sessions(), vec![token.session_id]);
}

#[tokio::test]
async fn session_expired_token_is_rejected() {
    let manager = SessionManager::new();
    let token = manager.issue_with_ttl(
        Uuid::new_v4(),
        vec![Capability::Inference],
        chrono::Duration::milliseconds(-1), // already expired at issue time
    );

    let err = manager.validate(&token).expect_err("expired token must fail");
    assert!(matches!(err, ConnectivityError::Transport { .. }));
    assert!(err.to_string().contains("expired"), "got: {err}");
}

#[tokio::test]
async fn session_tampered_token_is_rejected() {
    let manager = SessionManager::new();
    let mut token = manager.issue(Uuid::new_v4(), vec![Capability::Inference]);

    // Forge a longer expiry without re-signing.
    token.expires_at += chrono::Duration::days(365);
    let err = manager.validate(&token).expect_err("tampered token must fail");
    assert!(err.to_string().contains("signature"), "got: {err}");
}

#[tokio::test]
async fn session_kill_invalidates_and_downstream_sends_fail() {
    let manager = Arc::new(SessionManager::new());
    let token = manager.issue(Uuid::new_v4(), vec![Capability::Inference, Capability::Takeover]);
    manager.validate(&token).expect("valid before kill");

    // The takeover ("controlling") indicator flag.
    manager
        .set_controlling(token.session_id, true)
        .expect("takeover-capable session can control");
    assert!(manager.is_controlling(token.session_id));

    // Bind a bus to the session: sends work while the session is alive...
    let bus = SessionBoundBus::new(LocalBus::new(), Arc::clone(&manager), token.session_id);
    let mut sub = bus.subscribe(ChannelId::Inference);
    bus.send(inference_chunk_envelope(token.session_id, 1))
        .await
        .expect("send on live session succeeds");
    timeout(RECV_TIMEOUT, sub.next())
        .await
        .expect("delivered")
        .expect("stream open");

    // ...kill it...
    assert!(manager.kill(token.session_id));

    // ...and everything downstream is dead.
    let err = manager.validate(&token).expect_err("killed session must fail");
    assert!(err.to_string().contains("killed"), "got: {err}");
    assert!(!manager.is_controlling(token.session_id));
    assert!(manager.active_sessions().is_empty());

    let send_err = bus
        .send(inference_chunk_envelope(token.session_id, 2))
        .await
        .expect_err("downstream send after kill must fail with ConnectivityError");
    assert!(matches!(send_err, ConnectivityError::Transport { .. }));
}

#[tokio::test]
async fn session_idle_timeout_rejects_stale_sessions() {
    let manager = SessionManager::with_config(
        Duration::from_millis(50),       // idle timeout
        chrono::Duration::minutes(10),   // token TTL (not the trigger here)
    );
    let token = manager.issue(Uuid::new_v4(), vec![Capability::Inference]);
    manager.validate(&token).expect("fresh session is active");

    tokio::time::sleep(Duration::from_millis(120)).await;
    let err = manager.validate(&token).expect_err("idle session must fail");
    assert!(err.to_string().contains("idle"), "got: {err}");
}

#[tokio::test]
async fn session_lacking_takeover_capability_cannot_control() {
    let manager = SessionManager::new();
    let token = manager.issue(Uuid::new_v4(), vec![Capability::Inference]);
    let err = manager
        .set_controlling(token.session_id, true)
        .expect_err("inference-only session must not take over");
    assert!(err.to_string().contains("takeover"), "got: {err}");
    assert!(!manager.is_controlling(token.session_id));
}
