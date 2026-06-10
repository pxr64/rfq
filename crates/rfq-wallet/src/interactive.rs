//! Interactive (`dialoguer`) wallet-setup prompts and the resolve-or-prompt flow.
//!
//! Kept separate from [`crate::defaults`] so a future non-interactive caller can
//! build a [`ResolvedWallet`] from defaults alone without touching this module.

use std::error::Error;

use dialoguer::{theme::ColorfulTheme, Input, Password, Select};

use crate::defaults::{default_account_file, default_data_dir, default_electrum_url, expand_tilde};
use crate::{ResolvedWallet, WalletInput};

/// Network picker (regtest / signet / testnet / mainnet).
pub fn prompt_network() -> Result<String, dialoguer::Error> {
    let options = ["regtest", "signet", "testnet", "mainnet"];
    let idx = Select::with_theme(&ColorfulTheme::default())
        .with_prompt("network")
        .items(&options)
        .default(0)
        .interact()?;
    Ok(options[idx].to_owned())
}

/// Resolve every wallet field: use the CLI-provided value when present, else
/// prompt with a name-derived default (mirrors `maker init`). `need_electrum`
/// and `need_signer` gate the optional prompts so offline / read-only commands
/// don't ask for things they won't use.
pub fn resolve_wallet(
    input: WalletInput,
    need_electrum: bool,
    need_signer: bool,
) -> Result<ResolvedWallet, Box<dyn Error>> {
    let theme = ColorfulTheme::default();

    let name = match input.name {
        Some(n) => n,
        None => Input::with_theme(&theme)
            .with_prompt("wallet name")
            .default("maker".to_owned())
            .interact_text()?,
    };

    let network = match input.network {
        Some(n) => n,
        None => prompt_network()?,
    };

    let data_dir = match input.data_dir {
        Some(d) => expand_tilde(&d.to_string_lossy()),
        None => {
            let s: String = Input::with_theme(&theme)
                .with_prompt("RGB data dir")
                .default(default_data_dir(&name))
                .interact_text()?;
            expand_tilde(&s)
        }
    };

    let electrum_url = if need_electrum {
        match input.electrum_url {
            Some(e) => e.trim().to_owned(),
            None => {
                let s: String = Input::with_theme(&theme)
                    .with_prompt("electrum URL")
                    .default(default_electrum_url(&network).to_owned())
                    .interact_text()?;
                s.trim().to_owned()
            }
        }
    } else {
        // Not needed for this command; carry a flag value through if one was given.
        input.electrum_url.map(|e| e.trim().to_owned()).unwrap_or_default()
    };

    let (account_file, password) = if need_signer {
        let account_file = match input.account_file {
            Some(p) => expand_tilde(&p.to_string_lossy()),
            None => {
                let def = default_account_file(&data_dir).to_string_lossy().into_owned();
                let s: String = Input::with_theme(&theme)
                    .with_prompt("signer account file")
                    .default(def)
                    .interact_text()?;
                expand_tilde(&s)
            }
        };
        let password = match input.password {
            Some(p) => p,
            None => Password::with_theme(&theme)
                .with_prompt("signer password (empty for none)")
                .allow_empty_password(true)
                .interact()?,
        };
        (account_file, password)
    } else {
        // Offline/read-only: derive a path but never prompt or require a signer.
        let account_file = input
            .account_file
            .map(|p| expand_tilde(&p.to_string_lossy()))
            .unwrap_or_else(|| default_account_file(&data_dir));
        (account_file, input.password.unwrap_or_default())
    };

    Ok(ResolvedWallet {
        name,
        network,
        data_dir,
        account_file,
        electrum_url,
        password,
    })
}
