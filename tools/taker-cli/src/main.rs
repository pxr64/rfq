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

use std::path::{Path, PathBuf};

use clap::{Parser, Subcommand};
use rfq_wallet::{resolve_wallet, WalletInput};
use dialoguer::{theme::ColorfulTheme, Confirm, Input};
use rfq_client::{RfqClient, Url};
use rfq_rgb::{SignGuard, Taker};
use rfq_types::{
    AcceptQuoteRequest, AssetId, AssetKind, BitcoinNetwork, CreateRfqRequest, Outpoint, Side,
    SwapLeg,
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
    /// Interactively create the taker wallet + signing account and write
    /// `taker.toml` (name-keyed, like `colorex maker init`).
    Init {
        /// Overwrite an existing config without prompting.
        #[arg(long)]
        force: bool,
    },
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
    /// Sync against electrum and print the taker wallet's BTC balance.
    Balance,
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

    // `init` bootstraps the wallet + config, so it must run before config load.
    if let Command::Init { force } = &cli.command {
        return init(&cli.config, *force).await;
    }

    let config = TakerConfig::load(&cli.config)?;
    let taker = config.taker();
    let asset = config.rgb_asset();
    let client = RfqClient::new(Url::parse(&config.broker_url)?);

    match cli.command {
        Command::Init { .. } => unreachable!("handled above"),
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
            // Ticker + precision from the contract spec; fall back to the raw
            // contract id / 0-precision if it isn't in the stash yet.
            let (ticker, precision) = taker
                .contract_spec(&asset)
                .unwrap_or_else(|_| (asset.id.clone(), 0));
            println!("taker inventory for {ticker} ({}):", asset.id);
            let mut total = 0u64;
            for u in &utxos {
                println!(
                    "  {}:{}  {ticker} {}",
                    u.outpoint.txid,
                    u.outpoint.vout,
                    rfq_types::format_amount(u.amount, precision)
                );
                total += u.amount;
            }
            println!(
                "total {ticker} {} across {} allocation(s)",
                rfq_types::format_amount(total, precision),
                utxos.len()
            );
            Ok(())
        }
        Command::Balance => {
            let rw = rfq_wallet::ResolvedWallet {
                name: config.rgb.wallet_name.clone(),
                network: config.rgb.network.clone(),
                data_dir: config.rgb.data_dir.clone(),
                account_file: config.rgb.signer.account_file.clone(),
                electrum_url: config.rgb.electrum_url.clone(),
                password: config.rgb.signer.password.clone(),
            };
            let utxos = rw.backend().wallet_balance().await?;
            print!("{}", rfq_wallet::render_balance(&utxos));
            Ok(())
        }
    }
}

/// Interactively create the taker wallet + signing account and write `taker.toml`.
/// Name-keyed via `rfq_wallet::resolve_wallet`, mirroring `colorex maker init`.
async fn init(config_path: &Path, force: bool) -> Result<(), Box<dyn std::error::Error>> {
    let theme = ColorfulTheme::default();

    if config_path.exists() && !force {
        let overwrite = Confirm::with_theme(&theme)
            .with_prompt(format!(
                "config {} already exists. Overwrite?",
                config_path.display()
            ))
            .default(false)
            .interact()?;
        if !overwrite {
            return Err("init aborted: existing config kept".into());
        }
    }

    // Wallet name defaults to "taker"; resolve_wallet prompts network / data dir /
    // electrum / signer with name-derived defaults.
    let name: String = Input::with_theme(&theme)
        .with_prompt("wallet name")
        .default("taker".to_owned())
        .interact_text()?;
    let resolved = resolve_wallet(
        WalletInput {
            name: Some(name),
            ..Default::default()
        },
        true,
        true,
    )?;

    let broker_url: String = Input::with_theme(&theme)
        .with_prompt("broker URL")
        .default("http://127.0.0.1:3000".to_owned())
        .interact_text()?;
    let contract_id: String = Input::with_theme(&theme)
        .with_prompt("RGB contract id (leave empty to set later)")
        .allow_empty(true)
        .interact_text()?;

    match resolved.create_wallet()? {
        Some(addr) => {
            println!("created taker wallet '{}'", resolved.name);
            println!("  RGB (keychain-10) funding address: {addr}");
        }
        None => println!("taker wallet '{}' already exists — kept as-is", resolved.name),
    }
    // The taker pays BTC on buy / receives BTC on sell from its keychain-0 address.
    let btc_address = resolved.backend().funding_address(false)?;
    println!("  BTC (keychain-0) address: {btc_address}");

    // Persist the per-wallet config too, so `colorex wallet ...`/`colorex-taker`
    // commands resolve this wallet by --name.
    rfq_wallet::WalletConfig::from_resolved(&resolved, &contract_id).save()?;

    let toml = render_taker_toml(&TakerRender {
        broker_url: &broker_url,
        btc_address: &btc_address,
        network: &resolved.network,
        data_dir: &resolved.data_dir.to_string_lossy(),
        wallet_name: &resolved.name,
        electrum_url: &resolved.electrum_url,
        contract_id: &contract_id,
        account_file: &resolved.account_file.to_string_lossy(),
        password: &resolved.password,
    });
    write_config(config_path, &toml)?;

    println!();
    println!("{toml}");
    println!("wrote {}", config_path.display());
    if contract_id.is_empty() {
        println!("set [rgb] contract_id once the asset is issued, then `colorex-taker buy/sell`.");
    }
    Ok(())
}

struct TakerRender<'a> {
    broker_url: &'a str,
    btc_address: &'a str,
    network: &'a str,
    data_dir: &'a str,
    wallet_name: &'a str,
    electrum_url: &'a str,
    contract_id: &'a str,
    account_file: &'a str,
    password: &'a str,
}

/// Render a `taker.toml` whose keys match `TakerConfig` / `RgbSection` /
/// `SignerSection`. Top-level keys precede the `[rgb]` table (TOML requirement).
fn render_taker_toml(r: &TakerRender<'_>) -> String {
    let mut out = String::new();
    out.push_str(&format!("broker_url  = \"{}\"\n", r.broker_url));
    out.push_str(&format!("btc_address = \"{}\"\n", r.btc_address));
    out.push('\n');
    out.push_str("[rgb]\n");
    out.push_str(&format!("network      = \"{}\"\n", r.network));
    out.push_str(&format!("data_dir     = \"{}\"\n", r.data_dir));
    out.push_str(&format!("wallet_name  = \"{}\"\n", r.wallet_name));
    out.push_str(&format!("electrum_url = \"{}\"\n", r.electrum_url));
    out.push_str(&format!("contract_id  = \"{}\"\n", r.contract_id));
    out.push('\n');
    out.push_str("[rgb.signer]\n");
    out.push_str(&format!("account_file = \"{}\"\n", r.account_file));
    out.push_str(&format!("password     = \"{}\"\n", r.password));
    out
}

fn write_config(path: &Path, contents: &str) -> Result<(), std::io::Error> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, contents)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&tmp)?.permissions();
        perms.set_mode(0o600);
        std::fs::set_permissions(&tmp, perms)?;
    }
    std::fs::rename(&tmp, path)?;
    Ok(())
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
            quote_asset: btc_asset(asset.network.clone()),
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
                rgb_invoice: rgb_invoice.clone(),
                btc_funding_addr,
            },
        })
        .await?;
    let transfer = accepted
        .transfer
        .ok_or("buy accept did not return a partial PSBT")?;

    // Capture hook: dump the maker's partial PSBT (base64) for the browser
    // wallet's decode work. Set COLOREX_DUMP_PSBT=/path to enable.
    if let Ok(path) = std::env::var("COLOREX_DUMP_PSBT") {
        std::fs::write(&path, &transfer.partial_psbt).ok();
        println!("dumped maker partial PSBT (base64) → {path}");
    }

    // Buy-side security gate: refuse to sign/pay unless the maker's RGB ancestry is mined
    // on-chain (the swap tx — `expected_witness_txid` — is the only allowed-unmined hop).
    let consignment = transfer
        .consignment
        .as_deref()
        .ok_or("buy accept did not return a consignment to validate before signing")?;
    let expected_wt = transfer
        .expected_witness_txid
        .as_deref()
        .ok_or("buy accept did not return an expected witness txid to validate against")?;
    taker
        .validate_buy_consignment(asset, consignment, expected_wt)
        .await?;
    // #38 buy delivered-value: the maker's consignment must deliver at least the amount we're
    // paying for, to OUR own seal (blinded; witness-vout deferred to accept).
    taker
        .verify_delivery(asset, consignment, &rgb_invoice, amount, false)
        .await?;

    // #38 buy gate: we pay BTC, so refuse to sign any of our own RGB anchors the maker may have
    // spliced in, and confirm the tx is the swap the maker published (anti-substitution).
    let rgb_anchors: Vec<Outpoint> = taker
        .inventory(asset)
        .await?
        .into_iter()
        .map(|u| u.outpoint)
        .collect();
    let guard = SignGuard {
        forbidden_outpoints: Some(rgb_anchors),
        expected_witness_txid: Some(expected_wt.to_owned()),
        ..Default::default()
    };
    let signed = taker.sign_and_finalize(&transfer.partial_psbt, Some(&guard))?;
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
            quote_asset: btc_asset(asset.network.clone()),
            side: Side::Sell,
            amount,
        })
        .await?;
    let quote = quotes
        .into_iter()
        .next()
        .ok_or("no maker quoted the sell")?;
    println!("quote {} price={} sats fee~{} sats", quote.quote_id.0, quote.price, quote.estimated_fee_sats);

    // Pick the taker's own RGB UTXOs to sell — enough to cover `amount`,
    // largest-first. These outpoints are what the maker spends into the swap tx;
    // it learns them from us (provenance model — the consignment proves the asset,
    // we name which outpoints we're offering).
    let mut inv = taker.inventory(asset).await?;
    inv.sort_by(|a, b| b.amount.cmp(&a.amount));
    let mut chosen: Vec<Outpoint> = Vec::new();
    let mut have: u64 = 0;
    for u in inv {
        if have >= amount {
            break;
        }
        have = have.saturating_add(u.amount);
        chosen.push(u.outpoint);
    }
    if have < amount {
        return Err(format!("insufficient RGB to sell: have {have}, need {amount}").into());
    }

    // Change invoice for surplus RGB (the chosen inputs usually exceed `amount`).
    // The maker reads only the beneficiary seal off it; the amount is ignored.
    let rgb_change_invoice = taker.create_invoice(asset, amount).await?;
    let btc_payout_addr = btc_address.to_owned();

    let accepted = client
        .accept_quote(AcceptQuoteRequest {
            quote_id: quote.quote_id.clone(),
            leg: SwapLeg::Sell {
                btc_payout_addr,
                rgb_change_invoice: Some(rgb_change_invoice.clone()),
            },
        })
        .await?;
    if accepted.transfer.is_some() {
        return Err("sell accept unexpectedly returned a PSBT before consignment".into());
    }

    // Export provenance for the chosen outpoints (no PSBT, no fee, no anchor); the
    // maker validates it, spends those outpoints into the swap tx, and broadcasts.
    let outpoint_strs: Vec<String> = chosen
        .iter()
        .map(|o| format!("{}:{}", o.txid, o.vout))
        .collect();
    let consignment = taker.export_provenance(&asset.id, &outpoint_strs)?;

    let delivered = client
        .submit_consignment(quote.quote_id.clone(), consignment, chosen.clone())
        .await?;
    let transfer = delivered
        .transfer
        .ok_or("consignment delivery did not return a partial PSBT")?;

    // #38 sell change-back: the RGB change (gross − sold) must come back to OUR own seal — a short
    // or misrouted change is rejected before we sign (blinded; witness-vout deferred to accept).
    if let Some(change_consignment) = transfer.consignment.as_deref() {
        taker
            .verify_delivery(asset, change_consignment, &rgb_change_invoice, have - amount, true)
            .await?;
    }

    // #38 sell gate: sign ONLY the named sale outpoints (anti-sweep — the maker can't splice in our
    // other RGB UTXOs), confirm the BTC payout reaches OUR address at >= price minus the quoted fee,
    // and that the tx is the swap the maker published.
    let guard = SignGuard {
        allowed_outpoints: Some(chosen.clone()),
        expected_witness_txid: transfer.expected_witness_txid.clone(),
        expected_payout: Some((
            taker.payout_spk(btc_address)?,
            quote.price.saturating_sub(quote.estimated_fee_sats),
        )),
        ..Default::default()
    };
    let signed = taker.sign_and_finalize(&transfer.partial_psbt, Some(&guard))?;
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


fn btc_asset(network: BitcoinNetwork) -> AssetId {
    AssetId {
        network,
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
        Self::load_str(&text)
    }

    fn load_str(text: &str) -> Result<Self, Box<dyn std::error::Error>> {
        let config: TakerConfig =
            toml::from_str(text).map_err(|e| format!("parse taker config: {e}"))?;
        // Validate the network up front so `rgb_asset` can rely on it.
        config.rgb.network.parse::<BitcoinNetwork>()?;
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
            network: self
                .rgb
                .network
                .parse()
                .expect("network validated in TakerConfig::load"),
            kind: AssetKind::Rgb20,
            id: self.rgb.contract_id.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rendered_taker_toml_round_trips() {
        let toml = render_taker_toml(&TakerRender {
            broker_url: "http://127.0.0.1:3000",
            btc_address: "tb1qexamplebtcaddr",
            network: "signet",
            data_dir: "/home/x/.local/share/colorex/taker",
            wallet_name: "taker",
            electrum_url: "ssl://mempool.space:60602",
            contract_id: "rgb:abc-123",
            account_file: "/home/x/.local/share/colorex/taker/account.key",
            password: "",
        });
        let cfg = TakerConfig::load_str(&toml).expect("rendered taker.toml parses");
        assert_eq!(cfg.broker_url, "http://127.0.0.1:3000");
        assert_eq!(cfg.btc_address, "tb1qexamplebtcaddr");
        assert_eq!(cfg.rgb.network, "signet");
        assert_eq!(cfg.rgb.contract_id, "rgb:abc-123");
        assert_eq!(cfg.rgb.electrum_url, "ssl://mempool.space:60602");
    }
}
