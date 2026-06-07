//! Library entry points for the `colorex` binary. Exposes the config
//! types, runtime builder, axum `Router`, and background-loop spawners so
//! the binary (`src/main.rs`) and integration tests
//! (`tests/regtest_http_round_trip.rs`) can share them.

pub mod broker_client;
pub mod init;
pub mod node_key;
pub mod orders;
pub mod output;

use std::{path::PathBuf, sync::Arc, time::Duration};

use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use rfq_btc::{BitcoinClient, ElectrumClient, MockBitcoinClient};
use rfq_maker::{CoinSelector, GreedyExactFitSelector, Maker, RebalancePolicy};
use rfq_rgb::{LibRgbBackend, MockRgbBackend, RgbBackend};
use rfq_router::MakerConnector;
use rfq_store::{
    BtcInventoryStore, ConsignmentStore, InMemoryQuoteStore, InventoryStore, QuoteStore,
    SqliteBtcInventoryStore, SqliteConsignmentStore, SqliteInventoryStore,
};
use rfq_types::{
    AcceptQuoteRequest, AssetId, AssetKind, BitcoinNetwork, BtcInventoryStatus, BtcInventoryUtxo,
    HealthResponse, InventorySnapshot, InventoryStatus, InventoryUtxo, MakerId, Outpoint, Quote,
    QuoteId, QuoteRequest, RgbInventoryUtxo, SettlementIntent,
};
use serde::{Deserialize, Serialize};
use tokio::{task::JoinHandle, time};

/// Top-level config for the `colorex maker` daemon. Loaded from a TOML file
/// at `~/.config/colorex/maker.toml` (XDG-style). Every section has serde
/// defaults so a minimal file (or even an empty one) still parses.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MakerNodeConfig {
    #[serde(default)]
    pub maker: MakerSection,
    #[serde(default)]
    pub intervals: IntervalsConfig,
    #[serde(default)]
    pub rebalance: RebalancePolicyConfig,
    /// Optional. Absence → mock RGB backend (no on-chain settlement).
    #[serde(default)]
    pub rgb: Option<RgbConfig>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MakerSection {
    #[serde(default = "default_node_id")]
    pub node_id: String,
    #[serde(default = "default_listen_addr")]
    pub listen_addr: String,
    #[serde(default = "default_broker_url")]
    pub broker_url: String,
}

impl Default for MakerSection {
    fn default() -> Self {
        Self {
            node_id: default_node_id(),
            listen_addr: default_listen_addr(),
            broker_url: default_broker_url(),
        }
    }
}

fn default_node_id() -> String {
    "mock-maker-node".to_owned()
}
fn default_listen_addr() -> String {
    "127.0.0.1:4000".to_owned()
}
fn default_broker_url() -> String {
    "http://127.0.0.1:3000".to_owned()
}

/// Background-loop tick rates. Durations parsed via `humantime_serde`
/// (e.g. `"1s"`, `"60s"`, `"500ms"`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IntervalsConfig {
    #[serde(with = "humantime_serde", default = "default_cleanup_interval")]
    pub cleanup: Duration,
    #[serde(with = "humantime_serde", default = "default_rebalance_interval")]
    pub rebalance: Duration,
    /// How often the chain-observer loop refreshes the wallet UTXO cache +
    /// BTC inventory + checks pending-confirm txes. Only fires when the
    /// real RGB backend is configured (mock has nothing to observe).
    #[serde(with = "humantime_serde", default = "default_chain_observer_interval")]
    pub chain_observer: Duration,
}

impl Default for IntervalsConfig {
    fn default() -> Self {
        Self {
            cleanup: default_cleanup_interval(),
            rebalance: default_rebalance_interval(),
            chain_observer: default_chain_observer_interval(),
        }
    }
}

fn default_cleanup_interval() -> Duration {
    Duration::from_secs(1)
}
fn default_rebalance_interval() -> Duration {
    Duration::from_secs(60)
}
fn default_chain_observer_interval() -> Duration {
    Duration::from_secs(5)
}

/// Mirror of `RebalancePolicy` with `PartialEq` for the config tests. The
/// `RebalancePolicy` struct itself contains an `f64` and can't derive `Eq`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RebalancePolicyConfig {
    #[serde(default = "default_fragmentation_threshold")]
    pub fragmentation_threshold: f64,
    #[serde(default = "default_max_utxo_count")]
    pub max_utxo_count: u64,
    #[serde(default = "default_min_utxo_count")]
    pub min_utxo_count: u64,
}

fn default_fragmentation_threshold() -> f64 {
    0.7
}
fn default_max_utxo_count() -> u64 {
    50
}
fn default_min_utxo_count() -> u64 {
    3
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

/// Library-backed RGB adapter config. Presence of `[rgb]` in the TOML
/// activates the real `LibRgbBackend`; absence keeps the mock.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RgbConfig {
    pub network: String,
    pub data_dir: PathBuf,
    pub wallet_name: String,
    pub electrum_url: String,
    pub contract_id: String,
    pub signer: SignerConfig,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignerConfig {
    pub account_file: PathBuf,
    /// Inline password. Regtest accounts are typically written with an empty
    /// string; mainnet operators set this. Future enhancement: support
    /// `account_password_file` indirection so the TOML doesn't carry the
    /// secret directly.
    #[serde(default)]
    pub password: String,
}

/// Errors surfaced when loading or parsing a `MakerNodeConfig`.
#[derive(Debug)]
pub enum ConfigError {
    Io(std::io::Error),
    Parse(toml::de::Error),
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConfigError::Io(e) => write!(f, "read config: {e}"),
            ConfigError::Parse(e) => write!(f, "parse config: {e}"),
        }
    }
}

impl std::error::Error for ConfigError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ConfigError::Io(e) => Some(e),
            ConfigError::Parse(e) => Some(e),
        }
    }
}

impl From<std::io::Error> for ConfigError {
    fn from(e: std::io::Error) -> Self {
        ConfigError::Io(e)
    }
}

impl From<toml::de::Error> for ConfigError {
    fn from(e: toml::de::Error) -> Self {
        ConfigError::Parse(e)
    }
}

impl MakerNodeConfig {
    /// Default config path: `$XDG_CONFIG_HOME/colorex/maker.toml`, falling
    /// back to `~/.config/colorex/maker.toml`. Uses `dirs::config_dir()`
    /// which honours `XDG_CONFIG_HOME` on Unix; on macOS we deliberately
    /// override to `~/.config/colorex/` (not the Apple `Application Support`
    /// path that `directories::ProjectDirs` would pick) for CLI ergonomics.
    pub fn default_path() -> PathBuf {
        #[cfg(target_os = "macos")]
        {
            // dirs::config_dir() on macOS = ~/Library/Application Support
            // — wrong for a CLI tool. Use $HOME/.config/colorex/ directly.
            if let Some(home) = dirs::home_dir() {
                return home.join(".config/colorex/maker.toml");
            }
        }
        let base = dirs::config_dir().unwrap_or_else(|| PathBuf::from("."));
        base.join("colorex/maker.toml")
    }

    /// Read + parse a TOML config from disk. Tilde paths inside the RGB
    /// section are expanded post-parse via `shellexpand::tilde`.
    pub fn load(path: &std::path::Path) -> Result<Self, ConfigError> {
        let raw = std::fs::read_to_string(path)?;
        Self::load_str(&raw)
    }

    /// Parse a TOML string (test-friendly entry point, used by both the
    /// `loads_example_config` test and any in-process config injection).
    pub fn load_str(s: &str) -> Result<Self, ConfigError> {
        let mut cfg: MakerNodeConfig = toml::from_str(s)?;
        if let Some(rgb) = cfg.rgb.as_mut() {
            rgb.data_dir = expand_tilde_path(&rgb.data_dir);
            rgb.signer.account_file = expand_tilde_path(&rgb.signer.account_file);
        }
        Ok(cfg)
    }
}

fn expand_tilde_path(p: &std::path::Path) -> PathBuf {
    let s = p.to_string_lossy();
    PathBuf::from(shellexpand::tilde(s.as_ref()).into_owned())
}

/// Output of [`build_runtime`]. Holds the maker + (when a real RGB backend
/// is configured) the dependencies the chain-observer loop needs to refresh
/// wallet state out-of-band of the request path.
pub struct MakerNodeRuntime {
    pub maker: Maker,
    pub chain_observer: Option<ChainObserverDeps>,
}

/// Shared with the chain observer so it can drive `LibRgbBackend::sync_wallet`
/// + `list_btc_only_utxos` against the same RGB stash + asset the maker uses.
/// `None` for the mock fallback (nothing on-chain to observe).
pub struct ChainObserverDeps {
    pub rgb_backend: Arc<LibRgbBackend>,
    pub asset: AssetId,
}

/// Thin compatibility shim around [`build_runtime`] for tests + the
/// `inventory` CLI subcommand that don't need the chain-observer deps.
pub async fn build_maker(config: &MakerNodeConfig) -> Result<Maker, Box<dyn std::error::Error>> {
    Ok(build_runtime(config).await?.maker)
}

/// Mint an RGB invoice for the maker's configured contract — the receive side
/// of acquiring inventory from an issuer (`colorex maker invoice`). Requires an
/// `[rgb]` section with a `contract_id`.
pub async fn create_inventory_invoice(
    config: &MakerNodeConfig,
    amount: u64,
) -> Result<String, Box<dyn std::error::Error>> {
    let rgb = config
        .rgb
        .as_ref()
        .ok_or("no [rgb] config: a wallet is required to create an invoice")?;
    if rgb.contract_id.is_empty() {
        return Err("set contract_id in maker.toml [rgb] before creating an invoice".into());
    }
    let asset = AssetId {
        network: rgb.network.parse::<BitcoinNetwork>()?,
        kind: AssetKind::Rgb20,
        id: rgb.contract_id.clone(),
    };
    let backend = LibRgbBackend::new(
        rgb.data_dir.clone(),
        rgb.wallet_name.clone(),
        rgb.network.clone(),
        rgb.electrum_url.clone(),
        rgb.signer.account_file.clone(),
        rgb.signer.password.clone(),
    );
    Ok(backend.create_invoice(&asset, amount).await?)
}

/// Re-derive a consignment the maker already produced, for recovery. `contract`
/// defaults to the config's `[rgb] contract_id` when omitted. Reads the stash only
/// — no chain access, no signing. See [`LibRgbBackend::reconsign`].
pub fn reconsign_consignment(
    config: &MakerNodeConfig,
    contract: Option<String>,
    outpoint: &str,
) -> Result<String, Box<dyn std::error::Error>> {
    let rgb = config
        .rgb
        .as_ref()
        .ok_or("no [rgb] config: a wallet is required to reconsign")?;
    let contract_id = match contract {
        Some(c) if !c.is_empty() => c,
        _ if !rgb.contract_id.is_empty() => rgb.contract_id.clone(),
        _ => return Err("no --contract given and no [rgb] contract_id in maker.toml".into()),
    };
    let backend = LibRgbBackend::new(
        rgb.data_dir.clone(),
        rgb.wallet_name.clone(),
        rgb.network.clone(),
        rgb.electrum_url.clone(),
        rgb.signer.account_file.clone(),
        rgb.signer.password.clone(),
    );
    Ok(backend.reconsign(&contract_id, outpoint)?)
}

/// Re-serve a consignment the maker persisted at settlement, by quote id. Reads
/// `maker.db` only (no chain, no stash) — the cheap recovery path that doesn't
/// re-derive. `None` if the maker never recorded one for that quote.
pub async fn fetch_consignment(
    config: &MakerNodeConfig,
    quote_id: &str,
) -> Result<Option<String>, Box<dyn std::error::Error>> {
    let rgb = config
        .rgb
        .as_ref()
        .ok_or("no [rgb] config: a wallet is required to read consignments")?;
    let db_path = rgb.data_dir.join(&rgb.network).join("maker.db");
    let store = SqliteConsignmentStore::open(&db_path).await?;
    Ok(store
        .get_consignment(&QuoteId(quote_id.to_owned()))
        .await?
        .map(|r| r.consignment))
}

pub async fn build_runtime(
    config: &MakerNodeConfig,
) -> Result<MakerNodeRuntime, Box<dyn std::error::Error>> {
    let maker_id = MakerId(config.maker.node_id.clone());
    let asset = AssetId {
        // Network comes from the RGB config (signet/mainnet/...); the mock path
        // (no [rgb] section) stays on regtest.
        network: match &config.rgb {
            Some(r) => r.network.parse::<BitcoinNetwork>()?,
            None => BitcoinNetwork::Regtest,
        },
        kind: AssetKind::Rgb20,
        id: config
            .rgb
            .as_ref()
            .map(|r| r.contract_id.clone())
            .unwrap_or_else(|| "rgb-test-asset".to_owned()),
    };

    match &config.rgb {
        Some(rgb_cfg) => {
            // Production-ish path: real RGB stash + real electrum-backed
            // chain access + real wallet-derived BTC inventory.
            let backend = Arc::new(LibRgbBackend::new(
                rgb_cfg.data_dir.clone(),
                rgb_cfg.wallet_name.clone(),
                rgb_cfg.network.clone(),
                rgb_cfg.electrum_url.clone(),
                rgb_cfg.signer.account_file.clone(),
                rgb_cfg.signer.password.clone(),
            ));
            let rgb_utxos = backend.list_inventory_utxos(&asset).await?;
            let now_ms = now_ms();
            let btc_inventory = backend.list_btc_only_utxos(&asset, now_ms).await?;
            let bitcoin_client: Arc<dyn BitcoinClient> =
                Arc::new(ElectrumClient::connect(&rgb_cfg.electrum_url)?);
            let rgb_backend_trait: Arc<dyn RgbBackend> = backend.clone();

            // Durable inventory: `maker.db` sits under the wallet-name namespace
            // AND the network sub-dir (alongside the rgb stock), so the same
            // wallet on different networks never shares inventory state.
            let db_path = rgb_cfg.data_dir.join(&rgb_cfg.network).join("maker.db");
            if let Some(parent) = db_path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let inv_store = SqliteInventoryStore::open(&db_path).await?;
            let btc_store = SqliteBtcInventoryStore::open(&db_path).await?;
            let consignment_store = SqliteConsignmentStore::open(&db_path).await?;
            reconcile_rgb_inventory(&inv_store, &asset, &rgb_utxos, now_ms).await?;
            reconcile_btc_inventory(&btc_store, &btc_inventory).await?;

            let inv_store: Arc<dyn InventoryStore> = Arc::new(inv_store);
            let btc_store: Arc<dyn BtcInventoryStore> = Arc::new(btc_store);
            let consignment_store: Arc<dyn ConsignmentStore> = Arc::new(consignment_store);
            let selector: Arc<dyn CoinSelector> = Arc::new(GreedyExactFitSelector);
            let maker =
                Maker::with_components(maker_id, inv_store, selector, rgb_backend_trait, bitcoin_client)
                    .with_btc_store(btc_store)
                    .with_consignment_store(consignment_store);
            Ok(MakerNodeRuntime {
                maker,
                chain_observer: Some(ChainObserverDeps {
                    rgb_backend: backend,
                    asset,
                }),
            })
        }
        None => {
            // Mock fallback: useful for tests + the `maker-node` demo runs
            // without infra. Seeds a single RGB allocation and the
            // deterministic mock BTC inventory the docs/swap-flows.md
            // round trip walks through.
            let utxo = RgbInventoryUtxo {
                outpoint: Outpoint::new(format!("{:064x}", 0u64), 0),
                asset_id: asset,
                amount: 1_000_000,
                btc_sats: 0,
            };
            let rgb_backend: Arc<dyn RgbBackend> =
                Arc::new(MockRgbBackend::new(vec![utxo.clone()]));
            let bitcoin_client = Arc::new(MockBitcoinClient::new());
            bitcoin_client.seed_address_unspent("bcrt1qtaker", mock_taker_funding());
            let maker = Maker::new(maker_id, vec![utxo], rgb_backend, bitcoin_client)
                .with_btc_inventory(mock_btc_inventory());
            Ok(MakerNodeRuntime {
                maker,
                chain_observer: None,
            })
        }
    }
}

/// Reconcile the durable RGB inventory with on-chain UTXOs. A fresh db is
/// seeded outright; a populated one (restart) keeps its persisted reservations
/// and settlement statuses and only ingests UTXOs it isn't already tracking —
/// the chain observer owns confirmation/spend transitions.
async fn reconcile_rgb_inventory(
    store: &SqliteInventoryStore,
    asset: &AssetId,
    rgb_utxos: &[RgbInventoryUtxo],
    now_ms: u64,
) -> Result<(), Box<dyn std::error::Error>> {
    let to_inv = |u: &RgbInventoryUtxo| InventoryUtxo {
        outpoint: u.outpoint.clone(),
        asset_id: u.asset_id.clone(),
        amount: u.amount,
        btc_sats: u.btc_sats,
        status: InventoryStatus::Available,
        created_at_ms: now_ms,
        updated_at_ms: now_ms,
        pending_txid: None,
    };
    if store.is_empty().await {
        store
            .replace_for_asset(asset, rgb_utxos.iter().map(to_inv).collect())
            .await?;
    } else {
        for u in rgb_utxos {
            if store.get(&u.outpoint).await.is_none() {
                let _ = store.ingest_change_utxo(to_inv(u)).await;
            }
        }
    }
    Ok(())
}

/// BTC analogue of [`reconcile_rgb_inventory`].
async fn reconcile_btc_inventory(
    store: &SqliteBtcInventoryStore,
    btc_utxos: &[BtcInventoryUtxo],
) -> Result<(), Box<dyn std::error::Error>> {
    if store.is_empty().await {
        store.replace_all(btc_utxos.to_vec()).await;
    } else {
        for u in btc_utxos {
            if store.get(&u.outpoint).await.is_none() {
                let _ = store.ingest_change_utxo(u.clone()).await;
            }
        }
    }
    Ok(())
}

pub fn now_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Demo buy side: a single large UTXO at the taker's declared funding address,
/// returned by the mock `list_unspent`. Real deployments query electrum.
fn mock_taker_funding() -> Vec<(Outpoint, rfq_btc::TxOut)> {
    let mut p2wpkh = vec![0x00, 0x14];
    p2wpkh.extend(std::iter::repeat_n(0x22, 20));
    vec![(
        Outpoint::new(format!("{:064x}", 0xfeedu64), 0),
        rfq_btc::TxOut {
            value_sats: 100_000_000,
            script_pubkey: p2wpkh,
        },
    )]
}

/// Deterministic segwit BTC UTXOs the mock maker pays sell-side takers from.
fn mock_btc_inventory() -> Vec<BtcInventoryUtxo> {
    let p2wpkh = || {
        let mut s = vec![0x00, 0x14];
        s.extend(std::iter::repeat_n(0x11, 20));
        s
    };
    (0..3u64)
        .map(|i| BtcInventoryUtxo {
            outpoint: Outpoint::new(format!("{:064x}", 0xb7c0 + i), 0),
            value_sats: 1_000_000,
            script_pubkey: p2wpkh(),
            status: BtcInventoryStatus::Available,
            created_at_ms: 0,
            updated_at_ms: 0,
            pending_txid: None,
        })
        .collect()
}

#[derive(Clone)]
pub struct MakerNodeState {
    pub maker: Maker,
    pub store: InMemoryQuoteStore,
}

pub fn maker_app(maker: Maker) -> Router {
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
        state
            .maker
            .submit_signed_psbt(quote_id, body.signed_psbt)
            .await?,
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
pub enum MakerNodeHttpError {
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

pub fn spawn_cleanup_loop(maker: Maker, cleanup_interval: Duration) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = time::interval(cleanup_interval);

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
pub fn spawn_rebalance_loop(
    maker: Maker,
    rebalance_interval: Duration,
    policy: RebalancePolicy,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = time::interval(rebalance_interval);

        loop {
            interval.tick().await;
            let plan = maker.rebalance(&policy).await;
            if !plan.is_empty() {
                println!("rebalance_plan triggers={:?}", plan.triggers);
            }
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
    use rfq_types::{RfqId, SettlementStatus, Side, SwapLeg};
    use tower::ServiceExt;

    fn test_config() -> MakerNodeConfig {
        MakerNodeConfig {
            maker: MakerSection {
                node_id: "test-maker".to_owned(),
                ..MakerSection::default()
            },
            intervals: IntervalsConfig::default(),
            rebalance: RebalancePolicyConfig::default(),
            rgb: None,
        }
    }

    #[test]
    fn loads_minimal_config_uses_defaults() {
        let cfg = MakerNodeConfig::load_str("").expect("empty TOML parses with defaults");
        assert_eq!(cfg.maker.node_id, "mock-maker-node");
        assert_eq!(cfg.maker.listen_addr, "127.0.0.1:4000");
        assert_eq!(cfg.maker.broker_url, "http://127.0.0.1:3000");
        assert_eq!(cfg.intervals.cleanup, Duration::from_secs(1));
        assert_eq!(cfg.intervals.rebalance, Duration::from_secs(60));
        assert_eq!(cfg.intervals.chain_observer, Duration::from_secs(5));
        assert!(cfg.rgb.is_none());
    }

    #[test]
    fn loads_example_config() {
        let raw = include_str!("../config.toml.example");
        let cfg = MakerNodeConfig::load_str(raw).expect("example TOML parses");
        assert_eq!(cfg.maker.node_id, "node·7af2");
        let rgb = cfg.rgb.expect("example has [rgb] block");
        assert_eq!(rgb.network, "regtest");
        assert_eq!(
            rgb.contract_id,
            "rgb:HvGfPj8l-7PK6bkl-WgWvEPH-_zV4VSZ-v2EPZ_p-6Wr7PvM---"
        );
        // Tilde expansion fires at load time.
        assert!(
            !rgb.data_dir.to_string_lossy().starts_with('~'),
            "data_dir tilde should expand: {}",
            rgb.data_dir.display()
        );
        assert!(!rgb.signer.account_file.to_string_lossy().starts_with('~'));
    }

    async fn test_app() -> Router {
        maker_app(build_maker(&test_config()).await.unwrap())
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
                btc_funding_addr: "bcrt1qtaker".to_owned(),
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
                btc_funding_addr: "bcrt1qtaker".to_owned(),
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

    async fn request_quote(app: Router, rfq_id: &str) -> Quote {
        let response = app
            .oneshot(json_request(Method::POST, "/quotes", &quote_request(rfq_id)))
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
}

/// Periodic chain-observer loop. On each tick:
/// 1. Refresh the bp-wallet on-disk UTXO cache via electrum
///    (`LibRgbBackend::sync_wallet`).
/// 2. Re-list wallet-derived BTC inventory and ingest any new outpoints
///    into the maker's BTC store (`Maker::ingest_btc_change_utxos`).
/// 3. Sweep `PendingBitcoinConfirm` reservations against the chain
///    (`Maker::sweep_confirmations`).
///
/// Only spawned when `RgbConfig` is present; the mock fallback has no
/// chain to observe. Closes the runtime gap from issue #27: without this
/// loop the daemon's view of its own wallet state freezes at startup,
/// and pending-confirm reservations stay pending forever.
pub fn spawn_chain_observer_loop(
    maker: Maker,
    deps: ChainObserverDeps,
    tick_interval: Duration,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = time::interval(tick_interval);
        // Skip the immediate first-tick `interval.tick()` returns so the
        // observer starts ~`tick_interval` after spawn rather than racing
        // the maker's own startup snapshot.
        interval.tick().await;
        loop {
            interval.tick().await;
            if let Err(e) = deps.rgb_backend.sync_wallet().await {
                eprintln!("chain_observer wallet sync failed (continuing): {e}");
                continue;
            }
            let now = now_ms();
            match deps.rgb_backend.list_btc_only_utxos(&deps.asset, now).await {
                Ok(utxos) => {
                    let added = maker.ingest_btc_change_utxos(utxos).await;
                    if added > 0 {
                        println!("chain_observer ingested_btc_utxos={added}");
                    }
                }
                Err(e) => {
                    eprintln!("chain_observer list_btc_only_utxos failed: {e}");
                }
            }
            // Refresh RGB inventory too — consecutive maker-side swaps
            // would otherwise stall after the first one. The maker's `/sign`
            // intentionally does *not* ingest the change UTXO; the chain
            // observer adds it here with `Available` status once `sync_wallet`
            // sees the new outpoint, mirroring `ingest_btc_change_utxos`.
            match deps.rgb_backend.list_inventory_utxos(&deps.asset).await {
                Ok(utxos) => {
                    let added = maker.ingest_rgb_change_utxos(utxos).await;
                    if added > 0 {
                        println!("chain_observer ingested_rgb_utxos={added}");
                    }
                }
                Err(e) => {
                    eprintln!("chain_observer list_inventory_utxos failed: {e}");
                }
            }
            let spent = maker.sweep_confirmations().await;
            if spent > 0 {
                println!("chain_observer marked_spent={spent}");
            }
        }
    })
}
