use std::{env, path::PathBuf, sync::Arc, time::Duration};

use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use clap::{Parser, Subcommand};
use rfq_btc::MockBitcoinClient;
use rfq_client::{RfqClient, Url};
use rfq_maker::{MockMaker, RebalancePolicy};
use rfq_rgb::{LibRgbBackend, MockRgbBackend, RgbBackend};
use rfq_router::MakerConnector;
use rfq_store::{InMemoryQuoteStore, QuoteStore};
use rfq_types::{
    AcceptQuoteRequest, AssetId, AssetKind, BitcoinNetwork, HealthResponse, InventorySnapshot,
    MakerId, Outpoint, Quote, QuoteId, QuoteRequest, RgbInventoryUtxo, SettlementIntent,
};
use rfq_wallet::{MockWalletBackend, WalletBackend};
use tokio::{net::TcpListener, sync::oneshot, task::JoinHandle, time};

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

#[derive(Debug, Clone, PartialEq)]
struct MakerNodeConfig {
    rfq_api_url: String,
    maker_listen_addr: String,
    maker_id: String,
    poll_interval_ms: u64,
    cleanup_interval_ms: u64,
    rebalance_interval_ms: u64,
    rebalance_policy: RebalancePolicyConfig,
    rgb: Option<RgbConfig>,
}

/// Mirror of `RebalancePolicy` with `PartialEq` for the config tests. The
/// `RebalancePolicy` struct itself contains an `f64` and can't derive `Eq`.
#[derive(Debug, Clone, PartialEq)]
struct RebalancePolicyConfig {
    fragmentation_threshold: f64,
    max_utxo_count: u64,
    min_utxo_count: u64,
}

impl From<&RebalancePolicyConfig> for RebalancePolicy {
    fn from(c: &RebalancePolicyConfig) -> Self {
        Self {
            fragmentation_threshold: c.fragmentation_threshold,
            max_utxo_count: c.max_utxo_count,
            min_utxo_count: c.min_utxo_count,
        }
    }
}

impl Default for RebalancePolicyConfig {
    fn default() -> Self {
        let p = RebalancePolicy::default();
        Self {
            fragmentation_threshold: p.fragmentation_threshold,
            max_utxo_count: p.max_utxo_count,
            min_utxo_count: p.min_utxo_count,
        }
    }
}

/// Library-backed RGB adapter config. Populated from env when ALL fields resolve;
/// missing any → maker-node falls back to `MockRgbBackend`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct RgbConfig {
    data_dir: PathBuf,
    wallet_name: String,
    network: String,
    electrum_url: String,
    contract_id: String,
}

impl MakerNodeConfig {
    fn from_env() -> Self {
        Self {
            rfq_api_url: env::var("RFQ_API_URL")
                .unwrap_or_else(|_| "http://127.0.0.1:3000".to_owned()),
            maker_listen_addr: env::var("MAKER_LISTEN_ADDR")
                .unwrap_or_else(|_| "127.0.0.1:4000".to_owned()),
            maker_id: env::var("MAKER_ID").unwrap_or_else(|_| "mock-maker-node".to_owned()),
            poll_interval_ms: env::var("POLL_INTERVAL_MS")
                .ok()
                .and_then(|value| value.parse().ok())
                .unwrap_or(1_000),
            cleanup_interval_ms: env::var("CLEANUP_INTERVAL_MS")
                .ok()
                .and_then(|value| value.parse().ok())
                .unwrap_or(1_000),
            rebalance_interval_ms: env::var("REBALANCE_INTERVAL_MS")
                .ok()
                .and_then(|value| value.parse().ok())
                .unwrap_or(60_000),
            rebalance_policy: RebalancePolicyConfig {
                fragmentation_threshold: env::var("REBALANCE_FRAGMENTATION_THRESHOLD")
                    .ok()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(0.7),
                max_utxo_count: env::var("REBALANCE_MAX_UTXO_COUNT")
                    .ok()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(50),
                min_utxo_count: env::var("REBALANCE_MIN_UTXO_COUNT")
                    .ok()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(3),
            },
            rgb: RgbConfig::from_env(),
        }
    }

    fn api_url(&self) -> Result<Url, String> {
        Url::parse(&self.rfq_api_url).map_err(|error| error.to_string())
    }
}

impl RgbConfig {
    fn from_env() -> Option<Self> {
        let data_dir = env::var("RGB_DATA_DIR").ok()?;
        let contract_id = env::var("RGB_CONTRACT_ID").ok()?;
        let electrum_url =
            env::var("ELECTRUM_URL").unwrap_or_else(|_| "localhost:50001".to_owned());
        let wallet_name = env::var("RGB_WALLET").unwrap_or_else(|_| "maker".to_owned());
        let network = env::var("RGB_NETWORK").unwrap_or_else(|_| "regtest".to_owned());
        Some(Self {
            data_dir: PathBuf::from(data_dir),
            wallet_name,
            network,
            electrum_url,
            contract_id,
        })
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
        Command::Health => health(config).await,
        Command::Inventory => inventory(config).await,
    }
}

async fn run(config: MakerNodeConfig) -> Result<(), Box<dyn std::error::Error>> {
    let _client = RfqClient::new(config.api_url()?);
    let wallet = MockWalletBackend::default();
    let maker = build_maker(&config).await?;
    let app = maker_app(maker.clone());
    let listener = TcpListener::bind(&config.maker_listen_addr).await?;
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let maker_type = std::any::type_name::<MockMaker>();
    let sample_invoice = wallet.create_rgb_invoice("mock-contract", 1)?;

    println!("maker-node starting");
    println!("maker_id={}", config.maker_id);
    println!("rfq_api_url={}", config.rfq_api_url);
    println!("maker_listen_addr={}", config.maker_listen_addr);
    println!("poll_interval_ms={}", config.poll_interval_ms);
    println!("cleanup_interval_ms={}", config.cleanup_interval_ms);
    println!("rebalance_interval_ms={}", config.rebalance_interval_ms);
    println!("wallet_sample_invoice={sample_invoice}");
    println!("maker_runtime={maker_type}");

    let cleanup_task = spawn_cleanup_loop(maker.clone(), config.cleanup_interval_ms);
    let rebalance_task = spawn_rebalance_loop(
        maker.clone(),
        config.rebalance_interval_ms,
        (&config.rebalance_policy).into(),
    );
    let placeholder_task = spawn_placeholder_loop(config.poll_interval_ms);
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
    placeholder_task.abort();
    let _ = server_task.await;
    let _ = cleanup_task.await;
    let _ = rebalance_task.await;
    let _ = placeholder_task.await;

    Ok(())
}

async fn health(config: MakerNodeConfig) -> Result<(), Box<dyn std::error::Error>> {
    let broker_status = broker_health_status(&config).await?;
    let wallet = MockWalletBackend::default();
    let _sample_signed_psbt = wallet.sign_psbt("mock-psbt")?;

    println!("maker-node health ok");
    println!("maker_id={}", config.maker_id);
    println!("rfq_api_url={}", config.rfq_api_url);
    println!("broker_status={broker_status}");
    println!("wallet=mock-ready");
    println!(
        "maker_runtime={}",
        std::any::type_name::<rfq_maker::MockMaker>()
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

async fn build_maker(config: &MakerNodeConfig) -> Result<MockMaker, Box<dyn std::error::Error>> {
    let maker_id = MakerId(config.maker_id.clone());
    let asset = AssetId {
        network: BitcoinNetwork::Regtest,
        kind: AssetKind::Rgb20,
        id: config
            .rgb
            .as_ref()
            .map(|r| r.contract_id.clone())
            .unwrap_or_else(|| "rgb-test-asset".to_owned()),
    };

    let (utxos, rgb_backend): (Vec<RgbInventoryUtxo>, Arc<dyn RgbBackend>) = match &config.rgb {
        Some(rgb_cfg) => {
            let backend = LibRgbBackend::new(
                rgb_cfg.data_dir.clone(),
                rgb_cfg.wallet_name.clone(),
                rgb_cfg.network.clone(),
                rgb_cfg.electrum_url.clone(),
            );
            let utxos = backend.list_inventory_utxos(&asset).await?;
            (utxos, Arc::new(backend))
        }
        None => {
            let utxo = RgbInventoryUtxo {
                outpoint: Outpoint::new(format!("{:064x}", 0u64), 0),
                asset_id: asset,
                amount: 1_000_000,
                btc_sats: 0,
            };
            let backend = MockRgbBackend::new(vec![utxo.clone()]);
            (vec![utxo], Arc::new(backend))
        }
    };

    // 15c wires a BitcoinClient into the maker for broadcast + feerate
    // estimation. maker-node runs against a mock until the electrum-backed
    // client is wired through config (#15b follow-up).
    let bitcoin_client = Arc::new(MockBitcoinClient::new());
    Ok(MockMaker::new(maker_id, utxos, rgb_backend, bitcoin_client))
}

#[derive(Clone)]
struct MakerNodeState {
    maker: MockMaker,
    store: InMemoryQuoteStore,
}

fn maker_app(maker: MockMaker) -> Router {
    Router::new()
        .route("/health", get(maker_health))
        .route("/inventory", get(maker_inventory))
        .route("/quotes", post(maker_quote))
        .route("/quotes/:quote_id/accept", post(maker_accept_quote))
        .route("/quotes/:quote_id/consignment", post(maker_consignment))
        .route("/quotes/:quote_id/sign", post(maker_sign_quote))
        .with_state(MakerNodeState {
            maker,
            store: InMemoryQuoteStore::new(),
        })
}

async fn maker_health() -> Json<HealthResponse> {
    Json(HealthResponse {
        status: "ok".to_owned(),
    })
}

async fn maker_inventory(State(state): State<MakerNodeState>) -> Json<InventorySnapshot> {
    Json(state.maker.inventory_summary().await)
}

async fn maker_quote(
    State(state): State<MakerNodeState>,
    Json(request): Json<QuoteRequest>,
) -> Result<Json<Option<Quote>>, MakerNodeHttpError> {
    let quote = state.maker.request_quote(request).await?;
    if let Some(quote) = &quote {
        state.store.save_quote(quote.clone()).await;
    }

    Ok(Json(quote))
}

async fn maker_accept_quote(
    State(state): State<MakerNodeState>,
    Path(quote_id): Path<String>,
    Json(mut request): Json<AcceptQuoteRequest>,
) -> Result<Json<SettlementIntent>, MakerNodeHttpError> {
    let quote_id = QuoteId(quote_id);
    let quote = state
        .store
        .get_quote(&quote_id)
        .await
        .ok_or(MakerNodeHttpError::NotFound)?;
    request.quote_id = quote_id;

    Ok(Json(state.maker.accept_quote(quote, request).await?))
}

#[derive(Debug, serde::Deserialize)]
struct SignedPsbtBody {
    signed_psbt: String,
}

async fn maker_sign_quote(
    State(state): State<MakerNodeState>,
    Path(quote_id): Path<String>,
    Json(body): Json<SignedPsbtBody>,
) -> Result<Json<SettlementIntent>, MakerNodeHttpError> {
    let quote_id = QuoteId(quote_id);
    // 404 for an unknown quote, mirroring maker_accept_quote; settlement-stage
    // expiry is enforced inside submit_signed_psbt.
    state
        .store
        .get_quote(&quote_id)
        .await
        .ok_or(MakerNodeHttpError::NotFound)?;

    Ok(Json(
        state.maker.submit_signed_psbt(quote_id, body.signed_psbt).await?,
    ))
}

#[derive(Debug, serde::Deserialize)]
struct ConsignmentBody {
    consignment: String,
}

async fn maker_consignment(
    State(state): State<MakerNodeState>,
    Path(quote_id): Path<String>,
    Json(body): Json<ConsignmentBody>,
) -> Result<Json<SettlementIntent>, MakerNodeHttpError> {
    let quote_id = QuoteId(quote_id);
    // 404 for an unknown quote, mirroring maker_sign_quote.
    state
        .store
        .get_quote(&quote_id)
        .await
        .ok_or(MakerNodeHttpError::NotFound)?;

    Ok(Json(
        state
            .maker
            .deliver_consignment(quote_id, body.consignment)
            .await?,
    ))
}

#[derive(Debug)]
enum MakerNodeHttpError {
    NotFound,
    Maker(String),
}

impl IntoResponse for MakerNodeHttpError {
    fn into_response(self) -> Response {
        let (status, message) = match self {
            MakerNodeHttpError::NotFound => (StatusCode::NOT_FOUND, "quote not found".to_owned()),
            MakerNodeHttpError::Maker(message) => (StatusCode::BAD_REQUEST, message),
        };

        (status, message).into_response()
    }
}

impl From<rfq_router::RouterError> for MakerNodeHttpError {
    fn from(error: rfq_router::RouterError) -> Self {
        MakerNodeHttpError::Maker(error.to_string())
    }
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

/// Periodic rebalance planner loop. Mirrors `spawn_cleanup_loop` in shape but
/// runs on a slower cadence (default 60s vs 1s). Calls `maker.rebalance(policy)`
/// and logs the trigger reasons when a plan fires. In 14e the loop only logs;
/// the executor (settlement-tx piggyback) is a follow-up issue. See
/// docs/rebalancing-strategy.md.
fn spawn_rebalance_loop(
    maker: MockMaker,
    rebalance_interval_ms: u64,
    policy: RebalancePolicy,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = time::interval(Duration::from_millis(rebalance_interval_ms));

        loop {
            interval.tick().await;
            let plan = maker.rebalance(&policy).await;
            if !plan.is_empty() {
                println!("rebalance_plan triggers={:?}", plan.triggers);
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
    use axum::{
        body::{to_bytes, Body},
        http::{Method, Request},
    };
    use clap::CommandFactory;
    use rfq_types::{RfqId, SettlementStatus, Side, SwapLeg};
    use std::sync::Mutex;
    use tower::ServiceExt;

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
        let old_maker_listen_addr = env::var("MAKER_LISTEN_ADDR").ok();
        let old_maker_id = env::var("MAKER_ID").ok();
        let old_poll_interval = env::var("POLL_INTERVAL_MS").ok();
        let old_cleanup_interval = env::var("CLEANUP_INTERVAL_MS").ok();
        env::remove_var("RFQ_API_URL");
        env::remove_var("MAKER_LISTEN_ADDR");
        env::remove_var("MAKER_ID");
        env::remove_var("POLL_INTERVAL_MS");
        env::remove_var("CLEANUP_INTERVAL_MS");

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
        let maker = build_maker(&test_config()).await.unwrap();
        let snapshot = maker.inventory_summary().await;

        assert_eq!(snapshot.total_amount, 1_000_000);
        assert_eq!(snapshot.available_amount, 1_000_000);
        assert_eq!(snapshot.reserved_amount, 0);
        assert_eq!(snapshot.spent_amount, 0);
        assert_eq!(snapshot.total_allocations, 1);
        assert_eq!(snapshot.available_allocations, 1);
        assert_eq!(snapshot.reserved_allocations, 0);
        assert_eq!(snapshot.spent_allocations, 0);
    }

    #[tokio::test]
    async fn maker_http_health_returns_ok() {
        let response = test_app()
            .await
            .oneshot(empty_request(Method::GET, "/health"))
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let health: HealthResponse = read_json(response).await;
        assert_eq!(health.status, "ok");
    }

    #[tokio::test]
    async fn maker_http_inventory_returns_snapshot() {
        let response = test_app()
            .await
            .oneshot(empty_request(Method::GET, "/inventory"))
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let snapshot: InventorySnapshot = read_json(response).await;
        assert_eq!(snapshot.available_amount, 1_000_000);
        assert_eq!(snapshot.available_allocations, 1);
    }

    #[tokio::test]
    async fn maker_http_quote_returns_quote_and_reserves_inventory() {
        let app = test_app().await;

        let quote = request_quote(app.clone(), "rfq-1").await;
        let response = app
            .oneshot(empty_request(Method::GET, "/inventory"))
            .await
            .unwrap();
        let snapshot: InventorySnapshot = read_json(response).await;

        assert_eq!(quote.amount, 100);
        assert_eq!(snapshot.available_amount, 0);
        assert_eq!(snapshot.reserved_amount, 1_000_000);
        assert_eq!(snapshot.reserved_allocations, 1);
    }

    #[tokio::test]
    async fn maker_http_accept_returns_settlement_intent() {
        let app = test_app().await;
        let quote = request_quote(app.clone(), "rfq-accept").await;
        let request = AcceptQuoteRequest {
            quote_id: quote.quote_id.clone(),
            leg: SwapLeg::Buy {
                rgb_invoice: "rgb:test_invoice".to_owned(),
            },
        };

        let response = app
            .oneshot(json_request(
                Method::POST,
                &format!("/quotes/{}/accept", quote.quote_id.0),
                &request,
            ))
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let settlement: SettlementIntent = read_json(response).await;
        assert_eq!(settlement.quote_id, quote.quote_id);
        assert_eq!(settlement.status, SettlementStatus::AwaitingTakerSignature);
        assert!(settlement.transfer.is_some());
    }

    #[tokio::test]
    async fn maker_http_accept_unknown_quote_returns_not_found() {
        let request = AcceptQuoteRequest {
            quote_id: QuoteId("missing".to_owned()),
            leg: SwapLeg::Buy {
                rgb_invoice: "rgb:test_invoice".to_owned(),
            },
        };

        let response = test_app()
            .await
            .oneshot(json_request(
                Method::POST,
                "/quotes/missing/accept",
                &request,
            ))
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    async fn test_app() -> Router {
        maker_app(build_maker(&test_config()).await.unwrap())
    }

    fn test_config() -> MakerNodeConfig {
        MakerNodeConfig {
            rfq_api_url: "http://127.0.0.1:3000".to_owned(),
            maker_listen_addr: "127.0.0.1:4000".to_owned(),
            maker_id: "test-maker".to_owned(),
            poll_interval_ms: 1_000,
            cleanup_interval_ms: 1_000,
            rebalance_interval_ms: 60_000,
            rebalance_policy: RebalancePolicyConfig::default(),
            rgb: None,
        }
    }

    async fn request_quote(app: Router, rfq_id: &str) -> Quote {
        let response = app
            .oneshot(json_request(
                Method::POST,
                "/quotes",
                &quote_request(rfq_id),
            ))
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let quote: Option<Quote> = read_json(response).await;
        quote.unwrap()
    }

    fn quote_request(rfq_id: &str) -> QuoteRequest {
        QuoteRequest {
            rfq_id: RfqId(rfq_id.to_owned()),
            base_asset: AssetId {
                network: BitcoinNetwork::Regtest,
                kind: AssetKind::Rgb20,
                id: "rgb-test-asset".to_owned(),
            },
            quote_asset: AssetId {
                network: BitcoinNetwork::Regtest,
                kind: AssetKind::Btc,
                id: "btc".to_owned(),
            },
            side: Side::Buy,
            amount: 100,
            created_at_ms: 1,
        }
    }

    fn empty_request(method: Method, uri: &str) -> Request<Body> {
        Request::builder()
            .method(method)
            .uri(uri)
            .body(Body::empty())
            .unwrap()
    }

    fn json_request<T: serde::Serialize>(method: Method, uri: &str, body: &T) -> Request<Body> {
        Request::builder()
            .method(method)
            .uri(uri)
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(body).unwrap()))
            .unwrap()
    }

    async fn read_json<T: serde::de::DeserializeOwned>(response: Response) -> T {
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        serde_json::from_slice(&body).unwrap()
    }

    fn restore_env(key: &str, value: Option<String>) {
        match value {
            Some(value) => env::set_var(key, value),
            None => env::remove_var(key),
        }
    }
}
