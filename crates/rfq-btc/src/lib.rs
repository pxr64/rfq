//! Bitcoin chain-access adapter used by the maker during atomic-swap
//! settlement.
//!
//! See `docs/swap-flows.md` for context. The maker needs three pieces of
//! bitcoin information during a swap that the RGB layer can't supply on its
//! own:
//!
//! - **prevout data** (`get_outpoint`) — when the taker sends a consignment on
//!   sell side, the named RGB-bearing outpoints have to become PSBT inputs,
//!   which requires their `scriptPubKey` and `value_sats`. The consignment
//!   doesn't carry either.
//! - **broadcast** (`broadcast`) — the maker is the only broadcaster in both
//!   directions; this is the wire-out point.
//! - **feerate estimate** (`estimate_feerate`) — checked at PSBT-build time
//!   against `Quote.fee_slippage_bps`.
//!
//! Plus `block_height` for confirmation tracking by the cleanup loop.
//!
//! The crate intentionally surfaces raw `Vec<u8>` for `script_pubkey` and
//! `raw_tx` rather than typed bitcoin primitives. Two reasons: (1) we don't
//! want to pull `bp-std` / `bitcoin` into this crate so the broker can use
//! it without inheriting an RGB or full-node dep graph; (2) the parsing /
//! signing already happens behind the `rfq-rgb` boundary in 15c.

use async_trait::async_trait;
use rfq_types::Outpoint;
use thiserror::Error;

#[cfg(feature = "electrum")]
mod electrum;
#[cfg(feature = "electrum")]
pub use electrum::ElectrumClient;

#[async_trait]
pub trait BitcoinClient: Send + Sync {
    /// Confirmed or mempool prevout at `outpoint`. Rejects non-segwit outputs
    /// (atomic-swap PSBTs are segwit-only so the witness txid is committed
    /// once all inputs are present — see `docs/swap-flows.md`).
    async fn get_outpoint(&self, outpoint: &Outpoint) -> Result<TxOut, BtcError>;

    /// Unspent outputs at `address`. Used on the declared-funding buy side: the
    /// taker declares its BTC funding address in the ACCEPT, and the maker
    /// discovers the spendable UTXOs here. Each returned `TxOut` carries the
    /// address's `script_pubkey` (shared across results) plus the per-UTXO
    /// `value_sats`, so the pairs feed directly into coin selection + PSBT
    /// input enrichment. An address with no UTXOs returns `Ok(vec![])`.
    async fn list_unspent(&self, address: &str) -> Result<Vec<(Outpoint, TxOut)>, BtcError>;

    /// Broadcast a finalized (witness) transaction. Returns the witness txid
    /// as lowercase hex.
    async fn broadcast(&self, raw_tx: &[u8]) -> Result<String, BtcError>;

    /// Estimated feerate in sat/vbyte to confirm within `target_blocks`.
    async fn estimate_feerate(&self, target_blocks: u32) -> Result<u64, BtcError>;

    /// Current best block height.
    async fn block_height(&self) -> Result<u32, BtcError>;

    /// Confirmation status of `txid`: `confirmed = false` for a mempool or
    /// unknown tx; `height` is the block once mined. The broker's settlement
    /// confirmation loop polls this to flip `PendingBitcoinConfirm → Settled`.
    async fn tx_status(&self, txid: &str) -> Result<TxConfirmation, BtcError>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TxOut {
    pub value_sats: u64,
    pub script_pubkey: Vec<u8>,
}

/// On-chain status of a transaction. `height` is `Some` only when `confirmed`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TxConfirmation {
    pub confirmed: bool,
    pub height: Option<u32>,
}

#[derive(Debug, Error)]
pub enum BtcError {
    #[error("outpoint not found: {0}")]
    OutpointNotFound(String),
    #[error("non-segwit outpoint rejected: {0}")]
    NonSegwitOutpoint(String),
    #[error("broadcast failed: {0}")]
    BroadcastFailed(String),
    #[error("backend error: {0}")]
    Backend(String),
}

/// Return true for P2WPKH (`OP_0 <20>`), P2WSH (`OP_0 <32>`), and P2TR
/// (`OP_1 <32>`) outputs. Everything else (P2PKH, P2SH, P2PK, OP_RETURN,
/// non-standard) is rejected at the `BitcoinClient` boundary.
pub(crate) fn is_segwit_script(script: &[u8]) -> bool {
    match script {
        // P2WPKH: 0x00 0x14 <20 bytes> = 22 bytes
        [0x00, 0x14, rest @ ..] if rest.len() == 20 => true,
        // P2WSH: 0x00 0x20 <32 bytes> = 34 bytes
        [0x00, 0x20, rest @ ..] if rest.len() == 32 => true,
        // P2TR: 0x51 0x20 <32 bytes> = 34 bytes (segwit v1)
        [0x51, 0x20, rest @ ..] if rest.len() == 32 => true,
        _ => false,
    }
}

mod mock;
pub use mock::MockBitcoinClient;

#[cfg(test)]
mod tests {
    use super::*;

    fn p2wpkh() -> Vec<u8> {
        let mut v = vec![0x00, 0x14];
        v.extend(std::iter::repeat_n(0x42, 20));
        v
    }

    fn p2wsh() -> Vec<u8> {
        let mut v = vec![0x00, 0x20];
        v.extend(std::iter::repeat_n(0x42, 32));
        v
    }

    fn p2tr() -> Vec<u8> {
        let mut v = vec![0x51, 0x20];
        v.extend(std::iter::repeat_n(0x42, 32));
        v
    }

    fn p2pkh() -> Vec<u8> {
        // 0x76 0xa9 0x14 <20> 0x88 0xac = 25 bytes
        let mut v = vec![0x76, 0xa9, 0x14];
        v.extend(std::iter::repeat_n(0x42, 20));
        v.push(0x88);
        v.push(0xac);
        v
    }

    #[test]
    fn segwit_script_accepts_p2wpkh_p2wsh_p2tr() {
        assert!(is_segwit_script(&p2wpkh()));
        assert!(is_segwit_script(&p2wsh()));
        assert!(is_segwit_script(&p2tr()));
    }

    #[test]
    fn segwit_script_rejects_legacy_and_malformed() {
        assert!(!is_segwit_script(&p2pkh()));
        assert!(!is_segwit_script(&[]));
        // P2WPKH-shaped but wrong length
        assert!(!is_segwit_script(&[0x00, 0x14, 0x42]));
    }
}
