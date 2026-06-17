//! The SPV **prover** (RFQIP-1 §2) and an electrs-backed [`HeaderSource`] — both
//! `electrs`-gated. The prover is the *untrusted producer*: it turns a set of witness
//! txids into a self-verifying [`SpvProofPack`]. Because the pack is self-verifying, a
//! buggy or malicious prover can only cause a verification *failure*, never a false accept
//! — which is what lets it run as a standalone, replaceable service.
//!
//! Implemented over electrum `raw_call` (no `bp::Txid` round-tripping) so this module needs
//! only the `electrum` crate plus our pure [`crate::merkle`] / [`crate::proofpack`] modules.

use std::collections::BTreeMap;

use electrum::{Client, ElectrumApi, Param};
use serde_json::Value;

use crate::merkle::{bytes_to_hex, dsha256, header_merkle_root};
use crate::proofpack::{SpvProofPack, WitnessInclusion};
use crate::verify::{HeaderInfo, HeaderSource};

/// Result of building a proof-pack: the pack plus any txids that could NOT be proven
/// (not mined / not found) and are therefore absent from `pack.anchors`.
#[derive(Debug, Clone)]
pub struct ProofPackResult {
    /// The assembled proof-pack (covers only the txids that could be proven).
    pub pack: SpvProofPack,
    /// Witness txids with no inclusion proof — a verifier will reject these as MissingAnchor.
    pub unproven: Vec<String>,
}

/// Display-order (big-endian) block-hash hex for an 80-byte header.
fn block_hash_display(header_80: &[u8]) -> String {
    let mut h = dsha256(header_80); // internal order
    h.reverse(); // → display order
    bytes_to_hex(&h)
}

/// Confirmations from a verbose `blockchain.transaction.get` result (0 if absent).
fn confirmations_of(v: &Value) -> u64 {
    v.get("confirmations").and_then(|x| x.as_u64()).unwrap_or(0)
}

/// Build an [`SpvProofPack`] covering `txids` against the electrum endpoint `url`.
///
/// For each mined txid: read its depth (`transaction.get` verbose) to locate its height,
/// fetch `{pos, merkle}` (`transaction.get_merkle`) and the block header (`block_header_raw`)
/// for the root + hash, and assemble the anchor. The raw header is also stashed in
/// `pack.headers` so a header-less client can bootstrap (header-ful clients ignore it).
///
/// `network` is recorded verbatim (`regtest` | `signet` | `testnet` | `mainnet`). Unmined or
/// unknown txids are returned in `unproven`, not errored — the caller decides if that's fatal.
pub fn build_proof_pack(
    url: &str,
    txids: &[String],
    network: &str,
) -> Result<ProofPackResult, String> {
    let client = Client::new(url).map_err(|e| format!("connect {url}: {e}"))?;
    let tip = client
        .block_headers_subscribe()
        .map_err(|e| format!("tip: {e}"))?
        .height;

    let mut anchors = BTreeMap::new();
    let mut headers = BTreeMap::new();
    let mut unproven = Vec::new();

    for txid in txids {
        // 1. Depth → height. An unmined / unknown tx can't be proven.
        let verbose = match client.raw_call(
            "blockchain.transaction.get",
            [Param::String(txid.clone()), Param::Bool(true)],
        ) {
            Ok(v) => v,
            Err(_) => {
                unproven.push(txid.clone());
                continue;
            }
        };
        let confs = confirmations_of(&verbose);
        if confs == 0 {
            unproven.push(txid.clone());
            continue;
        }
        let height = tip + 1 - confs as usize; // tip - confs + 1

        // 2. Merkle path at that height.
        let merkle_res = match client.raw_call(
            "blockchain.transaction.get_merkle",
            [Param::String(txid.clone()), Param::Usize(height)],
        ) {
            Ok(v) => v,
            Err(_) => {
                unproven.push(txid.clone());
                continue;
            }
        };
        let pos = merkle_res.get("pos").and_then(|x| x.as_u64()).unwrap_or(0) as u32;
        // electrs returns `block_height` authoritatively; prefer it over our derived height.
        let block_height = merkle_res
            .get("block_height")
            .and_then(|x| x.as_u64())
            .unwrap_or(height as u64) as u32;
        let merkle_proof: Vec<String> = merkle_res
            .get("merkle")
            .and_then(|x| x.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|n| n.as_str().map(|s| s.to_owned()))
                    .collect()
            })
            .unwrap_or_default();

        // 3. Header for the root + block hash. Use the authoritative block_height.
        let header_raw = match client.block_header_raw(block_height as usize) {
            Ok(h) if h.len() == 80 => h,
            _ => {
                unproven.push(txid.clone());
                continue;
            }
        };
        let block_hash = block_hash_display(&header_raw);

        headers.insert(block_hash.clone(), bytes_to_hex(&header_raw));
        anchors.insert(
            txid.clone(),
            WitnessInclusion {
                block_hash,
                block_height,
                tx_index: pos,
                merkle_proof,
            },
        );
    }

    Ok(ProofPackResult {
        pack: SpvProofPack {
            version: 1,
            network: network.to_owned(),
            anchors,
            headers,
        },
        unproven,
    })
}

/// A [`HeaderSource`] backed by a live electrs endpoint — the maker/broker/native verifier
/// path. It vouches for a block only if electrs's own header at the claimed height hashes to
/// the claimed block hash, so it can't be fooled by a pack lying about which block it's in.
pub struct ElectrsHeaderSource {
    client: Client,
}

impl ElectrsHeaderSource {
    /// Connect to the electrum endpoint `url` (`tcp://host:port`).
    pub fn new(url: &str) -> Result<Self, String> {
        let client = Client::new(url).map_err(|e| format!("connect {url}: {e}"))?;
        Ok(Self { client })
    }
}

impl HeaderSource for ElectrsHeaderSource {
    fn header_at(&self, block_hash: &str, claimed_height: u32) -> Option<HeaderInfo> {
        let header = self.client.block_header_raw(claimed_height as usize).ok()?;
        if header.len() != 80 {
            return None;
        }
        // Independently confirm the claimed hash IS electrs's block at that height.
        if !block_hash_display(&header).eq_ignore_ascii_case(block_hash) {
            return None;
        }
        let merkle_root = header_merkle_root(&header)?;
        let tip = self.client.block_headers_subscribe().ok()?.height;
        let confirmations = (tip + 1).saturating_sub(claimed_height as usize) as u32;
        Some(HeaderInfo {
            merkle_root,
            confirmations,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::verify::{verify_pack, RejectReason};
    use electrum::{Client, ElectrumApi, Param};
    use std::collections::HashSet;

    fn electrum_url() -> String {
        std::env::var("RFQ_TEST_ELECTRUM").unwrap_or_else(|_| "tcp://127.0.0.1:60001".to_owned())
    }

    /// Gather `want` real mined txids by walking blocks down from the tip via id_from_pos
    /// (same approach as the MinedChecker regression test).
    fn gather(client: &Client, want: usize) -> Vec<String> {
        let tip = client.block_headers_subscribe().expect("tip").height;
        let mut txids = Vec::new();
        let mut h = tip;
        let mut scanned = 0usize;
        while txids.len() < want && h > 0 && scanned < 5000 {
            let mut pos = 0usize;
            while let Ok(v) = client.raw_call(
                "blockchain.transaction.id_from_pos",
                [Param::Usize(h), Param::Usize(pos), Param::Bool(false)],
            ) {
                let txid = v.as_str().or_else(|| v.get("tx_hash").and_then(|x| x.as_str()));
                match txid {
                    Some(t) => {
                        txids.push(t.to_owned());
                        pos += 1;
                        if txids.len() >= want {
                            break;
                        }
                    }
                    None => break,
                }
            }
            h -= 1;
            scanned += 1;
        }
        txids
    }

    #[test]
    #[ignore = "needs the regtest electrs up (tcp://127.0.0.1:60001, or RFQ_TEST_ELECTRUM)"]
    fn prover_pack_verifies_and_tampering_is_rejected() {
        let url = electrum_url();
        let client = Client::new(&url).expect("connect electrs");
        let txids = gather(&client, 6);
        assert!(!txids.is_empty(), "no txids gathered — is the chain synced?");

        // Produce a pack and verify it round-trips against a live header source.
        let result = build_proof_pack(&url, &txids, "regtest").expect("build pack");
        assert!(
            result.unproven.is_empty(),
            "all gathered txids are mined; unproven={:?}",
            result.unproven
        );
        assert_eq!(result.pack.anchors.len(), txids.len());

        let headers = ElectrsHeaderSource::new(&url).expect("header source");

        // (1) genuine pack → all mined.
        let v = verify_pack(&txids, &HashSet::new(), &result.pack, &headers, 1);
        assert!(v.all_mined, "genuine pack should verify; rejected={:?}", v.rejected);
        assert_eq!(v.checked, txids.len());

        // (2) drop an anchor → MissingAnchor for that witness.
        let mut dropped = result.pack.clone();
        let victim = txids[0].clone();
        dropped.anchors.remove(&victim);
        let v2 = verify_pack(&txids, &HashSet::new(), &dropped, &headers, 1);
        assert!(!v2.all_mined);
        assert!(v2
            .rejected
            .iter()
            .any(|(t, r)| *t == victim && *r == RejectReason::MissingAnchor));

        // (3) tamper a merkle proof (append a bogus node) → BadMerkle.
        let mut tampered = result.pack.clone();
        if let Some(a) = tampered.anchors.get_mut(&victim) {
            a.merkle_proof.push("ab".repeat(32));
        }
        let v3 = verify_pack(
            std::slice::from_ref(&victim),
            &HashSet::new(),
            &tampered,
            &headers,
            1,
        );
        assert!(!v3.all_mined);
        assert!(v3
            .rejected
            .iter()
            .any(|(t, r)| *t == victim && *r == RejectReason::BadMerkle));

        // (4) a never-broadcast txid is unprovable + rejected as MissingAnchor.
        let fake = "ab".repeat(32);
        let r_fake = build_proof_pack(&url, std::slice::from_ref(&fake), "regtest").expect("build");
        assert_eq!(r_fake.unproven, vec![fake.clone()]);
        assert!(r_fake.pack.anchors.is_empty());
        let v4 = verify_pack(std::slice::from_ref(&fake), &HashSet::new(), &r_fake.pack, &headers, 1);
        assert!(!v4.all_mined);
        assert_eq!(v4.rejected, vec![(fake, RejectReason::MissingAnchor)]);

        // (5) demanding more confirmations than exist → Unmined.
        let tip = client.block_headers_subscribe().expect("tip").height as u32;
        let v5 = verify_pack(&txids, &HashSet::new(), &result.pack, &headers, tip + 100);
        assert!(!v5.all_mined);
        assert!(v5
            .rejected
            .iter()
            .all(|(_, r)| matches!(r, RejectReason::Unmined { .. })));
    }
}
