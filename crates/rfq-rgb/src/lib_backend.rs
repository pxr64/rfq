use std::path::PathBuf;
use std::str::FromStr;

use async_trait::async_trait;
use rfq_types::{Allocation, AssetId, MakerId, RgbTransfer};

use rgb::contract::FilterIncludeAll;
use rgb::invoice::RgbInvoice;
use rgb::persistence::Stock;
use rgb::persistence::fs::FsBinStore;
use rgb::ContractId;

use crate::{RgbBackend, RgbError, WitnessTxid};

/// Library-backed `RgbBackend` implementation. Talks directly to `rgb-api` /
/// `rgb-ops` (the same libraries `rgb-cmd` is built on) — no subprocess.
///
/// Stashes follow the rgb-cmd convention: `<data_dir>/<network>/...`. So
/// existing stashes created by `make rgb-wallets-init` + `rgb_<role> create
/// --wpkh ...` are reusable without re-import.
pub struct LibRgbBackend {
    data_dir: PathBuf,
    #[allow(dead_code)] // reserved for upcoming create_transfer + finalize work
    wallet_name: String,
    network: String,
    #[allow(dead_code)] // reserved for finalize_and_broadcast (electrum resolver)
    electrum_url: String,
    maker_id: MakerId,
}

impl LibRgbBackend {
    pub fn new(
        data_dir: PathBuf,
        wallet_name: String,
        network: String,
        electrum_url: String,
        maker_id: MakerId,
    ) -> Self {
        Self {
            data_dir,
            wallet_name,
            network,
            electrum_url,
            maker_id,
        }
    }

    fn stock_path(&self) -> PathBuf {
        self.data_dir.join(&self.network)
    }

    fn load_stock(&self) -> Result<Stock, RgbError> {
        let provider = FsBinStore::new(self.stock_path())
            .map_err(|e| RgbError::StashLoad(e.to_string()))?;
        Stock::load(provider, true).map_err(|e| RgbError::StashLoad(e.to_string()))
    }
}

#[async_trait]
impl RgbBackend for LibRgbBackend {
    async fn list_allocations(&self, asset: &AssetId) -> Result<Vec<Allocation>, RgbError> {
        let stock = self.load_stock()?;
        let contract_id = ContractId::from_str(&asset.id)
            .map_err(|e| RgbError::ContractNotFound(format!("invalid contract id: {e}")))?;
        let contract = stock
            .contract_data(contract_id)
            .map_err(|e| RgbError::ContractNotFound(e.to_string()))?;

        let mut allocations = Vec::new();
        let filter = FilterIncludeAll;
        for details in contract.schema.owned_types.values() {
            if let Ok(rgb_allocations) = contract.fungible(details.name.clone(), &filter) {
                for alloc in rgb_allocations {
                    allocations.push(Allocation {
                        maker_id: self.maker_id.clone(),
                        asset: asset.clone(),
                        available_amount: alloc.state.value(),
                    });
                }
            }
        }
        Ok(allocations)
    }

    async fn validate_invoice(&self, invoice: &str) -> Result<(), RgbError> {
        RgbInvoice::from_str(invoice)
            .map(|_| ())
            .map_err(|_| RgbError::InvalidInvoice)
    }

    async fn create_transfer(
        &self,
        _invoice: &str,
        _amount: u64,
    ) -> Result<RgbTransfer, RgbError> {
        // TODO(#13): implement via rgb-api's TransferBuilder + WalletProvider::pay
        // (mirrors Command::Transfer in rgb-cmd 0.11.1-rc.6 command.rs).
        // For now, callers should use the manual rgb-cmd flow described in
        // docs/regtest-rgb20-nia-dev-infra.md.
        Err(RgbError::TransferBuild(
            "LibRgbBackend::create_transfer is not implemented yet (issue #13); \
             use the manual rgb-cmd flow for now"
                .to_owned(),
        ))
    }

    async fn finalize_and_broadcast(
        &self,
        _signed_psbt: &[u8],
    ) -> Result<WitnessTxid, RgbError> {
        // TODO(#13): finalize PSBT with bp-std, extract tx, broadcast via
        // rgb::resolvers::AnyResolver against self.electrum_url.
        Err(RgbError::BroadcastFailed(
            "LibRgbBackend::finalize_and_broadcast is not implemented yet (issue #13)"
                .to_owned(),
        ))
    }
}
