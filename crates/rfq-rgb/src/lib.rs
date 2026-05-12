use async_trait::async_trait;
use rfq_types::{Allocation, AssetId, Outpoint, RgbInventoryUtxo, RgbTransfer};
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
    async fn list_allocations(&self, asset: &AssetId) -> Result<Vec<Allocation>, RgbError>;

    /// Per-UTXO view of inventory. Each entry corresponds to one bitcoin
    /// outpoint holding an RGB allocation for `asset`. Replaces the aggregated
    /// `list_allocations` view as the source of truth for the maker's
    /// inventory store; `list_allocations` will be removed in issue #14e once
    /// all callers migrate.
    async fn list_inventory_utxos(
        &self,
        asset: &AssetId,
    ) -> Result<Vec<RgbInventoryUtxo>, RgbError>;

    async fn validate_invoice(&self, invoice: &str) -> Result<(), RgbError>;

    /// Produce an unsigned PSBT and the accompanying consignment for a transfer
    /// matching `invoice` for `amount` smallest units. Signing happens outside
    /// this trait (in `rfq-wallet`); the returned PSBT is later handed to
    /// `finalize_and_broadcast`.
    async fn create_transfer(&self, invoice: &str, amount: u64) -> Result<RgbTransfer, RgbError>;

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
    allocations: Vec<Allocation>,
}

impl MockRgbBackend {
    pub fn new(allocations: Vec<Allocation>) -> Self {
        Self { allocations }
    }
}

#[async_trait]
impl RgbBackend for MockRgbBackend {
    async fn list_allocations(&self, asset: &AssetId) -> Result<Vec<Allocation>, RgbError> {
        Ok(self
            .allocations
            .iter()
            .filter(|allocation| allocation.asset == *asset)
            .cloned()
            .collect())
    }

    async fn list_inventory_utxos(
        &self,
        asset: &AssetId,
    ) -> Result<Vec<RgbInventoryUtxo>, RgbError> {
        Ok(self
            .allocations
            .iter()
            .enumerate()
            .filter(|(_, allocation)| allocation.asset == *asset)
            .map(|(idx, allocation)| RgbInventoryUtxo {
                outpoint: Outpoint {
                    txid: format!("{idx:064x}"),
                    vout: 0,
                },
                asset_id: allocation.asset.clone(),
                amount: allocation.available_amount,
                btc_sats: 0,
            })
            .collect())
    }

    async fn validate_invoice(&self, invoice: &str) -> Result<(), RgbError> {
        if invoice.starts_with("rgb:") {
            Ok(())
        } else {
            Err(RgbError::InvalidInvoice)
        }
    }

    async fn create_transfer(&self, invoice: &str, amount: u64) -> Result<RgbTransfer, RgbError> {
        self.validate_invoice(invoice).await?;

        Ok(RgbTransfer {
            psbt: format!("mock-psbt-for-{amount}"),
            consignment: "mock-consignment".to_owned(),
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
    use rfq_types::{AssetKind, BitcoinNetwork, MakerId};

    fn asset(id: &str) -> AssetId {
        AssetId {
            network: BitcoinNetwork::Regtest,
            kind: AssetKind::Rgb20,
            id: id.to_owned(),
        }
    }

    fn allocation(asset: AssetId, amount: u64) -> Allocation {
        Allocation {
            maker_id: MakerId("test-maker".to_owned()),
            asset,
            available_amount: amount,
        }
    }

    #[tokio::test]
    async fn mock_list_inventory_utxos_yields_one_per_seed_allocation() {
        let target = asset("rgb-target");
        let other = asset("rgb-other");
        let backend = MockRgbBackend::new(vec![
            allocation(target.clone(), 100),
            allocation(other.clone(), 50),
            allocation(target.clone(), 200),
        ]);

        let utxos = backend.list_inventory_utxos(&target).await.unwrap();

        assert_eq!(utxos.len(), 2);
        assert_eq!(utxos[0].amount, 100);
        assert_eq!(utxos[1].amount, 200);
        for utxo in &utxos {
            assert_eq!(utxo.asset_id, target);
            assert_eq!(utxo.outpoint.txid.len(), 64);
        }
    }

    #[tokio::test]
    async fn mock_outpoints_are_deterministic_and_unique() {
        let target = asset("rgb-target");
        let backend = MockRgbBackend::new(vec![
            allocation(target.clone(), 1),
            allocation(target.clone(), 2),
            allocation(target.clone(), 3),
        ]);

        let first = backend.list_inventory_utxos(&target).await.unwrap();
        let second = backend.list_inventory_utxos(&target).await.unwrap();
        assert_eq!(first, second, "mock should be deterministic across calls");

        let mut outpoints: Vec<_> = first.iter().map(|u| &u.outpoint).collect();
        outpoints.sort();
        outpoints.dedup();
        assert_eq!(outpoints.len(), 3, "all outpoints should be unique");
    }

    #[tokio::test]
    async fn mock_list_allocations_matches_inventory_utxo_totals() {
        let target = asset("rgb-target");
        let backend = MockRgbBackend::new(vec![
            allocation(target.clone(), 100),
            allocation(target.clone(), 250),
        ]);

        let allocations = backend.list_allocations(&target).await.unwrap();
        let utxos = backend.list_inventory_utxos(&target).await.unwrap();

        let allocations_total: u64 = allocations.iter().map(|a| a.available_amount).sum();
        let utxos_total: u64 = utxos.iter().map(|u| u.amount).sum();
        assert_eq!(allocations_total, utxos_total);
    }
}
