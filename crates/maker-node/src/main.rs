use clap::{Parser, Subcommand};
use maker_node::{
    build_maker, build_runtime, maker_app, spawn_chain_observer_loop, spawn_cleanup_loop,
    spawn_rebalance_loop, MakerNodeConfig,
};
use rfq_client::RfqClient;
use rfq_maker::Maker;
use rfq_types::InventorySnapshot;
use tokio::{net::TcpListener, sync::oneshot};

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
        Command::Health => health(config).await,
        Command::Inventory => inventory(config).await,
    }
}

async fn run(config: MakerNodeConfig) -> Result<(), Box<dyn std::error::Error>> {
    let _client = RfqClient::new(config.api_url()?);
    let runtime = build_runtime(&config).await?;
    let maker = runtime.maker;
    let chain_observer_deps = runtime.chain_observer;
    let app = maker_app(maker.clone());
    let listener = TcpListener::bind(&config.maker_listen_addr).await?;
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let maker_type = std::any::type_name::<Maker>();

    println!("maker-node starting");
    println!("maker_id={}", config.maker_id);
    println!("rfq_api_url={}", config.rfq_api_url);
    println!("maker_listen_addr={}", config.maker_listen_addr);
    println!("cleanup_interval_ms={}", config.cleanup_interval_ms);
    println!("rebalance_interval_ms={}", config.rebalance_interval_ms);
    println!(
        "chain_observer={}",
        if chain_observer_deps.is_some() {
            format!("enabled (interval_ms={})", config.chain_observer_interval_ms)
        } else {
            "disabled (no RGB config)".to_owned()
        }
    );
    println!("maker_runtime={maker_type}");

    let cleanup_task = spawn_cleanup_loop(maker.clone(), config.cleanup_interval_ms);
    let rebalance_task = spawn_rebalance_loop(
        maker.clone(),
        config.rebalance_interval_ms,
        (&config.rebalance_policy).into(),
    );
    let chain_observer_task = chain_observer_deps.map(|deps| {
        spawn_chain_observer_loop(maker.clone(), deps, config.chain_observer_interval_ms)
    });
    let server_task = tokio::spawn(async move {
        let result = axum::serve(listener, app)
            .with_graceful_shutdown(async {
                let _ = shutdown_rx.await;
            })
            .await;
        if let Err(error) = result {
            eprintln!("maker-node HTTP server error: {error}");
        }
    });

    tokio::signal::ctrl_c().await?;
    println!("maker-node shutting down");

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

    println!("maker-node health ok");
    println!("maker_id={}", config.maker_id);
    println!("rfq_api_url={}", config.rfq_api_url);
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
    let client = RfqClient::new(config.api_url()?);
    let response = client.health().await?;

    Ok(response.status)
}

async fn inventory(config: MakerNodeConfig) -> Result<(), Box<dyn std::error::Error>> {
    let _api_url = config.api_url()?;
    let maker = build_maker(&config).await?;
    let snapshot = maker.inventory_summary().await;

    println!("maker-node inventory");
    println!("maker_id={}", config.maker_id);
    print_inventory_snapshot(&snapshot);

    Ok(())
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
        let old_api_url = std::env::var("RFQ_API_URL").ok();
        let old_maker_listen_addr = std::env::var("MAKER_LISTEN_ADDR").ok();
        let old_maker_id = std::env::var("MAKER_ID").ok();
        let old_poll_interval = std::env::var("POLL_INTERVAL_MS").ok();
        let old_cleanup_interval = std::env::var("CLEANUP_INTERVAL_MS").ok();
        std::env::remove_var("RFQ_API_URL");
        std::env::remove_var("MAKER_LISTEN_ADDR");
        std::env::remove_var("MAKER_ID");
        std::env::remove_var("POLL_INTERVAL_MS");
        std::env::remove_var("CLEANUP_INTERVAL_MS");

        let config = MakerNodeConfig::from_env();

        restore_env("RFQ_API_URL", old_api_url);
        restore_env("MAKER_LISTEN_ADDR", old_maker_listen_addr);
        restore_env("MAKER_ID", old_maker_id);
        restore_env("POLL_INTERVAL_MS", old_poll_interval);
        restore_env("CLEANUP_INTERVAL_MS", old_cleanup_interval);

        assert_eq!(config.rfq_api_url, "http://127.0.0.1:3000");
        assert_eq!(config.maker_listen_addr, "127.0.0.1:4000");
        assert_eq!(config.maker_id, "mock-maker-node");
        assert_eq!(config.poll_interval_ms, 1_000);
        assert_eq!(config.cleanup_interval_ms, 1_000);
    }

    fn restore_env(key: &str, value: Option<String>) {
        match value {
            Some(value) => std::env::set_var(key, value),
            None => std::env::remove_var(key),
        }
    }
}
