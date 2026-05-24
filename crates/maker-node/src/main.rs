use std::path::{Path, PathBuf};

use clap::{Parser, Subcommand};
use maker_node::{
    build_maker, build_runtime, init, maker_app, spawn_chain_observer_loop, spawn_cleanup_loop,
    spawn_rebalance_loop, MakerNodeConfig,
};
use rfq_client::{RfqClient, Url};
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
            MakerCmd::Up => run(load_config(&config_path)?).await,
            MakerCmd::Health => health(load_config(&config_path)?).await,
            MakerCmd::Inventory => inventory(load_config(&config_path)?).await,
        },
    }
}

fn load_config(path: &Path) -> Result<MakerNodeConfig, String> {
    MakerNodeConfig::load(path).map_err(|e| format!("config {}: {e}", path.display()))
}

async fn run(config: MakerNodeConfig) -> Result<(), Box<dyn std::error::Error>> {
    let _client = RfqClient::new(parse_broker_url(&config)?);
    let runtime = build_runtime(&config).await?;
    let maker = runtime.maker;
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
