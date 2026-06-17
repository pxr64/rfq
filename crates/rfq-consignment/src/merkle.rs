//! Pure Bitcoin merkle-inclusion math — no chain, no electrum. The trust-critical
//! core of the SPV verifier; compiles to native, wasm, and the ICP canister.
//!
//! ## Byte-order convention (the footgun)
//! Bitcoin hashes are *displayed* big-endian (the txid you see in explorers) but
//! *hashed and stored* little-endian ("internal" order). The block header's merkle-root
//! field (bytes 36..68 of the 80-byte header) is in internal order. This module's public
//! inputs (`txid`, `merkle_proof` nodes) are **display/big-endian hex** — the natural
//! electrum form — and it reverses each to internal order before hashing, returning the
//! computed root in internal order so it compares directly against the header field.

use sha2::{Digest, Sha256};

/// Bitcoin's double-SHA256.
pub fn dsha256(data: &[u8]) -> [u8; 32] {
    let first = Sha256::digest(data);
    let second = Sha256::digest(first);
    let mut out = [0u8; 32];
    out.copy_from_slice(&second);
    out
}

/// Decode a lowercase/uppercase hex string into bytes (`None` on odd length or non-hex).
pub fn hex_to_bytes(s: &str) -> Option<Vec<u8>> {
    let s = s.trim();
    if !s.len().is_multiple_of(2) {
        return None;
    }
    let mut out = Vec::with_capacity(s.len() / 2);
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let hi = (bytes[i] as char).to_digit(16)?;
        let lo = (bytes[i + 1] as char).to_digit(16)?;
        out.push((hi * 16 + lo) as u8);
        i += 2;
    }
    Some(out)
}

/// Encode bytes as lowercase hex.
pub fn bytes_to_hex(b: &[u8]) -> String {
    let mut s = String::with_capacity(b.len() * 2);
    for byte in b {
        s.push_str(&format!("{byte:02x}"));
    }
    s
}

/// Parse a 32-byte hash from a **display-order** (big-endian) hex string into
/// **internal** (little-endian) byte order used for hashing. `None` unless exactly 32 bytes.
pub fn hash32_display_to_internal(display_hex: &str) -> Option<[u8; 32]> {
    let mut bytes = hex_to_bytes(display_hex)?;
    if bytes.len() != 32 {
        return None;
    }
    bytes.reverse();
    let mut out = [0u8; 32];
    out.copy_from_slice(&bytes);
    Some(out)
}

/// Fold `txid_display` up `merkle_proof` (each node display-order hex), directed by
/// `tx_index`, and return the recomputed merkle root in **internal** byte order — ready to
/// compare against a block header's stored merkle-root field. `None` if any hash is malformed.
///
/// At level `i`, bit `i` of `tx_index` selects the current node's side: 0 = current is the
/// left child (sibling on the right), 1 = current is the right child (sibling on the left).
pub fn compute_merkle_root(
    txid_display: &str,
    merkle_proof: &[String],
    tx_index: u32,
) -> Option<[u8; 32]> {
    let mut cur = hash32_display_to_internal(txid_display)?;
    let mut idx = tx_index;
    for node in merkle_proof {
        let sib = hash32_display_to_internal(node)?;
        let mut buf = [0u8; 64];
        if idx & 1 == 0 {
            buf[..32].copy_from_slice(&cur);
            buf[32..].copy_from_slice(&sib);
        } else {
            buf[..32].copy_from_slice(&sib);
            buf[32..].copy_from_slice(&cur);
        }
        cur = dsha256(&buf);
        idx >>= 1;
    }
    Some(cur)
}

/// Extract the merkle-root field (internal order) from an 80-byte block header.
pub fn header_merkle_root(header_80: &[u8]) -> Option<[u8; 32]> {
    if header_80.len() != 80 {
        return None;
    }
    let mut out = [0u8; 32];
    out.copy_from_slice(&header_80[36..68]);
    Some(out)
}

/// The double-SHA256 of an 80-byte header, in internal order — i.e. the block hash as
/// stored/hashed. Reverse for display order.
pub fn header_block_hash_internal(header_80: &[u8]) -> Option<[u8; 32]> {
    if header_80.len() != 80 {
        return None;
    }
    Some(dsha256(header_80))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_round_trips() {
        let b = vec![0x00, 0xab, 0xff, 0x10];
        assert_eq!(bytes_to_hex(&b), "00abff10");
        assert_eq!(hex_to_bytes("00ABff10").unwrap(), b);
        assert!(hex_to_bytes("abc").is_none()); // odd length
        assert!(hex_to_bytes("zz").is_none()); // non-hex
    }

    #[test]
    fn single_tx_root_is_the_txid() {
        // Bitcoin genesis block: one tx, so merkle_root == coinbase txid. Locks the
        // display→internal reversal and the empty-path case against a real, fixed vector.
        let genesis_coinbase =
            "4a5e1e4baab89f3a32518a88c31bc87f618f76673e2cc77ab2127b7afdeda33b";
        let root = compute_merkle_root(genesis_coinbase, &[], 0).unwrap();
        // Expected root in internal order = reverse of the displayed merkle root (== txid).
        let expected = hash32_display_to_internal(genesis_coinbase).unwrap();
        assert_eq!(root, expected);
    }

    #[test]
    fn two_leaf_fold_matches_hand_computed() {
        // Build a 2-leaf tree from two arbitrary "txids" and check both index sides.
        let a_internal = [0x11u8; 32];
        let b_internal = [0x22u8; 32];
        let a_display = {
            let mut d = a_internal;
            d.reverse();
            bytes_to_hex(&d)
        };
        let b_display = {
            let mut d = b_internal;
            d.reverse();
            bytes_to_hex(&d)
        };
        // root = dsha256(a || b)
        let mut concat = [0u8; 64];
        concat[..32].copy_from_slice(&a_internal);
        concat[32..].copy_from_slice(&b_internal);
        let expected = dsha256(&concat);

        // leaf A at index 0: sibling B on the right.
        let root_a = compute_merkle_root(&a_display, std::slice::from_ref(&b_display), 0).unwrap();
        assert_eq!(root_a, expected);
        // leaf B at index 1: sibling A on the left → same root.
        let root_b = compute_merkle_root(&b_display, &[a_display], 1).unwrap();
        assert_eq!(root_b, expected);
    }

    #[test]
    fn tampered_node_changes_root() {
        let txid = "4a5e1e4baab89f3a32518a88c31bc87f618f76673e2cc77ab2127b7afdeda33b";
        let good = "ff".repeat(32);
        let bad = "fe".repeat(32);
        let r_good = compute_merkle_root(txid, std::slice::from_ref(&good), 0).unwrap();
        let r_bad = compute_merkle_root(txid, std::slice::from_ref(&bad), 0).unwrap();
        assert_ne!(r_good, r_bad);
    }
}
