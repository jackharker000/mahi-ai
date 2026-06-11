//! The approval gate: pending approval registry shared between a parked turn
//! task and the surface calling `MahiEngine::resolve_approval`.

use mahi_contracts::error::ContractError;
use std::collections::HashMap;
use std::sync::Mutex;
use tokio::sync::oneshot;
use uuid::Uuid;

/// Registry of pending approvals. A turn that hits a `requires_approval` tool
/// registers a oneshot here, emits `AgentEvent::ApprovalRequired`, and parks
/// on the receiver until the surface resolves (or the turn is cancelled).
#[derive(Default)]
pub(crate) struct ApprovalRegistry {
    pending: Mutex<HashMap<Uuid, oneshot::Sender<bool>>>,
}

impl ApprovalRegistry {
    /// Register a new pending approval, returning its id and the receiver the
    /// turn task should await.
    pub(crate) fn register(&self) -> (Uuid, oneshot::Receiver<bool>) {
        let id = Uuid::new_v4();
        let (tx, rx) = oneshot::channel();
        self.pending
            .lock()
            .expect("approval registry lock poisoned")
            .insert(id, tx);
        (id, rx)
    }

    /// Deliver a decision to the parked turn. Errors if the id is unknown
    /// (already resolved, expired, or never issued).
    pub(crate) fn resolve(&self, id: Uuid, approved: bool) -> Result<(), ContractError> {
        let tx = self
            .pending
            .lock()
            .expect("approval registry lock poisoned")
            .remove(&id)
            .ok_or_else(|| ContractError::other(format!("no pending approval with id {id}")))?;
        tx.send(approved)
            .map_err(|_| ContractError::other("approval waiter gone (turn ended or was cancelled)"))
    }

    /// Drop a pending approval without resolving it (e.g. the turn was cancelled).
    pub(crate) fn discard(&self, id: Uuid) {
        self.pending
            .lock()
            .expect("approval registry lock poisoned")
            .remove(&id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn resolve_fires_registered_oneshot() {
        let reg = ApprovalRegistry::default();
        let (id, rx) = reg.register();
        reg.resolve(id, true).unwrap();
        assert!(rx.await.unwrap());
    }

    #[tokio::test]
    async fn resolve_unknown_id_errors() {
        let reg = ApprovalRegistry::default();
        assert!(reg.resolve(Uuid::new_v4(), true).is_err());
    }

    #[tokio::test]
    async fn discard_removes_pending_entry() {
        let reg = ApprovalRegistry::default();
        let (id, _rx) = reg.register();
        reg.discard(id);
        assert!(reg.resolve(id, true).is_err());
    }
}
