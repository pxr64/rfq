use std::path::{Path, PathBuf};

use clap::{Args, Parser, Subcommand};
use maker_node::{
    broker_client, build_maker, build_runtime, create_inventory_invoice, init, maker_app, orders,
    fetch_consignment, output, reconsign_consignment, spawn_chain_observer_loop,
    spawn_cleanup_loop, spawn_rebalance_loop, MakerNodeConfig,
};
use colorex_wallet::{resolve_named, resolve_wallet, WalletConfig, WalletInput};
use rfq_rgb::RgbBackend;
use rfq_client::{RfqClient, Url};
use rfq_types::{InventorySnapshot, MakerId};
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
    Inventory,
    /// Mint an RGB invoice to receive inventory from an issuer.
    Invoice {
        /// Amount (in smallest RGB units) the invoice requests.
        #[arg(long)]
        amount: u64,
    },
    /// Manage standing orders — the prices the maker quotes per (asset, side).
    Order {
        #[command(subcommand)]
        cmd: OrderCmd,
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
    },
    /// List standing orders.
    List,
    /// Cancel a standing order by id.
    Cancel {
        /// Order id (from `order list`).
        id: String,
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
            MakerCmd::Inventory => inventory(load_config(&config_path)?).await,
            MakerCmd::Invoice { amount } => maker_invoice(load_config(&config_path)?, amount).await,
            MakerCmd::Order { cmd } => match cmd {
                OrderCmd::Create {
                    side,
                    asset,
                    price,
                    size,
                } => order_create(&config_path, side, asset, price, size),
                OrderCmd::List => order_list(&config_path),
                OrderCmd::Cancel { id } => order_cancel(&config_path, &id),
            },
            MakerCmd::Reconsign {
                contract,
                outpoint,
                out,
            } => maker_reconsign(load_config(&config_path)?, contract, outpoint, out),
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
    print!("{}", colorex_wallet::render_balance(&utxos));
    Ok(())
}

async fn maker_invoice(
    config: MakerNodeConfig,
    amount: u64,
) -> Result<(), Box<dyn std::error::Error>> {
    let invoice = create_inventory_invoice(&config, amount).await?;
    println!("{invoice}");
    Ok(())
}

fn maker_reconsign(
    config: MakerNodeConfig,
    contract: Option<String>,
    outpoint: String,
    out: Option<PathBuf>,
) -> Result<(), Box<dyn std::error::Error>> {
    let consignment = reconsign_consignment(&config, contract, &outpoint)?;
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

fn order_create(
    config_path: &Path,
    side: String,
    asset: Option<String>,
    price: u64,
    size: u64,
) -> Result<(), Box<dyn std::error::Error>> {
    if orders::parse_side(&side).is_none() {
        return Err(format!("invalid --side '{side}': expected 'buy' or 'sell'").into());
    }
    let asset_id = match asset {
        Some(a) => a,
        None => load_config(config_path)?
            .rgb
            .map(|r| r.contract_id)
            .filter(|id| !id.is_empty())
            .ok_or("no --asset given and no [rgb] contract_id in config")?,
    };
    let path = orders::OrderBook::path_for(config_path);
    let mut book = orders::OrderBook::load(&path)?;
    let order = orders::new_order(&side, asset_id, price, size);
    let id = order.id.clone();
    match book.upsert(order) {
        Some(old) => println!("created order {id} (replaced {old})"),
        None => println!("created order {id}"),
    }
    book.save(&path)?;
    Ok(())
}

fn order_list(config_path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let path = orders::OrderBook::path_for(config_path);
    let book = orders::OrderBook::load(&path)?;
    if book.orders.is_empty() {
        println!("no standing orders ({})", path.display());
        return Ok(());
    }
    for o in &book.orders {
        println!(
            "{}  side={}  asset={}  price/unit={}  size={}",
            o.id, o.side, o.asset_id, o.price, o.size
        );
    }
    Ok(())
}

fn order_cancel(config_path: &Path, id: &str) -> Result<(), Box<dyn std::error::Error>> {
    let path = orders::OrderBook::path_for(config_path);
    let mut book = orders::OrderBook::load(&path)?;
    if !book.cancel(id) {
        return Err(format!("no order with id '{id}'").into());
    }
    book.save(&path)?;
    println!("cancelled order {id}");
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
    println!("  put this contract_id in maker.toml / taker.toml [rgb]");
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

    // Load the operator's standing orders and feed their prices into the maker.
    let order_path = orders::OrderBook::path_for(config_path);
    let book = orders::OrderBook::load(&order_path)?;
    let order_count = book.orders.len();

    let runtime = build_runtime(&config).await?;
    let maker = runtime.maker.with_price_policy(book.price_policy());
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
    output::info(&format!(
        "standing orders {order_count} ({})",
        order_path.display()
    ));
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
    cleanup_task.abort();
    rebalance_task.abort();
    broker_task.abort();
    if let Some(t) = &chain_observer_task {
        t.abort();
    }
    let _ = server_task.await;
    let _ = broker_task.await;
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

async fn inventory(config: MakerNodeConfig) -> Result<(), Box<dyn std::error::Error>> {
    let _broker_url = parse_broker_url(&config)?;
    let maker = build_maker(&config).await?;
    let snapshot = maker.inventory_summary().await;

    println!("colorex maker inventory");
    println!("node_id={}", config.maker.node_id);
    print_inventory_snapshot(&snapshot, maker_contract_spec(&config).as_ref());

    Ok(())
}

/// Best-effort ticker + precision for the configured contract, for display.
/// `None` (mock / no contract id) → raw amounts.
fn maker_contract_spec(config: &MakerNodeConfig) -> Option<(String, u8)> {
    let r = config.rgb.as_ref()?;
    if r.contract_id.is_empty() {
        return None;
    }
    let backend = rfq_rgb::LibRgbBackend::new(
        r.data_dir.clone(),
        r.wallet_name.clone(),
        r.network.clone(),
        String::new(),
        std::path::PathBuf::new(),
        String::new(),
    );
    let asset = rfq_types::AssetId {
        network: r.network.parse().ok()?,
        kind: rfq_types::AssetKind::Rgb20,
        id: r.contract_id.clone(),
    };
    backend.contract_spec(&asset).ok()
}

fn parse_broker_url(config: &MakerNodeConfig) -> Result<Url, Box<dyn std::error::Error>> {
    Url::parse(&config.maker.broker_url).map_err(|e| e.into())
}

fn print_inventory_snapshot(snapshot: &InventorySnapshot, spec: Option<&(String, u8)>) {
    let amt = |v: u64| match spec {
        Some((ticker, precision)) => format!("{ticker} {}", rfq_types::format_amount(v, *precision)),
        None => v.to_string(),
    };
    println!("total_amount={}", amt(snapshot.total_amount));
    println!("available_amount={}", amt(snapshot.available_amount));
    println!("reserved_amount={}", amt(snapshot.reserved_amount));
    println!("spent_amount={}", amt(snapshot.spent_amount));
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
                cmd: MakerCmd::Inventory
            },
        );
        assert_eq!(
            Cli::parse_from(["colorex", "maker", "init", "--force"]).command,
            TopCommand::Maker {
                cmd: MakerCmd::Init(init::InitArgs { force: true }),
            },
        );
    }

    #[test]
    fn cli_accepts_global_config_flag() {
        let cli = Cli::parse_from(["colorex", "--config", "/tmp/x.toml", "maker", "up"]);
        assert_eq!(cli.config, Some(PathBuf::from("/tmp/x.toml")));
    }

    #[test]
    fn cli_definition_is_valid() {
        Cli::command().debug_assert();
    }
}
