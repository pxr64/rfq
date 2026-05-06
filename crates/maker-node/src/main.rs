use std::{env, time::Duration};

use clap::{Parser, Subcommand};
use rfq_client::{RfqClient, Url};
use rfq_wallet::{MockWalletBackend, WalletBackend};
use tokio::time;

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
        Command::Inventory => inventory(config),
    }
}

async fn run(config: MakerNodeConfig) -> Result<(), Box<dyn std::error::Error>> {
    let _client = RfqClient::new(config.api_url()?);
    let wallet = MockWalletBackend::default();
    let maker_type = std::any::type_name::<rfq_maker::MockMaker>();
    let sample_invoice = wallet.create_rgb_invoice("mock-contract", 1)?;

    println!("maker-node starting");
    println!("maker_id={}", config.maker_id);
    println!("rfq_api_url={}", config.rfq_api_url);
    println!("poll_interval_ms={}", config.poll_interval_ms);
    println!("wallet_sample_invoice={sample_invoice}");
    println!("maker_runtime={maker_type}");

    let mut interval = time::interval(Duration::from_millis(config.poll_interval_ms));
    loop {
        interval.tick().await;
        println!("maker-node placeholder tick");
    }
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

fn inventory(config: MakerNodeConfig) -> Result<(), Box<dyn std::error::Error>> {
    let _client = RfqClient::new(config.api_url()?);

    println!("maker-node inventory placeholder");
    println!("maker_id={}", config.maker_id);
    println!("allocations=mock-only");

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

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
        let old_api_url = env::var("RFQ_API_URL").ok();
        let old_maker_id = env::var("MAKER_ID").ok();
        let old_poll_interval = env::var("POLL_INTERVAL_MS").ok();
        env::remove_var("RFQ_API_URL");
        env::remove_var("MAKER_ID");
        env::remove_var("POLL_INTERVAL_MS");

        let config = MakerNodeConfig::from_env();

        restore_env("RFQ_API_URL", old_api_url);
        restore_env("MAKER_ID", old_maker_id);
        restore_env("POLL_INTERVAL_MS", old_poll_interval);

        assert_eq!(config.rfq_api_url, "http://127.0.0.1:3000");
        assert_eq!(config.maker_id, "mock-maker-node");
        assert_eq!(config.poll_interval_ms, 1_000);
    }

    fn restore_env(key: &str, value: Option<String>) {
        match value {
            Some(value) => env::set_var(key, value),
            None => env::remove_var(key),
        }
    }
}
