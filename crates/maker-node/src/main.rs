use std::{env, sync::Arc, time::Duration};

use clap::{Parser, Subcommand};
use rfq_client::{RfqClient, Url};
use rfq_maker::MockMaker;
use rfq_rgb::MockRgbBackend;
use rfq_types::{Allocation, AssetId, AssetKind, BitcoinNetwork, InventorySnapshot, MakerId};
use rfq_wallet::{MockWalletBackend, WalletBackend};
use tokio::{task::JoinHandle, time};

#[derive(Debug, Parser)]
#[command(name = "maker-node", about = "Mock RGB RFQ maker daemon")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand, Clone, PartialEq, Eq)]
enum Command {
    Run,
    Health,
    Inventory,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct MakerNodeConfig {
    rfq_api_url: String,
    maker_id: String,
    poll_interval_ms: u64,
    cleanup_interval_ms: u64,
}

impl MakerNodeConfig {
    fn from_env() -> Self {
        Self {
            rfq_api_url: env::var("RFQ_API_URL")
                .unwrap_or_else(|_| "http://127.0.0.1:3000".to_owned()),
            maker_id: env::var("MAKER_ID").unwrap_or_else(|_| "mock-maker-node".to_owned()),
            poll_interval_ms: env::var("POLL_INTERVAL_MS")
                .ok()
                .and_then(|value| value.parse().ok())
                .unwrap_or(1_000),
            cleanup_interval_ms: env::var("CLEANUP_INTERVAL_MS")
                .ok()
                .and_then(|value| value.parse().ok())
                .unwrap_or(1_000),
        }
    }

    fn api_url(&self) -> Result<Url, String> {
        Url::parse(&self.rfq_api_url).map_err(|error| error.to_string())
    }
}

#[tokio::main]
async fn main() {
    if let Err(error) = run_cli().await {
        eprintln!("maker-node error: {error}");
        std::process::exit(1);
    }
}

async fn run_cli() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();
    let config = MakerNodeConfig::from_env();

    match cli.command {
        Command::Run => run(config).await,
        Command::Health => health(config),
        Command::Inventory => inventory(config).await,
    }
}

async fn run(config: MakerNodeConfig) -> Result<(), Box<dyn std::error::Error>> {
    let _client = RfqClient::new(config.api_url()?);
    let wallet = MockWalletBackend::default();
    let maker = mock_maker(&config);
    let maker_type = std::any::type_name::<MockMaker>();
    let sample_invoice = wallet.create_rgb_invoice("mock-contract", 1)?;

    println!("maker-node starting");
    println!("maker_id={}", config.maker_id);
    println!("rfq_api_url={}", config.rfq_api_url);
    println!("poll_interval_ms={}", config.poll_interval_ms);
    println!("cleanup_interval_ms={}", config.cleanup_interval_ms);
    println!("wallet_sample_invoice={sample_invoice}");
    println!("maker_runtime={maker_type}");

    let cleanup_task = spawn_cleanup_loop(maker.clone(), config.cleanup_interval_ms);
    let placeholder_task = spawn_placeholder_loop(config.poll_interval_ms);

    tokio::signal::ctrl_c().await?;
    println!("maker-node shutting down");

    cleanup_task.abort();
    placeholder_task.abort();
    let _ = cleanup_task.await;
    let _ = placeholder_task.await;

    Ok(())
}

fn health(config: MakerNodeConfig) -> Result<(), Box<dyn std::error::Error>> {
    let _client = RfqClient::new(config.api_url()?);
    let wallet = MockWalletBackend::default();
    let _sample_signed_psbt = wallet.sign_psbt("mock-psbt")?;

    println!("maker-node health ok");
    println!("maker_id={}", config.maker_id);
    println!("rfq_api_url={}", config.rfq_api_url);
    println!("wallet=mock-ready");
    println!(
        "maker_runtime={}",
        std::any::type_name::<rfq_maker::MockMaker>()
    );

    Ok(())
}

async fn inventory(config: MakerNodeConfig) -> Result<(), Box<dyn std::error::Error>> {
    let _api_url = config.api_url()?;
    let maker = mock_maker(&config);
    let snapshot = maker.inventory_summary().await;

    println!("maker-node inventory");
    println!("maker_id={}", config.maker_id);
    print_inventory_snapshot(&snapshot);

    Ok(())
}

fn mock_maker(config: &MakerNodeConfig) -> MockMaker {
    let maker_id = MakerId(config.maker_id.clone());
    let asset = AssetId {
        network: BitcoinNetwork::Regtest,
        kind: AssetKind::Rgb20,
        id: "rgb-test-asset".to_owned(),
    };
    let allocation = Allocation {
        maker_id: maker_id.clone(),
        asset,
        available_amount: 1_000_000,
    };
    let rgb_backend = Arc::new(MockRgbBackend::new(vec![allocation.clone()]));

    MockMaker::new(maker_id, vec![allocation], rgb_backend)
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

fn spawn_cleanup_loop(maker: MockMaker, cleanup_interval_ms: u64) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = time::interval(Duration::from_millis(cleanup_interval_ms));

        loop {
            interval.tick().await;
            let released = maker.release_expired_reservations().await;
            if released > 0 {
                println!("released_expired_reservations={released}");
            }
        }
    })
}

fn spawn_placeholder_loop(poll_interval_ms: u64) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = time::interval(Duration::from_millis(poll_interval_ms));

        loop {
            interval.tick().await;
            println!("maker-node placeholder tick");
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;
    use std::sync::Mutex;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn cli_parses_commands() {
        assert_eq!(Cli::parse_from(["maker-node", "run"]).command, Command::Run);
        assert_eq!(
            Cli::parse_from(["maker-node", "health"]).command,
            Command::Health
        );
        assert_eq!(
            Cli::parse_from(["maker-node", "inventory"]).command,
            Command::Inventory
        );
    }

    #[test]
    fn cli_definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn config_uses_defaults_when_env_missing() {
        let _guard = ENV_LOCK.lock().unwrap();
        let old_api_url = env::var("RFQ_API_URL").ok();
        let old_maker_id = env::var("MAKER_ID").ok();
        let old_poll_interval = env::var("POLL_INTERVAL_MS").ok();
        let old_cleanup_interval = env::var("CLEANUP_INTERVAL_MS").ok();
        env::remove_var("RFQ_API_URL");
        env::remove_var("MAKER_ID");
        env::remove_var("POLL_INTERVAL_MS");
        env::remove_var("CLEANUP_INTERVAL_MS");

        let config = MakerNodeConfig::from_env();

        restore_env("RFQ_API_URL", old_api_url);
        restore_env("MAKER_ID", old_maker_id);
        restore_env("POLL_INTERVAL_MS", old_poll_interval);
        restore_env("CLEANUP_INTERVAL_MS", old_cleanup_interval);

        assert_eq!(config.rfq_api_url, "http://127.0.0.1:3000");
        assert_eq!(config.maker_id, "mock-maker-node");
        assert_eq!(config.poll_interval_ms, 1_000);
        assert_eq!(config.cleanup_interval_ms, 1_000);
    }

    #[test]
    fn config_reads_custom_cleanup_interval() {
        let _guard = ENV_LOCK.lock().unwrap();
        let old_cleanup_interval = env::var("CLEANUP_INTERVAL_MS").ok();
        env::set_var("CLEANUP_INTERVAL_MS", "2500");

        let config = MakerNodeConfig::from_env();

        restore_env("CLEANUP_INTERVAL_MS", old_cleanup_interval);

        assert_eq!(config.cleanup_interval_ms, 2_500);
    }

    #[tokio::test]
    async fn mock_inventory_summary_is_available_by_default() {
        let config = MakerNodeConfig {
            rfq_api_url: "http://127.0.0.1:3000".to_owned(),
            maker_id: "test-maker".to_owned(),
            poll_interval_ms: 1_000,
            cleanup_interval_ms: 1_000,
        };

        let snapshot = mock_maker(&config).inventory_summary().await;

        assert_eq!(snapshot.total_amount, 1_000_000);
        assert_eq!(snapshot.available_amount, 1_000_000);
        assert_eq!(snapshot.reserved_amount, 0);
        assert_eq!(snapshot.spent_amount, 0);
        assert_eq!(snapshot.total_allocations, 1);
        assert_eq!(snapshot.available_allocations, 1);
        assert_eq!(snapshot.reserved_allocations, 0);
        assert_eq!(snapshot.spent_allocations, 0);
    }

    fn restore_env(key: &str, value: Option<String>) {
        match value {
            Some(value) => env::set_var(key, value),
            None => env::remove_var(key),
        }
    }
}
