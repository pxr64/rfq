//! Role-agnostic wallet setup shared by `colorex` (maker / issuer commands) and
//! `colorex-taker`: per-network defaults, interactive prompts, and the single
//! `LibRgbBackend` construction site — so wallet/issuer/taker setup only needs a
//! wallet name, with everything else derived or prompted (`maker init` style).
//!
//! Depends only on `rfq-rgb`; neither binary depends on the other, so this crate
//! is the natural shared home (a module inside `maker-node` would force
//! `taker-cli` to depend on the whole maker daemon).

pub mod defaults;
pub mod interactive;

use std::path::PathBuf;

use rfq_rgb::{LibRgbBackend, RgbError, WalletUtxo};

pub use defaults::{default_account_file, default_data_dir, default_electrum_url, expand_tilde};
pub use interactive::{prompt_network, resolve_wallet};

/// Human-readable BTC balance for a wallet's UTXOs: a total line plus one line
/// per UTXO, labelled by keychain (0 = BTC, 10 = RGB anchor). Shared by
/// `colorex wallet balance` and `colorex-taker balance`.
pub fn render_balance(utxos: &[WalletUtxo]) -> String {
    let total: u64 = utxos.iter().map(|u| u.sats).sum();
    let mut out = format!(
        "balance: {total} sats ({} utxo{})\n",
        utxos.len(),
        if utxos.len() == 1 { "" } else { "s" }
    );
    for u in utxos {
        let label = match u.keychain {
            0 => "BTC",
            10 => "RGB-anchor",
            _ => "other",
        };
        out.push_str(&format!(
            "  {}:{}  {} sats  [keychain {} · {label}]\n",
            u.txid, u.vout, u.sats, u.keychain
        ));
    }
    out
}

/// CLI-provided wallet values; any `None` is filled by [`resolve_wallet`]
/// (prompt with a name-derived default).
#[derive(Debug, Default, Clone)]
pub struct WalletInput {
    pub name: Option<String>,
    pub network: Option<String>,
    pub data_dir: Option<PathBuf>,
    pub account_file: Option<PathBuf>,
    pub electrum_url: Option<String>,
    pub password: Option<String>,
}

/// Fully-resolved, tilde-expanded wallet parameters plus a backend factory. The
/// one place the 6-arg [`LibRgbBackend::new`] call lives now.
#[derive(Debug, Clone)]
pub struct ResolvedWallet {
    pub name: String,
    pub network: String,
    pub data_dir: PathBuf,
    pub account_file: PathBuf,
    pub electrum_url: String,
    pub password: String,
}

impl ResolvedWallet {
    /// Build the role-agnostic RGB backend.
    pub fn backend(&self) -> LibRgbBackend {
        LibRgbBackend::new(
            self.data_dir.clone(),
            self.name.clone(),
            self.network.clone(),
            self.electrum_url.clone(),
            self.account_file.clone(),
            self.password.clone(),
        )
    }

    /// Create the taproot wallet + signing account if absent, returning the
    /// keychain-10 funding address. `Ok(None)` if a wallet already existed
    /// (kept as-is).
    pub fn create_wallet(&self) -> Result<Option<String>, RgbError> {
        let backend = self.backend();
        if backend.wallet_exists() {
            return Ok(None);
        }
        backend.create_wallet()?;
        Ok(Some(backend.funding_address(true)?))
    }
}
