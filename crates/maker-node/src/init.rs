//! `colorex maker init` — interactive bootstrap.
//!
//! Sequence:
//! 1. Resolve target paths from `--config` (or default `~/.config/colorex/maker.toml`).
//! 2. If the config already exists and `--force` isn't set, prompt to overwrite.
//! 3. Generate a fresh secp256k1 node-identity keypair (in memory).
//! 4. Prompt for network, broker URL, listen addr, and optional RGB backend params.
//! 5. Create the RGB taproot wallet + signing account if absent (kept if present),
//!    and print the keychain-10 funding address.
//! 6. Probe the RGB stash + broker /health (degrades to `[warn]` + continue).
//! 7. Atomically write `maker.toml`, then the keypair files.
//! 8. Print the rendered config.

use std::path::Path;

use clap::Args;
use colorex_wallet::{
    default_account_file, default_data_dir, default_electrum_url, expand_tilde, prompt_network,
    ResolvedWallet,
};
use dialoguer::{theme::ColorfulTheme, Confirm, Input, Password};
use rfq_client::{RfqClient, Url};
use rfq_rgb::RgbBackend;
use rfq_types::{AssetId, AssetKind, BitcoinNetwork};

use crate::{node_key::NodeKey, output};

#[derive(Debug, Args, Clone, PartialEq, Eq)]
pub struct InitArgs {
    /// Overwrite an existing config without prompting.
    #[arg(long)]
    pub force: bool,
}

pub async fn run(args: InitArgs, config_path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let config_dir = config_path.parent().unwrap_or_else(|| Path::new("."));
    let node_key_path = config_dir.join("node.key");
    let node_pub_path = config_dir.join("node.pub");

    if config_path.exists() && !args.force {
        let overwrite = Confirm::with_theme(&ColorfulTheme::default())
            .with_prompt(format!(
                "config {} already exists. Overwrite?",
                config_path.display()
            ))
            .default(false)
            .interact()?;
        if !overwrite {
            return Err("init aborted: existing config kept".into());
        }
    }

    output::step("generating keypair");
    let key = NodeKey::generate();
    output::step_ok();

    println!();
    let node_id = key.node_id();

    let network = prompt_network()?;
    let broker_url: String = Input::with_theme(&ColorfulTheme::default())
        .with_prompt("broker URL")
        .default("http://127.0.0.1:3000".into())
        .interact_text()?;
    let listen_addr: String = Input::with_theme(&ColorfulTheme::default())
        .with_prompt("listen addr")
        .default("127.0.0.1:4000".into())
        .interact_text()?;

    let setup_rgb = Confirm::with_theme(&ColorfulTheme::default())
        .with_prompt("Configure RGB backend?")
        .default(true)
        .interact()?;

    let rgb = if setup_rgb {
        let electrum_default = default_electrum_url(&network);
        let electrum_url: String = Input::with_theme(&ColorfulTheme::default())
            .with_prompt("electrum URL")
            .default(electrum_default.into())
            .interact_text()?;
        let data_dir: String = Input::with_theme(&ColorfulTheme::default())
            .with_prompt("RGB data dir")
            .default(default_data_dir("maker"))
            .interact_text()?;
        let wallet_name: String = Input::with_theme(&ColorfulTheme::default())
            .with_prompt("RGB wallet name")
            .default("maker".into())
            .interact_text()?;
        let contract_id: String = Input::with_theme(&ColorfulTheme::default())
            .with_prompt("RGB contract id (leave empty to set later)")
            .allow_empty(true)
            .interact_text()?;
        let account_file: String = Input::with_theme(&ColorfulTheme::default())
            .with_prompt("signer account file")
            .default(
                default_account_file(Path::new(&default_data_dir("maker")))
                    .to_string_lossy()
                    .into_owned(),
            )
            .interact_text()?;
        let password = Password::with_theme(&ColorfulTheme::default())
            .with_prompt("signer password (empty for regtest)")
            .allow_empty_password(true)
            .interact()?;
        Some(RgbAnswers {
            network: network.clone(),
            electrum_url,
            data_dir,
            wallet_name,
            contract_id,
            account_file,
            password,
        })
    } else {
        None
    };

    println!();

    if let Some(r) = &rgb {
        output::step("creating rgb wallet");
        match resolved_from(r).create_wallet() {
            Ok(Some(addr)) => {
                output::step_ok();
                println!("  fund this tapret (keychain-10) address, then run `colorex wallet sync`:");
                println!("    {addr}");
            }
            Ok(None) => output::step_skip(), // wallet already exists — kept as-is
            Err(e) => {
                output::step_warn(&truncate_error(&e.to_string()));
                let cont = Confirm::with_theme(&ColorfulTheme::default())
                    .with_prompt("Continue anyway?")
                    .default(true)
                    .interact()?;
                if !cont {
                    return Err("init aborted: rgb wallet creation failed".into());
                }
            }
        }
    }

    output::step("probing rgb20 inventory");
    match &rgb {
        Some(r) if !r.contract_id.is_empty() => match probe_rgb_inventory(r).await {
            Ok(n) => println!(
                "[ok] (found {n} UTXO{}, contract {})",
                if n == 1 { "" } else { "s" },
                truncate_contract(&r.contract_id),
            ),
            Err(e) => {
                output::step_warn(&truncate_error(&e.to_string()));
                let cont = Confirm::with_theme(&ColorfulTheme::default())
                    .with_prompt("Continue anyway?")
                    .default(true)
                    .interact()?;
                if !cont {
                    return Err("init aborted: RGB probe failed".into());
                }
            }
        },
        // RGB configured but no contract id yet, or no RGB at all.
        _ => output::step_skip(),
    }

    output::step("subscribing to broker");
    match probe_broker(&broker_url).await {
        Ok(status) => println!("[ok] (status={status})"),
        Err(e) => {
            output::step_warn(&truncate_error(&e.to_string()));
            let cont = Confirm::with_theme(&ColorfulTheme::default())
                .with_prompt("Continue anyway?")
                .default(true)
                .interact()?;
            if !cont {
                return Err("init aborted: broker probe failed".into());
            }
        }
    }

    println!();

    let toml = render_toml(&RenderInput {
        node_id: &node_id,
        listen_addr: &listen_addr,
        broker_url: &broker_url,
        rgb: rgb.as_ref(),
    });
    write_config(config_path, &toml)?;
    key.persist(&node_key_path, &node_pub_path)?;

    println!("{toml}");
    println!(
        "Wrote {} + {} + {}",
        config_path.display(),
        node_key_path.display(),
        node_pub_path.display(),
    );

    Ok(())
}

struct RgbAnswers {
    network: String,
    electrum_url: String,
    data_dir: String,
    wallet_name: String,
    contract_id: String,
    account_file: String,
    password: String,
}

struct RenderInput<'a> {
    node_id: &'a str,
    listen_addr: &'a str,
    broker_url: &'a str,
    rgb: Option<&'a RgbAnswers>,
}

/// Build a [`ResolvedWallet`] from the maker's RGB answers (tilde-expanded), so
/// wallet creation + inventory probing share the one construction path used by
/// the `wallet` / `issuer` / taker commands. `maker init`'s extra prompts
/// (broker, listen, contract id) stay maker-specific above.
fn resolved_from(r: &RgbAnswers) -> ResolvedWallet {
    ResolvedWallet {
        name: r.wallet_name.clone(),
        network: r.network.clone(),
        data_dir: expand_tilde(&r.data_dir),
        account_file: expand_tilde(&r.account_file),
        electrum_url: r.electrum_url.clone(),
        password: r.password.clone(),
    }
}

async fn probe_rgb_inventory(r: &RgbAnswers) -> Result<usize, Box<dyn std::error::Error>> {
    let backend = resolved_from(r).backend();
    let asset = AssetId {
        network: parse_network(&r.network).ok_or("unknown network")?,
        kind: AssetKind::Rgb20,
        id: r.contract_id.clone(),
    };
    let utxos = backend.list_inventory_utxos(&asset).await?;
    Ok(utxos.len())
}

async fn probe_broker(broker_url: &str) -> Result<String, Box<dyn std::error::Error>> {
    let url = Url::parse(broker_url)?;
    let client = RfqClient::new(url);
    let response = client.health().await?;
    Ok(response.status)
}

fn parse_network(s: &str) -> Option<BitcoinNetwork> {
    match s {
        "regtest" => Some(BitcoinNetwork::Regtest),
        "signet" => Some(BitcoinNetwork::Signet),
        "testnet" => Some(BitcoinNetwork::Testnet),
        "mainnet" => Some(BitcoinNetwork::Mainnet),
        _ => None,
    }
}

fn truncate_contract(id: &str) -> String {
    if id.len() <= 12 {
        id.to_owned()
    } else {
        format!("{}…", &id[..12])
    }
}

fn truncate_error(msg: &str) -> String {
    const MAX: usize = 80;
    if msg.len() <= MAX {
        msg.to_owned()
    } else {
        format!("{}…", &msg[..MAX])
    }
}

fn render_toml(r: &RenderInput<'_>) -> String {
    let mut out = String::new();
    out.push_str("[maker]\n");
    out.push_str(&format!("node_id     = \"{}\"\n", r.node_id));
    out.push_str(&format!("listen_addr = \"{}\"\n", r.listen_addr));
    out.push_str(&format!("broker_url  = \"{}\"\n", r.broker_url));
    if let Some(rgb) = r.rgb {
        out.push('\n');
        out.push_str("[rgb]\n");
        out.push_str(&format!("network      = \"{}\"\n", rgb.network));
        out.push_str(&format!("data_dir     = \"{}\"\n", rgb.data_dir));
        out.push_str(&format!("wallet_name  = \"{}\"\n", rgb.wallet_name));
        out.push_str(&format!("electrum_url = \"{}\"\n", rgb.electrum_url));
        out.push_str(&format!("contract_id  = \"{}\"\n", rgb.contract_id));
        out.push('\n');
        out.push_str("[rgb.signer]\n");
        out.push_str(&format!("account_file = \"{}\"\n", rgb.account_file));
        out.push_str(&format!("password     = \"{}\"\n", rgb.password));
    }
    out
}

fn write_config(path: &Path, contents: &str) -> Result<(), std::io::Error> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, contents)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&tmp)?.permissions();
        perms.set_mode(0o600);
        std::fs::set_permissions(&tmp, perms)?;
    }
    std::fs::rename(&tmp, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_toml_mock_section_omits_rgb_block() {
        let toml = render_toml(&RenderInput {
            node_id: "node·dead",
            listen_addr: "127.0.0.1:4000",
            broker_url: "http://127.0.0.1:3000",
            rgb: None,
        });
        assert!(toml.contains("[maker]"));
        assert!(toml.contains("node_id     = \"node·dead\""));
        assert!(!toml.contains("[rgb]"));
    }

    #[test]
    fn render_toml_with_rgb_block_round_trips_via_load_str() {
        let rgb = RgbAnswers {
            network: "regtest".into(),
            electrum_url: "localhost:60001".into(),
            data_dir: "/tmp/data".into(),
            wallet_name: "maker".into(),
            contract_id: "rgb:abcd-1234".into(),
            account_file: "/tmp/maker.account".into(),
            password: "".into(),
        };
        let toml = render_toml(&RenderInput {
            node_id: "node·beef",
            listen_addr: "127.0.0.1:4000",
            broker_url: "http://127.0.0.1:3000",
            rgb: Some(&rgb),
        });
        let cfg = crate::MakerNodeConfig::load_str(&toml).expect("rendered TOML parses");
        assert_eq!(cfg.maker.node_id, "node·beef");
        let rgb_cfg = cfg.rgb.expect("rgb block parsed");
        assert_eq!(rgb_cfg.contract_id, "rgb:abcd-1234");
        assert_eq!(rgb_cfg.signer.password, "");
    }

    #[test]
    fn truncate_contract_keeps_short_ids_intact() {
        assert_eq!(truncate_contract("rgb:abcd"), "rgb:abcd");
        assert_eq!(truncate_contract("rgb:abcdef0123456"), "rgb:abcdef01…");
    }
}
