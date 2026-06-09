//! Production taker counterparty for BTC↔RGB atomic swaps.
//!
//! The maker side is [`LibRgbBackend`] (the `RgbBackend` trait); the taker is
//! the counterparty that owns BTC inputs (buy) or RGB inputs (sell), mints its
//! own RGB invoice, builds its sell consignment, and applies signatures to the
//! maker-built PSBT. This logic used to live only in the test harness
//! (`test_helpers::TakerGuard`); it's promoted here so non-test drivers (a
//! future client wallet) can reuse it. `TakerGuard` now delegates to `Taker`.
//!
//! `Taker`'s read paths reuse `LibRgbBackend` (role-agnostic). The bp-wallet
//! ops (`lookup_prevout`, `spare_btc_input`, `sign_and_finalize`) load the
//! taker's own wallet cache directly, since signing a maker-built PSBT is the
//! counterparty's job, not part of the maker-side `RgbBackend` abstraction.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::str::FromStr;

use bpstd::psbt::{Psbt, PsbtConstructor};
use bpstd::signers::TestnetRefSigner;
use bpstd::{Derive, Sats, Terminal, XprivAccount, XpubDerivable};
use bpwallet::fs::FsTextStore;
use bpwallet::hot::SecureIo;
use bpwallet::Wallet as BpWallet;
use rfq_types::{AssetId, Outpoint, RgbInventoryUtxo};
use rgb::{ContractId, RgbDescr};

use crate::{enrich_psbt_input, LibRgbBackend, RgbBackend, RgbError, TxOut};

/// A swap counterparty's RGB/BTC wallet. Constructed from the same on-disk
/// stash + signer inputs as [`LibRgbBackend`]; the wallet cache lives at
/// `<data_dir>/<network>/<wallet_name>` (rgb-cmd layout).
pub struct Taker {
    data_dir: PathBuf,
    wallet_name: String,
    network: String,
    electrum_url: String,
    account_file: PathBuf,
    signer_password: String,
}

impl Taker {
    pub fn new(
        data_dir: PathBuf,
        wallet_name: String,
        network: String,
        electrum_url: String,
        account_file: PathBuf,
        signer_password: String,
    ) -> Self {
        Self {
            data_dir,
            wallet_name,
            network,
            electrum_url,
            account_file,
            signer_password,
        }
    }

    /// Role-agnostic read/transfer backend over the taker's own stash.
    fn lib_backend(&self) -> LibRgbBackend {
        LibRgbBackend::new(
            self.data_dir.clone(),
            self.wallet_name.clone(),
            self.network.clone(),
            self.electrum_url.clone(),
            self.account_file.clone(),
            self.signer_password.clone(),
        )
    }

    fn wallet_path(&self) -> PathBuf {
        self.data_dir.join(&self.network).join(&self.wallet_name)
    }

    fn load_wallet(&self) -> Result<BpWallet<XpubDerivable, RgbDescr>, RgbError> {
        let provider = FsTextStore::new(self.wallet_path())
            .map_err(|e| RgbError::StashLoad(format!("taker wallet provider: {e}")))?;
        BpWallet::load(provider, true)
            .map_err(|e| RgbError::StashLoad(format!("taker wallet load: {e}")))
    }

    /// The taker's RGB inventory for `asset`.
    pub async fn inventory(&self, asset: &AssetId) -> Result<Vec<RgbInventoryUtxo>, RgbError> {
        self.lib_backend().list_inventory_utxos(asset).await
    }

    /// The contract's ticker + precision (decimals), for formatting inventory.
    pub fn contract_spec(&self, asset: &AssetId) -> Result<(String, u8), RgbError> {
        self.lib_backend().contract_spec(asset)
    }

    /// Mint a fresh RGB invoice on the taker's stash (buy: receive bought
    /// tokens; sell: receive over-consigned change).
    pub async fn create_invoice(&self, asset: &AssetId, amount: u64) -> Result<String, RgbError> {
        self.lib_backend().create_invoice(asset, amount).await
    }

    /// Refresh the taker's bp-wallet UTXO cache against electrum so
    /// post-broadcast change becomes visible to `spare_btc_input` /
    /// `lookup_prevout` / `sign_and_finalize`.
    pub async fn sync_wallet(&self) -> Result<(), RgbError> {
        self.lib_backend().sync_wallet().await
    }

    /// Absorb a maker-returned consignment into the taker's stash so the RGB it
    /// just received becomes visible: tokens bought (buy) or change from a sell.
    /// The maker hands this back as `SettlementIntent.final_consignment` after
    /// broadcasting the swap tx. The allocation surfaces in [`Self::inventory`]
    /// once the witness confirms and [`Self::sync_wallet`] refreshes the cache.
    pub async fn accept_consignment(
        &self,
        asset: &AssetId,
        consignment_base64: &str,
    ) -> Result<(), RgbError> {
        let contract_id = ContractId::from_str(&asset.id)
            .map_err(|e| RgbError::ContractNotFound(format!("invalid contract id: {e}")))?;
        self.lib_backend()
            .accept_incoming_transfer(consignment_base64, contract_id)
            .await
    }

    /// Build the taker's sell consignment: a unilateral RGB transfer to the
    /// maker's invoice. The taker does NOT broadcast this — the maker anchors
    /// it into the swap tx and broadcasts. See
    /// [`LibRgbBackend::create_transfer_to_invoice`].
    pub async fn create_transfer_to_invoice(
        &self,
        recipient_invoice: &str,
        fee_sats: u64,
    ) -> Result<String, RgbError> {
        self.lib_backend()
            .create_transfer_to_invoice(recipient_invoice, fee_sats)
            .await
    }

    /// Export a **provenance** consignment for the taker's own `outpoints` — the
    /// sell-leg primitive: the taker proves its RGB + history, the maker spends
    /// those outpoints into the swap tx. No PSBT, no fee, no anchor. See
    /// [`LibRgbBackend::export_provenance`] and
    /// `docs/provenance-consignment-proposal.md`.
    pub fn export_provenance(
        &self,
        contract: &str,
        outpoints: &[String],
    ) -> Result<String, RgbError> {
        self.lib_backend().export_provenance(contract, outpoints)
    }

    /// Look up a specific taker UTXO and return its `(Outpoint, TxOut)`. Used
    /// to resolve the RGB-input prevouts a validated sell consignment reports.
    pub fn lookup_prevout(&self, want: &Outpoint) -> Result<(Outpoint, TxOut), RgbError> {
        let wallet = self.load_wallet()?;
        let utxo = wallet
            .utxos()
            .find(|u| {
                u.outpoint.txid.to_string() == want.txid && u.outpoint.vout.into_u32() == want.vout
            })
            .ok_or_else(|| {
                RgbError::TransferBuild(format!("taker outpoint {want} not in wallet"))
            })?;
        let derived = wallet
            .descriptor()
            .derive(utxo.terminal.keychain, utxo.terminal.index)
            .next()
            .ok_or_else(|| RgbError::TransferBuild("descriptor produced no script".to_owned()))?;
        let spk: Vec<u8> = derived.to_script_pubkey().as_slice().to_vec();
        Ok((
            want.clone(),
            TxOut {
                value_sats: utxo.value.sats(),
                script_pubkey: spk,
            },
        ))
    }

    /// Any taker-controlled UTXO that does **not** carry an RGB allocation for
    /// `asset`, paired with its prevout `TxOut`. Used as `taker_btc_inputs[0]`
    /// for the buy-side swap PSBT — must skip RGB-bearing outpoints so the swap
    /// tx doesn't silently consume the taker's RGB at the bitcoin layer.
    pub async fn spare_btc_input(&self, asset: &AssetId) -> Result<(Outpoint, TxOut), RgbError> {
        let rgb_utxos = self.inventory(asset).await?;
        let rgb_set: HashSet<(String, u32)> = rgb_utxos
            .iter()
            .map(|u| (u.outpoint.txid.clone(), u.outpoint.vout))
            .collect();

        let wallet = self.load_wallet()?;
        let descriptor = wallet.descriptor().clone();
        let utxo = wallet
            .utxos()
            .find(|u| {
                let key = (u.outpoint.txid.to_string(), u.outpoint.vout.into_u32());
                !rgb_set.contains(&key)
            })
            .ok_or_else(|| {
                RgbError::TransferBuild(
                    "no spare taker BTC outpoint — every wallet UTXO is RGB-bearing".to_owned(),
                )
            })?;
        let derived = descriptor
            .derive(utxo.terminal.keychain, utxo.terminal.index)
            .next()
            .ok_or_else(|| RgbError::TransferBuild("descriptor produced no script".to_owned()))?;
        let spk: Vec<u8> = derived.to_script_pubkey().as_slice().to_vec();
        Ok((
            Outpoint {
                txid: utxo.outpoint.txid.to_string(),
                vout: utxo.outpoint.vout.into_u32(),
            },
            TxOut {
                value_sats: utxo.value.sats(),
                script_pubkey: spk,
            },
        ))
    }

    /// Enrich every PSBT input owned by the taker wallet, then sign + finalize
    /// against the taker descriptor. Inputs the taker doesn't own (the maker's,
    /// already carrying `partial_sigs`) are left untouched for the maker's own
    /// finalize step. Enrichment is mandatory: the maker leaves taker inputs
    /// with only `witness_utxo` + `sighash_type`, but `TestnetRefSigner` keys
    /// off `bip32_derivation`.
    pub fn sign_and_finalize(&self, partial_psbt_b64: &str) -> Result<String, RgbError> {
        let wallet = self.load_wallet()?;
        let descriptor = wallet.descriptor().clone();
        let owned: HashMap<(String, u32), (Terminal, Sats)> = wallet
            .utxos()
            .map(|u| {
                (
                    (u.outpoint.txid.to_string(), u.outpoint.vout.into_u32()),
                    (u.terminal, u.value),
                )
            })
            .collect();

        let account = XprivAccount::read(&self.account_file, &self.signer_password)
            .map_err(|e| RgbError::StashLoad(format!("taker account read: {e}")))?;
        let mut psbt = Psbt::from_base64(partial_psbt_b64)
            .map_err(|e| RgbError::TransferBuild(format!("decode partial PSBT: {e}")))?;

        let prev_outs: Vec<(usize, String, u32)> = psbt
            .inputs()
            .enumerate()
            .map(|(i, inp)| {
                (
                    i,
                    inp.previous_outpoint.txid.to_string(),
                    inp.previous_outpoint.vout.into_u32(),
                )
            })
            .collect();
        let mut enriched = 0usize;
        for (i, txid, vout) in prev_outs {
            if let Some((terminal, value)) = owned.get(&(txid, vout)).cloned() {
                // Taker inputs are its own untweaked receives/funding (only the
                // maker hosts tapret commitments), so the base derived script is
                // correct — no scriptPubkey match needed.
                enrich_psbt_input(&mut psbt, i, &descriptor, terminal, value, None)?;
                enriched += 1;
            }
        }
        if enriched == 0 {
            return Err(RgbError::TransferBuild(
                "no PSBT input matched a taker UTXO; check funding".to_owned(),
            ));
        }

        let signer = TestnetRefSigner::new(&account);
        let sig_count = psbt
            .sign(&signer)
            .map_err(|e| RgbError::TransferBuild(format!("taker sign: {e}")))?;
        if sig_count == 0 {
            return Err(RgbError::TransferBuild(
                "taker signer matched no inputs after enrichment".to_owned(),
            ));
        }
        psbt.finalize(&descriptor);
        Ok(psbt.to_base64())
    }
}
