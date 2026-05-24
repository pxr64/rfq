//! Electrum-backed `BitcoinClient`. Gated behind the `electrum` feature so the
//! default build doesn't pull rustls or any networking dep.
//!
//! All five trait methods are wired through. `get_outpoint` reuses the
//! `bitcoin` types already vendored by `electrum-client` (via
//! `electrum_client::bitcoin::Transaction`) — we don't need a separate
//! bp-std/bitcoin dep at the rfq-btc layer since electrum-client carries
//! one transitively.
//!
//! The blocking `electrum-client` calls are wrapped in
//! `tokio::task::spawn_blocking` so the trait's `async fn` contract is honored
//! without holding a tokio worker thread on socket I/O.

use std::str::FromStr;
use std::sync::Arc;

use async_trait::async_trait;
use electrum_client::bitcoin::address::{Address, NetworkUnchecked};
use electrum_client::bitcoin::Txid;
use electrum_client::{Client, ConfigBuilder, ElectrumApi};
use rfq_types::Outpoint;

use crate::{is_segwit_script, BitcoinClient, BtcError, TxOut};

pub struct ElectrumClient {
    client: Arc<Client>,
}

impl ElectrumClient {
    /// Connect to an electrum server at `url` (e.g. `tcp://127.0.0.1:50001`
    /// or `ssl://electrum.example:50002`). Synchronous handshake — call from
    /// startup wiring, not from a request path.
    pub fn connect(url: &str) -> Result<Self, BtcError> {
        let config = ConfigBuilder::new().build();
        let client = Client::from_config(url, config)
            .map_err(|e| BtcError::Backend(format!("electrum connect: {e}")))?;
        Ok(Self {
            client: Arc::new(client),
        })
    }
}

#[async_trait]
impl BitcoinClient for ElectrumClient {
    async fn get_outpoint(&self, outpoint: &Outpoint) -> Result<TxOut, BtcError> {
        // `blockchain.transaction.get` returns the full witness tx; we just
        // grab the requested output. Reuses the `bitcoin` crate already
        // vendored by electrum-client (no new dep at this layer).
        let txid = Txid::from_str(&outpoint.txid)
            .map_err(|e| BtcError::Backend(format!("invalid txid {}: {e}", outpoint.txid)))?;
        let vout = outpoint.vout;

        let client = Arc::clone(&self.client);
        let tx = tokio::task::spawn_blocking(move || client.transaction_get(&txid))
            .await
            .map_err(|e| BtcError::Backend(format!("blocking task join: {e}")))?
            .map_err(|e| {
                BtcError::Backend(format!("electrum transaction_get {}: {e}", outpoint.txid))
            })?;

        let txout = tx.output.get(vout as usize).ok_or_else(|| {
            BtcError::OutpointNotFound(format!("vout {vout} out of bounds for {}", outpoint.txid))
        })?;

        let script_bytes = txout.script_pubkey.to_bytes();
        if !is_segwit_script(&script_bytes) {
            return Err(BtcError::NonSegwitOutpoint(format!(
                "{}:{vout}",
                outpoint.txid
            )));
        }

        Ok(TxOut {
            value_sats: txout.value.to_sat(),
            script_pubkey: script_bytes,
        })
    }

    async fn list_unspent(&self, address: &str) -> Result<Vec<(Outpoint, TxOut)>, BtcError> {
        // The taker's declared funding address is checked for the maker's
        // network at the protocol layer (accept_quote_buy), so the trait stays
        // network-naive and we assume_checked here.
        let addr = Address::<NetworkUnchecked>::from_str(address)
            .map_err(|e| BtcError::Backend(format!("invalid address: {e}")))?
            .assume_checked();
        let script = addr.script_pubkey();
        let script_bytes = script.to_bytes();

        let client = Arc::clone(&self.client);
        let utxos = tokio::task::spawn_blocking(move || client.script_list_unspent(&script))
            .await
            .map_err(|e| BtcError::Backend(format!("blocking task join: {e}")))?
            .map_err(|e| BtcError::Backend(format!("electrum script_list_unspent: {e}")))?;

        Ok(utxos
            .into_iter()
            .map(|u| {
                (
                    Outpoint::new(u.tx_hash.to_string(), u.tx_pos as u32),
                    TxOut {
                        value_sats: u.value,
                        script_pubkey: script_bytes.clone(),
                    },
                )
            })
            .collect())
    }

    async fn broadcast(&self, raw_tx: &[u8]) -> Result<String, BtcError> {
        let client = Arc::clone(&self.client);
        let tx = raw_tx.to_owned();
        tokio::task::spawn_blocking(move || client.transaction_broadcast_raw(&tx))
            .await
            .map_err(|e| BtcError::Backend(format!("blocking task join: {e}")))?
            .map(|txid| txid.to_string())
            .map_err(|e| BtcError::BroadcastFailed(e.to_string()))
    }

    async fn estimate_feerate(&self, target_blocks: u32) -> Result<u64, BtcError> {
        let client = Arc::clone(&self.client);
        let blocks = target_blocks as usize;
        let btc_per_kvbyte: f64 = tokio::task::spawn_blocking(move || client.estimate_fee(blocks))
            .await
            .map_err(|e| BtcError::Backend(format!("blocking task join: {e}")))?
            .map_err(|e| BtcError::Backend(format!("electrum estimate_fee: {e}")))?;

        // electrum returns BTC per kvbyte; convert to sat/vbyte.
        //   sat/vB = BTC/kvB × 100_000_000 sat/BTC ÷ 1000 vB/kvB = × 100_000.
        // Saturate at zero — electrum returns -1.0 when it can't estimate.
        let sat_per_vbyte = (btc_per_kvbyte * 100_000.0).round();
        if sat_per_vbyte.is_sign_negative() || !sat_per_vbyte.is_finite() {
            return Err(BtcError::Backend(
                "electrum returned no feerate estimate".to_owned(),
            ));
        }
        Ok(sat_per_vbyte as u64)
    }

    async fn block_height(&self) -> Result<u32, BtcError> {
        let client = Arc::clone(&self.client);
        let header = tokio::task::spawn_blocking(move || client.block_headers_subscribe_raw())
            .await
            .map_err(|e| BtcError::Backend(format!("blocking task join: {e}")))?
            .map_err(|e| BtcError::Backend(format!("electrum block_headers_subscribe: {e}")))?;
        Ok(header.height as u32)
    }
}
