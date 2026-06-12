use std::path::{Path, PathBuf};

use clap::{Args, Parser, Subcommand};
use maker_node::{
    accept_inventory_consignment, broker_client, build_maker, build_runtime,
    create_inventory_invoice, init, maker_app, now_ms, orders, fetch_consignment, output,
    reconsign_consignment, spawn_chain_observer_loop, spawn_cleanup_loop, spawn_order_reload_loop,
    spawn_rebalance_loop, spawn_strategy_loop, MakerNodeConfig,
};
use rfq_wallet::{resolve_named, resolve_wallet, WalletConfig, WalletInput};
use rfq_rgb::RgbBackend;
use rfq_store::{BtcInventoryStore as _, ContractStore as _, InventoryStore as _, OrderStore as _};
use rfq_client::{RfqClient, Url};
use rfq_types::{InventorySnapshot, MakerId, Side};
use tokio::{net::TcpListener, sync::oneshot};

#[derive(Debug, Parser)]
#[command(name = "colorex", version, about = "RGB RFQ tooling")]
struct Cli {
    #[command(subcommand)]
    command: TopCommand,
    /// Path to the maker config TOML. Defaults to `~/.config/colorex/maker.toml`.
    #[arg(long, global = true)]
    config: Option<PathBuf>,
}

#[derive(Debug, Subcommand, Clone, PartialEq, Eq)]
enum TopCommand {
    /// Maker daemon: serve quotes, manage inventory.
    Maker {
        #[command(subcommand)]
        cmd: MakerCmd,
    },
    /// Wallet tooling: create / address / sync a taproot RGB wallet (Rust-native,
    /// no rgb-cmd or docker). Works for any role (issuer, maker, taker).
    Wallet {
        #[command(subcommand)]
        cmd: WalletCmd,
    },
    /// Issuer tooling: mint NIA tokens + list issued contracts (Rust-native).
    Issuer {
        #[command(subcommand)]
        cmd: IssuerCmd,
    },
}

#[derive(Debug, Subcommand, Clone, PartialEq, Eq)]
enum IssuerCmd {
    /// Mint a new Non-Inflatable Asset (fixed-supply fungible token). Prints the
    /// new contract id to put in the maker/taker config.
    Issue {
        #[command(flatten)]
        common: WalletCommon,
        /// Ticker, e.g. FOO.
        #[arg(long)]
        ticker: String,
        /// Human-readable asset name.
        #[arg(long)]
        asset_name: String,
        /// Decimal places (0–18).
        #[arg(long)]
        precision: u8,
        /// Total supply (in the smallest unit), all allocated to the issuer.
        #[arg(long)]
        supply: u64,
        /// Optional free-text details.
        #[arg(long)]
        details: Option<String>,
        /// Genesis seal `txid:vout` (a funded keychain-10 UTXO). If omitted,
        /// auto-picks one from the synced issuer wallet.
        #[arg(long)]
        seal: Option<String>,
        /// Issuer identity label embedded in genesis (no signing).
        #[arg(long, default_value = "ssi:anonymous")]
        issuer: String,
    },
    /// List issued contracts in the issuer's stock.
    Contracts {
        #[command(flatten)]
        common: WalletCommon,
    },
    /// Distribute tokens to a recipient's RGB invoice (signs + broadcasts the
    /// anchoring tx; hand the printed consignment to the recipient).
    Transfer {
        #[command(flatten)]
        common: WalletCommon,
        /// Recipient RGB invoice string.
        #[arg(long)]
        invoice: String,
        /// Electrum URL to broadcast the anchoring tx. Defaults per-network;
        /// prompted if omitted.
        #[arg(long)]
        electrum: Option<String>,
        /// Encrypted signing-account file (the issuer's hot key). Defaults to
        /// `<data_dir>/account.key`; prompted if omitted.
        #[arg(long)]
        account_file: Option<PathBuf>,
        /// Signer password. Prompted if omitted.
        #[arg(long)]
        password: Option<String>,
        /// Fee for the transfer tx, in sats.
        #[arg(long, default_value_t = 1000)]
        fee: u64,
    },
}

#[derive(Debug, Subcommand, Clone, PartialEq, Eq)]
enum WalletCmd {
    /// Create a fresh taproot RGB wallet + empty stock on disk.
    Create {
        #[command(flatten)]
        common: WalletCommon,
        /// Encrypted signing-account file to write. Defaults to
        /// `<data_dir>/account.key`; prompted if omitted.
        #[arg(long)]
        account_file: Option<PathBuf>,
        /// Signer password. Prompted if omitted.
        #[arg(long)]
        password: Option<String>,
    },
    /// Print a receive address to fund manually from a faucet (default: the
    /// tapret keychain-10 RGB anchor address; `--btc` for the keychain-0 BTC one).
    Address {
        #[command(flatten)]
        common: WalletCommon,
        #[arg(long)]
        btc: bool,
    },
    /// Sync the wallet against electrum (run after manual funding confirms).
    Sync {
        #[command(flatten)]
        common: WalletCommon,
        /// Electrum URL. Defaults per-network; prompted if omitted.
        #[arg(long)]
        electrum: Option<String>,
    },
    /// Sync against electrum and print the wallet's BTC balance (per-utxo).
    Balance {
        #[command(flatten)]
        common: WalletCommon,
        /// Electrum URL. Defaults per-network; prompted if omitted.
        #[arg(long)]
        electrum: Option<String>,
    },
    /// Mint a witness-vout RGB receive invoice for a contract. No funded UTXO
    /// needed — the RGB lands on a fresh output of the sender's transfer tx.
    Invoice {
        #[command(flatten)]
        common: WalletCommon,
        /// RGB contract id to receive.
        #[arg(long)]
        contract: String,
        /// Amount (smallest RGB units) the invoice requests.
        #[arg(long)]
        amount: u64,
    },
}

#[derive(Debug, Args, Clone, PartialEq, Eq)]
struct WalletCommon {
    /// Wallet name — the only required discriminator; everything else derives
    /// from it (`~/.local/share/colorex/<name>`). Prompted if omitted.
    #[arg(long)]
    name: Option<String>,
    /// Network: regtest | signet | testnet | mainnet. Prompted if omitted.
    #[arg(long)]
    network: Option<String>,
    /// RGB data dir (stock lives at `<data_dir>/<network>`). Defaults to
    /// `~/.local/share/colorex/<name>`; prompted if omitted.
    #[arg(long)]
    data_dir: Option<PathBuf>,
}

impl WalletCommon {
    /// Seed a [`WalletInput`] with the shared name/network/data-dir; per-command
    /// extras (electrum/account/password) are layered on by each handler.
    fn into_input(self) -> WalletInput {
        WalletInput {
            name: self.name,
            network: self.network,
            data_dir: self.data_dir,
            ..Default::default()
        }
    }
}

#[derive(Debug, Subcommand, Clone, PartialEq, Eq)]
enum MakerCmd {
    /// Generate keypair + interactively write a fresh config TOML.
    Init(init::InitArgs),
    /// Start the maker daemon (HTTP server + background loops).
    Up,
    /// Probe broker health.
    Health,
    /// Print the maker's RGB inventory snapshot.
    Inventory {
        /// Also dump the BTC inventory across all three layers — the on-chain
        /// wallet (electrum), the RGB-exclusion filter, and the SQLite cache
        /// coin-selection reads — to diagnose "no BTC inventory" funding gaps.
        #[arg(long)]
        btc: bool,
        /// Electrum URL to sync before reading on-chain UTXOs (for `--btc`).
        /// Defaults to the config's `[rgb] electrum_url`; omit to read the
        /// last-synced wallet cache without a fresh sync.
        #[arg(long)]
        electrum: Option<String>,
    },
    /// Maker wallet + funding ops: addresses, balances, rescan, recover, and
    /// inventory invoice/accept — everything that touches the maker's wallet.
    Wallet {
        #[command(subcommand)]
        cmd: MakerWalletCmd,
    },
    /// Manage standing orders — the prices the maker quotes per (asset, side).
    Order {
        #[command(subcommand)]
        cmd: OrderCmd,
    },
    /// Manage the contract registry — the RGB assets this maker trades. Replaces
    /// the single `[rgb] contract_id` in the TOML; the daemon seeds + quotes
    /// every registered contract.
    Contract {
        #[command(subcommand)]
        cmd: ContractCmd,
    },
    /// Re-derive a consignment for an already-settled transfer (recovery). Reads
    /// the maker's stash only — no chain access, no signing. Use when a recipient
    /// lost their consignment (failed delivery / wallet reset).
    Reconsign {
        /// RGB contract id. Defaults to the config's `[rgb] contract_id`.
        #[arg(long)]
        contract: Option<String>,
        /// The recipient's witness outpoint `txid:vout` (the swap output holding
        /// their RGB; the txid is the witness tx). Witness-vout seals only.
        #[arg(long)]
        outpoint: String,
        /// Write the base64 consignment here instead of stdout.
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// Re-serve a consignment the maker recorded at settlement, by quote id.
    /// Reads maker.db only — the cheap recovery path (no re-derive). Use
    /// `reconsign` if the maker never recorded one.
    Consignment {
        /// Quote id of the settled swap.
        #[arg(long)]
        quote_id: String,
        /// Write the base64 consignment here instead of stdout.
        #[arg(long)]
        out: Option<PathBuf>,
    },
}

#[derive(Debug, Subcommand, Clone, PartialEq, Eq)]
enum OrderCmd {
    /// Create (or replace) the standing order for an (asset, side).
    Create {
        /// `buy` (taker buys RGB) or `sell` (taker sells RGB).
        #[arg(long)]
        side: String,
        /// RGB contract id. Defaults to the config's `[rgb] contract_id`.
        #[arg(long)]
        asset: Option<String>,
        /// Price in sats per smallest RGB unit.
        #[arg(long)]
        price: u64,
        /// Max single-quote size (smallest RGB units) this order backs.
        #[arg(long)]
        size: u64,
        /// Auto-mirror: on each fill of this order, place the opposite-side order
        /// (buy⇄sell) at `--mirror-spread-bps` off the fill price.
        #[arg(long)]
        mirror: bool,
        /// Spread (basis points) for the mirror order's price. Buy-back is
        /// cheaper / re-sell is dearer by this much. Required for a useful mirror.
        #[arg(long, default_value_t = 0)]
        mirror_spread_bps: u16,
    },
    /// List standing orders.
    List,
    /// Cancel a standing order by id.
    Cancel {
        /// Order id (from `order list`).
        id: String,
    },
    /// Cancel ALL standing orders.
    Clear,
}

#[derive(Debug, Subcommand, Clone, PartialEq, Eq)]
enum ContractCmd {
    /// Register an RGB asset to trade. The contract must be in the maker's stock
    /// (mint it, or `--consignment` to accept one first). Ticker + precision are
    /// read from the stock and cached. Re-importing updates in place.
    Import {
        /// RGB contract id (`rgb:...`).
        id: String,
        /// Optional consignment to accept into the stock before registering —
        /// the "I was handed a new asset" path (folds in `accept`). Either a file
        /// path OR the inline base64 string.
        #[arg(long)]
        consignment: Option<String>,
    },
    /// List the registered contracts.
    List,
    /// Stop trading a contract (leaves its stock + inventory untouched).
    Remove {
        /// RGB contract id (`rgb:...`).
        id: String,
    },
}

#[derive(Debug, Subcommand, Clone, PartialEq, Eq)]
enum MakerWalletCmd {
    /// Print the maker's funding addresses (BTC keychain 0 + RGB anchor keychain
    /// 10). Offline — derived from the wallet, no chain access. Use `balances` to
    /// also fetch the funded amounts.
    Addresses,
    /// Show funded balances per keychain (BTC payment + RGB anchor), syncing the
    /// wallet against electrum. `--electrum` overrides the config's electrum_url.
    Balances {
        /// Electrum URL to sync against. Defaults to the config's `[rgb] electrum_url`.
        #[arg(long)]
        electrum: Option<String>,
    },
    /// Full from-scratch wallet rescan (vs the daemon's incremental sync).
    /// Re-derives every keychain from index 0 with the descriptor's tapret
    /// tweaks applied, recovering tapret host outputs the incremental scan
    /// stranded. Run with the daemon stopped; re-check with `inventory --btc`.
    Rescan {
        /// Electrum URL to scan against. Defaults to the config's
        /// `[rgb] electrum_url`.
        #[arg(long)]
        electrum: Option<String>,
    },
    /// Recover stranded RGB: sweep allocations the wallet can't see (tapret
    /// host outputs bp-wallet never tracked) into one fresh output at the pinned
    /// host, so they become spendable inventory again. Run with the daemon
    /// stopped; `--dry-run` first to see what would be swept.
    Recover {
        /// RGB contract id. Defaults to the sole registered contract.
        #[arg(long)]
        contract: Option<String>,
        /// Electrum URL. Defaults to the config's `[rgb] electrum_url`.
        #[arg(long)]
        electrum: Option<String>,
        /// Tx fee for the sweep, in sats.
        #[arg(long, default_value_t = 1000)]
        fee: u64,
        /// Report what would be swept without building/broadcasting anything.
        #[arg(long)]
        dry_run: bool,
    },
    /// Mint an RGB invoice to receive inventory from an issuer.
    Invoice {
        /// RGB contract id. Defaults to the sole registered contract.
        #[arg(long)]
        contract: Option<String>,
        /// Amount (in smallest RGB units) the invoice requests.
        #[arg(long)]
        amount: u64,
    },
    /// Accept an incoming consignment into the maker's stash — the receive-side
    /// counterpart to `invoice`. After you `invoice` and the issuer runs
    /// `issuer transfer` against it, they return a consignment; this imports it
    /// so `maker up` sees the RGB as inventory (once the anchoring tx confirms).
    Accept {
        /// The consignment the issuer returned: a file path OR the inline base64
        /// string. (`--path` is kept as an alias.)
        #[arg(long, visible_alias = "path")]
        consignment: String,
        /// RGB contract id. Defaults to the sole registered contract.
        #[arg(long)]
        contract: Option<String>,
    },
    /// Send RGB from the maker's inventory to a recipient's invoice (the maker
    /// analogue of `issuer transfer`): builds + signs + broadcasts the anchoring
    /// tx and prints the base64 consignment to hand back. The contract + amount
    /// come from the invoice. Run with the daemon STOPPED (it spends the maker's
    /// wallet); the chain observer reconciles inventory on the next `maker up`.
    Transfer {
        /// Recipient RGB invoice string (carries the contract id + amount).
        #[arg(long)]
        invoice: String,
        /// Electrum URL to broadcast against. Defaults to the config's `[rgb] electrum_url`.
        #[arg(long)]
        electrum: Option<String>,
        /// Fee for the transfer tx, in sats.
        #[arg(long, default_value_t = 1000)]
        fee: u64,
        /// Write the base64 consignment here instead of stdout.
        #[arg(long)]
        out: Option<PathBuf>,
    },
}

#[tokio::main]
async fn main() {
    if let Err(error) = run_cli().await {
        eprintln!("colorex error: {error}");
        std::process::exit(1);
    }
}

async fn run_cli() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();
    let config_path = cli.config.unwrap_or_else(MakerNodeConfig::default_path);

    match cli.command {
        TopCommand::Maker { cmd } => match cmd {
            MakerCmd::Init(args) => init::run(args, &config_path).await,
            MakerCmd::Up => run(load_config(&config_path)?, &config_path).await,
            MakerCmd::Health => health(load_config(&config_path)?).await,
            MakerCmd::Inventory { btc, electrum } => {
                inventory(load_config(&config_path)?, btc, electrum).await
            }
            MakerCmd::Wallet { cmd } => match cmd {
                MakerWalletCmd::Addresses => maker_addresses(load_config(&config_path)?),
                MakerWalletCmd::Balances { electrum } => {
                    maker_balances(load_config(&config_path)?, electrum).await
                }
                MakerWalletCmd::Rescan { electrum } => {
                    maker_rescan(load_config(&config_path)?, electrum).await
                }
                MakerWalletCmd::Recover {
                    contract,
                    electrum,
                    fee,
                    dry_run,
                } => maker_recover(load_config(&config_path)?, contract, electrum, fee, dry_run).await,
                MakerWalletCmd::Invoice { contract, amount } => {
                    maker_invoice(load_config(&config_path)?, contract, amount).await
                }
                MakerWalletCmd::Accept {
                    consignment,
                    contract,
                } => maker_accept(load_config(&config_path)?, consignment, contract).await,
                MakerWalletCmd::Transfer {
                    invoice,
                    electrum,
                    fee,
                    out,
                } => maker_transfer(load_config(&config_path)?, invoice, electrum, fee, out).await,
            },
            MakerCmd::Order { cmd } => match cmd {
                OrderCmd::Create {
                    side,
                    asset,
                    price,
                    size,
                    mirror,
                    mirror_spread_bps,
                } => {
                    order_create(&config_path, side, asset, price, size, mirror, mirror_spread_bps)
                        .await
                }
                OrderCmd::List => order_list(&config_path).await,
                OrderCmd::Cancel { id } => order_cancel(&config_path, &id).await,
                OrderCmd::Clear => order_clear(&config_path).await,
            },
            MakerCmd::Contract { cmd } => match cmd {
                ContractCmd::Import { id, consignment } => {
                    contract_import(load_config(&config_path)?, id, consignment).await
                }
                ContractCmd::List => contract_list(load_config(&config_path)?).await,
                ContractCmd::Remove { id } => contract_remove(load_config(&config_path)?, id).await,
            },
            MakerCmd::Reconsign {
                contract,
                outpoint,
                out,
            } => maker_reconsign(load_config(&config_path)?, contract, outpoint, out).await,
            MakerCmd::Consignment { quote_id, out } => {
                maker_get_consignment(load_config(&config_path)?, quote_id, out).await
            }
        },
        TopCommand::Wallet { cmd } => match cmd {
            WalletCmd::Create {
                common,
                account_file,
                password,
            } => wallet_create(common, account_file, password),
            WalletCmd::Address { common, btc } => wallet_address(common, btc),
            WalletCmd::Sync { common, electrum } => wallet_sync(common, electrum).await,
            WalletCmd::Balance { common, electrum } => wallet_balance(common, electrum).await,
            WalletCmd::Invoice {
                common,
                contract,
                amount,
            } => wallet_invoice(common, contract, amount).await,
        },
        TopCommand::Issuer { cmd } => match cmd {
            IssuerCmd::Issue {
                common,
                ticker,
                asset_name,
                precision,
                supply,
                details,
                seal,
                issuer,
            } => issuer_issue(common, ticker, asset_name, precision, supply, details, seal, issuer),
            IssuerCmd::Contracts { common } => issuer_contracts(common),
            IssuerCmd::Transfer {
                common,
                invoice,
                electrum,
                account_file,
                password,
                fee,
            } => issuer_transfer(common, invoice, electrum, account_file, password, fee).await,
        },
    }
}

fn load_config(path: &Path) -> Result<MakerNodeConfig, String> {
    MakerNodeConfig::load(path).map_err(|e| format!("config {}: {e}", path.display()))
}

fn wallet_create(
    common: WalletCommon,
    account_file: Option<PathBuf>,
    password: Option<String>,
) -> Result<(), Box<dyn std::error::Error>> {
    let input = WalletInput {
        account_file,
        password,
        ..common.into_input()
    };
    let resolved = resolve_wallet(input, false, true)?;
    match resolved.create_wallet()? {
        Some(addr) => {
            println!("created wallet '{}'", resolved.name);
            println!(
                "fund this tapret (keychain-10) address from a faucet, then run `wallet sync`:\n  {addr}"
            );
        }
        None => println!("wallet '{}' already exists — kept as-is", resolved.name),
    }
    // Persist the per-wallet config so future commands resolve by `--name` alone.
    let cfg = WalletConfig::from_resolved(&resolved, "");
    cfg.save()?;
    println!("wrote {}", cfg.saved_path().display());
    Ok(())
}

fn wallet_address(common: WalletCommon, btc: bool) -> Result<(), Box<dyn std::error::Error>> {
    let resolved = resolve_named(common.into_input())?;
    println!("{}", resolved.backend().funding_address(!btc)?);
    Ok(())
}

async fn wallet_invoice(
    common: WalletCommon,
    contract: String,
    amount: u64,
) -> Result<(), Box<dyn std::error::Error>> {
    let resolved = resolve_named(common.into_input())?;
    let asset = rfq_types::AssetId {
        network: resolved.network.parse()?,
        kind: rfq_types::AssetKind::Rgb20,
        id: contract,
    };
    let invoice = resolved.backend().create_invoice(&asset, amount).await?;
    println!("{invoice}");
    Ok(())
}

async fn wallet_sync(
    common: WalletCommon,
    electrum: Option<String>,
) -> Result<(), Box<dyn std::error::Error>> {
    let input = WalletInput {
        electrum_url: electrum,
        ..common.into_input()
    };
    let resolved = resolve_named(input)?;
    resolved.backend().sync_wallet().await?;
    println!("synced '{}'", resolved.name);
    Ok(())
}

async fn wallet_balance(
    common: WalletCommon,
    electrum: Option<String>,
) -> Result<(), Box<dyn std::error::Error>> {
    let input = WalletInput {
        electrum_url: electrum,
        ..common.into_input()
    };
    let resolved = resolve_named(input)?;
    let utxos = resolved.backend().wallet_balance().await?;
    print!("{}", rfq_wallet::render_balance(&utxos));
    Ok(())
}

async fn maker_invoice(
    config: MakerNodeConfig,
    contract: Option<String>,
    amount: u64,
) -> Result<(), Box<dyn std::error::Error>> {
    let contract_id = resolve_contract_id(&config, contract).await?;
    let invoice = create_inventory_invoice(&config, contract_id, amount).await?;
    println!("{invoice}");
    Ok(())
}

/// The two wallet addresses an operator funds: keychain-0 (BTC payment + fees) and
/// keychain-10 (tapret RGB seal anchors). Derivation is xpub-only — no electrum or
/// signer needed. `Err` if there's no `[rgb]` config.
fn maker_funding_addresses(
    config: &MakerNodeConfig,
) -> Result<(String, String), Box<dyn std::error::Error>> {
    let rgb = config
        .rgb
        .as_ref()
        .ok_or("no [rgb] config: a wallet is required to derive addresses")?;
    let backend = rfq_rgb::LibRgbBackend::new(
        rgb.data_dir.clone(),
        rgb.wallet_name.clone(),
        rgb.network.clone(),
        String::new(),
        std::path::PathBuf::new(),
        String::new(),
    );
    Ok((backend.funding_address(false)?, backend.funding_address(true)?))
}

/// Per-keychain BTC totals (k0, k10) — syncs the wallet against `electrum`.
async fn maker_keychain_balances(
    config: &MakerNodeConfig,
    electrum: &str,
) -> Result<(u64, u64), Box<dyn std::error::Error>> {
    let rgb = config.rgb.as_ref().ok_or("no [rgb] config")?;
    let backend = rfq_rgb::LibRgbBackend::new(
        rgb.data_dir.clone(),
        rgb.wallet_name.clone(),
        rgb.network.clone(),
        electrum.to_owned(),
        std::path::PathBuf::new(),
        String::new(),
    );
    let (mut k0, mut k10) = (0u64, 0u64);
    for u in backend.wallet_balance().await? {
        match u.keychain {
            0 => k0 += u.sats,
            10 => k10 += u.sats,
            _ => {}
        }
    }
    Ok((k0, k10))
}

fn maker_addresses(config: MakerNodeConfig) -> Result<(), Box<dyn std::error::Error>> {
    let (btc, anchor) = maker_funding_addresses(&config)?;
    let network = config.rgb.as_ref().map(|r| r.network.as_str()).unwrap_or("");

    println!("colorex maker wallet addresses ({network})");
    output::kv("BTC payment · keychain 0", &btc);
    output::kv("RGB anchor · keychain 10", &anchor);
    output::note("");
    output::note("Fund BTC to pay sell-side takers + tx fees; fund RGB-anchor to mint/receive RGB.");
    output::note("Show funded balances: colorex maker wallet balances");
    Ok(())
}

/// Per-keychain BTC balances (keychain 0 = payment, 10 = RGB anchor), synced
/// against electrum. The funding addresses are shown alongside, so this answers
/// both "where do I fund" and "how much is there". `--electrum` overrides the
/// config's electrum_url. The shown address is the next-unused one (where to
/// SEND); the sats are the keychain TOTAL across all its UTXOs.
async fn maker_balances(
    config: MakerNodeConfig,
    electrum: Option<String>,
) -> Result<(), Box<dyn std::error::Error>> {
    let (btc, anchor) = maker_funding_addresses(&config)?;
    let network = config.rgb.as_ref().map(|r| r.network.as_str()).unwrap_or("");
    let electrum_url = electrum
        .filter(|s| !s.is_empty())
        .or_else(|| config.rgb.as_ref().map(|r| r.electrum_url.clone()).filter(|s| !s.is_empty()))
        .ok_or("no electrum URL: pass --electrum <url> or set [rgb] electrum_url")?;

    let (k0, k10) = maker_keychain_balances(&config, &electrum_url).await?;

    println!("colorex maker wallet balances ({network})");
    output::kv(&format!("BTC payment · keychain 0 · {k0} sats"), &btc);
    output::kv(&format!("RGB anchor · keychain 10 · {k10} sats"), &anchor);
    Ok(())
}

async fn maker_rescan(
    config: MakerNodeConfig,
    electrum: Option<String>,
) -> Result<(), Box<dyn std::error::Error>> {
    let rgb = config
        .rgb
        .as_ref()
        .ok_or("no [rgb] config: a wallet is required to rescan")?;
    let electrum_url = electrum
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| rgb.electrum_url.clone());
    if electrum_url.is_empty() {
        return Err("no --electrum given and no [rgb] electrum_url in config".into());
    }
    let backend = rfq_rgb::LibRgbBackend::new(
        rgb.data_dir.clone(),
        rgb.wallet_name.clone(),
        rgb.network.clone(),
        electrum_url.clone(),
        std::path::PathBuf::new(),
        String::new(),
    );
    output::step(&format!("rescanning wallet from scratch via {electrum_url}"));
    let tracked = backend.rescan_wallet().await?;
    output::step_ok();
    output::kv("tracked UTXOs after rescan", &tracked.to_string());
    output::note("Re-check recovery with: colorex maker inventory --btc");
    Ok(())
}

async fn maker_recover(
    config: MakerNodeConfig,
    contract: Option<String>,
    electrum: Option<String>,
    fee: u64,
    dry_run: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    use rfq_btc::BitcoinClient as _;
    let contract_id = resolve_contract_id(&config, contract).await?;
    let rgb = config
        .rgb
        .as_ref()
        .ok_or("no [rgb] config: a wallet is required to recover")?;
    let electrum_url = electrum
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| rgb.electrum_url.clone());
    if electrum_url.is_empty() {
        return Err("no --electrum given and no [rgb] electrum_url in config".into());
    }
    let asset = rfq_types::AssetId {
        network: rgb.network.parse()?,
        kind: rfq_types::AssetKind::Rgb20,
        id: contract_id,
    };
    // Recovery SIGNS, so the backend needs the real signer (account + password).
    let backend = rfq_rgb::LibRgbBackend::new(
        rgb.data_dir.clone(),
        rgb.wallet_name.clone(),
        rgb.network.clone(),
        electrum_url.clone(),
        rgb.signer.account_file.clone(),
        rgb.signer.password.clone(),
    );
    let probe = rfq_btc::ElectrumClient::connect(&electrum_url)?;

    output::step("syncing wallet");
    backend.sync_wallet().await?;
    output::step_ok();

    // Enumerate stranded allocations: live in the stock, NOT tracked by the
    // wallet, and provably unspent on-chain.
    let allocs = backend.debug_contract_allocations(&asset).await?;
    let mut stranded: Vec<(rfq_types::Outpoint, u64, Vec<u8>, u64)> = Vec::new();
    let mut probe_failed = 0usize;
    for (op, amount, in_wallet) in allocs {
        if in_wallet {
            continue;
        }
        match probe.outpoint_unspent(&op).await {
            Ok(true) => {
                let txout = probe.get_outpoint(&op).await?;
                stranded.push((op, txout.value_sats, txout.script_pubkey, amount));
            }
            Ok(false) => continue, // spent — skip
            Err(_) => {
                // Couldn't confirm on-chain (even after retries) — skip rather
                // than risk sweeping a spent output; surfaced below so the user
                // knows to re-run for the rest.
                probe_failed += 1;
            }
        }
    }
    if probe_failed > 0 {
        output::step_warn(&format!(
            "{probe_failed} allocation(s) could not be checked on-chain (electrum probe \
             failed) — skipped this pass; re-run `recover` to retry them"
        ));
    }

    if stranded.is_empty() {
        output::note("no stranded RGB found (nothing unspent + off-wallet to recover)");
        return Ok(());
    }
    let rgb_total: u64 = stranded.iter().map(|(_, _, _, amt)| amt).sum();
    output::kv(
        "stranded to sweep",
        &format!("{} outputs · {} units", stranded.len(), rgb_total),
    );
    for (op, sats, _, amt) in &stranded {
        println!("    {}:{}  {amt} units  ({sats} sats)", op.txid, op.vout);
    }

    // Pick the largest BTC-only UTXO to fund the fee + the new seal anchor.
    let now = now_ms();
    let btc = backend.list_btc_only_utxos(std::slice::from_ref(&asset), now).await?;
    let fee_utxo = btc
        .iter()
        .max_by_key(|u| u.value_sats)
        .ok_or("no BTC-only UTXO available to fund the sweep fee")?;
    output::kv(
        "fee input",
        &format!(
            "{}:{} · {} sats (fee {fee})",
            fee_utxo.outpoint.txid, fee_utxo.outpoint.vout, fee_utxo.value_sats
        ),
    );

    if dry_run {
        output::note("dry-run: nothing built or broadcast. Re-run without --dry-run to sweep.");
        return Ok(());
    }

    // Keep (outpoint, amount) for post-sweep reporting before consuming `stranded`.
    let stranded_meta: Vec<(rfq_types::Outpoint, u64)> =
        stranded.iter().map(|(op, _, _, amt)| (op.clone(), *amt)).collect();
    let stranded_inputs: Vec<(rfq_types::Outpoint, u64, Vec<u8>)> = stranded
        .into_iter()
        .map(|(op, sats, spk, _)| (op, sats, spk))
        .collect();
    let fee_input = (
        fee_utxo.outpoint.clone(),
        fee_utxo.value_sats,
        fee_utxo.script_pubkey.clone(),
    );

    output::step("building + signing recovery sweep");
    let (raw_tx, witness_txid, swept) = backend
        .recover_stranded_rgb(&asset, stranded_inputs, fee_input, fee)
        .await?;
    output::step_ok();

    // The sweep skips any candidate this wallet can't sign (counterparty
    // allocations FilterIncludeAll surfaced) — report what actually went in.
    let swept_set: std::collections::HashSet<(String, u32)> =
        swept.iter().map(|o| (o.txid.clone(), o.vout)).collect();
    let swept_units: u64 = stranded_meta
        .iter()
        .filter(|(op, _)| swept_set.contains(&(op.txid.clone(), op.vout)))
        .map(|(_, amt)| *amt)
        .sum();
    let skipped = stranded_meta.len() - swept.len();
    if skipped > 0 {
        output::step_warn(&format!(
            "skipped {skipped} allocation(s) not spendable by this wallet (not ours)"
        ));
    }

    output::step("broadcasting");
    let broadcast_txid = probe.broadcast(&raw_tx).await?;
    output::step_ok();
    output::kv("recovery tx", &broadcast_txid);
    if broadcast_txid != witness_txid {
        output::step_warn(&format!(
            "broadcast txid {broadcast_txid} != built witness id {witness_txid}"
        ));
    }
    output::note(&format!(
        "{swept_units} units ({} outputs) swept to the pinned host. Once confirmed: \
         re-sync and `colorex maker inventory --btc` to see them as sellable.",
        swept.len()
    ));
    Ok(())
}

async fn maker_accept(
    config: MakerNodeConfig,
    consignment: String,
    contract: Option<String>,
) -> Result<(), Box<dyn std::error::Error>> {
    let base64 = read_consignment(&consignment)?;
    let contract_id = resolve_contract_id(&config, contract).await?;
    accept_inventory_consignment(&config, contract_id, &base64).await?;
    println!("accepted consignment into the maker stash");
    Ok(())
}

/// Send RGB from the maker's wallet to a recipient invoice — the maker analogue
/// of `issuer transfer`. Builds + signs + broadcasts the anchoring tx via
/// `distribute` (the contract + amount are carried by the invoice) and returns
/// the consignment. Run with the daemon STOPPED: it spends the maker's bp-wallet
/// directly, and the chain observer reconciles the spent input + change on the
/// next `maker up`.
async fn maker_transfer(
    config: MakerNodeConfig,
    invoice: String,
    electrum: Option<String>,
    fee: u64,
    out: Option<PathBuf>,
) -> Result<(), Box<dyn std::error::Error>> {
    let rgb = config
        .rgb
        .as_ref()
        .ok_or("no [rgb] config: a wallet is required to transfer")?;
    let electrum_url = electrum
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| rgb.electrum_url.clone());
    if electrum_url.is_empty() {
        return Err("no --electrum given and no [rgb] electrum_url in config".into());
    }
    let backend = rfq_rgb::LibRgbBackend::new(
        rgb.data_dir.clone(),
        rgb.wallet_name.clone(),
        rgb.network.clone(),
        electrum_url,
        rgb.signer.account_file.clone(),
        rgb.signer.password.clone(),
    );
    let (txid, consignment) = backend.distribute(&invoice, fee).await?;
    println!("transfer broadcast: {txid}");
    match out {
        Some(path) => {
            std::fs::write(&path, &consignment)?;
            eprintln!(
                "wrote consignment to {} — hand it to the recipient (they accept after the tx confirms)",
                path.display()
            );
        }
        None => {
            println!("hand this consignment to the recipient (they accept after the tx confirms):");
            println!("{consignment}");
        }
    }
    Ok(())
}

async fn maker_reconsign(
    config: MakerNodeConfig,
    contract: Option<String>,
    outpoint: String,
    out: Option<PathBuf>,
) -> Result<(), Box<dyn std::error::Error>> {
    let contract_id = resolve_contract_id(&config, contract).await?;
    let consignment = reconsign_consignment(&config, contract_id, &outpoint)?;
    match out {
        Some(path) => {
            std::fs::write(&path, &consignment)?;
            eprintln!("wrote consignment to {}", path.display());
        }
        None => println!("{consignment}"),
    }
    Ok(())
}

async fn maker_get_consignment(
    config: MakerNodeConfig,
    quote_id: String,
    out: Option<PathBuf>,
) -> Result<(), Box<dyn std::error::Error>> {
    match fetch_consignment(&config, &quote_id).await? {
        Some(consignment) => match out {
            Some(path) => {
                std::fs::write(&path, &consignment)?;
                eprintln!("wrote consignment to {}", path.display());
            }
            None => println!("{consignment}"),
        },
        None => {
            return Err(format!(
                "no consignment recorded for quote {quote_id} (try `reconsign` to re-derive)"
            )
            .into())
        }
    }
    Ok(())
}

/// Open the maker.db `orders` store from the config's `[rgb]` db path, importing
/// any legacy orders.json first. Orders live in maker.db, so this requires an
/// `[rgb]` section (mock-only makers have no shared order store).
async fn open_order_store(
    config_path: &Path,
) -> Result<rfq_store::SqliteOrderStore, Box<dyn std::error::Error>> {
    let config = load_config(config_path)?;
    let rgb = config
        .rgb
        .as_ref()
        .ok_or("no [rgb] config: standing orders live in maker.db, which needs a wallet")?;
    let db_path = rgb.data_dir.join(&rgb.network).join("maker.db");
    if let Some(parent) = db_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let store = rfq_store::SqliteOrderStore::open(&db_path).await?;
    orders::migrate_orders_json(&store, config_path).await?;
    Ok(store)
}

/// Open the maker.db contract registry from the config's `[rgb]` db path.
async fn open_contract_store(
    config: &MakerNodeConfig,
) -> Result<rfq_store::SqliteContractStore, Box<dyn std::error::Error>> {
    let rgb = config
        .rgb
        .as_ref()
        .ok_or("no [rgb] config: the contract registry lives in maker.db, which needs a wallet")?;
    let db_path = rgb.data_dir.join(&rgb.network).join("maker.db");
    if let Some(parent) = db_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    Ok(rfq_store::SqliteContractStore::open(&db_path).await?)
}

/// Resolve a consignment argument that is EITHER a file path OR the inline base64
/// string. If `value` names an existing file, read it; otherwise treat `value`
/// itself as the base64 — so `--consignment` takes a path or a pasted blob.
fn read_consignment(value: &str) -> Result<String, Box<dyn std::error::Error>> {
    let trimmed = value.trim();
    if std::path::Path::new(trimmed).is_file() {
        Ok(std::fs::read_to_string(trimmed)?.trim().to_owned())
    } else {
        Ok(trimmed.to_owned())
    }
}

/// Resolve which contract a single-asset command operates on. An explicit
/// `--contract`/`--asset` wins; otherwise the registry must hold exactly one
/// contract (the unambiguous default). Zero or many → an actionable error rather
/// than a silent guess. Replaces the old `[rgb] contract_id` default.
async fn resolve_contract_id(
    config: &MakerNodeConfig,
    explicit: Option<String>,
) -> Result<String, Box<dyn std::error::Error>> {
    let registered = open_contract_store(config)
        .await?
        .list()
        .await?
        .into_iter()
        .map(|c| c.contract_id)
        .collect();
    pick_contract(explicit, registered).map_err(Into::into)
}

/// Pure resolution core (testable without a store): explicit id wins; else the
/// registry must hold exactly one contract; zero or many is an actionable error.
fn pick_contract(explicit: Option<String>, registered: Vec<String>) -> Result<String, String> {
    if let Some(id) = explicit.filter(|s| !s.is_empty()) {
        return Ok(id);
    }
    match registered.len() {
        0 => Err("no contracts registered — run `colorex maker contract import <id>` first".into()),
        1 => Ok(registered.into_iter().next().unwrap()),
        n => Err(format!("{n} contracts registered — pass --contract <id> to choose one")),
    }
}

/// All registered contracts as `AssetId`s (empty if none / no `[rgb]` network).
async fn registered_assets(config: &MakerNodeConfig) -> Vec<rfq_types::AssetId> {
    let Some(network) = config.rgb.as_ref().and_then(|r| r.network.parse().ok()) else {
        return Vec::new();
    };
    let Ok(store) = open_contract_store(config).await else {
        return Vec::new();
    };
    store
        .list()
        .await
        .unwrap_or_default()
        .into_iter()
        .map(|c| rfq_types::AssetId {
            network,
            kind: rfq_types::AssetKind::Rgb20,
            id: c.contract_id,
        })
        .collect()
}

/// Read `(ticker, precision)` for `contract_id` from the maker's RGB stock.
/// Errors if the contract isn't in the stock — the signal that it must be minted
/// or its consignment accepted before it can be registered/traded.
fn contract_spec_for(
    config: &MakerNodeConfig,
    contract_id: &str,
) -> Result<(String, u8), Box<dyn std::error::Error>> {
    let r = config
        .rgb
        .as_ref()
        .ok_or("no [rgb] config: a wallet stock is required to read a contract")?;
    let backend = rfq_rgb::LibRgbBackend::new(
        r.data_dir.clone(),
        r.wallet_name.clone(),
        r.network.clone(),
        String::new(),
        std::path::PathBuf::new(),
        String::new(),
    );
    let asset = rfq_types::AssetId {
        network: r
            .network
            .parse()
            .map_err(|_| format!("invalid [rgb] network '{}'", r.network))?,
        kind: rfq_types::AssetKind::Rgb20,
        id: contract_id.to_owned(),
    };
    backend.contract_spec(&asset).map_err(|e| {
        format!("contract {contract_id} not found in the maker's stock ({e}) — mint it or import its consignment first (--consignment)").into()
    })
}

async fn contract_import(
    config: MakerNodeConfig,
    id: String,
    consignment: Option<String>,
) -> Result<(), Box<dyn std::error::Error>> {
    // Validate the id parses before any side effects.
    id.parse::<rfq_rgb::ContractId>()
        .map_err(|e| format!("invalid contract id {id}: {e}"))?;

    // `--consignment` folds in `accept`: absorb the contract into the stock first,
    // so a freshly-handed-over asset registers in one step. The arg is a file path
    // OR the inline base64.
    if let Some(consignment) = consignment {
        let base64 = read_consignment(&consignment)?;
        accept_inventory_consignment(&config, id.clone(), &base64).await?;
        output::step_ok_with(&format!(
            "accepted consignment for {}",
            init::truncate_contract(&id)
        ));
    }

    // Cache ticker + precision from the stock (also verifies it's actually there).
    let (ticker, precision) = contract_spec_for(&config, &id)?;
    let network = config
        .rgb
        .as_ref()
        .map(|r| r.network.clone())
        .unwrap_or_default();

    let store = open_contract_store(&config).await?;
    let existed = store.get(&id).await?.is_some();
    store
        .upsert(rfq_store::ContractRecord {
            contract_id: id.clone(),
            ticker: ticker.clone(),
            precision,
            network,
            added_at_ms: now_ms(),
        })
        .await?;
    let verb = if existed { "updated" } else { "imported" };
    println!("{verb} contract {ticker} ({})", init::truncate_contract(&id));
    Ok(())
}

async fn contract_list(config: MakerNodeConfig) -> Result<(), Box<dyn std::error::Error>> {
    let store = open_contract_store(&config).await?;
    let contracts = store.list().await?;
    if contracts.is_empty() {
        output::note("no contracts registered — add one with: colorex maker contract import <id>");
        return Ok(());
    }
    println!("registered contracts ({})", contracts.len());
    for c in contracts {
        output::kv(
            &format!("{} · precision {}", c.ticker, c.precision),
            &c.contract_id,
        );
    }
    Ok(())
}

async fn contract_remove(
    config: MakerNodeConfig,
    id: String,
) -> Result<(), Box<dyn std::error::Error>> {
    let store = open_contract_store(&config).await?;
    if store.remove(&id).await? {
        println!("removed contract {}", init::truncate_contract(&id));
    } else {
        return Err(format!("no registered contract {id}").into());
    }
    Ok(())
}

async fn order_create(
    config_path: &Path,
    side: String,
    asset: Option<String>,
    price: u64,
    size: u64,
    mirror: bool,
    mirror_spread_bps: u16,
) -> Result<(), Box<dyn std::error::Error>> {
    let parsed_side = orders::parse_side(&side)
        .ok_or_else(|| format!("invalid --side '{side}': expected 'buy' or 'sell'"))?;
    let config = load_config(config_path)?;
    let asset_id = resolve_contract_id(&config, asset).await?;
    if mirror && mirror_spread_bps == 0 {
        output::step_warn(
            "--mirror with --mirror-spread-bps 0 ping-pongs at the same price (no margin)",
        );
    }
    let store = open_order_store(config_path).await?;
    let order = orders::new_order(&side, asset_id.clone(), price, size, mirror, mirror_spread_bps);
    let id = order.id.clone();
    let replaced = store.get(&order.asset_id, &order.side).await?;
    store.upsert(order).await?;
    match replaced {
        Some(old) => println!("created order {id} (replaced {})", old.id),
        None => println!("created order {id}"),
    }
    // Non-blocking heads-up if on-hand inventory can't back the order.
    warn_low_balance(&config, &parsed_side, &asset_id, price, size).await;
    Ok(())
}

/// Warn (non-blocking) if the maker's cached inventory can't back a full-size
/// quote for this order. Orders are just terms — the maker declines quotes it
/// can't fund — so this never blocks creation. Reads maker.db directly (WAL-safe;
/// no bp-wallet file access, so it's fine while the daemon runs).
async fn warn_low_balance(
    config: &MakerNodeConfig,
    side: &Side,
    asset_id: &str,
    price: u64,
    size: u64,
) {
    let Some(rgb) = config.rgb.as_ref() else {
        return;
    };
    let db_path = rgb.data_dir.join(&rgb.network).join("maker.db");
    if !db_path.exists() {
        return;
    }
    match side {
        // A buy order SELLS RGB — the maker needs the asset on hand.
        Side::Buy => {
            let Ok(network) = rgb.network.parse::<rfq_types::BitcoinNetwork>() else {
                return;
            };
            let Ok(store) = rfq_store::SqliteInventoryStore::open(&db_path).await else {
                return;
            };
            let asset = rfq_types::AssetId {
                network,
                kind: rfq_types::AssetKind::Rgb20,
                id: asset_id.to_owned(),
            };
            let avail: u64 = store
                .list_available(&asset)
                .await
                .iter()
                .map(|u| u.amount)
                .sum();
            if avail < size {
                output::step_warn(&format!(
                    "maker holds {avail} units of this asset (< order size {size}) — a buy order \
                     SELLS RGB, so quotes above your balance will decline until funded"
                ));
            }
        }
        // A sell order PAYS BTC — the maker needs BTC in its pool.
        Side::Sell => {
            let Ok(store) = rfq_store::SqliteBtcInventoryStore::open(&db_path).await else {
                return;
            };
            let avail: u64 = store.list_available().await.iter().map(|u| u.value_sats).sum();
            let need = size.saturating_mul(price);
            if avail < need {
                output::step_warn(&format!(
                    "maker BTC pool is {avail} sats (< ~{need} for a full {size}-unit quote) — a \
                     sell order PAYS BTC, so large quotes will decline until funded"
                ));
            }
        }
    }
}

async fn order_list(config_path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let store = open_order_store(config_path).await?;
    let orders = store.list().await?;
    if orders.is_empty() {
        println!("no standing orders (maker.db)");
        return Ok(());
    }
    for o in &orders {
        let mirror = if o.mirror {
            format!("  mirror=on spread_bps={}", o.mirror_spread_bps)
        } else {
            String::new()
        };
        println!(
            "{}  side={}  asset={}  price/unit={}  size={}{mirror}",
            o.id, o.side, o.asset_id, o.price, o.size
        );
    }
    Ok(())
}

async fn order_cancel(config_path: &Path, id: &str) -> Result<(), Box<dyn std::error::Error>> {
    let store = open_order_store(config_path).await?;
    if !store.cancel(id).await? {
        return Err(format!("no order with id '{id}'").into());
    }
    println!("cancelled order {id}");
    Ok(())
}

async fn order_clear(config_path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let store = open_order_store(config_path).await?;
    let n = store.clear().await?;
    println!("cleared {n} order(s)");
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn issuer_issue(
    common: WalletCommon,
    ticker: String,
    asset_name: String,
    precision: u8,
    supply: u64,
    details: Option<String>,
    seal: Option<String>,
    issuer: String,
) -> Result<(), Box<dyn std::error::Error>> {
    let backend = resolve_named(common.into_input())?.backend();
    let genesis_seal = match seal {
        Some(s) => s,
        None => backend.pick_genesis_seal()?,
    };
    let id = backend.issue_contract(
        &ticker,
        &asset_name,
        details.as_deref(),
        precision,
        supply,
        &genesis_seal,
        &issuer,
    )?;
    println!("issued contract: {id}");
    println!("  genesis seal: {genesis_seal}");
    println!("  register it on a maker with: colorex maker contract import {id}");
    Ok(())
}

fn issuer_contracts(common: WalletCommon) -> Result<(), Box<dyn std::error::Error>> {
    let backend = resolve_named(common.into_input())?.backend();
    for line in backend.list_contracts()? {
        println!("{line}");
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn issuer_transfer(
    common: WalletCommon,
    invoice: String,
    electrum: Option<String>,
    account_file: Option<PathBuf>,
    password: Option<String>,
    fee: u64,
) -> Result<(), Box<dyn std::error::Error>> {
    let input = WalletInput {
        electrum_url: electrum,
        account_file,
        password,
        ..common.into_input()
    };
    let backend = resolve_named(input)?.backend();
    let (txid, consignment) = backend.distribute(&invoice, fee).await?;
    println!("transfer broadcast: {txid}");
    println!("hand this consignment to the recipient (they accept after the tx confirms):");
    println!("{consignment}");
    Ok(())
}

async fn run(
    config: MakerNodeConfig,
    config_path: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    let _client = RfqClient::new(parse_broker_url(&config)?);

    let runtime = build_runtime(&config).await?;
    let order_store = runtime.order_store;
    // One-time import of any legacy orders.json into the maker.db `orders` table.
    orders::migrate_orders_json(order_store.as_ref(), config_path).await?;

    // Seed the maker's price policy from the standing orders in maker.db.
    let standing = order_store.list().await?;
    let order_count = standing.len();
    let maker = runtime.maker.with_price_policy(orders::price_policy(&standing));
    let chain_observer_deps = runtime.chain_observer;
    let app = maker_app(maker.clone());
    let listener = TcpListener::bind(&config.maker.listen_addr).await?;
    let (shutdown_tx, shutdown_rx) = oneshot::channel();

    let broker_ws = broker_client::broker_ws_url(&config.maker.broker_url);
    let network = config
        .rgb
        .as_ref()
        .map(|r| r.network.as_str())
        .unwrap_or("mock");
    let node_short = config
        .maker
        .node_id
        .get(..8)
        .unwrap_or(&config.maker.node_id);

    println!("colorex maker up");
    output::info(&format!("node {node_short}… on {network}"));
    output::info(&format!("broker {}", config.maker.broker_url));
    output::info(&format!("standing orders {order_count} (maker.db)"));
    // Funding addresses, so the operator can top up without a separate command.
    if let Ok((btc, anchor)) = maker_funding_addresses(&config) {
        output::kv("fund BTC  · keychain 0 ", &btc);
        output::kv("fund RGB  · keychain 10", &anchor);
    }
    output::step("chain observer");
    if chain_observer_deps.is_some() {
        output::step_ok_with(&format!("every {:?}", config.intervals.chain_observer));
    } else {
        output::step_skip();
    }
    output::step("http server");
    output::step_ok_with(&config.maker.listen_addr);
    output::step("broker stream");
    output::step_ok_with(&broker_ws);

    // Hot-reload standing orders so `order create`/`cancel` take effect without a
    // maker restart.
    let order_reload_task = spawn_order_reload_loop(
        maker.clone(),
        order_store.clone(),
        std::time::Duration::from_secs(5),
    );
    let strategy_task =
        spawn_strategy_loop(maker.clone(), order_store.clone(), config.intervals.strategy);
    let cleanup_task = spawn_cleanup_loop(maker.clone(), config.intervals.cleanup);
    let rebalance_task = spawn_rebalance_loop(
        maker.clone(),
        config.intervals.rebalance,
        (&config.rebalance).into(),
    );
    let chain_observer_task = chain_observer_deps
        .map(|deps| spawn_chain_observer_loop(maker.clone(), deps, config.intervals.chain_observer));
    let server_task = tokio::spawn(async move {
        let result = axum::serve(listener, app)
            .with_graceful_shutdown(async {
                let _ = shutdown_rx.await;
            })
            .await;
        if let Err(error) = result {
            eprintln!("colorex http server error: {error}");
        }
    });

    // Dial the broker and auto-register over WebSocket — the broker then pushes
    // quote/accept/settle requests over it (no `BROKER_MAKER` config needed).
    let broker_task = tokio::spawn(broker_client::run_broker_stream(
        broker_ws,
        MakerId(config.maker.node_id.clone()),
        maker.clone(),
    ));

    tokio::signal::ctrl_c().await?;
    println!("colorex maker shutting down");

    let _ = shutdown_tx.send(());
    order_reload_task.abort();
    strategy_task.abort();
    cleanup_task.abort();
    rebalance_task.abort();
    broker_task.abort();
    if let Some(t) = &chain_observer_task {
        t.abort();
    }
    let _ = server_task.await;
    let _ = broker_task.await;
    let _ = order_reload_task.await;
    let _ = strategy_task.await;
    let _ = cleanup_task.await;
    let _ = rebalance_task.await;
    if let Some(t) = chain_observer_task {
        let _ = t.await;
    }

    Ok(())
}

async fn health(config: MakerNodeConfig) -> Result<(), Box<dyn std::error::Error>> {
    let broker_status = broker_health_status(&config).await?;

    println!("colorex maker health ok");
    println!("node_id={}", config.maker.node_id);
    println!("broker_url={}", config.maker.broker_url);
    println!("broker_status={broker_status}");
    println!(
        "maker_runtime={}",
        std::any::type_name::<rfq_maker::Maker>()
    );

    Ok(())
}

async fn broker_health_status(
    config: &MakerNodeConfig,
) -> Result<String, Box<dyn std::error::Error>> {
    let client = RfqClient::new(parse_broker_url(config)?);
    let response = client.health().await?;

    Ok(response.status)
}

async fn inventory(
    config: MakerNodeConfig,
    btc: bool,
    electrum: Option<String>,
) -> Result<(), Box<dyn std::error::Error>> {
    let _broker_url = parse_broker_url(&config)?;
    let maker = build_maker(&config).await?;

    println!("colorex maker inventory");
    println!("node_id={}", config.maker.node_id);

    // Per-contract snapshots from the registry; fall back to the aggregate view
    // when nothing is registered (mock maker / fresh install pre-migration).
    let contracts = match open_contract_store(&config).await {
        Ok(store) => store.list().await.unwrap_or_default(),
        Err(_) => Vec::new(),
    };
    let network = config.rgb.as_ref().and_then(|r| r.network.parse().ok());
    match network {
        Some(network) if !contracts.is_empty() => {
            for c in &contracts {
                let asset = rfq_types::AssetId {
                    network,
                    kind: rfq_types::AssetKind::Rgb20,
                    id: c.contract_id.clone(),
                };
                let snapshot = maker.inventory_summary_for(&asset).await;
                println!();
                println!("[{} · {}]", c.ticker, init::truncate_contract(&c.contract_id));
                print_inventory_snapshot(&snapshot, Some(&(c.ticker.clone(), c.precision)));
            }
        }
        _ => {
            // No registry (mock maker / fresh install): aggregate, raw amounts.
            let snapshot = maker.inventory_summary().await;
            print_inventory_snapshot(&snapshot, None);
        }
    }

    inventory_orders(&config).await?;

    if btc {
        println!();
        inventory_btc(&config, electrum).await?;
    }

    Ok(())
}

/// Print the standing orders with each one's cumulative FILLED amount (per
/// `(asset, side)`, lifetime). Reads the maker.db `orders` + `fills` tables
/// directly (works whether or not the daemon is running). No-op for a mock /
/// no-`[rgb]` maker, which has no shared store.
async fn inventory_orders(config: &MakerNodeConfig) -> Result<(), Box<dyn std::error::Error>> {
    use rfq_store::{FillStore as _, OrderStore as _};
    let Some(rgb) = config.rgb.as_ref() else {
        return Ok(());
    };
    let db_path = rgb.data_dir.join(&rgb.network).join("maker.db");
    if !db_path.exists() {
        return Ok(());
    }
    let order_store = rfq_store::SqliteOrderStore::open(&db_path).await?;
    let orders = order_store.list().await?;
    if orders.is_empty() {
        return Ok(());
    }
    let fills = rfq_store::SqliteFillStore::open(&db_path).await?;
    // Per-asset ticker/precision from the registry, so each order's FILLED amount
    // renders in its own contract's units (orders are multi-asset).
    let specs: std::collections::HashMap<String, (String, u8)> =
        match open_contract_store(config).await {
            Ok(s) => s
                .list()
                .await
                .unwrap_or_default()
                .into_iter()
                .map(|c| (c.contract_id, (c.ticker, c.precision)))
                .collect(),
            Err(_) => Default::default(),
        };

    println!();
    println!("standing orders ({})", orders.len());
    for o in &orders {
        let filled = match orders::parse_side(&o.side) {
            Some(s) => fills.filled_for(&o.asset_id, &s, 0).await.unwrap_or(0),
            None => 0,
        };
        let filled_str = match specs.get(&o.asset_id) {
            Some((ticker, precision)) => {
                format!("{ticker} {}", rfq_types::format_amount(filled, *precision))
            }
            None => filled.to_string(),
        };
        let mirror = if o.mirror {
            format!("  mirror=on/{}bps", o.mirror_spread_bps)
        } else {
            String::new()
        };
        println!(
            "    {} {}  price/unit={}  size={}  FILLED={}{mirror}",
            o.side, o.id, o.price, o.size, filled_str
        );
    }
    Ok(())
}

/// Dump the BTC inventory across all three layers so a "maker has no BTC
/// inventory" report can be localized to the layer that drifted:
///
/// - **L1/L2 on-chain wallet** (bp-wallet via electrum) — the real UTXO set.
/// - **RGB-exclusion filter** (`list_btc_only_utxos`) — which of those are
///   spendable for funding (not carrying an RGB allocation).
/// - **L3 SQLite cache** (`maker.db` `btc_utxos`) — what coin-selection
///   actually reads, with per-status counts (only `Available` is selectable).
///
/// Read-only. The drift summary at the end tells you which hypothesis holds:
/// chain-empty → depletion/sync; chain-has-BTC-but-filter-empty → RGB filter;
/// filter-has-BTC-but-cache-Available-empty → reservation leak / ingest gap.
async fn inventory_btc(
    config: &MakerNodeConfig,
    electrum: Option<String>,
) -> Result<(), Box<dyn std::error::Error>> {
    use rfq_store::{BtcInventoryStore, SqliteBtcInventoryStore};
    use rfq_types::BtcInventoryStatus;

    let rgb = config
        .rgb
        .as_ref()
        .ok_or("no [rgb] config: a wallet is required for --btc inventory")?;

    let electrum_url = electrum
        .filter(|s| !s.is_empty())
        .or_else(|| Some(rgb.electrum_url.clone()).filter(|s| !s.is_empty()));

    println!("colorex maker btc-inventory ({})", rgb.network);

    // --- L1/L2: on-chain wallet (sync against electrum when available) ---
    let backend = rfq_rgb::LibRgbBackend::new(
        rgb.data_dir.clone(),
        rgb.wallet_name.clone(),
        rgb.network.clone(),
        electrum_url.clone().unwrap_or_default(),
        std::path::PathBuf::new(),
        String::new(),
    );
    match &electrum_url {
        Some(url) => match backend.sync_wallet().await {
            Ok(()) => output::note(&format!("on-chain wallet — synced via {url}")),
            Err(e) => output::step_warn(&format!("sync failed (showing stale cache): {e}")),
        },
        None => output::note("on-chain wallet — STALE (no --electrum / [rgb] electrum_url)"),
    }

    let raw = backend.wallet_balance().await?;
    let (mut k0_sats, mut k0_n, mut other_sats, mut other_n) = (0u64, 0usize, 0u64, 0usize);
    for u in &raw {
        if u.keychain == 0 {
            k0_sats += u.sats;
            k0_n += 1;
        } else {
            other_sats += u.sats;
            other_n += 1;
        }
    }
    output::kv(
        "keychain 0 (BTC funding)",
        &format!("{k0_n} utxos · {k0_sats} sats"),
    );
    output::kv(
        "other keychains (RGB anchor)",
        &format!("{other_n} utxos · {other_sats} sats"),
    );
    for u in &raw {
        println!("    {}:{}  {} sats  k{}/{}", u.txid, u.vout, u.sats, u.keychain, u.index);
    }

    // --- RGB-exclusion filter: which on-chain UTXOs are fundable ---
    println!();
    let assets = registered_assets(config).await;
    let filtered = if assets.is_empty() {
        output::note("RGB filter skipped — no contracts registered");
        None
    } else {
        let now = now_ms();
        let btc_only = backend.list_btc_only_utxos(&assets, now).await?;
        let only_total: u64 = btc_only.iter().map(|u| u.value_sats).sum();
        output::kv(
            "BTC-only (fundable)",
            &format!("{} utxos · {} sats", btc_only.len(), only_total),
        );
        // Anything on-chain but not in btc_only was excluded as RGB-bearing.
        let keep: std::collections::HashSet<(String, u32)> = btc_only
            .iter()
            .map(|u| (u.outpoint.txid.clone(), u.outpoint.vout))
            .collect();
        let excluded: Vec<_> = raw
            .iter()
            .filter(|u| !keep.contains(&(u.txid.clone(), u.vout)))
            .collect();
        if excluded.is_empty() {
            output::note("excluded as RGB-bearing: none");
        } else {
            output::note(&format!("excluded as RGB-bearing: {}", excluded.len()));
            for u in excluded {
                println!("    {}:{}  {} sats  k{}/{}", u.txid, u.vout, u.sats, u.keychain, u.index);
            }
        }
        Some(only_total)
    };

    // --- L3: SQLite cache (what coin-selection reads) ---
    println!();
    let db_path = rgb.data_dir.join(&rgb.network).join("maker.db");
    let store = SqliteBtcInventoryStore::open(&db_path).await?;
    let rows = store.list_all().await;
    output::kv("SQLite cache (maker.db btc_utxos)", &db_path.display().to_string());
    let now = now_ms();
    let (mut avail_s, mut avail_n) = (0u64, 0usize);
    let (mut resv_s, mut resv_n) = (0u64, 0usize);
    let (mut pend_s, mut pend_n) = (0u64, 0usize);
    let (mut spent_s, mut spent_n) = (0u64, 0usize);
    let (mut inval_s, mut inval_n) = (0u64, 0usize);
    let mut detail: Vec<String> = Vec::new();
    for u in &rows {
        let op = format!("{}:{}", u.outpoint.txid, u.outpoint.vout);
        match &u.status {
            BtcInventoryStatus::Available => {
                avail_s += u.value_sats;
                avail_n += 1;
            }
            BtcInventoryStatus::Reserved {
                quote_id,
                expires_at_ms,
                ..
            } => {
                resv_s += u.value_sats;
                resv_n += 1;
                let flag = if *expires_at_ms <= now {
                    "EXPIRED — should be released".to_string()
                } else {
                    format!("expires_in={}ms", expires_at_ms - now)
                };
                detail.push(format!(
                    "    RESERVED {op}  {} sats  quote={} {flag}",
                    u.value_sats, quote_id.0
                ));
            }
            BtcInventoryStatus::PendingBitcoinConfirm { witness_txid, .. } => {
                pend_s += u.value_sats;
                pend_n += 1;
                detail.push(format!(
                    "    PENDING  {op}  {} sats  witness={witness_txid}",
                    u.value_sats
                ));
            }
            BtcInventoryStatus::Spent { .. } => {
                spent_s += u.value_sats;
                spent_n += 1;
            }
            BtcInventoryStatus::Invalid { reason } => {
                inval_s += u.value_sats;
                inval_n += 1;
                detail.push(format!("    INVALID  {op}  {} sats  reason={reason}", u.value_sats));
            }
        }
    }
    println!("    Available:  {avail_n} utxos · {avail_s} sats   <-- selectable for funding");
    println!("    Reserved:   {resv_n} utxos · {resv_s} sats");
    println!("    Pending:    {pend_n} utxos · {pend_s} sats");
    println!("    Spent:      {spent_n} utxos · {spent_s} sats");
    println!("    Invalid:    {inval_n} utxos · {inval_s} sats");
    for line in &detail {
        println!("{line}");
    }

    // --- Drift summary: localize the fault ---
    println!();
    output::kv("DRIFT — on-chain BTC-only", &format!("{} sats", filtered.unwrap_or(k0_sats)));
    output::kv("DRIFT — SQLite Available", &format!("{avail_s} sats"));
    let chain_fundable = filtered.unwrap_or(k0_sats);
    if chain_fundable == 0 && k0_sats == 0 {
        output::note("=> chain k0 empty: DEPLETION or sync failure (check `maker addresses --electrum`)");
    } else if chain_fundable == 0 && k0_sats > 0 {
        output::note("=> k0 has BTC but filter returns none: RGB-FILTER over-inclusion");
    } else if avail_s == 0 && chain_fundable > 0 {
        output::note("=> fundable on-chain but SQLite Available empty: RESERVATION LEAK / INGEST GAP");
    } else if avail_s < chain_fundable {
        output::note("=> SQLite Available < on-chain fundable: partial ingest/reservation drift");
    } else {
        output::note("=> layers agree: inventory looks healthy");
    }

    // --- RGB allocations: stock vs wallet vs CHAIN ---
    // The stock's `FilterIncludeAll` lists every allocation it has ever seen,
    // INCLUDING spent ones — so "not in wallet" alone can't tell stranded from
    // spent. We probe the chain per outpoint to split them:
    //   - SPENT on-chain          → history, ignore
    //   - UNSPENT + in wallet      → sellable (should be Available inventory)
    //   - UNSPENT + NOT in wallet  → TRULY STRANDED (the recoverable bug)
    for asset in registered_assets(config).await {
        println!();
        output::kv("contract", &asset.id);
        let allocs = backend.debug_contract_allocations(&asset).await?;
        output::kv("RGB stock allocations (incl. spent)", &format!("{}", allocs.len()));

        let probe = match &electrum_url {
            Some(url) => rfq_btc::ElectrumClient::connect(url).ok(),
            None => None,
        };
        if probe.is_none() {
            output::step_warn("no electrum — cannot probe on-chain spent status; showing wallet view only");
        }

        let (mut sellable_amt, mut sellable_n) = (0u64, 0usize);
        let (mut stranded_amt, mut stranded_n) = (0u64, 0usize);
        let (mut spent_n, mut unknown_n) = (0usize, 0usize);
        for (op, amount, in_wallet) in &allocs {
            let unspent = match &probe {
                Some(c) => c.outpoint_unspent(op).await.ok(),
                None => None,
            };
            let tag = match (unspent, in_wallet) {
                (Some(true), true) => {
                    sellable_amt += amount;
                    sellable_n += 1;
                    "UNSPENT · in-wallet (sellable)"
                }
                (Some(true), false) => {
                    stranded_amt += amount;
                    stranded_n += 1;
                    "UNSPENT · NOT in wallet — STRANDED (recoverable)"
                }
                (Some(false), _) => {
                    spent_n += 1;
                    continue; // spent history — don't print the noise
                }
                (None, true) => "in-wallet (chain unknown)",
                (None, false) => {
                    unknown_n += 1;
                    "NOT in wallet (chain unknown)"
                }
            };
            println!("    {}:{}  {} units  {tag}", op.txid, op.vout, amount);
        }

        output::kv("  sellable (unspent, in-wallet)", &format!("{sellable_n} · {sellable_amt} units"));
        output::kv("  STRANDED (unspent, off-wallet)", &format!("{stranded_n} · {stranded_amt} units"));
        output::kv("  spent (history, hidden)", &format!("{spent_n}"));
        if unknown_n > 0 {
            output::kv("  chain-unknown (no probe)", &format!("{unknown_n}"));
        }
        if probe.is_some() {
            if stranded_amt > 0 {
                output::note(&format!(
                    "=> RGB: {stranded_amt} units STRANDED on tapret outputs bp-wallet never tracked — RECOVERABLE (index-reuse recognition bug)"
                ));
            } else if sellable_amt > 0 {
                output::note("=> RGB: sellable allocation present but store shows 0 available — STORE/INGEST stale (re-sync/reconcile)");
            } else {
                output::note("=> RGB: no unspent allocation on-chain — genuine DEPLETION (re-fund the maker)");
            }
        }
    }

    Ok(())
}

fn parse_broker_url(config: &MakerNodeConfig) -> Result<Url, Box<dyn std::error::Error>> {
    Url::parse(&config.maker.broker_url).map_err(|e| e.into())
}

fn print_inventory_snapshot(snapshot: &InventorySnapshot, spec: Option<&(String, u8)>) {
    let amt = |v: u64| match spec {
        Some((ticker, precision)) => format!("{ticker} {}", rfq_types::format_amount(v, *precision)),
        None => v.to_string(),
    };
    // `total_*` is current controllable inventory (available + reserved +
    // pending); spent allocations are excluded upstream. `spent_amount` is NOT
    // printed: in a UTXO change-chain it sums superseded links and balloons to a
    // phantom multiple of real holdings — `spent_allocations` (a count of
    // settled-away allocations) is the only spent diagnostic worth surfacing.
    println!("total_amount={}", amt(snapshot.total_amount));
    println!("available_amount={}", amt(snapshot.available_amount));
    println!("reserved_amount={}", amt(snapshot.reserved_amount));
    println!("total_allocations={}", snapshot.total_allocations);
    println!("available_allocations={}", snapshot.available_allocations);
    println!("reserved_allocations={}", snapshot.reserved_allocations);
    println!("spent_allocations={}", snapshot.spent_allocations);
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn cli_parses_commands() {
        assert_eq!(
            Cli::parse_from(["colorex", "maker", "up"]).command,
            TopCommand::Maker { cmd: MakerCmd::Up },
        );
        assert_eq!(
            Cli::parse_from(["colorex", "maker", "health"]).command,
            TopCommand::Maker {
                cmd: MakerCmd::Health
            },
        );
        assert_eq!(
            Cli::parse_from(["colorex", "maker", "inventory"]).command,
            TopCommand::Maker {
                cmd: MakerCmd::Inventory {
                    btc: false,
                    electrum: None,
                }
            },
        );
        assert_eq!(
            Cli::parse_from(["colorex", "maker", "init", "--force"]).command,
            TopCommand::Maker {
                cmd: MakerCmd::Init(init::InitArgs { force: true, systemd: false }),
            },
        );
    }

    #[test]
    fn cli_accepts_global_config_flag() {
        let cli = Cli::parse_from(["colorex", "--config", "/tmp/x.toml", "maker", "up"]);
        assert_eq!(cli.config, Some(PathBuf::from("/tmp/x.toml")));
    }

    #[test]
    fn cli_parses_contract_subcommands() {
        assert_eq!(
            Cli::parse_from(["colorex", "maker", "contract", "import", "rgb:abc"]).command,
            TopCommand::Maker {
                cmd: MakerCmd::Contract {
                    cmd: ContractCmd::Import {
                        id: "rgb:abc".into(),
                        consignment: None,
                    }
                }
            },
        );
        assert_eq!(
            Cli::parse_from(["colorex", "maker", "contract", "list"]).command,
            TopCommand::Maker {
                cmd: MakerCmd::Contract {
                    cmd: ContractCmd::List
                }
            },
        );
        assert_eq!(
            Cli::parse_from(["colorex", "maker", "contract", "remove", "rgb:abc"]).command,
            TopCommand::Maker {
                cmd: MakerCmd::Contract {
                    cmd: ContractCmd::Remove { id: "rgb:abc".into() }
                }
            },
        );
    }

    #[test]
    fn cli_parses_maker_wallet_group() {
        // Funding/wallet ops now live under `maker wallet`.
        assert_eq!(
            Cli::parse_from(["colorex", "maker", "wallet", "addresses"]).command,
            TopCommand::Maker {
                cmd: MakerCmd::Wallet {
                    cmd: MakerWalletCmd::Addresses
                }
            },
        );
        assert_eq!(
            Cli::parse_from(["colorex", "maker", "wallet", "invoice", "--amount", "100"]).command,
            TopCommand::Maker {
                cmd: MakerCmd::Wallet {
                    cmd: MakerWalletCmd::Invoice {
                        contract: None,
                        amount: 100,
                    }
                }
            },
        );
        assert_eq!(
            Cli::parse_from([
                "colorex", "maker", "wallet", "transfer", "--invoice", "rgb:inv", "--fee", "500",
            ])
            .command,
            TopCommand::Maker {
                cmd: MakerCmd::Wallet {
                    cmd: MakerWalletCmd::Transfer {
                        invoice: "rgb:inv".into(),
                        electrum: None,
                        fee: 500,
                        out: None,
                    }
                }
            },
        );
        // The old flat paths are gone — `maker addresses` must NOT parse.
        assert!(Cli::try_parse_from(["colorex", "maker", "addresses"]).is_err());
    }

    #[test]
    fn pick_contract_resolution() {
        // Explicit id always wins.
        assert_eq!(
            pick_contract(Some("rgb:x".into()), vec!["rgb:a".into()]).unwrap(),
            "rgb:x"
        );
        // Empty explicit falls through to the registry.
        assert_eq!(
            pick_contract(Some(String::new()), vec!["rgb:a".into()]).unwrap(),
            "rgb:a"
        );
        // Sole registered contract is the unambiguous default.
        assert_eq!(pick_contract(None, vec!["rgb:a".into()]).unwrap(), "rgb:a");
        // Zero and many are errors (must import / must disambiguate).
        assert!(pick_contract(None, vec![]).is_err());
        assert!(pick_contract(None, vec!["rgb:a".into(), "rgb:b".into()]).is_err());
    }

    #[test]
    fn cli_definition_is_valid() {
        Cli::command().debug_assert();
    }
}
