//! Per-wallet config, keyed by wallet name. Written once at `wallet create` /
//! `maker init` / `taker init`; every other command resolves by `--name` and
//! reads it — so the routine commands (balance / sync / issue / transfer / …)
//! take just a name, no prompts and no repeated flags.

use std::error::Error;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::defaults::expand_tilde;
use crate::ResolvedWallet;

/// The persisted, role-agnostic wallet parameters. Lives at
/// `~/.config/colorex/wallets/<wallet_name>.toml` (alongside `maker.toml`).
/// Paths are stored expanded (absolute); `contract_id` is optional.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WalletConfig {
    pub network: String,
    pub data_dir: String,
    pub wallet_name: String,
    pub electrum_url: String,
    pub account_file: String,
    #[serde(default)]
    pub password: String,
    #[serde(default)]
    pub contract_id: String,
}

impl WalletConfig {
    /// `~/.config/colorex/wallets/<name>.toml` (forces `$HOME/.config`, matching
    /// `MakerNodeConfig::default_path`, not macOS Application Support).
    pub fn path_for(name: &str) -> PathBuf {
        expand_tilde(&format!("~/.config/colorex/wallets/{name}.toml"))
    }

    pub fn exists(name: &str) -> bool {
        Self::path_for(name).exists()
    }

    pub fn load(name: &str) -> Result<Self, Box<dyn Error>> {
        let path = Self::path_for(name);
        let text = std::fs::read_to_string(&path).map_err(|e| {
            format!(
                "no wallet config for '{name}' ({}): {e}. Run: colorex wallet create --name {name}",
                path.display()
            )
        })?;
        Ok(toml::from_str(&text).map_err(|e| format!("parse {}: {e}", path.display()))?)
    }

    /// Build from a resolved wallet (e.g. just after `create_wallet`), carrying an
    /// optional contract id.
    pub fn from_resolved(r: &ResolvedWallet, contract_id: &str) -> Self {
        WalletConfig {
            network: r.network.clone(),
            data_dir: r.data_dir.to_string_lossy().into_owned(),
            wallet_name: r.name.clone(),
            electrum_url: r.electrum_url.clone(),
            account_file: r.account_file.to_string_lossy().into_owned(),
            password: r.password.clone(),
            contract_id: contract_id.to_owned(),
        }
    }

    pub fn to_resolved(&self) -> ResolvedWallet {
        ResolvedWallet {
            name: self.wallet_name.clone(),
            network: self.network.clone(),
            data_dir: expand_tilde(&self.data_dir),
            account_file: expand_tilde(&self.account_file),
            electrum_url: self.electrum_url.clone(),
            password: self.password.clone(),
        }
    }

    /// Atomic write with `0o600` perms (mirrors `maker init`'s `write_config`).
    pub fn save(&self) -> Result<(), Box<dyn Error>> {
        let path = Self::path_for(&self.wallet_name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let body = toml::to_string_pretty(self)?;
        let tmp = path.with_extension("toml.tmp");
        std::fs::write(&tmp, body)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&tmp)?.permissions();
            perms.set_mode(0o600);
            std::fs::set_permissions(&tmp, perms)?;
        }
        std::fs::rename(&tmp, &path)?;
        Ok(())
    }

    pub fn saved_path(&self) -> PathBuf {
        Self::path_for(&self.wallet_name)
    }
}

/// Resolve a wallet by name from its saved config, applying any CLI flags as
/// overrides. No prompts — the interactive step is `wallet create` / `init`.
/// `--name` is required (which wallet?).
pub fn resolve_named(input: crate::WalletInput) -> Result<ResolvedWallet, Box<dyn Error>> {
    let name = input
        .name
        .ok_or("--name is required (which wallet? e.g. --name issuer)")?;
    let mut cfg = WalletConfig::load(&name)?;
    if let Some(n) = input.network {
        cfg.network = n;
    }
    if let Some(d) = input.data_dir {
        cfg.data_dir = d.to_string_lossy().into_owned();
    }
    if let Some(e) = input.electrum_url {
        cfg.electrum_url = e.trim().to_owned();
    }
    if let Some(a) = input.account_file {
        cfg.account_file = a.to_string_lossy().into_owned();
    }
    if let Some(p) = input.password {
        cfg.password = p;
    }
    Ok(cfg.to_resolved())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_through_resolved() {
        let cfg = WalletConfig {
            network: "signet".into(),
            data_dir: "/home/x/.local/share/colorex/issuer".into(),
            wallet_name: "issuer".into(),
            electrum_url: "tcp://127.0.0.1:60601".into(),
            account_file: "/home/x/.local/share/colorex/issuer/account.key".into(),
            password: String::new(),
            contract_id: "rgb:abc".into(),
        };
        let r = cfg.to_resolved();
        assert_eq!(r.name, "issuer");
        assert_eq!(r.electrum_url, "tcp://127.0.0.1:60601");
        let back = WalletConfig::from_resolved(&r, &cfg.contract_id);
        assert_eq!(back.wallet_name, "issuer");
        assert_eq!(back.network, "signet");
    }

    #[test]
    fn path_is_name_keyed_under_config() {
        let p = WalletConfig::path_for("maker");
        assert!(p.ends_with(".config/colorex/wallets/maker.toml"));
    }
}
