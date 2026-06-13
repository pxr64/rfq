//! CLI surface: the `clap` command tree (parsed by `main`). Pure data
//! definitions — the handlers live in `main.rs` and the daemon/command modules.

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};
use maker_node::init;
use rfq_wallet::WalletInput;

#[derive(Debug, Parser)]
#[command(name = "colorex", version, about = "RGB RFQ tooling")]
pub(crate) struct Cli {
    #[command(subcommand)]
    pub(crate) command: TopCommand,
    /// Path to the maker config TOML. Defaults to `~/.config/colorex/maker.toml`.
    #[arg(long, global = true)]
    pub(crate) config: Option<PathBuf>,
}

#[derive(Debug, Subcommand, Clone, PartialEq, Eq)]
pub(crate) enum TopCommand {
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
pub(crate) enum IssuerCmd {
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
pub(crate) enum WalletCmd {
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
pub(crate) struct WalletCommon {
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
    pub(crate) fn into_input(self) -> WalletInput {
        WalletInput {
            name: self.name,
            network: self.network,
            data_dir: self.data_dir,
            ..Default::default()
        }
    }
}

#[derive(Debug, Subcommand, Clone, PartialEq, Eq)]
pub(crate) enum MakerCmd {
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
pub(crate) enum OrderCmd {
    /// Create (or replace) the standing order for an (asset, side).
    Create {
        /// Quote side, from the TAKER's view: `buy` = taker buys RGB (you sell
        /// RGB); `sell` = taker sells RGB (you buy, paying BTC).
        #[arg(long, value_parser = ["buy", "sell"])]
        side: String,
        /// RGB contract id. Defaults to the sole registered contract.
        #[arg(long)]
        asset: Option<String>,
        /// Price in SATS per smallest RGB unit.
        #[arg(long)]
        price: u64,
        /// Max single-quote size, in smallest RGB units, this order backs.
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
pub(crate) enum ContractCmd {
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
pub(crate) enum MakerWalletCmd {
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
    /// Rebalance UTXOs into a denomination ladder: split each asset + the BTC
    /// pool into large→small pieces so coin selection stays cheap. Builds ONE tx
    /// for all of it. Run with the daemon stopped (or rely on the daemon's own
    /// executor); `--dry-run` first to see the plan.
    Rebalance {
        /// RGB contract id to rebalance. Omit to rebalance every registered
        /// contract; ignored with `--btc-only`.
        #[arg(long)]
        asset: Option<String>,
        /// Rebalance only the BTC pool (skip every RGB asset).
        #[arg(long)]
        btc_only: bool,
        /// Override the tx fee (sats). Default: next-block feerate × tx vsize,
        /// capped at `[rebalance] rebalance_max_fee_sats`.
        #[arg(long)]
        fee: Option<u64>,
        /// Print the plan without building/broadcasting anything.
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
