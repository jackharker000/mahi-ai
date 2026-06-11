//! Capability publisher: packages registered providers into a
//! [`DeviceCapabilitySnapshot`] for the capability registry (domain ③,
//! "CapabilityRegistry publisher"). The Data/Sync layer persists and syncs
//! the snapshot; this crate only builds it.

use mahi_contracts::compute::InferenceProvider;
use mahi_contracts::types::{ComputeMode, DeviceCapabilitySnapshot, ModeCapability};
use std::collections::HashMap;
use uuid::Uuid;

/// All compute modes, so the snapshot always reports every mode explicitly
/// (unregistered modes appear with `available: false`).
const ALL_MODES: [ComputeMode; 4] = [
    ComputeMode::OnDevice,
    ComputeMode::MacLan,
    ComputeMode::MacRemote,
    ComputeMode::Hosted,
];

/// Build a [`DeviceCapabilitySnapshot`] from the registered providers.
///
/// Every [`ComputeMode`] is present in the result: modes with at least one
/// registered provider are `available: true` and list each provider's
/// [`ModelDescriptor`]; the rest are `available: false` with no models.
pub fn build_capability_snapshot<'a>(
    device_id: Uuid,
    providers: impl IntoIterator<Item = (ComputeMode, &'a dyn InferenceProvider)>,
) -> DeviceCapabilitySnapshot {
    let mut modes: HashMap<ComputeMode, ModeCapability> = ALL_MODES
        .iter()
        .map(|mode| {
            (
                *mode,
                ModeCapability { available: false, models: Vec::new(), tool_ids: Vec::new() },
            )
        })
        .collect();

    for (mode, provider) in providers {
        let entry = modes.entry(mode).or_insert_with(|| ModeCapability {
            available: false,
            models: Vec::new(),
            tool_ids: Vec::new(),
        });
        entry.available = true;
        entry.models.push(provider.descriptor());
        // TODO(contracts): `ModeCapability.tool_ids` cannot be derived from
        // `InferenceProvider` — tool availability is owned by mahi-tooling's
        // registry. Left empty here; the engine merges tool ids when it
        // publishes the snapshot.
    }

    DeviceCapabilitySnapshot { device_id, timestamp: chrono::Utc::now(), modes }
}
