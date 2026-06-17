//! `rfq-consignment-wasm` — the SPV consignment verifier compiled to WebAssembly for the
//! colorex-wallet browser extension (and any JS host). A thin `wasm-bindgen` shell over the
//! pure [`rfq_consignment`] verifier core (`default-features = false`, so no electrum).
//!
//! ## Trust boundary
//! Everything trust-critical — merkle-inclusion folding, checkpoint/linkage/PoW header
//! validation, K-confirmation depth — runs **here, in Rust/wasm**. The JS host only
//! *fetches* data (the wallet self-fetches merkle proofs + headers from esplora — no prover,
//! no broker is trusted) and passes it in as strings. A lying data source can only make
//! verification *fail*, never falsely pass.
//!
//! The host supplies the witness-txid set (it already decodes the consignment with its
//! existing RGB wasm), so this module needs neither the RGB stack nor a chain connection.

use std::collections::HashSet;

use wasm_bindgen::prelude::*;

use rfq_consignment::merkle::hex_to_bytes;
use rfq_consignment::{verify_pack, Checkpoint, CheckpointHeaderSource, Network, SpvProofPack};

fn parse_vec(json: &str, what: &str) -> Result<Vec<String>, JsError> {
    serde_json::from_str(json).map_err(|e| JsError::new(&format!("{what}: {e}")))
}

/// Verify that every witness txid (except `exempt`) has a valid, ≥`min_confs`-confirmed
/// merkle-inclusion proof in `pack`, against a checkpoint-anchored run of block headers the
/// host fetched. Returns the `SpvVerdict` as a JSON string
/// (`{ all_mined, rejected: [[txid, reason], …], checked }`).
///
/// Parameters (all JSON strings unless noted):
/// - `witness_txids_json` — `["<txid hex>", …]`, the consignment's witness set.
/// - `exempt_txids_json` — `["<txid hex>", …]`; buy side passes the not-yet-broadcast swap
///   tx, sell side passes `[]`.
/// - `pack_json` — an `SpvProofPack` (the wallet assembles it from esplora merkle proofs).
/// - `checkpoint_json` — `{ "height": u32, "block_hash": "<hex>" }`, baked into the wallet.
/// - `headers_hex_json` — `["<80-byte header hex>", …]`, contiguous from the checkpoint,
///   fetched from esplora; validated here (linkage + checkpoint + network PoW).
/// - `network` — `mainnet | testnet | signet | regtest`.
/// - `min_confs` — required confirmation depth K (use [`recommended_confs`]).
#[wasm_bindgen]
pub fn verify_consignment(
    witness_txids_json: &str,
    exempt_txids_json: &str,
    pack_json: &str,
    checkpoint_json: &str,
    headers_hex_json: &str,
    network: &str,
    min_confs: u32,
) -> Result<String, JsError> {
    let witness_txids = parse_vec(witness_txids_json, "witness_txids")?;
    let exempt: HashSet<String> = parse_vec(exempt_txids_json, "exempt")?.into_iter().collect();

    let pack = SpvProofPack::from_json(pack_json.as_bytes()).map_err(|e| JsError::new(&e))?;

    let checkpoint: Checkpoint = serde_json::from_str(checkpoint_json)
        .map_err(|e| JsError::new(&format!("checkpoint: {e}")))?;

    let headers_hex = parse_vec(headers_hex_json, "headers")?;
    let headers: Vec<Vec<u8>> = headers_hex
        .iter()
        .map(|h| hex_to_bytes(h).ok_or_else(|| JsError::new("header is not valid hex")))
        .collect::<Result<_, _>>()?;

    let net = Network::from_label(network)
        .ok_or_else(|| JsError::new(&format!("unknown network `{network}`")))?;

    let source =
        CheckpointHeaderSource::new(net, &checkpoint, &headers).map_err(|e| JsError::new(&e))?;

    let verdict = verify_pack(&witness_txids, &exempt, &pack, &source, min_confs);
    serde_json::to_string(&verdict).map_err(|e| JsError::new(&e.to_string()))
}

/// Recommended confirmation depth K for a network label (mainnet 6 / testnet 3 /
/// signet+regtest 1), so the wallet doesn't hardcode the policy.
#[wasm_bindgen]
pub fn recommended_confs(network: &str) -> u32 {
    Network::from_label(network)
        .map(|n| n.recommended_confs())
        .unwrap_or(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Real mainnet genesis: one tx, so merkle root == coinbase txid and the proof is empty.
    // Exercises the full host path — JSON in, CheckpointHeaderSource built + validated, JSON out.
    const GENESIS_HDR: &str = "0100000000000000000000000000000000000000000000000000000000000000000000003ba3edfd7a7b12b27ac72c3e67768f617fc81bc3888a51323a9fb8aa4b1e5e4a29ab5f49ffff001d1dac2b7c";
    const GENESIS_HASH: &str = "000000000019d6689c085ae165831e934ff763ae46a2a6c172b3f1b60a8ce26f";
    const COINBASE: &str = "4a5e1e4baab89f3a32518a88c31bc87f618f76673e2cc77ab2127b7afdeda33b";

    fn pack_json(block_hash: &str) -> String {
        format!(
            r#"{{"version":1,"network":"mainnet","anchors":{{"{COINBASE}":{{"block_hash":"{block_hash}","block_height":0,"tx_index":0,"merkle_proof":[]}}}},"headers":{{}}}}"#
        )
    }

    fn run(min_confs: u32, pack_block_hash: &str) -> serde_json::Value {
        let out = verify_consignment(
            &format!(r#"["{COINBASE}"]"#),
            "[]",
            &pack_json(pack_block_hash),
            &format!(r#"{{"height":0,"block_hash":"{GENESIS_HASH}"}}"#),
            &format!(r#"["{GENESIS_HDR}"]"#),
            "mainnet",
            min_confs,
        )
        .expect("verify_consignment");
        serde_json::from_str(&out).expect("verdict json")
    }

    #[test]
    fn verifies_genuine_mined_witness() {
        let v = run(1, GENESIS_HASH);
        assert_eq!(v["all_mined"], true, "verdict: {v}");
        assert_eq!(v["checked"], 1);
    }

    #[test]
    fn rejects_when_proof_block_not_in_headers() {
        // Pack points the witness at a block the header set doesn't vouch for → UnknownHeader.
        let v = run(1, &"ab".repeat(32));
        assert_eq!(v["all_mined"], false, "verdict: {v}");
    }

    #[test]
    fn rejects_shallow_confirmations() {
        // Genesis is 1 deep here; demanding 6 → not final.
        let v = run(6, GENESIS_HASH);
        assert_eq!(v["all_mined"], false, "verdict: {v}");
    }

    #[test]
    fn recommended_confs_matches_policy() {
        assert_eq!(recommended_confs("mainnet"), 6);
        assert_eq!(recommended_confs("signet"), 1);
    }
}
