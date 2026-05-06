use async_trait::async_trait;
use rfq_types::{Allocation, AssetId, RgbTransfer};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum RgbError {
    #[error("invalid RGB invoice")]
    InvalidInvoice,
}

#[async_trait]
pub trait RgbBackend: Send + Sync {
    async fn list_allocations(&self, asset: &AssetId) -> Result<Vec<Allocation>, RgbError>;

    async fn validate_invoice(&self, invoice: &str) -> Result<(), RgbError>;

    async fn create_transfer(&self, invoice: &str, amount: u64) -> Result<RgbTransfer, RgbError>;
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
}
