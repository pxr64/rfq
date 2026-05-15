use std::path::PathBuf;
use std::str::FromStr;

use async_trait::async_trait;
use rfq_types::{AssetId, Outpoint, RgbInventoryUtxo, SwapTransfer};

use rgb::contract::FilterIncludeAll;
use rgb::invoice::RgbInvoice;
use rgb::persistence::Stock;
use rgb::persistence::fs::FsBinStore;
use rgb::ContractId;

use crate::{FinalizedSwap, RgbBackend, RgbError};

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
}

impl LibRgbBackend {
    pub fn new(
        data_dir: PathBuf,
        wallet_name: String,
        network: String,
        electrum_url: String,
    ) -> Self {
        Self {
            data_dir,
            wallet_name,
            network,
            electrum_url,
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
    async fn list_inventory_utxos(
        &self,
        asset: &AssetId,
    ) -> Result<Vec<RgbInventoryUtxo>, RgbError> {
        let stock = self.load_stock()?;
        let contract_id = ContractId::from_str(&asset.id)
            .map_err(|e| RgbError::ContractNotFound(format!("invalid contract id: {e}")))?;
        let contract = stock
            .contract_data(contract_id)
            .map_err(|e| RgbError::ContractNotFound(e.to_string()))?;

        let mut utxos = Vec::new();
        let filter = FilterIncludeAll;
        for details in contract.schema.owned_types.values() {
            if let Ok(rgb_allocations) = contract.fungible(details.name.clone(), &filter) {
                for alloc in rgb_allocations {
                    let op = alloc.seal.to_outpoint();
                    utxos.push(RgbInventoryUtxo {
                        outpoint: Outpoint {
                            txid: op.txid.to_string(),
                            vout: op.vout.into_u32(),
                        },
                        asset_id: asset.clone(),
                        amount: alloc.state.value(),
                        btc_sats: 0,
                    });
                }
            }
        }
        Ok(utxos)
    }

    async fn validate_invoice(&self, invoice: &str) -> Result<(), RgbError> {
        RgbInvoice::from_str(invoice)
            .map(|_| ())
            .map_err(|_| RgbError::InvalidInvoice)
    }

    async fn create_swap_psbt_buy(
        &self,
        _rgb_invoice: &str,
        _amount: u64,
        _maker_rgb_utxos: &[Outpoint],
    ) -> Result<SwapTransfer, RgbError> {
        // TODO(#13): build the swap PSBT via rgb-api's TransferBuilder +
        // bp-std PSBT construction (mirrors Command::Transfer in rgb-cmd
        // 0.11.1-rc.6 command.rs). For now, callers should use MockRgbBackend
        // or the manual rgb-cmd flow in docs/regtest-rgb20-nia-dev-infra.md.
        Err(RgbError::TransferBuild(
            "LibRgbBackend::create_swap_psbt_buy is not implemented yet (issue #13); \
             use MockRgbBackend for now"
                .to_owned(),
        ))
    }

    async fn finalize_after_taker_sign(
        &self,
        _signed_psbt_base64: &str,
        _original_consignment_base64: &str,
    ) -> Result<FinalizedSwap, RgbError> {
        // TODO(#13): finalize the PSBT with bp-std, extract the witness tx,
        // emit the witness-extended consignment.
        Err(RgbError::FinalizeFailed(
            "LibRgbBackend::finalize_after_taker_sign is not implemented yet (issue #13)"
                .to_owned(),
        ))
    }
}
