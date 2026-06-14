//! Library entry points for the `colorex` binary. Exposes the config
//! types, runtime builder, axum `Router`, and background-loop spawners so
//! the binary (`src/main.rs`) and integration tests
//! (`tests/regtest_http_round_trip.rs`) can share them.

pub mod broker_client;
pub mod init;
pub mod node_key;
pub mod orders;
pub mod output;

use std::{path::PathBuf, str::FromStr, sync::Arc, time::Duration};

use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use rfq_btc::{BitcoinClient, ElectrumClient, MockBitcoinClient};
use rfq_maker::{
    assemble_rebalance_tx, estimate_rebalance_vbytes, plan_ladder, AssetSplit, BtcSplit,
    CoinSelector, GreedyExactFitSelector, LadderSpec, Maker, RebalancePolicy,
    REBALANCE_ANCHOR_SATS,
};
use rfq_rgb::{ContractId, LibRgbBackend, MockRgbBackend, RgbBackend};
use rfq_router::MakerConnector;
use rfq_store::{
    BtcInventoryStore, ConsignmentStore, ContractRecord, ContractStore, InMemoryOrderStore,
    InMemoryQuoteStore, InventoryStore, OrderStore, QuoteStore, SqliteBtcInventoryStore,
    SqliteConsignmentStore, SqliteContractStore, SqliteFillStore, SqliteInventoryStore,
    SqliteOrderStore,
};
use rfq_types::{
    AcceptQuoteRequest, AssetId, AssetKind, BitcoinNetwork, BtcInventoryStatus, BtcInventoryUtxo,
    HealthResponse, InventorySnapshot, InventoryStatus, InventoryUtxo, MakerId, Outpoint, Quote,
    QuoteId, QuoteRequest, RgbInventoryUtxo, SettlementIntent,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
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
    /// How often the auto-mirror strategy loop processes unmirrored fills and
    /// places mirror orders. Matches the order-reload cadence (5s).
    #[serde(with = "humantime_serde", default = "default_strategy_interval")]
    pub strategy: Duration,
}

impl Default for IntervalsConfig {
    fn default() -> Self {
        Self {
            cleanup: default_cleanup_interval(),
            rebalance: default_rebalance_interval(),
            chain_observer: default_chain_observer_interval(),
            strategy: default_strategy_interval(),
        }
    }
}

fn default_strategy_interval() -> Duration {
    Duration::from_secs(5)
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
    /// Master switch for the proactive split EXECUTOR (the log-only planner runs
    /// regardless). Opt-in — the executor builds + broadcasts on-chain txs.
    #[serde(default)]
    pub enabled: bool,
    /// Largest RGB ladder rung, in asset units (per asset the maker trades).
    #[serde(default = "default_rgb_ladder_base")]
    pub rgb_ladder_base: u64,
    /// Largest BTC ladder rung, in sats.
    #[serde(default = "default_btc_ladder_base_sats")]
    pub btc_ladder_base_sats: u64,
    /// Geometric shrink factor per tier (halving = 0.5).
    #[serde(default = "default_ladder_ratio")]
    pub ladder_ratio: f64,
    /// Number of distinct rung sizes per ladder.
    #[serde(default = "default_ladder_tiers")]
    pub ladder_tiers: u32,
    /// Desired pieces per tier.
    #[serde(default = "default_ladder_copies")]
    pub ladder_copies: u32,
    /// BTC sats kept spendable for sell-side payouts — the executor won't drain
    /// the pool below this.
    #[serde(default = "default_min_btc_reserve_sats")]
    pub min_btc_reserve_sats: u64,
    /// Absolute cap (sats) on a rebalance tx's fee. The fee itself is the
    /// next-block feerate × the tx's estimated vsize (so it confirms promptly);
    /// this just bounds it against a wild feerate estimate.
    #[serde(default = "default_rebalance_max_fee_sats")]
    pub rebalance_max_fee_sats: u64,
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
fn default_rgb_ladder_base() -> u64 {
    1_000
}
fn default_btc_ladder_base_sats() -> u64 {
    50_000
}
fn default_ladder_ratio() -> f64 {
    0.5
}
fn default_ladder_tiers() -> u32 {
    4
}
fn default_ladder_copies() -> u32 {
    2
}
fn default_min_btc_reserve_sats() -> u64 {
    5_000
}
fn default_rebalance_max_fee_sats() -> u64 {
    10_000
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

impl RebalancePolicyConfig {
    /// RGB ladder spec: any positive rung amount is valid (the asset amount
    /// doesn't affect cost — every rung anchors on 546 sats).
    pub fn rgb_ladder(&self) -> LadderSpec {
        LadderSpec {
            base: self.rgb_ladder_base,
            ratio: self.ladder_ratio,
            tiers: self.ladder_tiers,
            copies: self.ladder_copies,
            min_piece: rfq_maker::DEFAULT_RGB_MIN_PIECE,
        }
    }

    /// BTC ladder spec: rungs floor at the smallest spendable BTC piece (1000).
    pub fn btc_ladder(&self) -> LadderSpec {
        LadderSpec {
            base: self.btc_ladder_base_sats,
            ratio: self.ladder_ratio,
            tiers: self.ladder_tiers,
            copies: self.ladder_copies,
            min_piece: rfq_maker::DEFAULT_BTC_MIN_PIECE_SATS,
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
            enabled: false,
            rgb_ladder_base: default_rgb_ladder_base(),
            btc_ladder_base_sats: default_btc_ladder_base_sats(),
            ladder_ratio: default_ladder_ratio(),
            ladder_tiers: default_ladder_tiers(),
            ladder_copies: default_ladder_copies(),
            min_btc_reserve_sats: default_min_btc_reserve_sats(),
            rebalance_max_fee_sats: default_rebalance_max_fee_sats(),
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
    /// A field parsed but holds a semantically invalid value.
    Invalid(String),
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConfigError::Io(e) => write!(f, "read config: {e}"),
            ConfigError::Parse(e) => write!(f, "parse config: {e}"),
            ConfigError::Invalid(msg) => write!(f, "invalid config: {msg}"),
        }
    }
}

impl std::error::Error for ConfigError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ConfigError::Io(e) => Some(e),
            ConfigError::Parse(e) => Some(e),
            ConfigError::Invalid(_) => None,
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
        // The ladder shrinks each tier by `ladder_ratio`; it must be in (0, 1).
        // >= 1 would grow rungs unboundedly, <= 0 collapses them — both nonsense.
        let ratio = cfg.rebalance.ladder_ratio;
        if !(ratio > 0.0 && ratio < 1.0) {
            return Err(ConfigError::Invalid(format!(
                "rebalance.ladder_ratio must be in (0, 1), got {ratio}"
            )));
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
    /// Same backend + assets as the chain observer, for the proactive rebalance
    /// executor. `None` for the mock fallback. Spawned only when the config's
    /// `rebalance.enabled` is set.
    pub rebalance_executor: Option<RebalanceExecutorDeps>,
    /// The standing-order store (maker.db `orders` table, or in-memory for the
    /// mock fallback) — driven by the order-reload + strategy loops in `run()`.
    pub order_store: Arc<dyn OrderStore>,
    /// Asset id → decimal precision, snapshot from the contract registry at
    /// startup. Used to price orders in sats-per-token (a quote of `amount`
    /// smallest units costs `price * amount / 10^precision`). Snapshot, not live:
    /// importing a new contract needs a daemon restart to be traded anyway
    /// (inventory is reconciled per asset at startup), so precision follows suit.
    pub precisions: Arc<HashMap<String, u8>>,
}

/// Shared with the chain observer so it can drive `LibRgbBackend::sync_wallet`
/// + `list_btc_only_utxos` against the same RGB stash + assets the maker trades.
/// `assets` is every registered contract (the observer refreshes each one's RGB
/// inventory and excludes all of them from the BTC pool). `None` for the mock
/// fallback (nothing on-chain to observe).
pub struct ChainObserverDeps {
    pub rgb_backend: Arc<LibRgbBackend>,
    pub assets: Vec<AssetId>,
}

/// Dependencies for the rebalance executor loop. Holds the concrete backend
/// (needed for `split_pools`, an inherent method) + the asset list, same as
/// [`ChainObserverDeps`]. `None` for the mock fallback (nothing on-chain).
pub struct RebalanceExecutorDeps {
    pub rgb_backend: Arc<LibRgbBackend>,
    pub assets: Vec<AssetId>,
}

/// Thin compatibility shim around [`build_runtime`] for tests + the
/// `inventory` CLI subcommand that don't need the chain-observer deps.
pub async fn build_maker(config: &MakerNodeConfig) -> Result<Maker, Box<dyn std::error::Error>> {
    Ok(build_runtime(config).await?.maker)
}

/// Register a contract into the maker's on-disk registry (`maker.db`) so the
/// daemon seeds + trades it on the next `build_runtime`. The CLI's `contract
/// import` reads `ticker`/`precision` from the stock; this lower-level seed (used
/// by setup code + e2e tests) leaves them blank since the daemon keys off the
/// contract id alone.
pub async fn seed_contract_registry(
    config: &MakerNodeConfig,
    contract_id: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let rgb = config
        .rgb
        .as_ref()
        .ok_or("no [rgb] config: the contract registry lives in maker.db")?;
    let db_path = rgb.data_dir.join(&rgb.network).join("maker.db");
    if let Some(parent) = db_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    SqliteContractStore::open(&db_path)
        .await?
        .upsert(ContractRecord {
            contract_id: contract_id.to_owned(),
            ticker: String::new(),
            precision: 0,
            network: rgb.network.clone(),
            added_at_ms: now_ms(),
        })
        .await?;
    Ok(())
}

/// Mint an RGB invoice for `contract_id` — the receive side of acquiring
/// inventory from an issuer (`colorex maker wallet invoice`). The contract is
/// resolved from the registry by the CLI layer.
pub async fn create_inventory_invoice(
    config: &MakerNodeConfig,
    contract_id: String,
    amount: u64,
) -> Result<String, Box<dyn std::error::Error>> {
    let rgb = config
        .rgb
        .as_ref()
        .ok_or("no [rgb] config: a wallet is required to create an invoice")?;
    let asset = AssetId {
        network: rgb.network.parse::<BitcoinNetwork>()?,
        kind: AssetKind::Rgb20,
        id: contract_id,
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

/// Accept an incoming consignment into the maker's stash — the receive-side
/// counterpart to [`create_inventory_invoice`]. After `maker invoice` and the
/// issuer's `issuer transfer`, the issuer hands back a consignment; this verifies
/// + absorbs it so `maker up` picks the RGB up as inventory (once the anchoring
/// tx confirms). `contract_id` is resolved from the registry by the CLI layer.
/// See [`LibRgbBackend::accept_incoming_transfer`].
pub async fn accept_inventory_consignment(
    config: &MakerNodeConfig,
    contract_id: String,
    consignment_base64: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let rgb = config
        .rgb
        .as_ref()
        .ok_or("no [rgb] config: a wallet is required to accept a consignment")?;
    let contract_id = ContractId::from_str(&contract_id)
        .map_err(|e| format!("invalid contract id {contract_id}: {e}"))?;
    let backend = LibRgbBackend::new(
        rgb.data_dir.clone(),
        rgb.wallet_name.clone(),
        rgb.network.clone(),
        rgb.electrum_url.clone(),
        rgb.signer.account_file.clone(),
        rgb.signer.password.clone(),
    );
    backend
        .accept_incoming_transfer(consignment_base64, contract_id)
        .await?;
    Ok(())
}

/// Re-derive a consignment the maker already produced, for recovery.
/// `contract_id` is resolved from the registry by the CLI layer. Reads the stash
/// only — no chain access, no signing. See [`LibRgbBackend::reconsign`].
pub fn reconsign_consignment(
    config: &MakerNodeConfig,
    contract_id: String,
    outpoint: &str,
) -> Result<String, Box<dyn std::error::Error>> {
    let rgb = config
        .rgb
        .as_ref()
        .ok_or("no [rgb] config: a wallet is required to reconsign")?;
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

    match &config.rgb {
        Some(rgb_cfg) => {
            let network = rgb_cfg.network.parse::<BitcoinNetwork>()?;
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

            // Durable inventory: `maker.db` sits under the wallet-name namespace
            // AND the network sub-dir (alongside the rgb stock), so the same
            // wallet on different networks never shares inventory state.
            let db_path = rgb_cfg.data_dir.join(&rgb_cfg.network).join("maker.db");
            if let Some(parent) = db_path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let contract_store = SqliteContractStore::open(&db_path).await?;
            // Snapshot asset id → precision so orders price in sats-per-token.
            let precisions: Arc<HashMap<String, u8>> = Arc::new(
                contract_store
                    .list()
                    .await?
                    .into_iter()
                    .map(|c| (c.contract_id, c.precision))
                    .collect(),
            );
            let assets = load_registered_assets(&contract_store, network).await?;

            let now_ms = now_ms();
            let bitcoin_client: Arc<dyn BitcoinClient> =
                Arc::new(ElectrumClient::connect(&rgb_cfg.electrum_url)?);
            let rgb_backend_trait: Arc<dyn RgbBackend> = backend.clone();

            let inv_store = SqliteInventoryStore::open(&db_path).await?;
            let btc_store = SqliteBtcInventoryStore::open(&db_path).await?;
            let consignment_store = SqliteConsignmentStore::open(&db_path).await?;
            let fills_store = SqliteFillStore::open(&db_path).await?;
            let order_store = SqliteOrderStore::open(&db_path).await?;

            // Seed/reconcile each traded contract's RGB inventory independently;
            // the BTC pool excludes every contract's allocations at once.
            for asset in &assets {
                let rgb_utxos = backend.list_inventory_utxos(asset).await?;
                reconcile_rgb_inventory(&inv_store, asset, &rgb_utxos, now_ms).await?;
            }
            let btc_inventory = backend.list_btc_only_utxos(&assets, now_ms).await?;
            reconcile_btc_inventory(&btc_store, &btc_inventory).await?;

            let inv_store: Arc<dyn InventoryStore> = Arc::new(inv_store);
            let btc_store: Arc<dyn BtcInventoryStore> = Arc::new(btc_store);
            let consignment_store: Arc<dyn ConsignmentStore> = Arc::new(consignment_store);
            let fills_store: Arc<dyn rfq_store::FillStore> = Arc::new(fills_store);
            let order_store: Arc<dyn OrderStore> = Arc::new(order_store);
            let selector: Arc<dyn CoinSelector> = Arc::new(GreedyExactFitSelector);
            let mut maker = Maker::with_components(
                maker_id,
                inv_store,
                selector,
                rgb_backend_trait,
                bitcoin_client,
            )
            .with_btc_store(btc_store)
            .with_consignment_store(consignment_store)
            .with_fills_store(fills_store);
            // Settlement-piggyback shares the rebalance opt-in: when enabled, buy
            // swaps split the maker's RGB change into the same RGB ladder.
            if config.rebalance.enabled {
                maker = maker
                    .with_piggyback_ladder(config.rebalance.rgb_ladder())
                    .with_piggyback_btc_ladder(config.rebalance.btc_ladder());
            }
            Ok(MakerNodeRuntime {
                maker,
                chain_observer: Some(ChainObserverDeps {
                    rgb_backend: backend.clone(),
                    assets: assets.clone(),
                }),
                rebalance_executor: Some(RebalanceExecutorDeps {
                    rgb_backend: backend,
                    assets,
                }),
                order_store,
                precisions,
            })
        }
        None => {
            // Mock fallback: useful for tests + the `maker-node` demo runs
            // without infra. Seeds a single RGB allocation and the
            // deterministic mock BTC inventory the docs/swap-flows.md
            // round trip walks through.
            let asset = AssetId {
                network: BitcoinNetwork::Regtest,
                kind: AssetKind::Rgb20,
                id: "rgb-test-asset".to_owned(),
            };
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
                rebalance_executor: None,
                order_store: Arc::new(InMemoryOrderStore::new()),
                precisions: Arc::new(HashMap::new()),
            })
        }
    }
}

/// Resolve the assets the maker trades from the contract registry. The registry
/// (`colorex maker contract import`) is the sole source of truth — an empty
/// registry means the maker trades nothing yet (the daemon still runs; it just
/// quotes no RGB until a contract is imported).
async fn load_registered_assets(
    contract_store: &SqliteContractStore,
    network: BitcoinNetwork,
) -> Result<Vec<AssetId>, Box<dyn std::error::Error>> {
    Ok(contract_store
        .list()
        .await?
        .into_iter()
        .map(|c| AssetId {
            network,
            kind: AssetKind::Rgb20,
            id: c.contract_id,
        })
        .collect())
}

/// Reconcile one contract's durable RGB inventory with on-chain UTXOs. A
/// not-yet-tracked asset is seeded outright; an already-tracked one (restart, or
/// another asset already in the db) keeps its persisted reservations and
/// settlement statuses and only ingests UTXOs it isn't tracking — the chain
/// observer owns confirmation/spend transitions. The empty-check is per-asset
/// (not the whole store) so seeding a second contract never trips the merge path.
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
    if store.list_for_asset(asset).await.is_empty() {
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
    /// Taker's RGB UTXOs being sold (provenance model). Optional for older callers.
    #[serde(default)]
    outpoints: Vec<Outpoint>,
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
            .deliver_consignment(quote_id, body.consignment, body.outpoints)
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

/// Hot-reload the standing-order book into the maker without a restart. The
/// `order create`/`cancel` CLI writes the order-book file (a separate process);
/// this loop re-reads it on `interval` and swaps the maker's price policy, so new
/// orders are quoted within one tick. (The broker's advertised `/prices` snapshot,
/// sent at WS registration, still lags until reconnect — a later #30 enhancement.)
pub fn spawn_order_reload_loop(
    maker: Maker,
    order_store: Arc<dyn OrderStore>,
    precisions: Arc<HashMap<String, u8>>,
    reload_interval: Duration,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = time::interval(reload_interval);
        let mut last: Option<usize> = None;
        loop {
            interval.tick().await;
            match order_store.list().await {
                Ok(orders) => {
                    let count = orders.len();
                    maker.reload_price_policy(crate::orders::price_policy(&orders, &precisions));
                    if last != Some(count) {
                        println!("reloaded standing orders: {count}");
                        last = Some(count);
                    }
                }
                Err(e) => eprintln!("order reload failed: {e}"),
            }
        }
    })
}

/// Auto-mirror strategy loop: on each tick, for every settled fill not yet
/// mirrored, if the order that produced it is `mirror`-enabled, upsert the
/// OPPOSITE-side order (priced off the fill by its spread, sized to the fill).
/// Marks the fill mirrored only AFTER the upsert persists, so a crash mid-cycle
/// retries idempotently next tick. Skips (and marks) fills whose source order is
/// gone or non-mirror, and won't clobber an operator-pinned opposite order.
pub fn spawn_strategy_loop(
    maker: Maker,
    order_store: Arc<dyn OrderStore>,
    precisions: Arc<HashMap<String, u8>>,
    interval: Duration,
) -> JoinHandle<()> {
    use crate::orders;
    use rfq_store::side_str;
    tokio::spawn(async move {
        let mut tick = time::interval(interval);
        loop {
            tick.tick().await;
            let fills = maker.list_unmirrored_fills().await;
            if fills.is_empty() {
                continue;
            }
            let mut placed = false;
            for f in fills {
                // Look up the order that produced this fill (same asset+side).
                let src = match order_store.get(&f.asset_id, side_str(&f.side)).await {
                    Ok(Some(o)) if o.mirror => o,
                    Ok(_) => {
                        // Order gone or not mirror-enabled — nothing to mirror.
                        maker.mark_fill_mirrored(&f.quote_id).await;
                        continue;
                    }
                    Err(e) => {
                        eprintln!("strategy: order lookup failed: {e}");
                        continue; // retry next tick
                    }
                };
                if f.amount == 0 {
                    maker.mark_fill_mirrored(&f.quote_id).await;
                    continue;
                }
                // Operator-pinned guard: don't overwrite a hand-set opposite order.
                let opp = orders::opposite(f.side.clone());
                // Accumulate onto the standing opposite-side mirror order so
                // repeated fills grow the buy-back instead of clobbering it.
                let base_size = match order_store.get(&f.asset_id, side_str(&opp)).await {
                    Ok(Some(existing)) if !existing.mirror => {
                        eprintln!(
                            "strategy: mirror skipped (opposite-side order is operator-pinned)"
                        );
                        maker.mark_fill_mirrored(&f.quote_id).await;
                        continue;
                    }
                    Ok(Some(existing)) => existing.size,
                    Ok(None) => 0,
                    Err(e) => {
                        eprintln!("strategy: opposite lookup failed: {e}");
                        continue;
                    }
                };
                // Fill price is TOTAL gross sats; recover the per-token unit price
                // (sats per whole token): total_sats * 10^precision / amount,
                // rounded to nearest. f.amount is guaranteed non-zero above.
                let precision = precisions.get(&f.asset_id).copied().unwrap_or(0);
                let scale = 10u128.pow(precision as u32);
                let unit = (((f.price as u128) * scale + (f.amount as u128) / 2)
                    / (f.amount as u128))
                    .min(u64::MAX as u128) as u64;
                let mirror = orders::build_mirror_order(
                    &f.asset_id,
                    f.side.clone(),
                    unit,
                    f.amount,
                    base_size,
                    src.mirror_spread_bps,
                );
                let mirror_side = mirror.side.clone();
                let mirror_price = mirror.price;
                if let Err(e) = order_store.upsert(mirror).await {
                    eprintln!("strategy: upsert mirror failed: {e}");
                    continue; // leave fill unmirrored — retry next tick (idempotent)
                }
                maker.mark_fill_mirrored(&f.quote_id).await;
                placed = true;
                println!(
                    "mirror placed: {mirror_side} {} @ {mirror_price}/token (from fill {})",
                    f.amount, f.quote_id.0
                );
            }
            if placed {
                // Instant effect (don't wait for the 5s reload tick).
                if let Ok(orders) = order_store.list().await {
                    maker.reload_price_policy(orders::price_policy(&orders, &precisions));
                }
            }
        }
    })
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

/// Outcome of one executor tick. `pub` so the e2e suite can drive a single
/// `rebalance_once` pass and assert on the result.
#[derive(Debug)]
pub enum RebalanceTick {
    /// Pools already match their ladders — nothing to do.
    Idle,
    /// A guard prevented action this tick (in-flight split, budget, contention).
    Skipped(String),
    /// A split tx was built + broadcast; `rgb_sources` are its consumed RGB
    /// sources, tracked until they confirm so the loop doesn't pile on.
    Split {
        txid: String,
        rgb_sources: Vec<Outpoint>,
    },
}

/// Proactive rebalance EXECUTOR loop (the acting successor to the log-only
/// [`spawn_rebalance_loop`]). Each tick: skip while a prior split is still
/// settling; else plan per-asset RGB ladders + the BTC ladder, build ONE
/// `split_pools` tx, broadcast it, and mark the consumed sources pending so the
/// chain observer's `sweep_confirmations` retires them and ingests the new
/// pieces. Anti-thrash: reserving a source removes it from Available (so it's
/// never re-picked), and the in-flight guard skips planning until the previous
/// split's sources read `Spent`. See `docs/rebalancing-strategy.md`.
#[allow(clippy::too_many_arguments)]
pub fn spawn_rebalance_executor_loop(
    maker: Maker,
    deps: RebalanceExecutorDeps,
    interval: Duration,
    rgb_spec: LadderSpec,
    btc_spec: LadderSpec,
    min_btc_reserve_sats: u64,
    max_fee_sats: u64,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut tick = time::interval(interval);
        tick.tick().await; // skip the immediate first tick
        let mut inflight: Option<Vec<Outpoint>> = None;
        loop {
            tick.tick().await;
            // Wait out a previous split: skip planning until its RGB sources
            // confirm (read `Spent`), so we don't stack a second split on top.
            if let Some(sources) = &inflight {
                if maker.rgb_sources_settled(sources).await {
                    inflight = None;
                } else {
                    continue;
                }
            }
            match rebalance_once(
                &maker,
                &deps,
                &rgb_spec,
                &btc_spec,
                min_btc_reserve_sats,
                max_fee_sats,
            )
            .await
            {
                RebalanceTick::Idle => {}
                RebalanceTick::Skipped(reason) => eprintln!("rebalance skipped: {reason}"),
                RebalanceTick::Split { txid, rgb_sources } => {
                    println!("rebalance split broadcast: {txid}");
                    inflight = Some(rgb_sources);
                }
            }
        }
    })
}

/// One executor pass: plan → reserve → build `split_pools` → broadcast → mark
/// pending. Releases reservations on any build/broadcast failure so a transient
/// error doesn't strand liquidity. `pub` so the e2e suite can drive a single
/// deterministic pass (the loop wrapper just adds cadence + the in-flight guard).
pub async fn rebalance_once(
    maker: &Maker,
    deps: &RebalanceExecutorDeps,
    rgb_spec: &LadderSpec,
    btc_spec: &LadderSpec,
    min_btc_reserve_sats: u64,
    max_fee_sats: u64,
) -> RebalanceTick {
    // 1. Per-asset RGB ladders.
    let mut asset_splits = Vec::new();
    for asset in &deps.assets {
        let avail = maker.available_rgb_for(asset).await;
        let pairs: Vec<(Outpoint, u64)> = avail.iter().map(|(o, a, _)| (o.clone(), *a)).collect();
        if let Some((source, source_amount, rungs)) = plan_ladder(&pairs, rgb_spec) {
            let source_btc_sats = avail
                .iter()
                .find(|(o, _, _)| *o == source)
                .map(|(_, _, s)| *s)
                .unwrap_or(0);
            asset_splits.push(AssetSplit {
                asset: asset.clone(),
                source,
                source_amount,
                source_btc_sats,
                rungs,
            });
        }
    }

    // 2. BTC ladder + the fattest BTC UTXO (which funds anchors + fee even when
    //    BTC itself needs no laddering).
    let btc_avail = maker.available_btc().await;
    let btc_total: u64 = btc_avail.iter().map(|(_, s)| *s).sum();
    let fattest_btc = btc_avail.iter().max_by_key(|(_, s)| *s).cloned();
    let btc_split =
        plan_ladder(&btc_avail, btc_spec).map(|(source, source_sats, rungs)| BtcSplit {
            source,
            source_sats,
            rungs,
        });

    if asset_splits.is_empty() && btc_split.is_none() {
        return RebalanceTick::Idle;
    }

    // The BTC input that funds the tx: the ladder's source if BTC is laddering,
    // else just the fattest UTXO as a fee/anchor funder.
    let btc_fee_source = btc_split
        .as_ref()
        .map(|b| (b.source.clone(), b.source_sats))
        .or_else(|| fattest_btc.clone());
    if !asset_splits.is_empty() && btc_fee_source.is_none() {
        return RebalanceTick::Skipped(
            "no BTC available to fund RGB split anchors + fee (fund keychain-0)".to_owned(),
        );
    }
    let btc_source_value = btc_fee_source.as_ref().map(|(_, v)| *v).unwrap_or(0);

    // Fee = next-block feerate × the tx's estimated vsize, capped. Computed from
    // input/output counts (all P2TR keyspend). `fee_of` is reused for the budget
    // (full plan) and the final tx (post-scaledown plan). Targets the next block
    // so fresh pieces are usable promptly.
    let network = deps
        .assets
        .first()
        .map(|a| a.network)
        .unwrap_or(BitcoinNetwork::Regtest);
    let feerate = maker.next_block_feerate(&network).await;
    let has_btc_source = btc_fee_source.is_some();
    let fee_of = |assets: usize, rgb_rungs: usize, btc_rungs: usize| -> u64 {
        let inputs = assets + usize::from(has_btc_source);
        // host (only when RGB present) + rgb rungs + btc rungs + change
        let outputs = usize::from(assets > 0) + rgb_rungs + btc_rungs + 1;
        feerate
            .saturating_mul(estimate_rebalance_vbytes(inputs, outputs))
            .min(max_fee_sats)
    };
    // Provisional fee from the full plan funds the budget (>= the final fee, so
    // the tx — with possibly fewer outputs after scaledown — always fits).
    let budget_fee = fee_of(
        asset_splits.len(),
        asset_splits.iter().map(|a| a.rungs.len()).sum(),
        btc_split.as_ref().map(|b| b.rungs.len()).unwrap_or(0),
    );

    // 3. Budget: scales RGB rungs down to what the BTC source can fund.
    let plan = match assemble_rebalance_tx(asset_splits, btc_split, btc_source_value, budget_fee) {
        Some(p) => p,
        None => {
            return RebalanceTick::Skipped(
                "budget can't fund any split (fund keychain-0)".to_owned(),
            )
        }
    };

    // Final fee for the actual (post-scaledown) tx.
    let rgb_rung_count: u64 = plan.assets.iter().map(|a| a.rungs.len() as u64).sum();
    let fee_sats = fee_of(
        plan.assets.len(),
        rgb_rung_count as usize,
        plan.btc.as_ref().map(|b| b.rungs.len()).unwrap_or(0),
    );

    // 4. Reserve floor: keep sell-side liquidity. Only RGB rungs net-drain BTC
    //    (anchors); BTC rungs/change return to the pool. Conservative estimate.
    if !plan.assets.is_empty() {
        let net_drain = fee_sats + REBALANCE_ANCHOR_SATS * (1 + rgb_rung_count);
        if btc_total.saturating_sub(net_drain) < min_btc_reserve_sats {
            return RebalanceTick::Skipped(format!(
                "would breach BTC reserve {min_btc_reserve_sats} (pool {btc_total}, drain ~{net_drain})"
            ));
        }
    }

    // 5. Reserve every source so quoting won't grab one mid-split.
    let mut rgb_reservations = Vec::new();
    let mut rgb_sources = Vec::new();
    for a in &plan.assets {
        match maker.reserve_rgb_for_rebalance(a.source.clone()).await {
            Ok(r) => {
                rgb_reservations.push(r);
                rgb_sources.push(a.source.clone());
            }
            Err(e) => {
                maker.release_rebalance(&rgb_reservations, None).await;
                return RebalanceTick::Skipped(format!("rgb reserve failed: {e}"));
            }
        }
    }
    let (btc_source_op, btc_reservation) = match &btc_fee_source {
        Some((op, _)) => match maker.reserve_btc_for_rebalance(op.clone()).await {
            Ok(r) => (Some(op.clone()), Some(r)),
            Err(e) => {
                maker.release_rebalance(&rgb_reservations, None).await;
                return RebalanceTick::Skipped(format!("btc reserve failed: {e}"));
            }
        },
        None => (None, None),
    };

    // 6. Build the one combined split tx.
    let assets_arg: Vec<(AssetId, Outpoint, Vec<u64>)> = plan
        .assets
        .iter()
        .map(|a| (a.asset.clone(), a.source.clone(), a.rungs.clone()))
        .collect();
    let btc_arg = btc_source_op.clone().map(|op| {
        (
            op,
            plan.btc
                .as_ref()
                .map(|b| b.rungs.clone())
                .unwrap_or_default(),
        )
    });
    let (raw_tx, txid) = match deps
        .rgb_backend
        .split_pools(assets_arg, btc_arg, fee_sats)
        .await
    {
        Ok(x) => x,
        Err(e) => {
            maker
                .release_rebalance(&rgb_reservations, btc_reservation.as_ref())
                .await;
            return RebalanceTick::Skipped(format!("split build failed: {e}"));
        }
    };

    // 7. Broadcast, then mark sources pending so `sweep_confirmations` retires
    //    them on confirmation (and the chain observer ingests the new pieces).
    if let Err(e) = maker.broadcast_rebalance(&raw_tx).await {
        maker
            .release_rebalance(&rgb_reservations, btc_reservation.as_ref())
            .await;
        return RebalanceTick::Skipped(format!("broadcast failed: {e}"));
    }
    for r in &rgb_reservations {
        maker.mark_rgb_rebalance_pending(r, txid.clone()).await;
    }
    if let Some(r) = &btc_reservation {
        maker.mark_btc_rebalance_pending(r, txid.clone()).await;
    }

    RebalanceTick::Split { txid, rgb_sources }
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

    /// Multi-asset seeding: each contract's inventory reconciles independently.
    /// Seeding a SECOND asset into a store that already holds the first must NOT
    /// trip the merge path (which keys off per-asset emptiness, not the whole
    /// store), or the second contract would never seed.
    #[tokio::test]
    async fn reconcile_seeds_each_asset_independently() {
        use rfq_types::{AssetKind, BitcoinNetwork};
        let path = std::env::temp_dir().join(format!("maker-reconcile-{}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let store = SqliteInventoryStore::open(&path).await.unwrap();

        let asset = |id: &str| AssetId {
            network: BitcoinNetwork::Signet,
            kind: AssetKind::Rgb20,
            id: id.to_owned(),
        };
        let utxo = |a: AssetId, vout: u32, amount: u64| RgbInventoryUtxo {
            outpoint: Outpoint::new(format!("{:064x}", vout as u64), vout),
            asset_id: a,
            amount,
            btc_sats: 546,
        };

        reconcile_rgb_inventory(&store, &asset("rgb:a"), &[utxo(asset("rgb:a"), 0, 100)], 1)
            .await
            .unwrap();
        // Second asset into a NON-empty store — still seeds (per-asset gate).
        reconcile_rgb_inventory(&store, &asset("rgb:b"), &[utxo(asset("rgb:b"), 1, 200)], 1)
            .await
            .unwrap();

        assert_eq!(store.list_for_asset(&asset("rgb:a")).await.len(), 1);
        assert_eq!(store.list_for_asset(&asset("rgb:b")).await.len(), 1);
        let total: u64 = store.list_all().await.iter().map(|u| u.amount).sum();
        assert_eq!(total, 300, "both assets seeded, neither dropped");
        let _ = std::fs::remove_file(&path);
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
    fn rejects_out_of_range_ladder_ratio() {
        for bad in ["1.0", "1.5", "0.0", "-0.5"] {
            let raw = format!("[rebalance]\nladder_ratio = {bad}\n");
            let err = MakerNodeConfig::load_str(&raw)
                .expect_err(&format!("ratio {bad} must be rejected"));
            assert!(
                matches!(err, ConfigError::Invalid(_)),
                "expected Invalid for ratio {bad}, got {err:?}"
            );
        }
        // A valid ratio in (0, 1) is accepted.
        MakerNodeConfig::load_str("[rebalance]\nladder_ratio = 0.5\n")
            .expect("0.5 is a valid ratio");
    }

    #[test]
    fn loads_example_config() {
        let raw = include_str!("../config.toml.example");
        let cfg = MakerNodeConfig::load_str(raw).expect("example TOML parses");
        assert_eq!(cfg.maker.node_id, "node·7af2");
        let rgb = cfg.rgb.expect("example has [rgb] block");
        assert_eq!(rgb.network, "regtest");
        assert_eq!(rgb.wallet_name, "maker");
        // Tilde expansion fires at load time.
        assert!(
            !rgb.data_dir.to_string_lossy().starts_with('~'),
            "data_dir tilde should expand: {}",
            rgb.data_dir.display()
        );
        assert!(!rgb.signer.account_file.to_string_lossy().starts_with('~'));
    }

    async fn test_app() -> Router {
        let maker = build_maker(&test_config()).await.unwrap();
        // No standing order ⇒ the maker declines; seed a flat policy for the mock asset.
        maker.reload_price_policy(orders::flat_policy("rgb-test-asset", 101, 1_000_000_000));
        maker_app(maker)
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
            // BTC pool excludes EVERY traded contract's allocations in one call.
            match deps
                .rgb_backend
                .list_btc_only_utxos(&deps.assets, now)
                .await
            {
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
            // Refresh each contract's RGB inventory too — consecutive maker-side
            // swaps would otherwise stall after the first one. The maker's `/sign`
            // intentionally does *not* ingest the change UTXO; the chain observer
            // adds it here with `Available` status once `sync_wallet` sees the new
            // outpoint, mirroring `ingest_btc_change_utxos`.
            for asset in &deps.assets {
                match deps.rgb_backend.list_inventory_utxos(asset).await {
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
            }
            let spent = maker.sweep_confirmations().await;
            if spent > 0 {
                println!("chain_observer marked_spent={spent}");
            }
        }
    })
}
