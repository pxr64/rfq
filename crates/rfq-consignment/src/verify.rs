//! The SPV verifier (RFQIP-1 §3) — pure, electrum-free, wasm/canister-ready. Given the
//! witness-txid set from RGB crypto-validation and an [`SpvProofPack`], confirm every
//! witness is mined ≥ K deep, trusting **only** the caller's [`HeaderSource`].
//!
//! Trust split (RFQIP-1): the pack's *producer* is untrusted — a bad pack can only make a
//! witness *fail* here, never falsely pass, because the merkle root must match a header the
//! verifier's own source vouches for. The header source is the single trust anchor.

use std::collections::HashSet;

use serde::Serialize;

use crate::merkle::compute_merkle_root;
use crate::proofpack::SpvProofPack;

/// Default ceiling on how many witnesses a single consignment may carry before validation
/// refuses it. A forged consignment could otherwise present an enormous ancestry to force
/// thousands of merkle folds / electrum round-trips (a DoS). Real consignments are far
/// smaller; callers needing more can raise it. Shared by [`verify_pack`] and
/// [`crate::MinedChecker`] so both halves enforce the same bound.
pub const DEFAULT_MAX_WITNESSES: usize = 10_000;

/// What a [`HeaderSource`] vouches for about a block: its real merkle root (internal byte
/// order, as in header bytes 36..68) and how deep it is buried on the source's most-work chain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeaderInfo {
    /// Merkle root in internal byte order — compare directly against [`compute_merkle_root`].
    pub merkle_root: [u8; 32],
    /// Confirmations on the source's most-work chain (`tip - height + 1`).
    pub confirmations: u32,
}

/// The verifier's header trust source — the RFQIP-1 §3 ladder: the client's own validated
/// header chain, checkpoint-anchored bundled headers, or ICP-native headers. The verifier
/// trusts ONLY this; it never trusts the pack producer or the broker.
pub trait HeaderSource {
    /// Resolve the block `block_hash` (display-order hex) that the pack *claims* sits at
    /// `claimed_height`. Implementations MUST independently confirm that hash is on their
    /// most-work chain at that height (not merely echo the claim) and return its true
    /// merkle root + confirmation depth. `None` if the source can't vouch for the block.
    fn header_at(&self, block_hash: &str, claimed_height: u32) -> Option<HeaderInfo>;
}

/// Why one witness failed verification.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum RejectReason {
    /// No inclusion proof in the pack for this witness txid.
    MissingAnchor,
    /// The header source won't vouch for the claimed block (wrong chain / unknown / reorged).
    UnknownHeader,
    /// The merkle proof did not reproduce the block's merkle root.
    BadMerkle,
    /// Included, but shallower than K confirmations.
    Unmined { confirmations: u32 },
    /// A hash field in the pack was malformed (bad hex / wrong length).
    Malformed,
    /// The whole ancestry exceeded the size cap; rejected before any per-witness work.
    AncestryTooLarge { count: usize, cap: usize },
}

/// Outcome of verifying a full witness set against a pack.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SpvVerdict {
    /// True iff every checked (non-exempt) witness verified and is ≥ K deep.
    pub all_mined: bool,
    /// `(witness txid, reason)` for each failure — structured for logging/telemetry.
    pub rejected: Vec<(String, RejectReason)>,
    /// How many witnesses were actually checked (excludes `exempt`).
    pub checked: usize,
}

/// Verify that every txid in `witness_txids` (except those in `exempt`) carries a valid,
/// ≥`min_confs`-confirmed merkle-inclusion proof in `pack`, against `headers`. Pure — no
/// chain access. `exempt` carries the one hop a buy-side caller tolerates unmined (the
/// not-yet-broadcast swap tx); pass an empty set for the sell-side (all-mined).
pub fn verify_pack<H: HeaderSource>(
    witness_txids: &[String],
    exempt: &HashSet<String>,
    pack: &SpvProofPack,
    headers: &H,
    min_confs: u32,
) -> SpvVerdict {
    let k = min_confs.max(1);
    let mut rejected = Vec::new();
    let mut checked = 0usize;

    // Size cap first: refuse an oversized ancestry before doing any per-witness work.
    let to_check = witness_txids.iter().filter(|t| !exempt.contains(*t)).count();
    if to_check > DEFAULT_MAX_WITNESSES {
        return SpvVerdict {
            all_mined: false,
            rejected: vec![(
                "<ancestry>".to_owned(),
                RejectReason::AncestryTooLarge {
                    count: to_check,
                    cap: DEFAULT_MAX_WITNESSES,
                },
            )],
            checked: to_check,
        };
    }

    for txid in witness_txids {
        if exempt.contains(txid) {
            continue;
        }
        checked += 1;

        let Some(anchor) = pack.anchors.get(txid) else {
            rejected.push((txid.clone(), RejectReason::MissingAnchor));
            continue;
        };
        let Some(info) = headers.header_at(&anchor.block_hash, anchor.block_height) else {
            rejected.push((txid.clone(), RejectReason::UnknownHeader));
            continue;
        };
        let Some(computed) = compute_merkle_root(txid, &anchor.merkle_proof, anchor.tx_index)
        else {
            rejected.push((txid.clone(), RejectReason::Malformed));
            continue;
        };
        if computed != info.merkle_root {
            rejected.push((txid.clone(), RejectReason::BadMerkle));
            continue;
        }
        if info.confirmations < k {
            rejected.push((
                txid.clone(),
                RejectReason::Unmined {
                    confirmations: info.confirmations,
                },
            ));
            continue;
        }
    }

    SpvVerdict {
        all_mined: rejected.is_empty(),
        rejected,
        checked,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::merkle::hash32_display_to_internal;
    use crate::proofpack::WitnessInclusion;
    use std::collections::BTreeMap;

    /// A header source backed by a fixed `block_hash -> (merkle_root, confs)` map. It
    /// honours the contract: only vouches for hashes it knows (a real one verifies the chain).
    struct MockHeaders(BTreeMap<String, HeaderInfo>);

    impl HeaderSource for MockHeaders {
        fn header_at(&self, block_hash: &str, _claimed_height: u32) -> Option<HeaderInfo> {
            self.0.get(block_hash).cloned()
        }
    }

    /// Build a single-tx pack: the witness IS the coinbase, so merkle_root == txid and the
    /// proof is empty. The mock vouches for the block with that root at `confs` depth.
    fn single_tx_fixture(txid: &str, block_hash: &str, confs: u32) -> (SpvProofPack, MockHeaders) {
        let mut anchors = BTreeMap::new();
        anchors.insert(
            txid.to_owned(),
            WitnessInclusion {
                block_hash: block_hash.to_owned(),
                block_height: 100,
                tx_index: 0,
                merkle_proof: vec![],
            },
        );
        let pack = SpvProofPack {
            version: 1,
            network: "regtest".to_owned(),
            anchors,
            headers: BTreeMap::new(),
        };
        let mut hmap = BTreeMap::new();
        hmap.insert(
            block_hash.to_owned(),
            HeaderInfo {
                merkle_root: hash32_display_to_internal(txid).unwrap(),
                confirmations: confs,
            },
        );
        (pack, MockHeaders(hmap))
    }

    const TXID: &str = "4a5e1e4baab89f3a32518a88c31bc87f618f76673e2cc77ab2127b7afdeda33b";
    const BLOCK: &str = "0f9188f13cb7b2c71f2a335e3a4fc328bf5beb436012afca590b1a11466e2206";

    #[test]
    fn accepts_a_valid_mined_witness() {
        let (pack, headers) = single_tx_fixture(TXID, BLOCK, 6);
        let v = verify_pack(&[TXID.to_owned()], &HashSet::new(), &pack, &headers, 1);
        assert!(v.all_mined, "rejected: {:?}", v.rejected);
        assert_eq!(v.checked, 1);
    }

    #[test]
    fn rejects_missing_anchor() {
        let (pack, headers) = single_tx_fixture(TXID, BLOCK, 6);
        let other = "ab".repeat(32);
        let v = verify_pack(std::slice::from_ref(&other), &HashSet::new(), &pack, &headers, 1);
        assert!(!v.all_mined);
        assert_eq!(v.rejected, vec![(other, RejectReason::MissingAnchor)]);
    }

    #[test]
    fn rejects_bad_merkle_root() {
        // Header source vouches for the block but with a DIFFERENT root → proof can't match.
        let (pack, _) = single_tx_fixture(TXID, BLOCK, 6);
        let mut hmap = BTreeMap::new();
        hmap.insert(
            BLOCK.to_owned(),
            HeaderInfo {
                merkle_root: [0x99; 32],
                confirmations: 6,
            },
        );
        let v = verify_pack(
            &[TXID.to_owned()],
            &HashSet::new(),
            &pack,
            &MockHeaders(hmap),
            1,
        );
        assert!(!v.all_mined);
        assert_eq!(v.rejected, vec![(TXID.to_owned(), RejectReason::BadMerkle)]);
    }

    #[test]
    fn rejects_unknown_header() {
        let (pack, _) = single_tx_fixture(TXID, BLOCK, 6);
        let v = verify_pack(
            &[TXID.to_owned()],
            &HashSet::new(),
            &pack,
            &MockHeaders(BTreeMap::new()),
            1,
        );
        assert!(!v.all_mined);
        assert_eq!(v.rejected, vec![(TXID.to_owned(), RejectReason::UnknownHeader)]);
    }

    #[test]
    fn rejects_shallow_confirmations() {
        let (pack, headers) = single_tx_fixture(TXID, BLOCK, 2);
        let v = verify_pack(&[TXID.to_owned()], &HashSet::new(), &pack, &headers, 6);
        assert!(!v.all_mined);
        assert_eq!(
            v.rejected,
            vec![(TXID.to_owned(), RejectReason::Unmined { confirmations: 2 })]
        );
    }

    #[test]
    fn rejects_oversized_ancestry() {
        let (pack, headers) = single_tx_fixture(TXID, BLOCK, 6);
        let txids: Vec<String> = (0..DEFAULT_MAX_WITNESSES + 1)
            .map(|i| format!("{i:064x}"))
            .collect();
        let v = verify_pack(&txids, &HashSet::new(), &pack, &headers, 1);
        assert!(!v.all_mined);
        assert!(matches!(
            v.rejected.first(),
            Some((_, RejectReason::AncestryTooLarge { .. }))
        ));
    }

    #[test]
    fn exempt_hop_is_skipped() {
        // The buy-side swap tx: absent from the pack, but exempt → not counted, not rejected.
        let (pack, headers) = single_tx_fixture(TXID, BLOCK, 6);
        let swap = "cc".repeat(32);
        let v = verify_pack(
            &[TXID.to_owned(), swap.clone()],
            &HashSet::from([swap]),
            &pack,
            &headers,
            1,
        );
        assert!(v.all_mined, "rejected: {:?}", v.rejected);
        assert_eq!(v.checked, 1, "exempt hop must not be counted");
    }
}
