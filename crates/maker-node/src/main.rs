use clap::Parser;
use maker_node::{init, MakerNodeConfig};

mod cli;
use cli::*;

mod commands;
use commands::*;

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
            MakerCmd::Up => run(load_config(&config_path)?, &config_path).await,
            MakerCmd::Health => health(load_config(&config_path)?).await,
            MakerCmd::Inventory { btc, electrum } => {
                inventory(load_config(&config_path)?, btc, electrum).await
            }
            MakerCmd::Wallet { cmd } => match cmd {
                MakerWalletCmd::Addresses => maker_addresses(load_config(&config_path)?),
                MakerWalletCmd::Balances { electrum } => {
                    maker_balances(load_config(&config_path)?, electrum).await
                }
                MakerWalletCmd::Rescan { electrum } => {
                    maker_rescan(load_config(&config_path)?, electrum).await
                }
                MakerWalletCmd::Recover {
                    contract,
                    electrum,
                    fee,
                    dry_run,
                } => {
                    maker_recover(load_config(&config_path)?, contract, electrum, fee, dry_run)
                        .await
                }
                MakerWalletCmd::Rebalance {
                    asset,
                    btc_only,
                    fee,
                    dry_run,
                } => {
                    maker_rebalance(load_config(&config_path)?, asset, btc_only, fee, dry_run).await
                }
                MakerWalletCmd::Invoice { contract, amount } => {
                    maker_invoice(load_config(&config_path)?, contract, amount).await
                }
                MakerWalletCmd::Accept {
                    consignment,
                    contract,
                } => maker_accept(load_config(&config_path)?, consignment, contract).await,
                MakerWalletCmd::Transfer {
                    invoice,
                    electrum,
                    fee,
                    out,
                } => maker_transfer(load_config(&config_path)?, invoice, electrum, fee, out).await,
            },
            MakerCmd::Order { cmd } => match cmd {
                OrderCmd::Create {
                    side,
                    asset,
                    price,
                    size,
                    mirror,
                    mirror_spread_bps,
                } => {
                    order_create(
                        &config_path,
                        side,
                        asset,
                        price,
                        size,
                        mirror,
                        mirror_spread_bps,
                    )
                    .await
                }
                OrderCmd::List => order_list(&config_path).await,
                OrderCmd::Cancel { id } => order_cancel(&config_path, &id).await,
                OrderCmd::Clear => order_clear(&config_path).await,
            },
            MakerCmd::Contract { cmd } => match cmd {
                ContractCmd::Import { id, consignment } => {
                    contract_import(load_config(&config_path)?, id, consignment).await
                }
                ContractCmd::List => contract_list(load_config(&config_path)?).await,
                ContractCmd::Remove { id } => contract_remove(load_config(&config_path)?, id).await,
            },
            MakerCmd::Reconsign {
                contract,
                outpoint,
                out,
            } => maker_reconsign(load_config(&config_path)?, contract, outpoint, out).await,
            MakerCmd::Consignment { quote_id, out } => {
                maker_get_consignment(load_config(&config_path)?, quote_id, out).await
            }
        },
        TopCommand::Wallet { cmd } => match cmd {
            WalletCmd::Create {
                common,
                account_file,
                password,
            } => wallet_create(common, account_file, password),
            WalletCmd::Address { common, btc } => wallet_address(common, btc),
            WalletCmd::Sync { common, electrum } => wallet_sync(common, electrum).await,
            WalletCmd::Balance { common, electrum } => wallet_balance(common, electrum).await,
            WalletCmd::Invoice {
                common,
                contract,
                amount,
            } => wallet_invoice(common, contract, amount).await,
        },
        TopCommand::Issuer { cmd } => match cmd {
            IssuerCmd::Issue {
                common,
                ticker,
                asset_name,
                precision,
                supply,
                details,
                seal,
                issuer,
            } => issuer_issue(
                common, ticker, asset_name, precision, supply, details, seal, issuer,
            ),
            IssuerCmd::Contracts { common } => issuer_contracts(common),
            IssuerCmd::Transfer {
                common,
                invoice,
                electrum,
                account_file,
                password,
                fee,
            } => issuer_transfer(common, invoice, electrum, account_file, password, fee).await,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;
    use std::path::PathBuf;

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
                cmd: MakerCmd::Inventory {
                    btc: false,
                    electrum: None,
                }
            },
        );
        assert_eq!(
            Cli::parse_from(["colorex", "maker", "init", "--force"]).command,
            TopCommand::Maker {
                cmd: MakerCmd::Init(init::InitArgs {
                    force: true,
                    systemd: false
                }),
            },
        );
    }

    #[test]
    fn cli_accepts_global_config_flag() {
        let cli = Cli::parse_from(["colorex", "--config", "/tmp/x.toml", "maker", "up"]);
        assert_eq!(cli.config, Some(PathBuf::from("/tmp/x.toml")));
    }

    #[test]
    fn cli_parses_contract_subcommands() {
        assert_eq!(
            Cli::parse_from(["colorex", "maker", "contract", "import", "rgb:abc"]).command,
            TopCommand::Maker {
                cmd: MakerCmd::Contract {
                    cmd: ContractCmd::Import {
                        id: "rgb:abc".into(),
                        consignment: None,
                    }
                }
            },
        );
        assert_eq!(
            Cli::parse_from(["colorex", "maker", "contract", "list"]).command,
            TopCommand::Maker {
                cmd: MakerCmd::Contract {
                    cmd: ContractCmd::List
                }
            },
        );
        assert_eq!(
            Cli::parse_from(["colorex", "maker", "contract", "remove", "rgb:abc"]).command,
            TopCommand::Maker {
                cmd: MakerCmd::Contract {
                    cmd: ContractCmd::Remove {
                        id: "rgb:abc".into()
                    }
                }
            },
        );
    }

    #[test]
    fn cli_parses_maker_wallet_group() {
        // Funding/wallet ops now live under `maker wallet`.
        assert_eq!(
            Cli::parse_from(["colorex", "maker", "wallet", "addresses"]).command,
            TopCommand::Maker {
                cmd: MakerCmd::Wallet {
                    cmd: MakerWalletCmd::Addresses
                }
            },
        );
        assert_eq!(
            Cli::parse_from(["colorex", "maker", "wallet", "invoice", "--amount", "100"]).command,
            TopCommand::Maker {
                cmd: MakerCmd::Wallet {
                    cmd: MakerWalletCmd::Invoice {
                        contract: None,
                        amount: 100,
                    }
                }
            },
        );
        assert_eq!(
            Cli::parse_from([
                "colorex",
                "maker",
                "wallet",
                "transfer",
                "--invoice",
                "rgb:inv",
                "--fee",
                "500",
            ])
            .command,
            TopCommand::Maker {
                cmd: MakerCmd::Wallet {
                    cmd: MakerWalletCmd::Transfer {
                        invoice: "rgb:inv".into(),
                        electrum: None,
                        fee: 500,
                        out: None,
                    }
                }
            },
        );
        // The old flat paths are gone — `maker addresses` must NOT parse.
        assert!(Cli::try_parse_from(["colorex", "maker", "addresses"]).is_err());
    }

    #[test]
    fn pick_contract_resolution() {
        // Explicit id always wins.
        assert_eq!(
            pick_contract(Some("rgb:x".into()), vec!["rgb:a".into()]).unwrap(),
            "rgb:x"
        );
        // Empty explicit falls through to the registry.
        assert_eq!(
            pick_contract(Some(String::new()), vec!["rgb:a".into()]).unwrap(),
            "rgb:a"
        );
        // Sole registered contract is the unambiguous default.
        assert_eq!(pick_contract(None, vec!["rgb:a".into()]).unwrap(), "rgb:a");
        // Zero and many are errors (must import / must disambiguate).
        assert!(pick_contract(None, vec![]).is_err());
        assert!(pick_contract(None, vec!["rgb:a".into(), "rgb:b".into()]).is_err());
    }

    #[test]
    fn cli_definition_is_valid() {
        Cli::command().debug_assert();
    }
}
