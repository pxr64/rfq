use async_trait::async_trait;
use rfq_types::{Allocation, AssetId, RgbTransfer};
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
