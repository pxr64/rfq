//! Electrum-backed `BitcoinClient`. Gated behind the `electrum` feature so the
//! default build doesn't pull rustls or any networking dep.
//!
//! `broadcast`, `estimate_feerate`, and `block_height` are wired through.
//! `get_outpoint` is stubbed for 15b: the implementation needs a bitcoin tx
//! parser to extract a single output's `(value_sats, script_pubkey)` from a
//! raw transaction returned by `blockchain.transaction.get`. We deliberately
//! don't pull `bitcoin` or `bp-std` into this crate (see crate-level doc); the
//! parsing lands in 15c alongside the PSBT-build code that already imports
//! bp-std.
//!
//! The blocking `electrum-client` calls are wrapped in
//! `tokio::task::spawn_blocking` so the trait's `async fn` contract is honored
//! without holding a tokio worker thread on socket I/O.

use std::sync::Arc;

use async_trait::async_trait;
use electrum_client::{Client, ConfigBuilder, ElectrumApi};
use rfq_types::Outpoint;

use crate::{BitcoinClient, BtcError, TxOut};

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
    async fn get_outpoint(&self, _outpoint: &Outpoint) -> Result<TxOut, BtcError> {
        // 15c will implement: fetch raw tx via blockchain.transaction.get,
        // parse output at vout, return (value_sats, script_pubkey). Needs a
        // bitcoin-tx parser which lives behind the bp-std boundary in rfq-rgb.
        Err(BtcError::Backend(
            "ElectrumClient::get_outpoint not yet wired (#15c follow-up)".to_owned(),
        ))
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
