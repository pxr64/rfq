//! Checkpoint-anchored Bitcoin header-chain validator — a pure [`HeaderSource`]
//! (RFQIP-1 §3, ladder rung 2) for thin clients that **fetch** headers from an untrusted
//! source (e.g. a wallet pulling from esplora) and must **validate** them rather than
//! trust them. No electrum, no chain access: feed it a contiguous run of headers and a
//! baked-in checkpoint, and it vouches for any block in that run.
//!
//! ## What it checks
//! 1. **Checkpoint anchor** — the first supplied header must hash to a `(height, hash)`
//!    checkpoint compiled into the client. A forger can't start the chain anywhere else.
//! 2. **Parent linkage** — each header's `prev_block` must equal the previous header's
//!    hash. The run is one unbroken chain rooted at the checkpoint.
//! 3. **Network validity** — on **mainnet**, every header must meet its own
//!    proof-of-work target (so headers above the checkpoint can't be cheaply forged).
//!    Signet (signer-signature) and regtest carry no usable PoW in the header alone, so
//!    there we rely on checkpoint + linkage. See the `checks_pow` note.
//!
//! Residual trust is exactly the SPV assumption: the checkpoint is honest (it ships in the
//! client) and no reorg deeper than the chain's depth-below-tip occurs.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::difficulty::{expected_retarget_bits, target_from_compact, RETARGET_INTERVAL};
use crate::merkle::{bytes_to_hex, dsha256, header_merkle_root};
use crate::verify::{HeaderInfo, HeaderSource};

/// Bitcoin networks, distinguished by how header validity is established.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Network {
    /// Bitcoin mainnet — proof-of-work secured; full difficulty validation applies.
    Mainnet,
    /// Testnet3 — PoW with the 20-minute min-difficulty rule (not yet fully validated here).
    Testnet3,
    /// Testnet4.
    Testnet4,
    /// Signet — blocks secured by a signer signature, not header PoW.
    Signet,
    /// Regtest — local testing; no real proof-of-work.
    Regtest,
}

impl Network {
    /// Parse the label used in [`crate::SpvProofPack::network`].
    pub fn from_label(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "mainnet" | "bitcoin" => Some(Self::Mainnet),
            "testnet" | "testnet3" => Some(Self::Testnet3),
            "testnet4" => Some(Self::Testnet4),
            "signet" => Some(Self::Signet),
            "regtest" => Some(Self::Regtest),
            _ => None,
        }
    }

    /// Recommended confirmation depth K before treating a witness as final — the single
    /// source of truth for the maker gates, broker precheck, and thin-client verifiers.
    /// Mainnet buries deep (reorg risk is real); test networks tolerate K=1.
    pub fn recommended_confs(&self) -> u32 {
        match self {
            Network::Mainnet => 6,
            Network::Testnet3 | Network::Testnet4 => 3,
            Network::Signet | Network::Regtest => 1,
        }
    }

    /// Whether header proof-of-work is the validity anchor for this network.
    ///
    /// Only mainnet today: testnet3's 20-minute min-difficulty rule and signet's
    /// coinbase signature aren't checkable from an 80-byte header alone, so those rely on
    /// checkpoint + linkage (acceptable for test networks). Tightening testnet/signet is a
    /// follow-up — see RFQIP-1 §3.
    fn checks_pow(&self) -> bool {
        matches!(self, Network::Mainnet)
    }
}

/// A `(height, block-hash)` pair the client trusts a priori — shipped in the binary.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct Checkpoint {
    /// Block height of the checkpoint (must be a retarget-epoch boundary on PoW networks).
    pub height: u32,
    /// Display-order block-hash hex.
    pub block_hash: String,
}

/// A validated, contiguous run of headers. Implements [`HeaderSource`] by vouching for any
/// block hash inside the run with its real merkle root + confirmation depth.
#[derive(Debug, Clone)]
pub struct CheckpointHeaderSource {
    /// block_hash (display hex) → (merkle_root internal, height, run_top_height), where
    /// `run_top_height` is the top of the *validated run* this block belongs to. Confirmation
    /// depth is measured against that validated height — never an externally-supplied tip, which
    /// an untrusted indexer could inflate to forge depth (see `header_at`). For a single full run
    /// (`new`) every block's `run_top_height` is the run's last block.
    by_hash: HashMap<String, ([u8; 32], u32, u32)>,
}

/// Display-order (big-endian) hash hex of an 80-byte header.
fn header_hash_display(header_80: &[u8]) -> String {
    let mut h = dsha256(header_80); // internal order
    h.reverse();
    bytes_to_hex(&h)
}

/// Display-order `prev_block` hex from an 80-byte header (bytes 4..36 are internal order).
fn header_prev_display(header_80: &[u8]) -> String {
    let mut p = [0u8; 32];
    p.copy_from_slice(&header_80[4..36]);
    p.reverse();
    bytes_to_hex(&p)
}

/// The compact "nBits" difficulty target from a header (bytes 72..76, little-endian).
fn header_bits(header_80: &[u8]) -> u32 {
    u32::from_le_bytes([header_80[72], header_80[73], header_80[74], header_80[75]])
}

/// The block timestamp from a header (bytes 68..72, little-endian).
fn header_time(header_80: &[u8]) -> u32 {
    u32::from_le_bytes([header_80[68], header_80[69], header_80[70], header_80[71]])
}

/// True iff the header's block hash meets its stated proof-of-work target. NOTE: this only
/// checks the *stated* `bits`; that the `bits` are themselves *correct* is enforced
/// separately by the retarget validation in [`CheckpointHeaderSource::new`] — without it,
/// PoW-meets-stated-bits is forgeable for free.
fn meets_pow(header_80: &[u8]) -> bool {
    let target = target_from_compact(header_bits(header_80));
    let mut hash_be = dsha256(header_80); // internal (LE)
    hash_be.reverse(); // → big-endian for numeric compare
    hash_be <= target
}

impl CheckpointHeaderSource {
    /// Validate a contiguous run of 80-byte headers starting AT `checkpoint.height`
    /// (`headers[0]` must hash to `checkpoint.block_hash`) and build a source that vouches
    /// for any block in the run. Errors on a broken checkpoint, broken linkage, a non-80-byte
    /// header, or (on PoW networks) a header that fails its target.
    pub fn new(
        network: Network,
        checkpoint: &Checkpoint,
        headers: &[Vec<u8>],
    ) -> Result<Self, String> {
        let mut by_hash = HashMap::with_capacity(headers.len());
        validate_run(network, checkpoint, headers, &mut by_hash)?;
        Ok(Self { by_hash })
    }

    /// Build a source from **multiple** validated segments — the dense-checkpoint path
    /// (RFQIP-1). Each segment is an epoch-aligned checkpoint plus a contiguous run starting
    /// at it; a thin client fetches only the short run around each witness (≤ one epoch from
    /// the nearest baked checkpoint, via [`nearest_checkpoint`]) instead of syncing the whole
    /// chain. Every segment is validated (linkage + PoW + difficulty) and merged into one
    /// lookup. Confirmation depth for each block is measured against **its own** validated run's
    /// top — there is no externally-supplied tip (a thin client proves a witness is K-deep by
    /// fetching K headers above it, so each run carries its own depth, and disjoint runs never
    /// lend each other confirmations).
    pub fn from_segments(
        network: Network,
        segments: &[(Checkpoint, Vec<Vec<u8>>)],
    ) -> Result<Self, String> {
        if segments.is_empty() {
            return Err("CheckpointHeaderSource: no segments supplied".to_owned());
        }
        let mut by_hash = HashMap::new();
        for (checkpoint, headers) in segments {
            validate_run(network, checkpoint, headers, &mut by_hash)?;
        }
        Ok(Self { by_hash })
    }

    /// Highest validated height this source vouches for — the top of the tallest validated run.
    /// Derived from validated headers; there is no externally-supplied tip.
    pub fn tip_height(&self) -> u32 {
        self.by_hash
            .values()
            .map(|(_, _, run_top)| *run_top)
            .max()
            .unwrap_or(0)
    }

    /// The epoch-boundary (`height % RETARGET_INTERVAL == 0`) checkpoints inside the validated
    /// run, ascending. A thin client uses this to **extend** its local checkpoint store forward:
    /// validate a run from its current frontier, then persist these as new trusted anchors. They
    /// are trustworthy because they come from a validated `CheckpointHeaderSource` (linkage [+ PoW
    /// + difficulty on mainnet]) anchored at an already-trusted checkpoint.
    pub fn epoch_checkpoints(&self) -> Vec<Checkpoint> {
        let mut out: Vec<Checkpoint> = self
            .by_hash
            .iter()
            .filter(|(_, (_, height, _))| height.is_multiple_of(RETARGET_INTERVAL))
            .map(|(block_hash, (_, height, _))| Checkpoint {
                height: *height,
                block_hash: block_hash.clone(),
            })
            .collect();
        out.sort_by_key(|c| c.height);
        out
    }
}

/// Pick the highest checkpoint at or below `height` — the anchor a thin client fetches its
/// bounded per-witness header run from. `checkpoints` need not be sorted.
pub fn nearest_checkpoint(checkpoints: &[Checkpoint], height: u32) -> Option<&Checkpoint> {
    checkpoints
        .iter()
        .filter(|c| c.height <= height)
        .max_by_key(|c| c.height)
}

/// Validate one contiguous header run anchored at `checkpoint` (checkpoint match + linkage +
/// network-gated PoW & difficulty) and insert every block into `by_hash`.
fn validate_run(
    network: Network,
    checkpoint: &Checkpoint,
    headers: &[Vec<u8>],
    by_hash: &mut HashMap<String, ([u8; 32], u32, u32)>,
) -> Result<u32, String> {
    if headers.is_empty() {
        return Err("CheckpointHeaderSource: no headers supplied".to_owned());
    }
    if headers.iter().any(|h| h.len() != 80) {
        return Err("CheckpointHeaderSource: every header must be exactly 80 bytes".to_owned());
    }

    // 1. Checkpoint anchor.
    let first_hash = header_hash_display(&headers[0]);
    if !first_hash.eq_ignore_ascii_case(&checkpoint.block_hash) {
        return Err(format!(
            "checkpoint mismatch: headers[0] hashes to {first_hash}, expected {}",
            checkpoint.block_hash
        ));
    }

    // On PoW networks, difficulty validation requires the checkpoint to sit on a retarget
    // (difficulty-epoch) boundary, so the first epoch in the run is complete from the
    // checkpoint and every later retarget's 2016-block lookback is in-run.
    let check_difficulty = network.checks_pow();
    if check_difficulty && !checkpoint.height.is_multiple_of(RETARGET_INTERVAL) {
        return Err(format!(
            "checkpoint height {} is not on a retarget boundary (multiple of {RETARGET_INTERVAL})",
            checkpoint.height
        ));
    }

    // Top of THIS validated run — the confirmation-depth anchor for every block in it (a
    // validated height, never an external tip). Known up front: the run is contiguous from the
    // checkpoint, so its last block is at `checkpoint.height + len - 1`.
    let run_top = checkpoint.height + (headers.len() as u32 - 1);

    let mut prev_hash = first_hash.clone();
    for (i, header) in headers.iter().enumerate() {
        // 2. Linkage (skip for the anchor itself).
        if i > 0 {
            let want_prev = header_prev_display(header);
            if !want_prev.eq_ignore_ascii_case(&prev_hash) {
                return Err(format!(
                    "broken linkage at index {i}: prev_block {want_prev} != {prev_hash}"
                ));
            }
            prev_hash = header_hash_display(header);
        }
        let height = checkpoint.height + i as u32;

        // 3. Proof-of-work + difficulty correctness (network-gated). PoW alone is forgeable
        //    (the attacker writes `bits`); we ALSO recompute the required difficulty so `bits`
        //    can't be faked low.
        if check_difficulty {
            if !meets_pow(header) {
                return Err(format!(
                    "header at index {i} (height {height}) fails its proof-of-work target"
                ));
            }
            if i > 0 {
                let bits = header_bits(header);
                let expected = if height.is_multiple_of(RETARGET_INTERVAL) {
                    // Retarget boundary: recompute from the previous epoch's first + last block
                    // (in-run, since the checkpoint is epoch-aligned).
                    let last = &headers[i - 1];
                    let first = &headers[i - RETARGET_INTERVAL as usize];
                    expected_retarget_bits(header_bits(last), header_time(last), header_time(first))
                } else {
                    // Mid-epoch: difficulty is constant, so `bits` must match the prior block.
                    header_bits(&headers[i - 1])
                };
                if bits != expected {
                    return Err(format!(
                        "header at index {i} (height {height}): difficulty bits {bits:#010x} \
                         != expected {expected:#010x}"
                    ));
                }
            }
        }

        let root = header_merkle_root(header).ok_or("bad header length")?;
        by_hash.insert(header_hash_display(header), (root, height, run_top));
    }
    Ok(run_top)
}

impl HeaderSource for CheckpointHeaderSource {
    fn header_at(&self, block_hash: &str, claimed_height: u32) -> Option<HeaderInfo> {
        let key = block_hash.to_ascii_lowercase();
        let (merkle_root, height, run_top) = self.by_hash.get(&key)?;
        // The pack must not lie about which height the block sits at.
        if *height != claimed_height {
            return None;
        }
        // Depth is measured against this block's own validated run top — NOT an external tip an
        // untrusted indexer could inflate. A thin client proves K confirmations by validating K
        // headers above the witness; disjoint runs never lend each other depth.
        let confirmations = (run_top + 1).saturating_sub(*height);
        Some(HeaderInfo {
            merkle_root: *merkle_root,
            confirmations,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::merkle::{hash32_display_to_internal, hex_to_bytes};

    // Real mainnet headers 0–2 (difficulty-1 PoW, bits 0x1d00ffff).
    const GENESIS: &str = "0100000000000000000000000000000000000000000000000000000000000000000000003ba3edfd7a7b12b27ac72c3e67768f617fc81bc3888a51323a9fb8aa4b1e5e4a29ab5f49ffff001d1dac2b7c";
    const GENESIS_HASH: &str = "000000000019d6689c085ae165831e934ff763ae46a2a6c172b3f1b60a8ce26f";
    const BLOCK1: &str = "010000006fe28c0ab6f1b372c1a6a246ae63f74f931e8365e15a089c68d6190000000000982051fd1e4ba744bbbe680e1fee14677ba1a3c3540bf7b1cdb606e857233e0e61bc6649ffff001d01e36299";
    const BLOCK1_HASH: &str = "00000000839a8e6886ab5951d76f411475428afc90947ee320161bbf18eb6048";
    const BLOCK2: &str = "010000004860eb18bf1b1620e37e9490fc8a427514416fd75159ab86688e9a8300000000d5fdcc541e25de1c7a5addedf24858b8bb665c9f36ef744ee42c316022c90f9bb0bc6649ffff001d08d2bd61";

    fn raw(h: &str) -> Vec<u8> {
        hex_to_bytes(h).unwrap()
    }

    fn checkpoint() -> Checkpoint {
        Checkpoint {
            height: 0,
            block_hash: GENESIS_HASH.to_owned(),
        }
    }

    /// Build `n` linked synthetic 80-byte headers (regtest — no PoW), each chaining to the
    /// previous by real double-SHA256. `prev0` is headers[0]'s `prev_block` (internal order);
    /// the caller anchors a checkpoint at `header_hash_display(&headers[0])`.
    fn synth_chain(prev0: [u8; 32], merkle_tag: u8, n: usize) -> Vec<Vec<u8>> {
        let mut out = Vec::with_capacity(n);
        let mut prev = prev0;
        for i in 0..n {
            let mut h = vec![0u8; 80];
            h[0] = 1; // version
            h[4..36].copy_from_slice(&prev);
            h[36..68].copy_from_slice(&[merkle_tag.wrapping_add(i as u8); 32]); // arbitrary root
            h[72..76].copy_from_slice(&0x1d00_ffffu32.to_le_bytes()); // bits (ignored on regtest)
            prev = crate::merkle::dsha256(&h);
            out.push(h);
        }
        out
    }

    #[test]
    fn validates_a_real_chain_and_vouches_for_each_block() {
        let headers = [raw(GENESIS), raw(BLOCK1), raw(BLOCK2)];
        let src = CheckpointHeaderSource::new(Network::Mainnet, &checkpoint(), &headers).unwrap();
        assert_eq!(src.tip_height(), 2);

        // Genesis: 3 deep, merkle root == its coinbase txid (internal).
        let g = src.header_at(GENESIS_HASH, 0).unwrap();
        assert_eq!(g.confirmations, 3);
        let genesis_coinbase =
            "4a5e1e4baab89f3a32518a88c31bc87f618f76673e2cc77ab2127b7afdeda33b";
        assert_eq!(g.merkle_root, hash32_display_to_internal(genesis_coinbase).unwrap());

        // Block 1: 2 deep.
        assert_eq!(src.header_at(BLOCK1_HASH, 1).unwrap().confirmations, 2);
    }

    #[test]
    fn rejects_wrong_claimed_height() {
        let headers = [raw(GENESIS), raw(BLOCK1)];
        let src = CheckpointHeaderSource::new(Network::Mainnet, &checkpoint(), &headers).unwrap();
        // Block 1 is at height 1; a pack claiming height 2 must not be vouched for.
        assert!(src.header_at(BLOCK1_HASH, 2).is_none());
        // Unknown block hash → None.
        assert!(src.header_at(&"ab".repeat(32), 1).is_none());
    }

    #[test]
    fn rejects_checkpoint_mismatch() {
        let wrong = Checkpoint {
            height: 0,
            block_hash: "ff".repeat(32),
        };
        let err = CheckpointHeaderSource::new(Network::Mainnet, &wrong, &[raw(GENESIS)]).unwrap_err();
        assert!(err.contains("checkpoint mismatch"), "{err}");
    }

    #[test]
    fn rejects_broken_linkage() {
        // Genesis followed by block 2 (skips block 1) → prev_block won't match.
        let headers = [raw(GENESIS), raw(BLOCK2)];
        let err = CheckpointHeaderSource::new(Network::Mainnet, &checkpoint(), &headers).unwrap_err();
        assert!(err.contains("linkage"), "{err}");
    }

    #[test]
    fn mainnet_requires_epoch_aligned_checkpoint() {
        // A mainnet checkpoint must sit on a retarget boundary (multiple of 2016) so the
        // difficulty validation has a complete first epoch to anchor on.
        let misaligned = Checkpoint {
            height: 5, // not a multiple of 2016
            block_hash: GENESIS_HASH.to_owned(),
        };
        let err =
            CheckpointHeaderSource::new(Network::Mainnet, &misaligned, &[raw(GENESIS)]).unwrap_err();
        assert!(err.contains("retarget boundary"), "{err}");

        // The same misalignment is fine on a non-PoW network (regtest/signet skip difficulty).
        assert!(CheckpointHeaderSource::new(Network::Regtest, &misaligned, &[raw(GENESIS)]).is_ok());
    }

    #[test]
    fn mainnet_rejects_mid_epoch_difficulty_change() {
        // genesis (bits 0x1d00ffff) + a forged "block 1" that links correctly but lies about
        // its difficulty (different bits mid-epoch) → rejected by the difficulty check.
        // We keep genesis's real PoW; only the second header's bits are tampered. Since its
        // hash won't meet PoW either, this proves the chain refuses the forgery on mainnet.
        let mut forged = vec![0u8; 80];
        forged[0] = 1;
        let prev = hash32_display_to_internal(GENESIS_HASH).unwrap();
        forged[4..36].copy_from_slice(&prev);
        forged[36..68].copy_from_slice(&[0x22; 32]);
        forged[72..76].copy_from_slice(&0x1c00_ffffu32.to_le_bytes()); // different bits
        let headers = [raw(GENESIS), forged];
        let err =
            CheckpointHeaderSource::new(Network::Mainnet, &checkpoint(), &headers).unwrap_err();
        // Fails either PoW or the difficulty check — both are the gate we want.
        assert!(
            err.contains("proof-of-work") || err.contains("difficulty"),
            "{err}"
        );
    }

    #[test]
    fn pow_gating_is_network_specific() {
        // A forged header that LINKS to genesis (prev = genesis) but is pure garbage
        // otherwise, so it won't meet difficulty-1 PoW.
        let mut forged = vec![0u8; 80];
        forged[0] = 1; // version
        let mut prev = hash32_display_to_internal(GENESIS_HASH).unwrap(); // internal order
        forged[4..36].copy_from_slice(&prev);
        // garbage merkle root + time; set easy-looking bits but hash won't meet target.
        forged[36..68].copy_from_slice(&[0x11; 32]);
        forged[72..76].copy_from_slice(&0x1d00ffffu32.to_le_bytes());
        let _ = &mut prev;

        let headers = [raw(GENESIS), forged.clone()];

        // Mainnet: PoW enforced → the forged header is rejected.
        let err =
            CheckpointHeaderSource::new(Network::Mainnet, &checkpoint(), &headers).unwrap_err();
        assert!(err.contains("proof-of-work"), "{err}");

        // Regtest: PoW not the anchor → linkage alone accepts it (and would vouch for it).
        let src = CheckpointHeaderSource::new(Network::Regtest, &checkpoint(), &headers).unwrap();
        assert_eq!(src.tip_height(), 1);
    }

    #[test]
    fn genesis_meets_pow() {
        assert!(meets_pow(&raw(GENESIS)));
        assert!(meets_pow(&raw(BLOCK1)));
        // Difficulty-1 target expands correctly.
        let t = target_from_compact(0x1d00ffff);
        let mut expected = [0u8; 32];
        expected[4] = 0xff;
        expected[5] = 0xff;
        assert_eq!(t, expected);
    }

    #[test]
    fn network_labels_parse() {
        assert_eq!(Network::from_label("MAINNET"), Some(Network::Mainnet));
        assert_eq!(Network::from_label("signet"), Some(Network::Signet));
        assert_eq!(Network::from_label("regtest"), Some(Network::Regtest));
        assert_eq!(Network::from_label("nope"), None);
    }

    #[test]
    fn from_segments_uses_run_top_for_confirmations() {
        // No external tip: confirmations come from the validated run's OWN top. A 3-header run
        // from genesis puts genesis 3 deep (run top = height 2 ⇒ 2+1-0) — a thin client that
        // wants more depth must validate more headers, it can't borrow depth from a claimed tip.
        let headers = vec![raw(GENESIS), raw(BLOCK1), raw(BLOCK2)];
        let src = CheckpointHeaderSource::from_segments(Network::Mainnet, &[(checkpoint(), headers)])
            .unwrap();
        assert_eq!(src.tip_height(), 2);
        assert_eq!(src.header_at(GENESIS_HASH, 0).unwrap().confirmations, 3);
        assert_eq!(src.header_at(BLOCK1_HASH, 1).unwrap().confirmations, 2);
    }

    #[test]
    fn from_segments_disjoint_runs_use_own_tops() {
        // Two disjoint validated runs at different epochs. A block's depth must derive from ITS
        // run's top — a taller run must NOT lend depth to a shorter one (else an attacker pads an
        // unrelated tall run to forge confirmations on a shallow witness). Regtest skips PoW, so
        // we synthesize linked headers and anchor each run at its own (synthetic) checkpoint.
        let run_a = synth_chain([0x00; 32], 0x10, 2); // heights 0,1 → run_top 1
        let cp_a = Checkpoint { height: 0, block_hash: header_hash_display(&run_a[0]) };
        let run_b = synth_chain([0xAB; 32], 0x40, 3); // heights 2016..=2018 → run_top 2018
        let cp_b = Checkpoint { height: 2016, block_hash: header_hash_display(&run_b[0]) };

        let src = CheckpointHeaderSource::from_segments(
            Network::Regtest,
            &[(cp_a, run_a.clone()), (cp_b, run_b.clone())],
        )
        .unwrap();

        // Run A's first block: 2 deep against run A's top (1) — NOT 2019 against run B's top.
        let a0 = header_hash_display(&run_a[0]);
        assert_eq!(src.header_at(&a0, 0).unwrap().confirmations, 2);
        // Run B's first block: 3 deep against run B's own top (2018).
        let b0 = header_hash_display(&run_b[0]);
        assert_eq!(src.header_at(&b0, 2016).unwrap().confirmations, 3);
        // tip_height() reports the tallest run's top.
        assert_eq!(src.tip_height(), 2018);
    }

    #[test]
    fn nearest_checkpoint_picks_highest_at_or_below() {
        let cps = vec![
            Checkpoint { height: 0, block_hash: "a".into() },
            Checkpoint { height: 4032, block_hash: "c".into() },
            Checkpoint { height: 2016, block_hash: "b".into() },
        ];
        assert_eq!(nearest_checkpoint(&cps, 3000).unwrap().height, 2016);
        assert_eq!(nearest_checkpoint(&cps, 5000).unwrap().height, 4032);
        assert_eq!(nearest_checkpoint(&cps, 2016).unwrap().height, 2016);
        assert_eq!(nearest_checkpoint(&cps, 0).unwrap().height, 0);
        // Nothing at or below → None.
        let high_only = [Checkpoint { height: 2016, block_hash: "b".into() }];
        assert!(nearest_checkpoint(&high_only, 100).is_none());
    }

    #[test]
    fn epoch_checkpoints_lists_only_boundary_blocks() {
        // genesis(0) + block1(1) + block2(2): only height 0 is on a 2016 boundary.
        let headers = [raw(GENESIS), raw(BLOCK1), raw(BLOCK2)];
        let src = CheckpointHeaderSource::new(Network::Mainnet, &checkpoint(), &headers).unwrap();
        let cps = src.epoch_checkpoints();
        assert_eq!(cps.len(), 1);
        assert_eq!(cps[0].height, 0);
        assert!(cps[0].block_hash.eq_ignore_ascii_case(GENESIS_HASH));
    }

    #[test]
    fn recommended_confs_policy() {
        assert_eq!(Network::Mainnet.recommended_confs(), 6);
        assert_eq!(Network::Testnet3.recommended_confs(), 3);
        assert_eq!(Network::Signet.recommended_confs(), 1);
        assert_eq!(Network::Regtest.recommended_confs(), 1);
    }
}
