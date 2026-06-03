//! `colorex-taker` — a reusable taker driver for BTC↔RGB swaps.
//!
//! The taker is the swap counterparty: it owns BTC (buy) or RGB (sell), mints
//! its own invoices, builds sell consignments, and signs the maker-built PSBT.
//! This binary wires the library `rfq_rgb::Taker` to an `rfq_client::RfqClient`
//! pointed at a broker, mirroring the canonical sequence in
//! `crates/maker-node/tests/broker_round_trip.rs`.
//!
//! ```text
//! colorex-taker --config taker.toml buy 100
//! colorex-taker --config taker.toml sell 50
//! ```

use std::path::PathBuf;

use clap::{Parser, Subcommand};
use rfq_client::{RfqClient, Url};
use rfq_rgb::Taker;
use rfq_types::{
    AcceptQuoteRequest, AssetId, AssetKind, BitcoinNetwork, CreateRfqRequest, Side, SwapLeg,
};
use serde::Deserialize;

#[derive(Debug, Parser)]
#[command(name = "colorex-taker", version, about = "RGB RFQ taker driver")]
struct Cli {
    /// Path to the taker config TOML.
    #[arg(long, default_value = "taker.toml")]
    config: PathBuf,
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Buy `amount` RGB, paying BTC.
    Buy { amount: u64 },
    /// Sell `amount` RGB, receiving BTC.
    Sell { amount: u64 },
    /// Accept a swap consignment file into the taker's stash (records bought
    /// RGB / sell change). Run this after the swap tx confirms — `buy`/`sell`
    /// write the consignment to a file and print its path.
    Accept { path: PathBuf },
    /// Print the taker's RGB inventory for the configured contract.
    Inventory,
}

#[tokio::main]
async fn main() {
    if let Err(e) = run().await {
        eprintln!("colorex-taker error: {e}");
        std::process::exit(1);
    }
}

async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();
    let config = TakerConfig::load(&cli.config)?;
    let taker = config.taker();
    let asset = config.rgb_asset();
    let client = RfqClient::new(Url::parse(&config.broker_url)?);

    match cli.command {
        Command::Buy { amount } => buy(&client, &taker, &asset, amount, &config.btc_address).await,
        Command::Sell { amount } => {
            sell(&client, &taker, &asset, amount, &config.btc_address).await
        }
        Command::Accept { path } => {
            let consignment = std::fs::read_to_string(&path)
                .map_err(|e| format!("read consignment {}: {e}", path.display()))?;
            taker.accept_consignment(&asset, consignment.trim()).await?;
            println!("accepted consignment from {} into taker stash", path.display());
            Ok(())
        }
        Command::Inventory => {
            let utxos = taker.inventory(&asset).await?;
            println!("taker inventory for {}:", asset.id);
            let mut total = 0u64;
            for u in &utxos {
                println!("  {}:{}  amount={}", u.outpoint.txid, u.outpoint.vout, u.amount);
                total += u.amount;
            }
            println!("total={total} across {} allocation(s)", utxos.len());
            Ok(())
        }
    }
}

/// Buy `amount` RGB through the broker. Mirrors `drive_buy_via_broker`.
async fn buy(
    client: &RfqClient,
    taker: &Taker,
    asset: &AssetId,
    amount: u64,
    btc_address: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    // Refresh the wallet so BTC funding UTXOs are visible to the maker's
    // `list_unspent(btc_funding_addr)` and to our own signer.
    taker.sync_wallet().await?;

    let rgb_invoice = taker.create_invoice(asset, amount).await?;
    let btc_funding_addr = btc_address.to_owned();

    let quotes = client
        .request_quotes(CreateRfqRequest {
            base_asset: asset.clone(),
            quote_asset: btc_asset(),
            side: Side::Buy,
            amount,
        })
        .await?;
    let quote = quotes
        .into_iter()
        .next()
        .ok_or("no maker quoted the buy")?;
    println!("quote {} price={} sats fee~{} sats", quote.quote_id.0, quote.price, quote.estimated_fee_sats);

    let accepted = client
        .accept_quote(AcceptQuoteRequest {
            quote_id: quote.quote_id.clone(),
            leg: SwapLeg::Buy {
                rgb_invoice,
                btc_funding_addr,
            },
        })
        .await?;
    let transfer = accepted
        .transfer
        .ok_or("buy accept did not return a partial PSBT")?;

    let signed = taker.sign_and_finalize(&transfer.partial_psbt)?;
    let settled = client.submit_signed_psbt(quote.quote_id, signed).await?;
    let txid = settled
        .witness_txid
        .ok_or("buy settled intent carried no witness txid")?;
    println!("buy broadcast: {txid}");

    if let Some(consignment) = settled.final_consignment {
        persist_and_try_accept(taker, asset, &txid, &consignment, "bought RGB").await;
    }
    Ok(())
}

/// Sell `amount` RGB through the broker. Mirrors `drive_sell_via_broker`.
async fn sell(
    client: &RfqClient,
    taker: &Taker,
    asset: &AssetId,
    amount: u64,
    btc_address: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    taker.sync_wallet().await?;

    let quotes = client
        .request_quotes(CreateRfqRequest {
            base_asset: asset.clone(),
            quote_asset: btc_asset(),
            side: Side::Sell,
            amount,
        })
        .await?;
    let quote = quotes
        .into_iter()
        .next()
        .ok_or("no maker quoted the sell")?;
    let maker_rgb_invoice = quote
        .maker_rgb_invoice
        .clone()
        .ok_or("sell quote carried no maker_rgb_invoice")?;
    println!("quote {} price={} sats fee~{} sats", quote.quote_id.0, quote.price, quote.estimated_fee_sats);

    // Change invoice for surplus RGB (taker's input usually exceeds `amount`).
    // The maker reads only the beneficiary seal off it; the amount is ignored.
    let rgb_change_invoice = taker.create_invoice(asset, amount).await?;
    let btc_payout_addr = btc_address.to_owned();

    let accepted = client
        .accept_quote(AcceptQuoteRequest {
            quote_id: quote.quote_id.clone(),
            leg: SwapLeg::Sell {
                btc_payout_addr,
                rgb_change_invoice: Some(rgb_change_invoice),
            },
        })
        .await?;
    if accepted.transfer.is_some() {
        return Err("sell accept unexpectedly returned a PSBT before consignment".into());
    }

    // Build the consignment in-process (unilateral RGB transfer to the maker's
    // invoice); the maker anchors it into the swap tx and broadcasts.
    let consignment = taker
        .create_transfer_to_invoice(&maker_rgb_invoice, SELL_RGB_FEE_SATS)
        .await?;

    let delivered = client
        .submit_consignment(quote.quote_id.clone(), consignment)
        .await?;
    let transfer = delivered
        .transfer
        .ok_or("consignment delivery did not return a partial PSBT")?;

    let signed = taker.sign_and_finalize(&transfer.partial_psbt)?;
    let settled = client.submit_signed_psbt(quote.quote_id, signed).await?;
    let txid = settled
        .witness_txid
        .ok_or("sell settled intent carried no witness txid")?;
    println!("sell broadcast: {txid}");

    // Absent when the taker consigned its inputs exactly (no change to receive).
    if let Some(consignment) = settled.final_consignment {
        persist_and_try_accept(taker, asset, &txid, &consignment, "RGB change").await;
    }
    Ok(())
}

/// Persist a swap consignment to a file and try to accept it now. The accept is
/// best-effort: RGB acceptance needs the swap witness confirmed, so right after
/// broadcast (still in mempool) it may not stick. The saved file lets the taker
/// re-accept with `colorex-taker accept <path>` once the tx confirms.
async fn persist_and_try_accept(
    taker: &Taker,
    asset: &AssetId,
    txid: &str,
    consignment: &str,
    what: &str,
) {
    let path = format!("taker-consignment-{txid}.b64");
    match std::fs::write(&path, consignment) {
        Ok(()) => {
            println!("consignment saved to {path} (accept after confirm: colorex-taker accept {path})")
        }
        Err(e) => eprintln!("warning: could not save consignment to {path}: {e}"),
    }
    match taker.accept_consignment(asset, consignment).await {
        Ok(()) => println!("accepted {what} into taker stash"),
        Err(e) => eprintln!("note: immediate accept failed (re-run accept after the swap confirms): {e}"),
    }
}

/// Fee (sats) for the taker's in-process sell consignment transfer. This tx is
/// a phantom: the maker discards it and rebuilds the real swap tx (with its own
/// fee), so this only has to let `wallet.pay` balance — keep it small so a
/// minimally-funded witness-vout RGB input still leaves room for a change output.
const SELL_RGB_FEE_SATS: u64 = 200;

fn btc_asset() -> AssetId {
    AssetId {
        network: BitcoinNetwork::Regtest,
        kind: AssetKind::Btc,
        id: "btc".to_owned(),
    }
}

#[derive(Debug, Deserialize)]
struct TakerConfig {
    broker_url: String,
    /// The taker's own keychain-0 BTC address, used both as `btc_funding_addr`
    /// (buy: the maker's `list_unspent` scans it for the taker's BTC inputs) and
    /// `btc_payout_addr` (sell: where the swap PSBT sends the taker's BTC).
    /// Obtain it with `rgb -n <net> -d <data_dir> -w <wallet> address -k 0`.
    btc_address: String,
    rgb: RgbSection,
}

#[derive(Debug, Deserialize)]
struct RgbSection {
    network: String,
    data_dir: PathBuf,
    wallet_name: String,
    electrum_url: String,
    contract_id: String,
    signer: SignerSection,
}

#[derive(Debug, Deserialize)]
struct SignerSection {
    account_file: PathBuf,
    #[serde(default)]
    password: String,
}

impl TakerConfig {
    fn load(path: &std::path::Path) -> Result<Self, Box<dyn std::error::Error>> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| format!("read taker config {}: {e}", path.display()))?;
        let config: TakerConfig =
            toml::from_str(&text).map_err(|e| format!("parse taker config: {e}"))?;
        Ok(config)
    }

    fn taker(&self) -> Taker {
        Taker::new(
            self.rgb.data_dir.clone(),
            self.rgb.wallet_name.clone(),
            self.rgb.network.clone(),
            self.rgb.electrum_url.clone(),
            self.rgb.signer.account_file.clone(),
            self.rgb.signer.password.clone(),
        )
    }

    fn rgb_asset(&self) -> AssetId {
        AssetId {
            network: BitcoinNetwork::Regtest,
            kind: AssetKind::Rgb20,
            id: self.rgb.contract_id.clone(),
        }
    }
}
