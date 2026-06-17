//! Bitcoin difficulty-retarget validation — the piece that makes header proof-of-work
//! *trustworthy* rather than self-asserted (Tier-2 mainnet hardening, RFQIP-1 §3).
//!
//! ## Why this exists
//! A header carries its own difficulty target in the `bits` field, and the PoW check is
//! "hash ≤ target". But the attacker *writes* `bits` — so on its own, PoW-meets-stated-bits
//! is forgeable for free (claim minimum difficulty, mine on a laptop). The fix is to
//! independently **recompute** the required difficulty from the previous epoch's block
//! timestamps and reject any header whose `bits` disagree. Then every block must carry the
//! real network difficulty, and forging the chain costs real Bitcoin-scale work.
//!
//! This mirrors Bitcoin Core's `arith_uint256::SetCompact`/`GetCompact` and
//! `CalculateNextWorkRequired`. Difficulty only changes every [`RETARGET_INTERVAL`] (2016)
//! blocks — one "epoch" ≈ 2 weeks; within an epoch every block shares the epoch's `bits`.
//!
//! Targets are 256-bit big-endian `[u8; 32]`, matching `header[36..68]`-style layout and
//! directly `Ord`-comparable (big-endian byte order == numeric order).

/// Blocks per difficulty epoch (one retarget interval).
pub const RETARGET_INTERVAL: u32 = 2016;

/// Target timespan for one epoch: 2016 × 10 min = 2 weeks, in seconds.
const TARGET_TIMESPAN: u64 = 14 * 24 * 60 * 60; // 1_209_600

/// Mainnet minimum difficulty (difficulty-1) target, compact form.
pub const POW_LIMIT_BITS: u32 = 0x1d00_ffff;

/// Compact "nBits" → 256-bit target (big-endian). Bitcoin `SetCompact` (non-negative).
pub fn target_from_compact(bits: u32) -> [u8; 32] {
    let exponent = (bits >> 24) as usize;
    let mantissa = bits & 0x007f_ffff;
    let mut target = [0u8; 32];
    if exponent <= 3 {
        let shifted = mantissa >> (8 * (3 - exponent));
        target[29] = (shifted >> 16) as u8;
        target[30] = (shifted >> 8) as u8;
        target[31] = shifted as u8;
    } else {
        let mant = [
            (mantissa >> 16) as u8,
            (mantissa >> 8) as u8,
            mantissa as u8,
        ];
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

/// 256-bit target (big-endian) → compact "nBits". Bitcoin `GetCompact` (non-negative).
pub fn compact_from_target(target: &[u8; 32]) -> u32 {
    // Number of significant bytes (size), and the top 3 as the mantissa.
    let first_nonzero = target.iter().position(|&b| b != 0).unwrap_or(32);
    let size = (32 - first_nonzero) as u32;
    if size == 0 {
        return 0;
    }
    let b0 = target[first_nonzero] as u32;
    let b1 = target.get(first_nonzero + 1).copied().unwrap_or(0) as u32;
    let b2 = target.get(first_nonzero + 2).copied().unwrap_or(0) as u32;
    let mut mantissa = (b0 << 16) | (b1 << 8) | b2;
    let mut nsize = size;
    // If the high mantissa bit is set it would look negative; shift right a byte and bump size.
    if mantissa & 0x0080_0000 != 0 {
        mantissa >>= 8;
        nsize += 1;
    }
    (nsize << 24) | (mantissa & 0x007f_ffff)
}

/// Multiply a 256-bit big-endian value by `m` (low 256 bits; overflow beyond 256 is dropped,
/// which never happens for real targets × clamped timespan).
fn mul_u64(a: &[u8; 32], m: u64) -> [u8; 32] {
    let mut res = [0u8; 32];
    let mut carry: u128 = 0;
    for i in (0..32).rev() {
        let prod = a[i] as u128 * m as u128 + carry;
        res[i] = (prod & 0xff) as u8;
        carry = prod >> 8;
    }
    res
}

/// Divide a 256-bit big-endian value by `d` (`d > 0`).
fn div_u64(a: &[u8; 32], d: u64) -> [u8; 32] {
    let mut res = [0u8; 32];
    let mut rem: u128 = 0;
    let d = d as u128;
    for i in 0..32 {
        let cur = (rem << 8) | a[i] as u128;
        res[i] = (cur / d) as u8;
        rem = cur % d;
    }
    res
}

/// Expected compact `bits` for the block at a retarget boundary — Bitcoin
/// `CalculateNextWorkRequired`. `last_bits`/`last_time` are the previous epoch's *last* block
/// (height `H-1`); `first_time` is its *first* block (height `H-2016`).
///
/// `new_target = clamp(actual/expected, ¼, 4) × old_target`, capped at the pow limit.
pub fn expected_retarget_bits(last_bits: u32, last_time: u32, first_time: u32) -> u32 {
    let mut actual = last_time.saturating_sub(first_time) as u64;
    actual = actual.clamp(TARGET_TIMESPAN / 4, TARGET_TIMESPAN * 4);
    let old = target_from_compact(last_bits);
    let mut new_t = div_u64(&mul_u64(&old, actual), TARGET_TIMESPAN);
    let limit = target_from_compact(POW_LIMIT_BITS);
    if new_t > limit {
        new_t = limit;
    }
    compact_from_target(&new_t)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compact_round_trips() {
        // Real historical bits values round-trip through target↔compact.
        for bits in [0x1d00_ffffu32, 0x1d00_d86a, 0x1b04_864c, 0x1903_a30c, 0x170e_2632] {
            let t = target_from_compact(bits);
            assert_eq!(compact_from_target(&t), bits, "round-trip {bits:#010x}");
        }
    }

    #[test]
    fn unchanged_when_timespan_is_exactly_expected() {
        // actual == expected → ratio 1 → target unchanged → same bits.
        let t = 1_700_000_000u32;
        let got = expected_retarget_bits(0x1903_a30c, t, t - TARGET_TIMESPAN as u32);
        assert_eq!(got, 0x1903_a30c);
    }

    #[test]
    fn faster_blocks_raise_difficulty() {
        // Half the expected time (too much hashpower) → target halves → harder.
        let bits = 0x1903_a30c;
        let t = 1_700_000_000u32;
        let faster = expected_retarget_bits(bits, t, t - (TARGET_TIMESPAN as u32 / 2));
        assert!(
            target_from_compact(faster) < target_from_compact(bits),
            "faster blocks must lower the target (raise difficulty)"
        );
    }

    #[test]
    fn slower_blocks_lower_difficulty() {
        let bits = 0x1903_a30c;
        let t = 1_700_000_000u32;
        let slower = expected_retarget_bits(bits, t, t - (TARGET_TIMESPAN as u32 * 2));
        assert!(
            target_from_compact(slower) > target_from_compact(bits),
            "slower blocks must raise the target (lower difficulty)"
        );
    }

    #[test]
    fn adjustment_is_clamped_to_four_x() {
        let bits = 0x1903_a30c;
        let t = 2_000_000_000u32;
        // Absurdly fast (≈0 timespan) clamps to ¼ → target = old/4, not 0.
        let clamped_hard = expected_retarget_bits(bits, t, t);
        let quarter = expected_retarget_bits(bits, t, t - (TARGET_TIMESPAN as u32 / 4));
        assert_eq!(clamped_hard, quarter, "below ¼ timespan clamps to ¼");
        // Absurdly slow clamps to 4×.
        let clamped_easy = expected_retarget_bits(bits, t, t - (TARGET_TIMESPAN as u32 * 10));
        let four_x = expected_retarget_bits(bits, t, t - (TARGET_TIMESPAN as u32 * 4));
        assert_eq!(clamped_easy, four_x, "above 4× timespan clamps to 4×");
    }

    #[test]
    fn matches_real_mainnet_retargets() {
        // Real mainnet retarget boundaries (data from blockstream.info). Each: the previous
        // epoch's last block (bits + time) and first block (time) must reproduce the exact
        // `bits` Bitcoin actually set at the boundary.
        // Boundary 840672 (= 417 × 2016):
        assert_eq!(
            expected_retarget_bits(0x1703_4219, 1_713_969_900, 1_712_783_853),
            0x1703_31db,
            "mainnet retarget at height 840672"
        );
    }

    #[test]
    fn never_easier_than_pow_limit() {
        // Even maximally-slow blocks can't push the target past difficulty-1.
        let t = 1_300_000_000u32;
        let got = expected_retarget_bits(POW_LIMIT_BITS, t, t - (TARGET_TIMESPAN as u32 * 100));
        assert_eq!(got, POW_LIMIT_BITS);
    }

    /// Differential test: the hand-rolled compact↔target conversion (the fiddly bignum part)
    /// must match rust-bitcoin byte-for-byte, across a wide sweep + real historical `bits`.
    /// rust-bitcoin is a DEV-dependency only — never shipped in the verifier.
    #[test]
    fn compact_target_matches_rust_bitcoin() {
        use bitcoin::{CompactTarget, Target};

        let mut bits_list: Vec<u32> = vec![
            0x1d00_ffff, // difficulty-1 (genesis)
            0x1d00_d86a, // first-ever retarget (block 32256)
            0x1b04_864c,
            0x1903_a30c,
            0x170e_2632,
            0x1715_a35c, // recent mainnet
        ];
        // Sweep the full valid exponent range (3..=32 bytes) × representative mantissas
        // (sign bit clear). Anything above 32 bytes is an overflow `bits` that never appears
        // in a real header and is rejected by the PoW + retarget checks regardless.
        for exp in 3u32..=0x20 {
            for mant in [0x00_0001u32, 0x00_8000, 0x12_3456, 0x7f_ffff, 0x00_ffff, 0x40_0000] {
                bits_list.push((exp << 24) | mant);
            }
        }

        for &bits in &bits_list {
            let mine = target_from_compact(bits);
            let theirs = Target::from_compact(CompactTarget::from_consensus(bits)).to_be_bytes();
            assert_eq!(mine, theirs, "from_compact mismatch for bits={bits:#010x}");

            // GetCompact on the resulting target must agree too.
            let mine_c = compact_from_target(&mine);
            let theirs_c = Target::from_be_bytes(theirs)
                .to_compact_lossy()
                .to_consensus();
            assert_eq!(mine_c, theirs_c, "to_compact mismatch for bits={bits:#010x}");
        }
    }
}
