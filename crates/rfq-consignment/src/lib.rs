//! `rfq-consignment` — shared consignment-validation primitives for the Colorex
//! sell/buy gates, the broker pre-check, the SPV prover, and the thin-client
//! (wallet / ICP) verifiers. Phase 1+ of
//! `docs/consignment-validation-hardening-plan.md` and `docs/rfqip-1-spv-consignment-anchoring.md`.
//!
//! ## Two backends, one verdict
//! Confirming "is every witness mined?" needs a Bitcoin chain source. There are two ways
//! to get one, split by trust zone:
//!
//! - **Live resolver ([`mined`], `electrs` feature):** a party that runs a node (maker,
//!   taker-cli, broker pre-check) queries electrs directly. Fast, simple, node-only.
//! - **SPV proof-pack ([`proofpack`] + [`verify`], always available):** a thin client
//!   (browser wallet, ICP canister) that cannot run a node verifies a self-certifying
//!   bundle of per-witness Bitcoin merkle-inclusion proofs against its own header source.
//!   The verifier core is **pure** (bytes + double-SHA256 + serde, no electrum) so it
//!   compiles to wasm and to the canister.
//!
//! The [`prove`] module (also `electrs`-gated) is the untrusted *producer* of those packs.
//! Because a pack is self-verifying, a lying or faulty producer can only cause a
//! verification *failure*, never a false *accept* — which is what lets the prover be a
//! standalone, replaceable service.

pub mod difficulty;
pub mod headers;
pub mod merkle;
pub mod proofpack;
pub mod verify;

#[cfg(feature = "electrs")]
pub mod mined;
#[cfg(feature = "electrs")]
pub mod prove;

pub use headers::{Checkpoint, CheckpointHeaderSource, Network};
pub use proofpack::{SpvProofPack, WitnessInclusion};
pub use verify::{verify_pack, HeaderInfo, HeaderSource, RejectReason, SpvVerdict};

#[cfg(feature = "electrs")]
pub use mined::{MinedChecker, MinedVerdict};
#[cfg(feature = "electrs")]
pub use prove::{build_proof_pack, ElectrsHeaderSource};
