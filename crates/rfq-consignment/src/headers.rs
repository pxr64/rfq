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

use crate::merkle::{bytes_to_hex, dsha256, header_merkle_root};
use crate::verify::{HeaderInfo, HeaderSource};

/// Bitcoin networks, distinguished by how header validity is established.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Network {
    Mainnet,
    Testnet3,
    Testnet4,
    Signet,
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
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Checkpoint {
    pub height: u32,
    /// Display-order block-hash hex.
    pub block_hash: String,
}

/// A validated, contiguous run of headers. Implements [`HeaderSource`] by vouching for any
/// block hash inside the run with its real merkle root + confirmation depth.
#[derive(Debug, Clone)]
pub struct CheckpointHeaderSource {
    /// block_hash (display hex) → (merkle_root internal, height).
    by_hash: HashMap<String, ([u8; 32], u32)>,
    tip_height: u32,
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

/// The compact "nBits" difficulty target (header bytes 72..76, little-endian) expanded to a
/// big-endian 256-bit value for direct comparison against a big-endian block hash.
fn target_from_compact(bits: u32) -> [u8; 32] {
    let exponent = (bits >> 24) as usize;
    let mantissa = bits & 0x007f_ffff;
    let mut target = [0u8; 32]; // big-endian
    let mant = [
        (mantissa >> 16) as u8,
        (mantissa >> 8) as u8,
        mantissa as u8,
    ];
    if exponent <= 3 {
        // Mantissa is shifted right; only the low bytes survive.
        let shifted = mantissa >> (8 * (3 - exponent));
        target[29] = (shifted >> 16) as u8;
        target[30] = (shifted >> 8) as u8;
        target[31] = shifted as u8;
    } else {
        // MSB of the mantissa sits at big-endian index 32 - exponent.
        let idx = 32usize.wrapping_sub(exponent);
        for (j, b) in mant.iter().enumerate() {
            let pos = idx.wrapping_add(j);
            if pos < 32 {
                target[pos] = *b;
            }
        }
    }
    target
}

/// True iff the header's block hash meets its stated proof-of-work target.
fn meets_pow(header_80: &[u8]) -> bool {
    let bits = u32::from_le_bytes([header_80[72], header_80[73], header_80[74], header_80[75]]);
    let target = target_from_compact(bits);
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

        let mut by_hash = HashMap::with_capacity(headers.len());
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
            // 3. Proof-of-work (network-gated).
            if network.checks_pow() && !meets_pow(header) {
                return Err(format!("header at index {i} fails its proof-of-work target"));
            }

            let height = checkpoint.height + i as u32;
            let root = header_merkle_root(header).ok_or("bad header length")?;
            by_hash.insert(header_hash_display(header), (root, height));
        }

        let tip_height = checkpoint.height + (headers.len() as u32 - 1);
        Ok(Self {
            by_hash,
            tip_height,
        })
    }

    /// Highest height in the validated run.
    pub fn tip_height(&self) -> u32 {
        self.tip_height
    }
}

impl HeaderSource for CheckpointHeaderSource {
    fn header_at(&self, block_hash: &str, claimed_height: u32) -> Option<HeaderInfo> {
        let key = block_hash.to_ascii_lowercase();
        let (merkle_root, height) = self.by_hash.get(&key)?;
        // The pack must not lie about which height the block sits at.
        if *height != claimed_height {
            return None;
        }
        let confirmations = (self.tip_height + 1).saturating_sub(*height);
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
    fn recommended_confs_policy() {
        assert_eq!(Network::Mainnet.recommended_confs(), 6);
        assert_eq!(Network::Testnet3.recommended_confs(), 3);
        assert_eq!(Network::Signet.recommended_confs(), 1);
        assert_eq!(Network::Regtest.recommended_confs(), 1);
    }
}
