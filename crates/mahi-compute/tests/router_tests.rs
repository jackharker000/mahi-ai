//! Default-feature tests (no network): router selection, pin semantics,
//! on-device streaming, and the capability publisher.

use async_trait::async_trait;
use futures::StreamExt;
use mahi_compute::{build_capability_snapshot, InferenceRouter, OnDeviceProvider};
use mahi_contracts::compute::{
    CanHandleResult, FinishReason, InferenceChunk, InferenceProvider, InferenceRequest,
    InferenceStream,
};
use mahi_contracts::data::{Message, MessageRole};
use mahi_contracts::error::{ContractError, InferenceError};
use mahi_contracts::types::{
    CapabilitySet, ComputeMode, ModelDescriptor, ModelSource, PerfProfile,
};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

/// Configurable test provider. Deliberately stamps every chunk with
/// `ComputeMode::OnDevice` so tests can prove the router rewrites
/// `active_mode` to the *selected* mode.
struct TestProvider {
    id: &'static str,
    caps: CapabilitySet,
}

impl TestProvider {
    fn basic(id: &'static str) -> Self {
        Self {
            id,
            caps: CapabilitySet::none(),
        }
    }

    fn with_tools(id: &'static str) -> Self {
        Self {
            id,
            caps: CapabilitySet {
                vision: false,
                tool_calling: true,
                min_context_window: 0,
                code_gen: false,
                thinking: false,
            },
        }
    }
}

#[async_trait]
impl InferenceProvider for TestProvider {
    fn descriptor(&self) -> ModelDescriptor {
        ModelDescriptor {
            id: self.id.to_string(),
            display_name: self.id.to_string(),
            context_window: 8192,
            capabilities: self.caps.clone(),
            limitations: vec![],
            size_bytes: None,
            quantization: None,
            source: ModelSource::OnDevice,
            perf_profile: PerfProfile::default(),
        }
    }

    async fn can_handle(&self, req: &InferenceRequest) -> CanHandleResult {
        if req.required_caps.satisfied_by(&self.caps) {
            CanHandleResult::capable()
        } else {
            CanHandleResult {
                capable: false,
                missing_caps: req.required_caps.missing_from(&self.caps),
                escalation_hint: None,
            }
        }
    }

    async fn generate(
        &self,
        _req: InferenceRequest,
        _cancel: CancellationToken,
    ) -> Result<InferenceStream, ContractError> {
        // Wrong-on-purpose active_mode: the router must overwrite it.
        let chunks = vec![
            Ok(InferenceChunk::text(self.id, ComputeMode::OnDevice)),
            Ok(InferenceChunk::finish(
                FinishReason::Stop,
                ComputeMode::OnDevice,
            )),
        ];
        Ok(Box::pin(futures::stream::iter(chunks)))
    }
}

fn text_request(text: &str) -> InferenceRequest {
    InferenceRequest::from_messages(vec![Message::text(
        Uuid::new_v4(),
        MessageRole::User,
        text,
        ComputeMode::OnDevice,
        0,
    )])
}

fn tool_request() -> InferenceRequest {
    let mut req = text_request("use a tool");
    req.required_caps = CapabilitySet {
        vision: false,
        tool_calling: true,
        min_context_window: 0,
        code_gen: false,
        thinking: false,
    };
    req
}

async fn collect(stream: InferenceStream) -> Vec<InferenceChunk> {
    stream
        .map(|c| c.expect("chunk should be Ok"))
        .collect()
        .await
}

fn expect_pin_violation(err: ContractError, expected: ComputeMode) {
    match err {
        ContractError::Inference(InferenceError::PinViolation { requested }) => {
            assert_eq!(requested, expected)
        }
        other => panic!("expected PinViolation, got: {other:?}"),
    }
}

#[tokio::test]
async fn router_prefers_mac_lan_over_hosted_and_on_device() {
    let router = InferenceRouter::builder()
        .add_provider(
            ComputeMode::OnDevice,
            Arc::new(TestProvider::basic("on-device")),
        )
        .add_provider(ComputeMode::Hosted, Arc::new(TestProvider::basic("hosted")))
        .add_provider(
            ComputeMode::MacLan,
            Arc::new(TestProvider::basic("mac-lan")),
        )
        .build();

    let chunks = collect(
        router
            .generate(text_request("hi"), CancellationToken::new())
            .await
            .unwrap(),
    )
    .await;
    assert!(!chunks.is_empty());
    assert!(
        chunks.iter().all(|c| c.active_mode == ComputeMode::MacLan),
        "every chunk must echo the chosen mode (MacLan)"
    );
    // descriptor() reflects the active provider.
    assert_eq!(router.descriptor().id, "mac-lan");
}

#[tokio::test]
async fn router_prefers_mac_remote_over_hosted_when_no_lan() {
    let router = InferenceRouter::builder()
        .add_provider(ComputeMode::Hosted, Arc::new(TestProvider::basic("hosted")))
        .add_provider(
            ComputeMode::MacRemote,
            Arc::new(TestProvider::basic("mac-remote")),
        )
        .add_provider(
            ComputeMode::OnDevice,
            Arc::new(TestProvider::basic("on-device")),
        )
        .build();

    let chunks = collect(
        router
            .generate(text_request("hi"), CancellationToken::new())
            .await
            .unwrap(),
    )
    .await;
    assert!(chunks
        .iter()
        .all(|c| c.active_mode == ComputeMode::MacRemote));
}

#[tokio::test]
async fn router_skips_incapable_providers() {
    // MacLan is most preferred but cannot do tool calling; Hosted can.
    let router = InferenceRouter::builder()
        .add_provider(
            ComputeMode::MacLan,
            Arc::new(TestProvider::basic("mac-lan")),
        )
        .add_provider(
            ComputeMode::Hosted,
            Arc::new(TestProvider::with_tools("hosted")),
        )
        .add_provider(
            ComputeMode::OnDevice,
            Arc::new(TestProvider::basic("on-device")),
        )
        .build();

    let chunks = collect(
        router
            .generate(tool_request(), CancellationToken::new())
            .await
            .unwrap(),
    )
    .await;
    assert!(
        chunks.iter().all(|c| c.active_mode == ComputeMode::Hosted),
        "router must skip incapable MacLan and pick Hosted"
    );
}

#[tokio::test]
async fn router_falls_back_to_on_device_as_last_resort() {
    let router = InferenceRouter::builder()
        .add_provider(
            ComputeMode::OnDevice,
            Arc::new(TestProvider::basic("on-device")),
        )
        .build();

    let chunks = collect(
        router
            .generate(text_request("hi"), CancellationToken::new())
            .await
            .unwrap(),
    )
    .await;
    assert!(chunks
        .iter()
        .all(|c| c.active_mode == ComputeMode::OnDevice));
}

#[tokio::test]
async fn pin_is_honored_over_preference_order() {
    // MacLan would normally win; the pin forces Hosted.
    let router = InferenceRouter::builder()
        .add_provider(
            ComputeMode::MacLan,
            Arc::new(TestProvider::basic("mac-lan")),
        )
        .add_provider(ComputeMode::Hosted, Arc::new(TestProvider::basic("hosted")))
        .pin(Some(ComputeMode::Hosted))
        .build();

    let chunks = collect(
        router
            .generate(text_request("hi"), CancellationToken::new())
            .await
            .unwrap(),
    )
    .await;
    assert!(chunks.iter().all(|c| c.active_mode == ComputeMode::Hosted));
    assert_eq!(
        router.descriptor().id,
        "hosted",
        "descriptor must reflect the pinned provider"
    );
}

#[tokio::test]
async fn pin_to_unregistered_mode_is_a_pin_violation() {
    let router = InferenceRouter::builder()
        .add_provider(
            ComputeMode::OnDevice,
            Arc::new(TestProvider::basic("on-device")),
        )
        .pin(Some(ComputeMode::MacRemote))
        .build();

    let err = router
        .generate(text_request("hi"), CancellationToken::new())
        .await
        .err()
        .expect("pinned-but-unregistered mode must error, never fall back");
    expect_pin_violation(err, ComputeMode::MacRemote);
}

#[tokio::test]
async fn pin_to_incapable_provider_is_a_pin_violation_not_a_fallback() {
    // OnDevice cannot do tools; Hosted could — but the pin must NOT silently
    // fall back to it.
    let router = InferenceRouter::builder()
        .add_provider(
            ComputeMode::OnDevice,
            Arc::new(TestProvider::basic("on-device")),
        )
        .add_provider(
            ComputeMode::Hosted,
            Arc::new(TestProvider::with_tools("hosted")),
        )
        .pin(Some(ComputeMode::OnDevice))
        .build();

    let err = router
        .generate(tool_request(), CancellationToken::new())
        .await
        .err()
        .expect("pinned-but-incapable mode must error, never fall back");
    expect_pin_violation(err, ComputeMode::OnDevice);
}

#[tokio::test]
async fn no_capable_provider_error_when_nothing_fits() {
    let router = InferenceRouter::builder()
        .add_provider(
            ComputeMode::OnDevice,
            Arc::new(TestProvider::basic("on-device")),
        )
        .build();

    let err = router
        .generate(tool_request(), CancellationToken::new())
        .await
        .err()
        .expect("no registered provider supports tools");
    match err {
        ContractError::Inference(InferenceError::NoCapableProvider) => {}
        other => panic!("expected NoCapableProvider, got: {other:?}"),
    }
}

#[tokio::test]
async fn router_can_handle_probes_in_preference_order() {
    let router = InferenceRouter::builder()
        .add_provider(
            ComputeMode::OnDevice,
            Arc::new(TestProvider::basic("on-device")),
        )
        .add_provider(
            ComputeMode::Hosted,
            Arc::new(TestProvider::with_tools("hosted")),
        )
        .build();

    assert!(router.can_handle(&text_request("hi")).await.capable);
    assert!(
        router.can_handle(&tool_request()).await.capable,
        "hosted supports tools"
    );

    let only_on_device = InferenceRouter::builder()
        .add_provider(
            ComputeMode::OnDevice,
            Arc::new(TestProvider::basic("on-device")),
        )
        .build();
    let result = only_on_device.can_handle(&tool_request()).await;
    assert!(!result.capable);
    assert!(result.missing_caps.tool_calling);
}

#[tokio::test]
async fn on_device_provider_streams_to_completion_through_router() {
    let router = InferenceRouter::builder()
        .add_provider(
            ComputeMode::OnDevice,
            Arc::new(OnDeviceProvider::canned("kia ora koutou")),
        )
        .build();

    let chunks = collect(
        router
            .generate(text_request("hi"), CancellationToken::new())
            .await
            .unwrap(),
    )
    .await;

    assert!(chunks
        .iter()
        .all(|c| c.active_mode == ComputeMode::OnDevice));
    let text: String = chunks.iter().filter_map(|c| c.delta.as_deref()).collect();
    assert_eq!(text, "kia ora koutou");
    assert_eq!(
        chunks.last().unwrap().finish_reason,
        Some(FinishReason::Stop),
        "stream must terminate with a Stop finish chunk"
    );
}

#[tokio::test]
async fn on_device_echo_replies_with_last_user_message() {
    let provider = OnDeviceProvider::echo();
    let chunks = collect(
        provider
            .generate(text_request("hello mahi"), CancellationToken::new())
            .await
            .unwrap(),
    )
    .await;
    let text: String = chunks.iter().filter_map(|c| c.delta.as_deref()).collect();
    assert_eq!(text, "You said: hello mahi");
}

#[tokio::test]
async fn on_device_provider_honors_cancellation() {
    let provider = OnDeviceProvider::new();
    let cancel = CancellationToken::new();
    cancel.cancel(); // pre-cancelled: first poll should yield Cancelled finish
    let chunks = collect(provider.generate(text_request("hi"), cancel).await.unwrap()).await;
    assert_eq!(chunks.len(), 1);
    assert_eq!(chunks[0].finish_reason, Some(FinishReason::Cancelled));
}

#[tokio::test]
async fn capability_snapshot_reports_all_modes() {
    let router = InferenceRouter::builder()
        .add_provider(ComputeMode::OnDevice, Arc::new(OnDeviceProvider::new()))
        .add_provider(ComputeMode::Hosted, Arc::new(TestProvider::basic("hosted")))
        .build();

    let device_id = Uuid::new_v4();
    let snapshot = router.capability_snapshot(device_id);
    assert_eq!(snapshot.device_id, device_id);
    assert_eq!(snapshot.modes.len(), 4, "all four modes must be reported");

    let on_device = &snapshot.modes[&ComputeMode::OnDevice];
    assert!(on_device.available);
    assert_eq!(on_device.models.len(), 1);
    assert_eq!(on_device.models[0].id, "mahi-on-device-v0");

    assert!(snapshot.modes[&ComputeMode::Hosted].available);
    assert!(!snapshot.modes[&ComputeMode::MacLan].available);
    assert!(!snapshot.modes[&ComputeMode::MacRemote].available);
}

#[tokio::test]
async fn free_function_snapshot_builder_works_without_router() {
    let on_device = OnDeviceProvider::new();
    let providers: Vec<(ComputeMode, &dyn InferenceProvider)> =
        vec![(ComputeMode::OnDevice, &on_device)];
    let snapshot = build_capability_snapshot(Uuid::new_v4(), providers);
    assert!(snapshot.modes[&ComputeMode::OnDevice].available);
    assert!(!snapshot.modes[&ComputeMode::Hosted].available);
}

#[tokio::test]
async fn empty_router_descriptor_is_placeholder_and_generate_errors() {
    let router = InferenceRouter::builder().build();
    assert_eq!(router.descriptor().id, "mahi-router/no-active-provider");
    let err = router
        .generate(text_request("hi"), CancellationToken::new())
        .await
        .err()
        .unwrap();
    match err {
        ContractError::Inference(InferenceError::NoCapableProvider) => {}
        other => panic!("expected NoCapableProvider, got: {other:?}"),
    }
}
