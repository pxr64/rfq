use std::path::{Path, PathBuf};

use clap::{Args, Parser, Subcommand};
use maker_node::{
    build_maker, build_runtime, create_inventory_invoice, init, maker_app, orders,
    spawn_chain_observer_loop, spawn_cleanup_loop, spawn_rebalance_loop, MakerNodeConfig,
};
use rfq_client::{RfqClient, Url};
use rfq_rgb::LibRgbBackend;
use rfq_maker::Maker;
use rfq_types::InventorySnapshot;
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
        /// Electrum URL to broadcast the anchoring tx.
        #[arg(long)]
        electrum: String,
        /// Encrypted signing-account file (the issuer's hot key).
        #[arg(long)]
        account_file: PathBuf,
        #[arg(long, default_value = "")]
        password: String,
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
        /// Encrypted signing-account file to write (the hot key used to sign swaps).
        #[arg(long)]
        account_file: PathBuf,
        #[arg(long, default_value = "")]
        password: String,
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
        #[arg(long)]
        electrum: String,
    },
}

#[derive(Debug, Args, Clone, PartialEq, Eq)]
struct WalletCommon {
    /// Network: regtest | signet | testnet | mainnet.
    #[arg(long)]
    network: String,
    /// RGB data dir (stock lives at `<data_dir>/<network>`, wallet one level below).
    #[arg(long)]
    data_dir: PathBuf,
    /// Wallet name.
    #[arg(long)]
    name: String,
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
        },
        TopCommand::Wallet { cmd } => match cmd {
            WalletCmd::Create {
                common,
                account_file,
                password,
            } => wallet_create(common, account_file, password),
            WalletCmd::Address { common, btc } => wallet_address(common, btc),
            WalletCmd::Sync { common, electrum } => wallet_sync(common, electrum).await,
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
    account_file: PathBuf,
    password: String,
) -> Result<(), Box<dyn std::error::Error>> {
    let backend = LibRgbBackend::new(
        common.data_dir,
        common.name.clone(),
        common.network,
        String::new(),
        account_file,
        password,
    );
    backend.create_wallet()?;
    println!("created wallet '{}'", common.name);
    println!(
        "fund this tapret (keychain-10) address from a faucet, then run `wallet sync`:\n  {}",
        backend.funding_address(true)?
    );
    Ok(())
}

fn wallet_address(common: WalletCommon, btc: bool) -> Result<(), Box<dyn std::error::Error>> {
    let backend = LibRgbBackend::new(
        common.data_dir,
        common.name,
        common.network,
        String::new(),
        PathBuf::new(),
        String::new(),
    );
    println!("{}", backend.funding_address(!btc)?);
    Ok(())
}

async fn wallet_sync(
    common: WalletCommon,
    electrum: String,
) -> Result<(), Box<dyn std::error::Error>> {
    let name = common.name.clone();
    let backend = LibRgbBackend::new(
        common.data_dir,
        common.name,
        common.network,
        electrum,
        PathBuf::new(),
        String::new(),
    );
    backend.sync_wallet().await?;
    println!("synced '{name}'");
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
    let backend = LibRgbBackend::new(
        common.data_dir,
        common.name,
        common.network,
        String::new(),
        PathBuf::new(),
        String::new(),
    );
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
    let backend = LibRgbBackend::new(
        common.data_dir,
        common.name,
        common.network,
        String::new(),
        PathBuf::new(),
        String::new(),
    );
    for line in backend.list_contracts()? {
        println!("{line}");
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn issuer_transfer(
    common: WalletCommon,
    invoice: String,
    electrum: String,
    account_file: PathBuf,
    password: String,
    fee: u64,
) -> Result<(), Box<dyn std::error::Error>> {
    let backend = LibRgbBackend::new(
        common.data_dir,
        common.name,
        common.network,
        electrum,
        account_file,
        password,
    );
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
    let maker_type = std::any::type_name::<Maker>();

    println!("colorex maker starting");
    println!("node_id={}", config.maker.node_id);
    println!("broker_url={}", config.maker.broker_url);
    println!("listen_addr={}", config.maker.listen_addr);
    println!("cleanup_interval={:?}", config.intervals.cleanup);
    println!("rebalance_interval={:?}", config.intervals.rebalance);
    println!(
        "standing_orders={order_count} ({})",
        order_path.display()
    );
    println!(
        "chain_observer={}",
        if chain_observer_deps.is_some() {
            format!("enabled (interval={:?})", config.intervals.chain_observer)
        } else {
            "disabled (no RGB config)".to_owned()
        }
    );
    println!("maker_runtime={maker_type}");

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

    tokio::signal::ctrl_c().await?;
    println!("colorex maker shutting down");

    let _ = shutdown_tx.send(());
    cleanup_task.abort();
    rebalance_task.abort();
    if let Some(t) = &chain_observer_task {
        t.abort();
    }
    let _ = server_task.await;
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
    print_inventory_snapshot(&snapshot);

    Ok(())
}

fn parse_broker_url(config: &MakerNodeConfig) -> Result<Url, Box<dyn std::error::Error>> {
    Url::parse(&config.maker.broker_url).map_err(|e| e.into())
}

fn print_inventory_snapshot(snapshot: &InventorySnapshot) {
    println!("total_amount={}", snapshot.total_amount);
    println!("available_amount={}", snapshot.available_amount);
    println!("reserved_amount={}", snapshot.reserved_amount);
    println!("spent_amount={}", snapshot.spent_amount);
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
