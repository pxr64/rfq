use async_trait::async_trait;
use rfq_types::{AssetId, Outpoint, RgbInventoryUtxo, SwapTransfer};
use thiserror::Error;

mod lib_backend;
pub use lib_backend::LibRgbBackend;

/// Output of `finalize_after_taker_sign`: the finalized witness transaction
/// ready to hand to `BitcoinClient::broadcast`, plus the witness txid and the
/// witness-extended consignment the RGB receiver imports post-broadcast.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FinalizedSwap {
    pub raw_tx: Vec<u8>,
    pub witness_txid: String,
    pub final_consignment_base64: String,
}

#[derive(Debug, Error)]
pub enum RgbError {
    #[error("invalid RGB invoice")]
    InvalidInvoice,
    #[error("failed to load stash: {0}")]
    StashLoad(String),
    #[error("contract not found in stash: {0}")]
    ContractNotFound(String),
    #[error("failed to build transfer: {0}")]
    TransferBuild(String),
    #[error("failed to finalize signed PSBT: {0}")]
    FinalizeFailed(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

#[async_trait]
pub trait RgbBackend: Send + Sync {
    /// Per-UTXO view of inventory. Each entry corresponds to one bitcoin
    /// outpoint holding an RGB allocation for `asset`.
    async fn list_inventory_utxos(
        &self,
        asset: &AssetId,
    ) -> Result<Vec<RgbInventoryUtxo>, RgbError>;

    async fn validate_invoice(&self, invoice: &str) -> Result<(), RgbError>;

    /// Buy-side swap PSBT construction. The maker contributes its RGB-bearing
    /// inputs (`maker_rgb_utxos`, the outpoints the inventory store reserved)
    /// and the RGB transition to `rgb_invoice`; the taker fills in BTC funding
    /// inputs at `/sign` time. Returns a `SwapTransfer` whose `partial_psbt` is
    /// maker-RGB-side-signed and whose `consignment` is `Some`.
    ///
    /// `expected_witness_txid` is `None` on buy side here — the witness txid
    /// isn't committed until the taker adds inputs and signs.
    async fn create_swap_psbt_buy(
        &self,
        rgb_invoice: &str,
        amount: u64,
        maker_rgb_utxos: &[Outpoint],
    ) -> Result<SwapTransfer, RgbError>;

    /// Finalize a fully-signed swap PSBT: extract the witness tx ready to
    /// broadcast, and emit the witness-extended consignment. Broadcasting
    /// itself is the caller's job (via `BitcoinClient::broadcast`) — this
    /// trait stays bitcoin-network-free.
    async fn finalize_after_taker_sign(
        &self,
        signed_psbt_base64: &str,
        original_consignment_base64: &str,
    ) -> Result<FinalizedSwap, RgbError>;
}

#[derive(Debug, Clone)]
pub struct MockRgbBackend {
    utxos: Vec<RgbInventoryUtxo>,
}

impl MockRgbBackend {
    pub fn new(utxos: Vec<RgbInventoryUtxo>) -> Self {
        Self { utxos }
    }
}

#[async_trait]
impl RgbBackend for MockRgbBackend {
    async fn list_inventory_utxos(
        &self,
        asset: &AssetId,
    ) -> Result<Vec<RgbInventoryUtxo>, RgbError> {
        Ok(self
            .utxos
            .iter()
            .filter(|utxo| utxo.asset_id == *asset)
            .cloned()
            .collect())
    }

    async fn validate_invoice(&self, invoice: &str) -> Result<(), RgbError> {
        if invoice.starts_with("rgb:") {
            Ok(())
        } else {
            Err(RgbError::InvalidInvoice)
        }
    }

    async fn create_swap_psbt_buy(
        &self,
        rgb_invoice: &str,
        amount: u64,
        maker_rgb_utxos: &[Outpoint],
    ) -> Result<SwapTransfer, RgbError> {
        self.validate_invoice(rgb_invoice).await?;

        // Deterministic mock PSBT: encodes the maker's RGB inputs + the
        // transition target so finalize_after_taker_sign can hash it into a
        // stable witness txid. Real bytes land with #13's LibRgbBackend.
        let inputs: Vec<String> = maker_rgb_utxos.iter().map(|o| o.to_string()).collect();
        let partial_psbt = format!(
            "mock-psbt:buy:invoice={rgb_invoice}:amount={amount}:rgb_in=[{}]",
            inputs.join(",")
        );
        Ok(SwapTransfer {
            partial_psbt,
            consignment: Some(format!("mock-consignment:buy:amount={amount}")),
            // Buy side: the taker still has to add BTC inputs, so the witness
            // txid isn't committed yet.
            expected_witness_txid: None,
        })
    }

    async fn finalize_after_taker_sign(
        &self,
        signed_psbt_base64: &str,
        original_consignment_base64: &str,
    ) -> Result<FinalizedSwap, RgbError> {
        if signed_psbt_base64.is_empty() {
            return Err(RgbError::FinalizeFailed("empty signed PSBT".to_owned()));
        }
        Ok(FinalizedSwap {
            raw_tx: signed_psbt_base64.as_bytes().to_vec(),
            witness_txid: mock_witness_txid(signed_psbt_base64),
            final_consignment_base64: format!("final:{original_consignment_base64}"),
        })
    }
}

/// Deterministic 64-hex mock txid derived from the signed PSBT string. Lets
/// tests assert a stable witness txid without a real bitcoin tx serializer.
fn mock_witness_txid(signed_psbt: &str) -> String {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    signed_psbt.hash(&mut hasher);
    let h = hasher.finish();
    format!("{h:064x}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use rfq_types::{AssetKind, BitcoinNetwork, Outpoint};

    fn asset(id: &str) -> AssetId {
        AssetId {
            network: BitcoinNetwork::Regtest,
            kind: AssetKind::Rgb20,
            id: id.to_owned(),
        }
    }

    fn utxo(asset: AssetId, idx: usize, amount: u64) -> RgbInventoryUtxo {
        RgbInventoryUtxo {
            outpoint: Outpoint {
                txid: format!("{idx:064x}"),
                vout: 0,
            },
            asset_id: asset,
            amount,
            btc_sats: 0,
        }
    }

    #[tokio::test]
    async fn mock_list_inventory_utxos_filters_by_asset() {
        let target = asset("rgb-target");
        let other = asset("rgb-other");
        let backend = MockRgbBackend::new(vec![
            utxo(target.clone(), 0, 100),
            utxo(other.clone(), 1, 50),
            utxo(target.clone(), 2, 200),
        ]);

        let utxos = backend.list_inventory_utxos(&target).await.unwrap();

        assert_eq!(utxos.len(), 2);
        assert_eq!(utxos[0].amount, 100);
        assert_eq!(utxos[1].amount, 200);
        for u in &utxos {
            assert_eq!(u.asset_id, target);
        }
    }

    #[tokio::test]
    async fn mock_list_inventory_utxos_is_deterministic() {
        let target = asset("rgb-target");
        let backend = MockRgbBackend::new(vec![
            utxo(target.clone(), 0, 1),
            utxo(target.clone(), 1, 2),
            utxo(target.clone(), 2, 3),
        ]);

        let first = backend.list_inventory_utxos(&target).await.unwrap();
        let second = backend.list_inventory_utxos(&target).await.unwrap();
        assert_eq!(first, second, "mock should be deterministic across calls");
    }
}
