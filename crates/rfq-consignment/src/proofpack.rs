//! The [`SpvProofPack`] container (RFQIP-1 §1) — a self-verifying sidecar that travels
//! next to (not inside) an RGB consignment, carrying one Bitcoin merkle-inclusion proof
//! per witness tx so a thin client can confirm the whole ancestry is mined.
//!
//! Encoding here is JSON-friendly (hex strings + plain ints) so it serialises with
//! `serde_json` and crosses the wasm/canister boundary without electrum or `bp-core`
//! types. A strict-encoded binary form (reusing `bp-core` header/hash types) is a planned
//! follow-up — see RFQIP-1 "Open questions"; the field layout is identical so it is a
//! drop-in re-encode, not a redesign.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Self-verifying bundle of per-witness inclusion proofs (+ optional headers).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpvProofPack {
    /// Format version (currently 1).
    pub version: u8,
    /// Network the proofs are anchored on: `regtest` | `signet` | `testnet` | `mainnet`.
    pub network: String,
    /// One entry per unique witness txid (display-order hex key) the verifier will walk.
    /// MUST cover the full ancestry the verifier requires (to genesis or a stash bookmark).
    pub anchors: BTreeMap<String, WitnessInclusion>,
    /// OPTIONAL convenience headers, keyed by display-order block-hash hex; value = the raw
    /// 80-byte header as hex. A verifier with its own header source MUST ignore these
    /// (RFQIP-1 §3); they exist only to bootstrap a header-less client, and even then are
    /// accepted only after PoW + checkpoint checks (never blindly).
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
}

/// A single witness tx's Bitcoin merkle-inclusion proof.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WitnessInclusion {
    /// Display-order hex of the block hash that includes the witness tx.
    pub block_hash: String,
    /// Height of that block.
    pub block_height: u32,
    /// Position of the tx within the block — fixes the merkle path's left/right direction.
    pub tx_index: u32,
    /// Branch from the witness txid up to the block merkle root; each node display-order hex.
    pub merkle_proof: Vec<String>,
}

impl SpvProofPack {
    /// Serialise to compact JSON bytes.
    pub fn to_json(&self) -> Result<Vec<u8>, String> {
        serde_json::to_vec(self).map_err(|e| format!("proof-pack encode: {e}"))
    }

    /// Parse from JSON bytes.
    pub fn from_json(bytes: &[u8]) -> Result<Self, String> {
        serde_json::from_slice(bytes).map_err(|e| format!("proof-pack decode: {e}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_round_trips() {
        let mut anchors = BTreeMap::new();
        anchors.insert(
            "ab".repeat(32),
            WitnessInclusion {
                block_hash: "cd".repeat(32),
                block_height: 42,
                tx_index: 3,
                merkle_proof: vec!["ef".repeat(32), "12".repeat(32)],
            },
        );
        let pack = SpvProofPack {
            version: 1,
            network: "regtest".to_owned(),
            anchors,
            headers: BTreeMap::new(),
        };
        let bytes = pack.to_json().unwrap();
        let back = SpvProofPack::from_json(&bytes).unwrap();
        assert_eq!(pack, back);
    }

    #[test]
    fn headers_field_is_optional_on_decode() {
        // A pack produced without the `headers` key (header-ful verifier) still decodes.
        let json = br#"{"version":1,"network":"signet","anchors":{}}"#;
        let pack = SpvProofPack::from_json(json).unwrap();
        assert!(pack.headers.is_empty());
        assert_eq!(pack.version, 1);
    }
}
