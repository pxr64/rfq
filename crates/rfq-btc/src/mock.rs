use std::collections::HashMap;
use std::sync::Mutex;

use async_trait::async_trait;
use rfq_types::Outpoint;

use crate::{is_segwit_script, BitcoinClient, BtcError, TxOut};

/// In-memory test double. Backs a `HashMap` of seeded prevouts, captures
/// broadcasts, returns the configured feerate / height.
///
/// All state lives behind a `std::sync::Mutex` rather than `tokio::sync::Mutex`
/// because every operation is short and synchronous-by-nature; we don't want
/// awaiters holding a mutex across an `.await` (there isn't one). Each method
/// is `async` to satisfy the trait, but the bodies are non-blocking.
pub struct MockBitcoinClient {
    state: Mutex<MockState>,
}

#[derive(Default)]
struct MockState {
    prevouts: HashMap<Outpoint, TxOut>,
    broadcasts: Vec<Vec<u8>>,
    /// Feerate in sat/vbyte returned for any `target_blocks`. Single-knob mock;
    /// fine for v0 since the slippage check in 15c just needs a feerate to
    /// compare to the quote-time estimate.
    feerate_sat_per_vbyte: u64,
    block_height: u32,
    /// Optional injection point so tests can simulate `broadcast` failures.
    /// `Some(_)` makes the next broadcast call fail with the contained error
    /// message and then clears the slot.
    next_broadcast_error: Option<String>,
}

impl Default for MockBitcoinClient {
    fn default() -> Self {
        Self::new()
    }
}

impl MockBitcoinClient {
    pub fn new() -> Self {
        Self {
            state: Mutex::new(MockState {
                feerate_sat_per_vbyte: 5,
                block_height: 0,
                ..MockState::default()
            }),
        }
    }

    /// Builder-style seed for a prevout. Used by tests + by maker bootstrap
    /// in 16a once `BtcInventoryStore` is populated against a mock.
    pub fn with_prevout(self, outpoint: Outpoint, txout: TxOut) -> Self {
        self.state.lock().expect("mock state lock").prevouts.insert(outpoint, txout);
        self
    }

    pub fn with_feerate(self, sat_per_vbyte: u64) -> Self {
        self.state.lock().expect("mock state lock").feerate_sat_per_vbyte = sat_per_vbyte;
        self
    }

    pub fn with_block_height(self, height: u32) -> Self {
        self.state.lock().expect("mock state lock").block_height = height;
        self
    }

    /// Captured broadcasts in submission order. Lets tests assert the maker
    /// actually broadcast at the right point in the flow.
    pub fn broadcasts(&self) -> Vec<Vec<u8>> {
        self.state.lock().expect("mock state lock").broadcasts.clone()
    }

    /// Fail the next `broadcast` call. One-shot — auto-clears after firing.
    pub fn fail_next_broadcast(&self, reason: impl Into<String>) {
        self.state.lock().expect("mock state lock").next_broadcast_error = Some(reason.into());
    }
}

#[async_trait]
impl BitcoinClient for MockBitcoinClient {
    async fn get_outpoint(&self, outpoint: &Outpoint) -> Result<TxOut, BtcError> {
        let state = self.state.lock().expect("mock state lock");
        let txout = state
            .prevouts
            .get(outpoint)
            .ok_or_else(|| BtcError::OutpointNotFound(outpoint.to_string()))?
            .clone();
        if !is_segwit_script(&txout.script_pubkey) {
            return Err(BtcError::NonSegwitOutpoint(outpoint.to_string()));
        }
        Ok(txout)
    }

    async fn broadcast(&self, raw_tx: &[u8]) -> Result<String, BtcError> {
        let mut state = self.state.lock().expect("mock state lock");
        if let Some(reason) = state.next_broadcast_error.take() {
            return Err(BtcError::BroadcastFailed(reason));
        }
        state.broadcasts.push(raw_tx.to_owned());
        // Deterministic txid for tests — the real impl returns the bitcoin
        // double-sha256 of the serialized witness tx.
        Ok(format!("mock-wt-{:064x}", state.broadcasts.len() - 1))
    }

    async fn estimate_feerate(&self, _target_blocks: u32) -> Result<u64, BtcError> {
        Ok(self.state.lock().expect("mock state lock").feerate_sat_per_vbyte)
    }

    async fn block_height(&self) -> Result<u32, BtcError> {
        Ok(self.state.lock().expect("mock state lock").block_height)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p2wpkh() -> Vec<u8> {
        let mut v = vec![0x00, 0x14];
        v.extend(std::iter::repeat(0x42).take(20));
        v
    }

    fn p2pkh() -> Vec<u8> {
        let mut v = vec![0x76, 0xa9, 0x14];
        v.extend(std::iter::repeat(0x42).take(20));
        v.push(0x88);
        v.push(0xac);
        v
    }

    fn op(idx: u32) -> Outpoint {
        Outpoint::new(format!("{idx:064x}"), 0)
    }

    #[tokio::test]
    async fn get_outpoint_returns_seeded_segwit_prevout() {
        let txout = TxOut {
            value_sats: 100_000,
            script_pubkey: p2wpkh(),
        };
        let client = MockBitcoinClient::new().with_prevout(op(0), txout.clone());

        let got = client.get_outpoint(&op(0)).await.unwrap();
        assert_eq!(got, txout);
    }

    #[tokio::test]
    async fn get_outpoint_returns_not_found_for_unseeded() {
        let client = MockBitcoinClient::new();
        assert!(matches!(
            client.get_outpoint(&op(42)).await,
            Err(BtcError::OutpointNotFound(_))
        ));
    }

    #[tokio::test]
    async fn get_outpoint_rejects_non_segwit() {
        let client = MockBitcoinClient::new().with_prevout(
            op(0),
            TxOut {
                value_sats: 100_000,
                script_pubkey: p2pkh(),
            },
        );

        assert!(matches!(
            client.get_outpoint(&op(0)).await,
            Err(BtcError::NonSegwitOutpoint(_))
        ));
    }

    #[tokio::test]
    async fn broadcast_captures_tx_and_returns_deterministic_txid() {
        let client = MockBitcoinClient::new();
        let tx_a = vec![0xaa, 0xbb];
        let tx_b = vec![0xcc, 0xdd];

        let txid_a = client.broadcast(&tx_a).await.unwrap();
        let txid_b = client.broadcast(&tx_b).await.unwrap();

        assert_ne!(txid_a, txid_b);
        assert_eq!(client.broadcasts(), vec![tx_a, tx_b]);
    }

    #[tokio::test]
    async fn fail_next_broadcast_fires_once() {
        let client = MockBitcoinClient::new();
        client.fail_next_broadcast("simulated rpc failure");

        assert!(matches!(
            client.broadcast(&[0x01]).await,
            Err(BtcError::BroadcastFailed(_))
        ));
        // Failure was one-shot — second call succeeds.
        assert!(client.broadcast(&[0x02]).await.is_ok());
    }

    #[tokio::test]
    async fn feerate_returns_configured_value() {
        let client = MockBitcoinClient::new().with_feerate(42);
        assert_eq!(client.estimate_feerate(3).await.unwrap(), 42);
        // Default feerate.
        assert_eq!(MockBitcoinClient::new().estimate_feerate(3).await.unwrap(), 5);
    }

    #[tokio::test]
    async fn block_height_returns_configured_value() {
        let client = MockBitcoinClient::new().with_block_height(123_456);
        assert_eq!(client.block_height().await.unwrap(), 123_456);
    }
}
