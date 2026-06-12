//! # mahi-contracts
//!
//! Canonical cross-domain seam types and traits for Mahi AI. This crate contains
//! data models, async traits, and the error taxonomy that every other crate
//! depends on. It performs no I/O of its own.
//!
//! See `docs/backend/phase-0/01-workspace-and-contracts.md` for the design.

pub mod agent;
pub mod compute;
pub mod connectivity;
pub mod data;
pub mod error;
pub mod tooling;
pub mod types;

#[cfg(feature = "testkit")]
pub mod testkit;

pub use error::ContractError;
pub use types::ComputeMode;
