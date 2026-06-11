//! The central mode-selection engine (domain ③ §4).
//!
//! [`InferenceRouter`] implements [`InferenceProvider`] so it drops straight
//! into `EngineConfig.inference` (see `docs/backend/phase-0/03-engine-facade.md`).
//! Selection policy (`docs/backend/DECISIONS.md` D1, local-first):
//!
//! 1. `MacLan` — reachable on LAN, capable
//! 2. `MacRemote` — reachable via tunnel, capable
//! 3. `Hosted` — network available, capable
//! 4. `OnDevice` — always available; the offline/last-resort default
//!
//! Providers whose `can_handle` is not capable for the request's
//! `required_caps` are skipped. A pinned mode is **never** silently overridden:
//! if the pinned provider is unregistered or incapable, the router returns
//! [`InferenceError::PinViolation`].

use crate::capability::build_capability_snapshot;
use async_trait::async_trait;
use futures::StreamExt;
use mahi_contracts::compute::{
    CanHandleResult, InferenceProvider, InferenceRequest, InferenceStream,
};
use mahi_contracts::error::{ContractError, InferenceError};
use mahi_contracts::types::{
    CapabilitySet, ComputeMode, DeviceCapabilitySnapshot, LimitationLabel, ModelDescriptor,
    ModelSource, PerfProfile,
};
use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

/// Local-first preference order (D1). `OnDevice` is the last resort but is
/// always available, so an empty network never strands the user.
pub const PREFERENCE_ORDER: [ComputeMode; 4] = [
    ComputeMode::MacLan,
    ComputeMode::MacRemote,
    ComputeMode::Hosted,
    ComputeMode::OnDevice,
];

/// Routes each request to the best available sub-provider.
pub struct InferenceRouter {
    providers: HashMap<ComputeMode, Arc<dyn InferenceProvider>>,
    pin: Option<ComputeMode>,
    /// Mode selected by the most recent successful `generate()`; lets
    /// `descriptor()` report the *active* provider after generation starts.
    last_active: RwLock<Option<ComputeMode>>,
}

impl InferenceRouter {
    /// Start building a router. API per the engine-facade doc.
    pub fn builder() -> InferenceRouterBuilder {
        InferenceRouterBuilder::default()
    }

    /// The currently pinned mode, if any.
    pub fn pinned_mode(&self) -> Option<ComputeMode> {
        self.pin
    }

    /// Modes with a registered provider, in local-first preference order.
    pub fn registered_modes(&self) -> Vec<ComputeMode> {
        PREFERENCE_ORDER
            .iter()
            .copied()
            .filter(|m| self.providers.contains_key(m))
            .collect()
    }

    /// Publish a [`DeviceCapabilitySnapshot`] covering the registered providers.
    pub fn capability_snapshot(&self, device_id: Uuid) -> DeviceCapabilitySnapshot {
        build_capability_snapshot(
            device_id,
            self.providers.iter().map(|(mode, p)| (*mode, p.as_ref())),
        )
    }

    /// Select the provider for `req`: pin (hard requirement) or the first
    /// capable provider in local-first preference order.
    async fn select(
        &self,
        req: &InferenceRequest,
    ) -> Result<(ComputeMode, Arc<dyn InferenceProvider>), ContractError> {
        if let Some(pinned) = self.pin {
            // A pin is never silently overridden (D1): unavailable or
            // incapable pinned provider => explicit PinViolation.
            let Some(provider) = self.providers.get(&pinned) else {
                return Err(InferenceError::PinViolation { requested: pinned }.into());
            };
            if !provider.can_handle(req).await.capable {
                return Err(InferenceError::PinViolation { requested: pinned }.into());
            }
            return Ok((pinned, Arc::clone(provider)));
        }

        for mode in PREFERENCE_ORDER {
            if let Some(provider) = self.providers.get(&mode) {
                if provider.can_handle(req).await.capable {
                    return Ok((mode, Arc::clone(provider)));
                }
            }
        }
        Err(InferenceError::NoCapableProvider.into())
    }

    /// The provider whose descriptor `descriptor()` should report: the pinned
    /// provider if pinned, else the most recently selected, else the most
    /// preferred registered provider.
    fn active_provider(&self) -> Option<&Arc<dyn InferenceProvider>> {
        if let Some(pinned) = self.pin {
            // Pin semantics: never silently report a different provider.
            return self.providers.get(&pinned);
        }
        if let Some(mode) = *self.last_active.read().expect("last_active lock poisoned") {
            if let Some(p) = self.providers.get(&mode) {
                return Some(p);
            }
        }
        PREFERENCE_ORDER.iter().find_map(|m| self.providers.get(m))
    }

    /// Descriptor returned when no provider is registered/available.
    fn unavailable_descriptor() -> ModelDescriptor {
        ModelDescriptor {
            id: "mahi-router/no-active-provider".to_string(),
            display_name: "Mahi Router (no active provider)".to_string(),
            context_window: 0,
            capabilities: CapabilitySet::none(),
            limitations: vec![LimitationLabel::Custom(
                "no provider registered or pinned mode unavailable".to_string(),
            )],
            size_bytes: None,
            quantization: None,
            // TODO(contracts): `ModelSource` has no neutral/unknown variant for
            // a router placeholder; `OnDevice` is the least-wrong stand-in.
            source: ModelSource::OnDevice,
            perf_profile: PerfProfile::default(),
        }
    }
}

#[async_trait]
impl InferenceProvider for InferenceRouter {
    fn descriptor(&self) -> ModelDescriptor {
        self.active_provider()
            .map(|p| p.descriptor())
            .unwrap_or_else(Self::unavailable_descriptor)
    }

    async fn can_handle(&self, req: &InferenceRequest) -> CanHandleResult {
        if let Some(pinned) = self.pin {
            return match self.providers.get(&pinned) {
                Some(p) => p.can_handle(req).await,
                None => CanHandleResult {
                    capable: false,
                    missing_caps: req.required_caps.clone(),
                    escalation_hint: None,
                },
            };
        }

        let mut first_failure: Option<CanHandleResult> = None;
        for mode in PREFERENCE_ORDER {
            if let Some(p) = self.providers.get(&mode) {
                let result = p.can_handle(req).await;
                if result.capable {
                    return result;
                }
                if first_failure.is_none() {
                    first_failure = Some(result);
                }
            }
        }
        first_failure.unwrap_or(CanHandleResult {
            capable: false,
            missing_caps: req.required_caps.clone(),
            escalation_hint: None,
        })
    }

    async fn generate(
        &self,
        req: InferenceRequest,
        cancel: CancellationToken,
    ) -> Result<InferenceStream, ContractError> {
        let (mode, provider) = self.select(&req).await?;
        *self.last_active.write().expect("last_active lock poisoned") = Some(mode);
        tracing::debug!(?mode, request_id = %req.request_id, "router selected provider");

        let inner = provider.generate(req, cancel).await?;
        // Every chunk echoes the *router's* chosen mode so the surface always
        // knows the active mode, regardless of what the sub-provider stamped.
        let mapped = inner.map(move |item| {
            item.map(|mut chunk| {
                chunk.active_mode = mode;
                chunk
            })
        });
        Ok(Box::pin(mapped))
    }
}

/// Builder for [`InferenceRouter`] (API fixed by the engine-facade doc).
#[derive(Default)]
pub struct InferenceRouterBuilder {
    providers: HashMap<ComputeMode, Arc<dyn InferenceProvider>>,
    pin: Option<ComputeMode>,
}

impl InferenceRouterBuilder {
    /// Register `provider` for `mode`. Registering the same mode twice
    /// replaces the earlier provider (last write wins).
    pub fn add_provider(
        mut self,
        mode: ComputeMode,
        provider: Arc<dyn InferenceProvider>,
    ) -> Self {
        self.providers.insert(mode, provider);
        self
    }

    /// Pin all requests to `mode` (or clear the pin with `None`). A pinned
    /// mode that is unavailable yields `InferenceError::PinViolation`.
    pub fn pin(mut self, mode: Option<ComputeMode>) -> Self {
        self.pin = mode;
        self
    }

    /// Finalize the router.
    pub fn build(self) -> InferenceRouter {
        InferenceRouter {
            providers: self.providers,
            pin: self.pin,
            last_active: RwLock::new(None),
        }
    }
}
