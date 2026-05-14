use async_trait::async_trait;
use rfq_types::{AssetId, RgbInventoryUtxo, SwapTransfer};
use thiserror::Error;

mod lib_backend;
pub use lib_backend::LibRgbBackend;

/// 32-byte bitcoin transaction id (lowercase hex). Returned by `finalize_and_broadcast`
/// once the witness tx has been published via the configured indexer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WitnessTxid(pub String);

impl WitnessTxid {
    pub fn new(hex: impl Into<String>) -> Self {
        Self(hex.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
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
    #[error("failed to broadcast witness tx: {0}")]
    BroadcastFailed(String),
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

    /// Produce an unsigned PSBT and the accompanying consignment for a transfer
    /// matching `invoice` for `amount` smallest units. Signing happens outside
    /// this trait (in `rfq-wallet`); the returned PSBT is later handed to
    /// `finalize_and_broadcast`.
    async fn create_transfer(&self, invoice: &str, amount: u64) -> Result<SwapTransfer, RgbError>;

    /// Finalize a signed PSBT, extract the witness tx, and broadcast it via the
    /// configured indexer. Returns the witness txid for tracking the
    /// state-transition anchor on chain.
    async fn finalize_and_broadcast(
        &self,
        signed_psbt: &[u8],
    ) -> Result<WitnessTxid, RgbError>;
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

    async fn create_transfer(&self, invoice: &str, amount: u64) -> Result<SwapTransfer, RgbError> {
        self.validate_invoice(invoice).await?;

        Ok(SwapTransfer {
            partial_psbt: format!("mock-psbt-for-{amount}"),
            consignment: Some("mock-consignment".to_owned()),
            expected_witness_txid: None,
        })
    }

    async fn finalize_and_broadcast(
        &self,
        _signed_psbt: &[u8],
    ) -> Result<WitnessTxid, RgbError> {
        Ok(WitnessTxid::new(
            "0000000000000000000000000000000000000000000000000000000000000000",
        ))
    }
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
