//! Command handlers + shared helpers for the `colorex` CLI. `main`/`run_cli`
//! parse the `cli` tree and dispatch into these.

use std::path::{Path, PathBuf};

use crate::cli::*;
use maker_node::{
    accept_inventory_consignment, broker_client, build_maker, build_runtime,
    create_inventory_invoice, fetch_consignment, init, maker_app, now_ms, orders, output,
    reconsign_consignment, spawn_chain_observer_loop, spawn_cleanup_loop, spawn_order_reload_loop,
    spawn_rebalance_executor_loop, spawn_rebalance_loop, spawn_strategy_loop, MakerNodeConfig,
};
use rfq_client::{RfqClient, Url};
use rfq_rgb::RgbBackend;
use rfq_store::{BtcInventoryStore as _, ContractStore as _, InventoryStore as _, OrderStore as _};
use rfq_types::{InventorySnapshot, MakerId, Side};
use rfq_wallet::{resolve_named, resolve_wallet, WalletConfig, WalletInput};
use tokio::{net::TcpListener, sync::oneshot};

pub(crate) fn load_config(path: &Path) -> Result<MakerNodeConfig, String> {
    MakerNodeConfig::load(path).map_err(|e| format!("config {}: {e}", path.display()))
}

pub(crate) fn wallet_create(
    common: WalletCommon,
    account_file: Option<PathBuf>,
    password: Option<String>,
) -> Result<(), Box<dyn std::error::Error>> {
    let input = WalletInput {
        account_file,
        password,
        ..common.into_input()
    };
    let resolved = resolve_wallet(input, false, true)?;
    match resolved.create_wallet()? {
        Some(addr) => {
            println!("created wallet '{}'", resolved.name);
            println!(
                "fund this tapret (keychain-10) address from a faucet, then run `wallet sync`:\n  {addr}"
            );
        }
        None => println!("wallet '{}' already exists — kept as-is", resolved.name),
    }
    // Persist the per-wallet config so future commands resolve by `--name` alone.
    let cfg = WalletConfig::from_resolved(&resolved, "");
    cfg.save()?;
    println!("wrote {}", cfg.saved_path().display());
    Ok(())
}

pub(crate) fn wallet_address(
    common: WalletCommon,
    btc: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let resolved = resolve_named(common.into_input())?;
    println!("{}", resolved.backend().funding_address(!btc)?);
    Ok(())
}

pub(crate) async fn wallet_invoice(
    common: WalletCommon,
    contract: String,
    amount: u64,
) -> Result<(), Box<dyn std::error::Error>> {
    let resolved = resolve_named(common.into_input())?;
    let asset = rfq_types::AssetId {
        network: resolved.network.parse()?,
        kind: rfq_types::AssetKind::Rgb20,
        id: contract,
    };
    let invoice = resolved.backend().create_invoice(&asset, amount).await?;
    println!("{invoice}");
    Ok(())
}

pub(crate) async fn wallet_sync(
    common: WalletCommon,
    electrum: Option<String>,
) -> Result<(), Box<dyn std::error::Error>> {
    let input = WalletInput {
        electrum_url: electrum,
        ..common.into_input()
    };
    let resolved = resolve_named(input)?;
    resolved.backend().sync_wallet().await?;
    println!("synced '{}'", resolved.name);
    Ok(())
}

pub(crate) async fn wallet_balance(
    common: WalletCommon,
    electrum: Option<String>,
) -> Result<(), Box<dyn std::error::Error>> {
    let input = WalletInput {
        electrum_url: electrum,
        ..common.into_input()
    };
    let resolved = resolve_named(input)?;
    let utxos = resolved.backend().wallet_balance().await?;
    print!("{}", rfq_wallet::render_balance(&utxos));
    Ok(())
}

pub(crate) async fn maker_invoice(
    config: MakerNodeConfig,
    contract: Option<String>,
    amount: u64,
) -> Result<(), Box<dyn std::error::Error>> {
    let contract_id = resolve_contract_id(&config, contract).await?;
    let invoice = create_inventory_invoice(&config, contract_id, amount).await?;
    println!("{invoice}");
    Ok(())
}

/// The two wallet addresses an operator funds: keychain-0 (BTC payment + fees) and
/// keychain-10 (tapret RGB seal anchors). Derivation is xpub-only — no electrum or
/// signer needed. `Err` if there's no `[rgb]` config.
pub(crate) fn maker_funding_addresses(
    config: &MakerNodeConfig,
) -> Result<(String, String), Box<dyn std::error::Error>> {
    let rgb = config
        .rgb
        .as_ref()
        .ok_or("no [rgb] config: a wallet is required to derive addresses")?;
    let backend = rfq_rgb::LibRgbBackend::new(
        rgb.data_dir.clone(),
        rgb.wallet_name.clone(),
        rgb.network.clone(),
        String::new(),
        std::path::PathBuf::new(),
        String::new(),
    );
    Ok((
        backend.funding_address(false)?,
        backend.funding_address(true)?,
    ))
}

/// Per-keychain BTC totals (k0, k10) — syncs the wallet against `electrum`.
pub(crate) async fn maker_keychain_balances(
    config: &MakerNodeConfig,
    electrum: &str,
) -> Result<(u64, u64), Box<dyn std::error::Error>> {
    let rgb = config.rgb.as_ref().ok_or("no [rgb] config")?;
    let backend = rfq_rgb::LibRgbBackend::new(
        rgb.data_dir.clone(),
        rgb.wallet_name.clone(),
        rgb.network.clone(),
        electrum.to_owned(),
        std::path::PathBuf::new(),
        String::new(),
    );
    let (mut k0, mut k10) = (0u64, 0u64);
    for u in backend.wallet_balance().await? {
        match u.keychain {
            0 => k0 += u.sats,
            10 => k10 += u.sats,
            _ => {}
        }
    }
    Ok((k0, k10))
}

pub(crate) fn maker_addresses(config: MakerNodeConfig) -> Result<(), Box<dyn std::error::Error>> {
    let (btc, anchor) = maker_funding_addresses(&config)?;
    let network = config
        .rgb
        .as_ref()
        .map(|r| r.network.as_str())
        .unwrap_or("");

    println!("colorex maker wallet addresses ({network})");
    output::kv("BTC payment · keychain 0", &btc);
    output::kv("RGB anchor · keychain 10", &anchor);
    output::note("");
    output::note(
        "Fund BTC to pay sell-side takers + tx fees; fund RGB-anchor to mint/receive RGB.",
    );
    output::note("Show funded balances: colorex maker wallet balances");
    Ok(())
}

/// Per-keychain BTC balances (keychain 0 = payment, 10 = RGB anchor), synced
/// against electrum. The funding addresses are shown alongside, so this answers
/// both "where do I fund" and "how much is there". `--electrum` overrides the
/// config's electrum_url. The shown address is the next-unused one (where to
/// SEND); the sats are the keychain TOTAL across all its UTXOs.
pub(crate) async fn maker_balances(
    config: MakerNodeConfig,
    electrum: Option<String>,
) -> Result<(), Box<dyn std::error::Error>> {
    let (btc, anchor) = maker_funding_addresses(&config)?;
    let network = config
        .rgb
        .as_ref()
        .map(|r| r.network.as_str())
        .unwrap_or("");
    let electrum_url = electrum
        .filter(|s| !s.is_empty())
        .or_else(|| {
            config
                .rgb
                .as_ref()
                .map(|r| r.electrum_url.clone())
                .filter(|s| !s.is_empty())
        })
        .ok_or("no electrum URL: pass --electrum <url> or set [rgb] electrum_url")?;

    let (k0, k10) = maker_keychain_balances(&config, &electrum_url).await?;

    println!("colorex maker wallet balances ({network})");
    output::kv(&format!("BTC payment · keychain 0 · {k0} sats"), &btc);
    output::kv(&format!("RGB anchor · keychain 10 · {k10} sats"), &anchor);
    Ok(())
}

pub(crate) async fn maker_rescan(
    config: MakerNodeConfig,
    electrum: Option<String>,
) -> Result<(), Box<dyn std::error::Error>> {
    let rgb = config
        .rgb
        .as_ref()
        .ok_or("no [rgb] config: a wallet is required to rescan")?;
    let electrum_url = electrum
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| rgb.electrum_url.clone());
    if electrum_url.is_empty() {
        return Err("no --electrum given and no [rgb] electrum_url in config".into());
    }
    let backend = rfq_rgb::LibRgbBackend::new(
        rgb.data_dir.clone(),
        rgb.wallet_name.clone(),
        rgb.network.clone(),
        electrum_url.clone(),
        std::path::PathBuf::new(),
        String::new(),
    );
    output::step(&format!(
        "rescanning wallet from scratch via {electrum_url}"
    ));
    let tracked = backend.rescan_wallet().await?;
    output::step_ok();
    output::kv("tracked UTXOs after rescan", &tracked.to_string());
    output::note("Re-check recovery with: colorex maker inventory --btc");
    Ok(())
}

pub(crate) async fn maker_recover(
    config: MakerNodeConfig,
    contract: Option<String>,
    electrum: Option<String>,
    fee: u64,
    dry_run: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    use rfq_btc::BitcoinClient as _;
    let contract_id = resolve_contract_id(&config, contract).await?;
    let rgb = config
        .rgb
        .as_ref()
        .ok_or("no [rgb] config: a wallet is required to recover")?;
    let electrum_url = electrum
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| rgb.electrum_url.clone());
    if electrum_url.is_empty() {
        return Err("no --electrum given and no [rgb] electrum_url in config".into());
    }
    let asset = rfq_types::AssetId {
        network: rgb.network.parse()?,
        kind: rfq_types::AssetKind::Rgb20,
        id: contract_id,
    };
    // Recovery SIGNS, so the backend needs the real signer (account + password).
    let backend = rfq_rgb::LibRgbBackend::new(
        rgb.data_dir.clone(),
        rgb.wallet_name.clone(),
        rgb.network.clone(),
        electrum_url.clone(),
        rgb.signer.account_file.clone(),
        rgb.signer.password.clone(),
    );
    let probe = rfq_btc::ElectrumClient::connect(&electrum_url)?;

    output::step("syncing wallet");
    backend.sync_wallet().await?;
    output::step_ok();

    // Enumerate stranded allocations: live in the stock, NOT tracked by the
    // wallet, and provably unspent on-chain.
    let allocs = backend.debug_contract_allocations(&asset).await?;
    let mut stranded: Vec<(rfq_types::Outpoint, u64, Vec<u8>, u64)> = Vec::new();
    let mut probe_failed = 0usize;
    for (op, amount, in_wallet) in allocs {
        if in_wallet {
            continue;
        }
        match probe.outpoint_unspent(&op).await {
            Ok(true) => {
                let txout = probe.get_outpoint(&op).await?;
                stranded.push((op, txout.value_sats, txout.script_pubkey, amount));
            }
            Ok(false) => continue, // spent — skip
            Err(_) => {
                // Couldn't confirm on-chain (even after retries) — skip rather
                // than risk sweeping a spent output; surfaced below so the user
                // knows to re-run for the rest.
                probe_failed += 1;
            }
        }
    }
    if probe_failed > 0 {
        output::step_warn(&format!(
            "{probe_failed} allocation(s) could not be checked on-chain (electrum probe \
             failed) — skipped this pass; re-run `recover` to retry them"
        ));
    }

    if stranded.is_empty() {
        output::note("no stranded RGB found (nothing unspent + off-wallet to recover)");
        return Ok(());
    }
    let rgb_total: u64 = stranded.iter().map(|(_, _, _, amt)| amt).sum();
    output::kv(
        "stranded to sweep",
        &format!("{} outputs · {} units", stranded.len(), rgb_total),
    );
    for (op, sats, _, amt) in &stranded {
        println!("    {}:{}  {amt} units  ({sats} sats)", op.txid, op.vout);
    }

    // Pick the largest BTC-only UTXO to fund the fee + the new seal anchor.
    let now = now_ms();
    let btc = backend
        .list_btc_only_utxos(std::slice::from_ref(&asset), now)
        .await?;
    let fee_utxo = btc
        .iter()
        .max_by_key(|u| u.value_sats)
        .ok_or("no BTC-only UTXO available to fund the sweep fee")?;
    output::kv(
        "fee input",
        &format!(
            "{}:{} · {} sats (fee {fee})",
            fee_utxo.outpoint.txid, fee_utxo.outpoint.vout, fee_utxo.value_sats
        ),
    );

    if dry_run {
        output::note("dry-run: nothing built or broadcast. Re-run without --dry-run to sweep.");
        return Ok(());
    }

    // Keep (outpoint, amount) for post-sweep reporting before consuming `stranded`.
    let stranded_meta: Vec<(rfq_types::Outpoint, u64)> = stranded
        .iter()
        .map(|(op, _, _, amt)| (op.clone(), *amt))
        .collect();
    let stranded_inputs: Vec<(rfq_types::Outpoint, u64, Vec<u8>)> = stranded
        .into_iter()
        .map(|(op, sats, spk, _)| (op, sats, spk))
        .collect();
    let fee_input = (
        fee_utxo.outpoint.clone(),
        fee_utxo.value_sats,
        fee_utxo.script_pubkey.clone(),
    );

    output::step("building + signing recovery sweep");
    let (raw_tx, witness_txid, swept) = backend
        .recover_stranded_rgb(&asset, stranded_inputs, fee_input, fee)
        .await?;
    output::step_ok();

    // The sweep skips any candidate this wallet can't sign (counterparty
    // allocations FilterIncludeAll surfaced) — report what actually went in.
    let swept_set: std::collections::HashSet<(String, u32)> =
        swept.iter().map(|o| (o.txid.clone(), o.vout)).collect();
    let swept_units: u64 = stranded_meta
        .iter()
        .filter(|(op, _)| swept_set.contains(&(op.txid.clone(), op.vout)))
        .map(|(_, amt)| *amt)
        .sum();
    let skipped = stranded_meta.len() - swept.len();
    if skipped > 0 {
        output::step_warn(&format!(
            "skipped {skipped} allocation(s) not spendable by this wallet (not ours)"
        ));
    }

    output::step("broadcasting");
    let broadcast_txid = probe.broadcast(&raw_tx).await?;
    output::step_ok();
    output::kv("recovery tx", &broadcast_txid);
    if broadcast_txid != witness_txid {
        output::step_warn(&format!(
            "broadcast txid {broadcast_txid} != built witness id {witness_txid}"
        ));
    }
    output::note(&format!(
        "{swept_units} units ({} outputs) swept to the pinned host. Once confirmed: \
         re-sync and `colorex maker inventory --btc` to see them as sellable.",
        swept.len()
    ));
    Ok(())
}

/// Manual one-shot rebalance: plan each asset's + the BTC pool's denomination
/// ladder, build ONE `split_pools` tx, and broadcast it. The daemon's executor
/// does this automatically when `[rebalance] enabled`; this is for operators who
/// keep it off (run with the daemon stopped) or want a `--dry-run` preview. No
/// reservations are taken — it assumes exclusive wallet access, like `recover`.
pub(crate) async fn maker_rebalance(
    config: MakerNodeConfig,
    asset_filter: Option<String>,
    btc_only: bool,
    fee_override: Option<u64>,
    dry_run: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    use rfq_btc::BitcoinClient as _;
    use rfq_rgb::RgbBackend as _;
    let rgb = config
        .rgb
        .as_ref()
        .ok_or("no [rgb] config: a wallet is required to rebalance")?;
    let electrum_url = rgb.electrum_url.clone();
    if electrum_url.is_empty() {
        return Err("no [rgb] electrum_url in config".into());
    }
    let network: rfq_types::BitcoinNetwork = rgb.network.parse()?;
    let backend = rfq_rgb::LibRgbBackend::new(
        rgb.data_dir.clone(),
        rgb.wallet_name.clone(),
        rgb.network.clone(),
        electrum_url.clone(),
        rgb.signer.account_file.clone(),
        rgb.signer.password.clone(),
    );
    let probe = rfq_btc::ElectrumClient::connect(&electrum_url)?;

    output::step("syncing wallet");
    backend.sync_wallet().await?;
    output::step_ok();

    // Every registered asset (to exclude RGB-bearing outputs from the BTC pool),
    // vs the subset to actually ladder.
    let all_assets = registered_assets(&config).await;
    let ladder_assets: Vec<rfq_types::AssetId> = if btc_only {
        Vec::new()
    } else if let Some(id) = asset_filter {
        let contract_id = resolve_contract_id(&config, Some(id)).await?;
        vec![rfq_types::AssetId {
            network,
            kind: rfq_types::AssetKind::Rgb20,
            id: contract_id,
        }]
    } else {
        all_assets.clone()
    };

    let rgb_spec = config.rebalance.rgb_ladder();
    let btc_spec = config.rebalance.btc_ladder();

    // Plan each asset's ladder.
    let mut asset_splits = Vec::new();
    for asset in &ladder_assets {
        let inv = backend.list_inventory_utxos(asset).await?;
        let pairs: Vec<(rfq_types::Outpoint, u64)> =
            inv.iter().map(|u| (u.outpoint.clone(), u.amount)).collect();
        if let Some((source, source_amount, rungs)) = rfq_maker::plan_ladder(&pairs, &rgb_spec) {
            let source_btc_sats = inv
                .iter()
                .find(|u| u.outpoint == source)
                .map(|u| u.btc_sats)
                .unwrap_or(0);
            asset_splits.push(rfq_maker::AssetSplit {
                asset: asset.clone(),
                source,
                source_amount,
                source_btc_sats,
                rungs,
            });
        }
    }

    // Plan the BTC ladder (excluding every registered asset's RGB outputs).
    let now = now_ms();
    let btc = backend.list_btc_only_utxos(&all_assets, now).await?;
    let btc_avail: Vec<(rfq_types::Outpoint, u64)> = btc
        .iter()
        .map(|u| (u.outpoint.clone(), u.value_sats))
        .collect();
    let btc_total: u64 = btc_avail.iter().map(|(_, s)| *s).sum();
    let fattest_btc = btc_avail.iter().max_by_key(|(_, s)| *s).cloned();
    let btc_split =
        rfq_maker::plan_ladder(&btc_avail, &btc_spec).map(|(source, source_sats, rungs)| {
            rfq_maker::BtcSplit {
                source,
                source_sats,
                rungs,
            }
        });

    if asset_splits.is_empty() && btc_split.is_none() {
        output::note("inventory already matches the ladder — nothing to rebalance.");
        return Ok(());
    }

    let btc_fee_source = btc_split
        .as_ref()
        .map(|b| (b.source.clone(), b.source_sats))
        .or_else(|| fattest_btc.clone());
    if !asset_splits.is_empty() && btc_fee_source.is_none() {
        return Err("no BTC-only UTXO to fund the split anchors + fee (fund keychain 0)".into());
    }
    let btc_source_value = btc_fee_source.as_ref().map(|(_, v)| *v).unwrap_or(0);

    // Fee: next-block feerate × estimated vsize (capped), unless overridden.
    let feerate = if fee_override.is_some() {
        0
    } else {
        rfq_maker::clamp_next_block_feerate(
            probe
                .estimate_feerate(rfq_maker::REBALANCE_CONF_TARGET_BLOCKS)
                .await
                .unwrap_or(0),
            &network,
        )
    };
    let max_fee = config.rebalance.rebalance_max_fee_sats;
    let has_btc = btc_fee_source.is_some();
    let fee_of = |assets: usize, rgb_rungs: usize, btc_rungs: usize| -> u64 {
        if let Some(f) = fee_override {
            return f;
        }
        let inputs = assets + usize::from(has_btc);
        let outputs = usize::from(assets > 0) + rgb_rungs + btc_rungs + 1;
        feerate
            .saturating_mul(rfq_maker::estimate_rebalance_vbytes(inputs, outputs))
            .min(max_fee)
    };
    let budget_fee = fee_of(
        asset_splits.len(),
        asset_splits.iter().map(|a| a.rungs.len()).sum(),
        btc_split.as_ref().map(|b| b.rungs.len()).unwrap_or(0),
    );

    let plan =
        rfq_maker::assemble_rebalance_tx(asset_splits, btc_split, btc_source_value, budget_fee)
            .ok_or("budget can't fund any split (fund keychain 0)")?;
    let fee = fee_of(
        plan.assets.len(),
        plan.assets.iter().map(|a| a.rungs.len()).sum(),
        plan.btc.as_ref().map(|b| b.rungs.len()).unwrap_or(0),
    );

    // Show the plan.
    for a in &plan.assets {
        output::kv(
            &format!("asset {}", a.asset.id),
            &format!(
                "{} rungs {:?} from {}:{}",
                a.rungs.len(),
                a.rungs,
                a.source.txid,
                a.source.vout
            ),
        );
    }
    if let Some(b) = &plan.btc {
        output::kv(
            "btc rungs",
            &format!(
                "{:?} sats from {}:{}",
                b.rungs, b.source.txid, b.source.vout
            ),
        );
    }
    output::kv("fee", &format!("{fee} sats"));
    output::kv(
        "btc needed",
        &format!("{} sats (pool {btc_total})", plan.btc_needed),
    );

    if dry_run {
        output::note("dry-run: nothing built or broadcast. Re-run without --dry-run.");
        return Ok(());
    }

    let assets_arg: Vec<(rfq_types::AssetId, rfq_types::Outpoint, Vec<u64>)> = plan
        .assets
        .iter()
        .map(|a| (a.asset.clone(), a.source.clone(), a.rungs.clone()))
        .collect();
    let btc_arg = btc_fee_source.as_ref().map(|(op, _)| {
        (
            op.clone(),
            plan.btc
                .as_ref()
                .map(|b| b.rungs.clone())
                .unwrap_or_default(),
        )
    });

    output::step("building + signing rebalance tx");
    let (raw_tx, witness_txid) = backend.split_pools(assets_arg, btc_arg, fee).await?;
    output::step_ok();

    output::step("broadcasting");
    let broadcast_txid = probe.broadcast(&raw_tx).await?;
    output::step_ok();
    output::kv("rebalance tx", &broadcast_txid);
    if broadcast_txid != witness_txid {
        output::step_warn(&format!(
            "broadcast txid {broadcast_txid} != built witness id {witness_txid}"
        ));
    }
    output::note(
        "once confirmed: re-sync; the new ladder pieces appear as RGB inventory + BTC pool UTXOs.",
    );
    Ok(())
}

pub(crate) async fn maker_accept(
    config: MakerNodeConfig,
    consignment: String,
    contract: Option<String>,
) -> Result<(), Box<dyn std::error::Error>> {
    let base64 = read_consignment(&consignment)?;
    let contract_id = resolve_contract_id(&config, contract).await?;
    accept_inventory_consignment(&config, contract_id, &base64).await?;
    println!("accepted consignment into the maker stash");
    Ok(())
}

/// Send RGB from the maker's wallet to a recipient invoice — the maker analogue
/// of `issuer transfer`. Builds + signs + broadcasts the anchoring tx via
/// `distribute` (the contract + amount are carried by the invoice) and returns
/// the consignment. Run with the daemon STOPPED: it spends the maker's bp-wallet
/// directly, and the chain observer reconciles the spent input + change on the
/// next `maker up`.
pub(crate) async fn maker_transfer(
    config: MakerNodeConfig,
    invoice: String,
    electrum: Option<String>,
    fee: u64,
    out: Option<PathBuf>,
) -> Result<(), Box<dyn std::error::Error>> {
    let rgb = config
        .rgb
        .as_ref()
        .ok_or("no [rgb] config: a wallet is required to transfer")?;
    let electrum_url = electrum
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| rgb.electrum_url.clone());
    if electrum_url.is_empty() {
        return Err("no --electrum given and no [rgb] electrum_url in config".into());
    }
    let backend = rfq_rgb::LibRgbBackend::new(
        rgb.data_dir.clone(),
        rgb.wallet_name.clone(),
        rgb.network.clone(),
        electrum_url,
        rgb.signer.account_file.clone(),
        rgb.signer.password.clone(),
    );
    let (txid, consignment) = backend.distribute(&invoice, fee).await?;
    println!("transfer broadcast: {txid}");
    match out {
        Some(path) => {
            std::fs::write(&path, &consignment)?;
            eprintln!(
                "wrote consignment to {} — hand it to the recipient (they accept after the tx confirms)",
                path.display()
            );
        }
        None => {
            println!("hand this consignment to the recipient (they accept after the tx confirms):");
            println!("{consignment}");
        }
    }
    Ok(())
}

pub(crate) async fn maker_reconsign(
    config: MakerNodeConfig,
    contract: Option<String>,
    outpoint: String,
    out: Option<PathBuf>,
) -> Result<(), Box<dyn std::error::Error>> {
    let contract_id = resolve_contract_id(&config, contract).await?;
    let consignment = reconsign_consignment(&config, contract_id, &outpoint)?;
    match out {
        Some(path) => {
            std::fs::write(&path, &consignment)?;
            eprintln!("wrote consignment to {}", path.display());
        }
        None => println!("{consignment}"),
    }
    Ok(())
}

pub(crate) async fn maker_get_consignment(
    config: MakerNodeConfig,
    quote_id: String,
    out: Option<PathBuf>,
) -> Result<(), Box<dyn std::error::Error>> {
    match fetch_consignment(&config, &quote_id).await? {
        Some(consignment) => match out {
            Some(path) => {
                std::fs::write(&path, &consignment)?;
                eprintln!("wrote consignment to {}", path.display());
            }
            None => println!("{consignment}"),
        },
        None => {
            return Err(format!(
                "no consignment recorded for quote {quote_id} (try `reconsign` to re-derive)"
            )
            .into())
        }
    }
    Ok(())
}

/// Open the maker.db `orders` store from the config's `[rgb]` db path, importing
/// any legacy orders.json first. Orders live in maker.db, so this requires an
/// `[rgb]` section (mock-only makers have no shared order store).
pub(crate) async fn open_order_store(
    config_path: &Path,
) -> Result<rfq_store::SqliteOrderStore, Box<dyn std::error::Error>> {
    let config = load_config(config_path)?;
    let rgb = config
        .rgb
        .as_ref()
        .ok_or("no [rgb] config: standing orders live in maker.db, which needs a wallet")?;
    let db_path = rgb.data_dir.join(&rgb.network).join("maker.db");
    if let Some(parent) = db_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let store = rfq_store::SqliteOrderStore::open(&db_path).await?;
    orders::migrate_orders_json(&store, config_path).await?;
    Ok(store)
}

/// Open the maker.db contract registry from the config's `[rgb]` db path.
pub(crate) async fn open_contract_store(
    config: &MakerNodeConfig,
) -> Result<rfq_store::SqliteContractStore, Box<dyn std::error::Error>> {
    let rgb = config
        .rgb
        .as_ref()
        .ok_or("no [rgb] config: the contract registry lives in maker.db, which needs a wallet")?;
    let db_path = rgb.data_dir.join(&rgb.network).join("maker.db");
    if let Some(parent) = db_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    Ok(rfq_store::SqliteContractStore::open(&db_path).await?)
}

/// Resolve a consignment argument that is EITHER a file path OR the inline base64
/// string. If `value` names an existing file, read it; otherwise treat `value`
/// itself as the base64 — so `--consignment` takes a path or a pasted blob.
pub(crate) fn read_consignment(value: &str) -> Result<String, Box<dyn std::error::Error>> {
    let trimmed = value.trim();
    if std::path::Path::new(trimmed).is_file() {
        Ok(std::fs::read_to_string(trimmed)?.trim().to_owned())
    } else {
        Ok(trimmed.to_owned())
    }
}

/// Resolve which contract a single-asset command operates on. An explicit
/// `--contract`/`--asset` wins; otherwise the registry must hold exactly one
/// contract (the unambiguous default). Zero or many → an actionable error rather
/// than a silent guess. Replaces the old `[rgb] contract_id` default.
pub(crate) async fn resolve_contract_id(
    config: &MakerNodeConfig,
    explicit: Option<String>,
) -> Result<String, Box<dyn std::error::Error>> {
    let registered = open_contract_store(config)
        .await?
        .list()
        .await?
        .into_iter()
        .map(|c| c.contract_id)
        .collect();
    pick_contract(explicit, registered).map_err(Into::into)
}

/// Pure resolution core (testable without a store): explicit id wins; else the
/// registry must hold exactly one contract; zero or many is an actionable error.
pub(crate) fn pick_contract(
    explicit: Option<String>,
    registered: Vec<String>,
) -> Result<String, String> {
    if let Some(id) = explicit.filter(|s| !s.is_empty()) {
        return Ok(id);
    }
    match registered.len() {
        0 => Err("no contracts registered — run `colorex maker contract import <id>` first".into()),
        1 => Ok(registered.into_iter().next().unwrap()),
        n => Err(format!(
            "{n} contracts registered — pass --contract <id> to choose one"
        )),
    }
}

/// All registered contracts as `AssetId`s (empty if none / no `[rgb]` network).
pub(crate) async fn registered_assets(config: &MakerNodeConfig) -> Vec<rfq_types::AssetId> {
    let Some(network) = config.rgb.as_ref().and_then(|r| r.network.parse().ok()) else {
        return Vec::new();
    };
    let Ok(store) = open_contract_store(config).await else {
        return Vec::new();
    };
    store
        .list()
        .await
        .unwrap_or_default()
        .into_iter()
        .map(|c| rfq_types::AssetId {
            network,
            kind: rfq_types::AssetKind::Rgb20,
            id: c.contract_id,
        })
        .collect()
}

/// Read `(ticker, precision)` for `contract_id` from the maker's RGB stock.
/// Errors if the contract isn't in the stock — the signal that it must be minted
/// or its consignment accepted before it can be registered/traded.
pub(crate) fn contract_spec_for(
    config: &MakerNodeConfig,
    contract_id: &str,
) -> Result<(String, u8), Box<dyn std::error::Error>> {
    let r = config
        .rgb
        .as_ref()
        .ok_or("no [rgb] config: a wallet stock is required to read a contract")?;
    let backend = rfq_rgb::LibRgbBackend::new(
        r.data_dir.clone(),
        r.wallet_name.clone(),
        r.network.clone(),
        String::new(),
        std::path::PathBuf::new(),
        String::new(),
    );
    let asset = rfq_types::AssetId {
        network: r
            .network
            .parse()
            .map_err(|_| format!("invalid [rgb] network '{}'", r.network))?,
        kind: rfq_types::AssetKind::Rgb20,
        id: contract_id.to_owned(),
    };
    backend.contract_spec(&asset).map_err(|e| {
        format!("contract {contract_id} not found in the maker's stock ({e}) — mint it or import its consignment first (--consignment)").into()
    })
}

pub(crate) async fn contract_import(
    config: MakerNodeConfig,
    id: String,
    consignment: Option<String>,
) -> Result<(), Box<dyn std::error::Error>> {
    // Validate the id parses before any side effects.
    id.parse::<rfq_rgb::ContractId>()
        .map_err(|e| format!("invalid contract id {id}: {e}"))?;

    // `--consignment` folds in `accept`: absorb the contract into the stock first,
    // so a freshly-handed-over asset registers in one step. The arg is a file path
    // OR the inline base64.
    if let Some(consignment) = consignment {
        let base64 = read_consignment(&consignment)?;
        accept_inventory_consignment(&config, id.clone(), &base64).await?;
        output::step_ok_with(&format!(
            "accepted consignment for {}",
            init::truncate_contract(&id)
        ));
    }

    // Cache ticker + precision from the stock (also verifies it's actually there).
    let (ticker, precision) = contract_spec_for(&config, &id)?;
    let network = config
        .rgb
        .as_ref()
        .map(|r| r.network.clone())
        .unwrap_or_default();

    let store = open_contract_store(&config).await?;
    let existed = store.get(&id).await?.is_some();
    store
        .upsert(rfq_store::ContractRecord {
            contract_id: id.clone(),
            ticker: ticker.clone(),
            precision,
            network,
            added_at_ms: now_ms(),
        })
        .await?;
    let verb = if existed { "updated" } else { "imported" };
    println!(
        "{verb} contract {ticker} ({})",
        init::truncate_contract(&id)
    );
    Ok(())
}

pub(crate) async fn contract_list(
    config: MakerNodeConfig,
) -> Result<(), Box<dyn std::error::Error>> {
    let store = open_contract_store(&config).await?;
    let contracts = store.list().await?;
    if contracts.is_empty() {
        output::note("no contracts registered — add one with: colorex maker contract import <id>");
        return Ok(());
    }
    println!("registered contracts ({})", contracts.len());
    for c in contracts {
        output::kv(
            &format!("{} · precision {}", c.ticker, c.precision),
            &c.contract_id,
        );
    }
    Ok(())
}

pub(crate) async fn contract_remove(
    config: MakerNodeConfig,
    id: String,
) -> Result<(), Box<dyn std::error::Error>> {
    let store = open_contract_store(&config).await?;
    if store.remove(&id).await? {
        println!("removed contract {}", init::truncate_contract(&id));
    } else {
        return Err(format!("no registered contract {id}").into());
    }
    Ok(())
}

pub(crate) async fn order_create(
    config_path: &Path,
    side: String,
    asset: Option<String>,
    price: u64,
    size: u64,
    mirror: bool,
    mirror_spread_bps: u16,
) -> Result<(), Box<dyn std::error::Error>> {
    let parsed_side = orders::parse_side(&side)
        .ok_or_else(|| format!("invalid --side '{side}': expected 'buy' or 'sell'"))?;
    let config = load_config(config_path)?;
    let asset_id = resolve_contract_id(&config, asset).await?;
    if mirror && mirror_spread_bps == 0 {
        output::step_warn(
            "--mirror with --mirror-spread-bps 0 ping-pongs at the same price (no margin)",
        );
    }
    let store = open_order_store(config_path).await?;
    let order = orders::new_order(
        &side,
        asset_id.clone(),
        price,
        size,
        mirror,
        mirror_spread_bps,
    );
    let id = order.id.clone();
    let replaced = store.get(&order.asset_id, &order.side).await?;
    store.upsert(order).await?;
    match replaced {
        Some(old) => println!("created order {id} (replaced {})", old.id),
        None => println!("created order {id}"),
    }
    // Non-blocking heads-up if on-hand inventory can't back the order.
    warn_low_balance(&config, &parsed_side, &asset_id, price, size).await;
    Ok(())
}

/// Warn (non-blocking) if the maker's cached inventory can't back a full-size
/// quote for this order. Orders are just terms — the maker declines quotes it
/// can't fund — so this never blocks creation. Reads maker.db directly (WAL-safe;
/// no bp-wallet file access, so it's fine while the daemon runs).
pub(crate) async fn warn_low_balance(
    config: &MakerNodeConfig,
    side: &Side,
    asset_id: &str,
    price: u64,
    size: u64,
) {
    let Some(rgb) = config.rgb.as_ref() else {
        return;
    };
    let db_path = rgb.data_dir.join(&rgb.network).join("maker.db");
    if !db_path.exists() {
        return;
    }
    match side {
        // A buy order SELLS RGB — the maker needs the asset on hand.
        Side::Buy => {
            let Ok(network) = rgb.network.parse::<rfq_types::BitcoinNetwork>() else {
                return;
            };
            let Ok(store) = rfq_store::SqliteInventoryStore::open(&db_path).await else {
                return;
            };
            let asset = rfq_types::AssetId {
                network,
                kind: rfq_types::AssetKind::Rgb20,
                id: asset_id.to_owned(),
            };
            let avail: u64 = store
                .list_available(&asset)
                .await
                .iter()
                .map(|u| u.amount)
                .sum();
            if avail < size {
                output::step_warn(&format!(
                    "maker holds {avail} units of this asset (< order size {size}) — a buy order \
                     SELLS RGB, so quotes above your balance will decline until funded"
                ));
            }
        }
        // A sell order PAYS BTC — the maker needs BTC in its pool.
        Side::Sell => {
            let Ok(store) = rfq_store::SqliteBtcInventoryStore::open(&db_path).await else {
                return;
            };
            let avail: u64 = store
                .list_available()
                .await
                .iter()
                .map(|u| u.value_sats)
                .sum();
            // `price` is sats-per-token; the gross for a `size`-smallest-unit
            // quote is `price * size / 10^precision`. Look up precision from the
            // registry (default 0 if unknown) so the estimate matches a real quote.
            let precision = match rfq_store::SqliteContractStore::open(&db_path).await {
                Ok(s) => s.get(asset_id).await.ok().flatten().map(|c| c.precision),
                Err(_) => None,
            }
            .unwrap_or(0);
            let need = rfq_maker::quote_total_sats(price, size, precision, true);
            if avail < need {
                output::step_warn(&format!(
                    "maker BTC pool is {avail} sats (< ~{need} for a full {size}-unit quote) — a \
                     sell order PAYS BTC, so large quotes will decline until funded"
                ));
            }
        }
    }
}

pub(crate) async fn order_list(config_path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let store = open_order_store(config_path).await?;
    let orders = store.list().await?;
    if orders.is_empty() {
        println!("no standing orders (maker.db)");
        return Ok(());
    }
    for o in &orders {
        let mirror = if o.mirror {
            format!("  mirror=on spread_bps={}", o.mirror_spread_bps)
        } else {
            String::new()
        };
        println!(
            "{}  side={}  asset={}  price/token={}  size={}{mirror}",
            o.id, o.side, o.asset_id, o.price, o.size
        );
    }
    Ok(())
}

pub(crate) async fn order_cancel(
    config_path: &Path,
    id: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let store = open_order_store(config_path).await?;
    if !store.cancel(id).await? {
        return Err(format!("no order with id '{id}'").into());
    }
    println!("cancelled order {id}");
    Ok(())
}

pub(crate) async fn order_clear(config_path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let store = open_order_store(config_path).await?;
    let n = store.clear().await?;
    println!("cleared {n} order(s)");
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn issuer_issue(
    common: WalletCommon,
    ticker: String,
    asset_name: String,
    precision: u8,
    supply: u64,
    details: Option<String>,
    seal: Option<String>,
    issuer: String,
) -> Result<(), Box<dyn std::error::Error>> {
    let backend = resolve_named(common.into_input())?.backend();
    let genesis_seal = match seal {
        Some(s) => s,
        None => backend.pick_genesis_seal()?,
    };
    let id = backend.issue_contract(
        &ticker,
        &asset_name,
        details.as_deref(),
        precision,
        supply,
        &genesis_seal,
        &issuer,
    )?;
    println!("issued contract: {id}");
    println!("  genesis seal: {genesis_seal}");
    println!("  register it on a maker with: colorex maker contract import {id}");
    Ok(())
}

pub(crate) fn issuer_contracts(common: WalletCommon) -> Result<(), Box<dyn std::error::Error>> {
    let backend = resolve_named(common.into_input())?.backend();
    for line in backend.list_contracts()? {
        println!("{line}");
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn issuer_transfer(
    common: WalletCommon,
    invoice: String,
    electrum: Option<String>,
    account_file: Option<PathBuf>,
    password: Option<String>,
    fee: u64,
) -> Result<(), Box<dyn std::error::Error>> {
    let input = WalletInput {
        electrum_url: electrum,
        account_file,
        password,
        ..common.into_input()
    };
    let backend = resolve_named(input)?.backend();
    let (txid, consignment) = backend.distribute(&invoice, fee).await?;
    println!("transfer broadcast: {txid}");
    println!("hand this consignment to the recipient (they accept after the tx confirms):");
    println!("{consignment}");
    Ok(())
}

pub(crate) async fn run(
    config: MakerNodeConfig,
    config_path: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    let _client = RfqClient::new(parse_broker_url(&config)?);

    let runtime = build_runtime(&config).await?;
    let order_store = runtime.order_store;
    let precisions = runtime.precisions;
    // One-time import of any legacy orders.json into the maker.db `orders` table.
    orders::migrate_orders_json(order_store.as_ref(), config_path).await?;

    // Seed the maker's price policy from the standing orders in maker.db.
    let standing = order_store.list().await?;
    let order_count = standing.len();
    let maker = runtime
        .maker
        .with_price_policy(orders::price_policy(&standing, &precisions));
    let chain_observer_deps = runtime.chain_observer;
    let rebalance_executor_deps = runtime.rebalance_executor;
    let app = maker_app(maker.clone());
    let listener = TcpListener::bind(&config.maker.listen_addr).await?;
    let (shutdown_tx, shutdown_rx) = oneshot::channel();

    let broker_ws = broker_client::broker_ws_url(&config.maker.broker_url);
    let network = config
        .rgb
        .as_ref()
        .map(|r| r.network.as_str())
        .unwrap_or("mock");
    let node_short = config
        .maker
        .node_id
        .get(..8)
        .unwrap_or(&config.maker.node_id);

    println!("colorex maker up");
    output::info(&format!("node {node_short}… on {network}"));
    output::info(&format!("broker {}", config.maker.broker_url));
    output::info(&format!("standing orders {order_count} (maker.db)"));
    // Funding addresses, so the operator can top up without a separate command.
    if let Ok((btc, anchor)) = maker_funding_addresses(&config) {
        output::kv("fund BTC  · keychain 0 ", &btc);
        output::kv("fund RGB  · keychain 10", &anchor);
    }
    output::step("chain observer");
    if chain_observer_deps.is_some() {
        output::step_ok_with(&format!("every {:?}", config.intervals.chain_observer));
    } else {
        output::step_skip();
    }
    output::step("rebalance executor");
    if config.rebalance.enabled && rebalance_executor_deps.is_some() {
        output::step_ok_with(&format!("every {:?}", config.intervals.rebalance));
    } else {
        output::step_skip();
    }
    output::step("http server");
    output::step_ok_with(&config.maker.listen_addr);
    output::step("broker stream");
    output::step_ok_with(&broker_ws);

    // Hot-reload standing orders so `order create`/`cancel` take effect without a
    // maker restart.
    let order_reload_task = spawn_order_reload_loop(
        maker.clone(),
        order_store.clone(),
        precisions.clone(),
        std::time::Duration::from_secs(5),
    );
    let strategy_task = spawn_strategy_loop(
        maker.clone(),
        order_store.clone(),
        precisions.clone(),
        config.intervals.strategy,
    );
    let cleanup_task = spawn_cleanup_loop(maker.clone(), config.intervals.cleanup);
    let rebalance_task = spawn_rebalance_loop(
        maker.clone(),
        config.intervals.rebalance,
        (&config.rebalance).into(),
    );
    // Proactive split executor — opt-in, only with a real backend.
    let rebalance_executor_task = if config.rebalance.enabled {
        rebalance_executor_deps.map(|deps| {
            spawn_rebalance_executor_loop(
                maker.clone(),
                deps,
                config.intervals.rebalance,
                config.rebalance.rgb_ladder(),
                config.rebalance.btc_ladder(),
                config.rebalance.min_btc_reserve_sats,
                config.rebalance.rebalance_max_fee_sats,
            )
        })
    } else {
        None
    };
    let chain_observer_task = chain_observer_deps.map(|deps| {
        spawn_chain_observer_loop(maker.clone(), deps, config.intervals.chain_observer)
    });
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

    // Dial the broker and auto-register over WebSocket — the broker then pushes
    // quote/accept/settle requests over it (no `BROKER_MAKER` config needed).
    let broker_task = tokio::spawn(broker_client::run_broker_stream(
        broker_ws,
        MakerId(config.maker.node_id.clone()),
        maker.clone(),
    ));

    tokio::signal::ctrl_c().await?;
    println!("colorex maker shutting down");

    let _ = shutdown_tx.send(());
    order_reload_task.abort();
    strategy_task.abort();
    cleanup_task.abort();
    rebalance_task.abort();
    broker_task.abort();
    if let Some(t) = &chain_observer_task {
        t.abort();
    }
    if let Some(t) = &rebalance_executor_task {
        t.abort();
    }
    let _ = server_task.await;
    let _ = broker_task.await;
    let _ = order_reload_task.await;
    let _ = strategy_task.await;
    let _ = cleanup_task.await;
    let _ = rebalance_task.await;
    if let Some(t) = chain_observer_task {
        let _ = t.await;
    }
    if let Some(t) = rebalance_executor_task {
        let _ = t.await;
    }

    Ok(())
}

pub(crate) async fn health(config: MakerNodeConfig) -> Result<(), Box<dyn std::error::Error>> {
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

pub(crate) async fn broker_health_status(
    config: &MakerNodeConfig,
) -> Result<String, Box<dyn std::error::Error>> {
    let client = RfqClient::new(parse_broker_url(config)?);
    let response = client.health().await?;

    Ok(response.status)
}

pub(crate) async fn inventory(
    config: MakerNodeConfig,
    btc: bool,
    electrum: Option<String>,
) -> Result<(), Box<dyn std::error::Error>> {
    let _broker_url = parse_broker_url(&config)?;
    let maker = build_maker(&config).await?;

    println!("colorex maker inventory");
    println!("node_id={}", config.maker.node_id);

    // Per-contract snapshots from the registry; fall back to the aggregate view
    // when nothing is registered (mock maker / fresh install pre-migration).
    let contracts = match open_contract_store(&config).await {
        Ok(store) => store.list().await.unwrap_or_default(),
        Err(_) => Vec::new(),
    };
    let network = config.rgb.as_ref().and_then(|r| r.network.parse().ok());
    match network {
        Some(network) if !contracts.is_empty() => {
            for c in &contracts {
                let asset = rfq_types::AssetId {
                    network,
                    kind: rfq_types::AssetKind::Rgb20,
                    id: c.contract_id.clone(),
                };
                let snapshot = maker.inventory_summary_for(&asset).await;
                println!();
                println!(
                    "[{} · {}]",
                    c.ticker,
                    init::truncate_contract(&c.contract_id)
                );
                print_inventory_snapshot(&snapshot, Some(&(c.ticker.clone(), c.precision)));
            }
        }
        _ => {
            // No registry (mock maker / fresh install): aggregate, raw amounts.
            let snapshot = maker.inventory_summary().await;
            print_inventory_snapshot(&snapshot, None);
        }
    }

    inventory_orders(&config).await?;

    if btc {
        println!();
        inventory_btc(&config, electrum).await?;
    }

    Ok(())
}

/// Print the standing orders with each one's cumulative FILLED amount (per
/// `(asset, side)`, lifetime). Reads the maker.db `orders` + `fills` tables
/// directly (works whether or not the daemon is running). No-op for a mock /
/// no-`[rgb]` maker, which has no shared store.
pub(crate) async fn inventory_orders(
    config: &MakerNodeConfig,
) -> Result<(), Box<dyn std::error::Error>> {
    use rfq_store::{FillStore as _, OrderStore as _};
    let Some(rgb) = config.rgb.as_ref() else {
        return Ok(());
    };
    let db_path = rgb.data_dir.join(&rgb.network).join("maker.db");
    if !db_path.exists() {
        return Ok(());
    }
    let order_store = rfq_store::SqliteOrderStore::open(&db_path).await?;
    let orders = order_store.list().await?;
    if orders.is_empty() {
        return Ok(());
    }
    let fills = rfq_store::SqliteFillStore::open(&db_path).await?;
    // Per-asset ticker/precision from the registry, so each order's FILLED amount
    // renders in its own contract's units (orders are multi-asset).
    let specs: std::collections::HashMap<String, (String, u8)> =
        match open_contract_store(config).await {
            Ok(s) => s
                .list()
                .await
                .unwrap_or_default()
                .into_iter()
                .map(|c| (c.contract_id, (c.ticker, c.precision)))
                .collect(),
            Err(_) => Default::default(),
        };

    println!();
    println!("standing orders ({})", orders.len());
    for o in &orders {
        let filled = match orders::parse_side(&o.side) {
            Some(s) => fills.filled_for(&o.asset_id, &s, 0).await.unwrap_or(0),
            None => 0,
        };
        let filled_str = match specs.get(&o.asset_id) {
            Some((ticker, precision)) => {
                format!("{ticker} {}", rfq_types::format_amount(filled, *precision))
            }
            None => filled.to_string(),
        };
        let mirror = if o.mirror {
            format!("  mirror=on/{}bps", o.mirror_spread_bps)
        } else {
            String::new()
        };
        println!(
            "    {} {}  price/unit={}  size={}  FILLED={}{mirror}",
            o.side, o.id, o.price, o.size, filled_str
        );
    }
    Ok(())
}

/// Dump the BTC inventory across all three layers so a "maker has no BTC
/// inventory" report can be localized to the layer that drifted:
///
/// - **L1/L2 on-chain wallet** (bp-wallet via electrum) — the real UTXO set.
/// - **RGB-exclusion filter** (`list_btc_only_utxos`) — which of those are
///   spendable for funding (not carrying an RGB allocation).
/// - **L3 SQLite cache** (`maker.db` `btc_utxos`) — what coin-selection
///   actually reads, with per-status counts (only `Available` is selectable).
///
/// Read-only. The drift summary at the end tells you which hypothesis holds:
/// chain-empty → depletion/sync; chain-has-BTC-but-filter-empty → RGB filter;
/// filter-has-BTC-but-cache-Available-empty → reservation leak / ingest gap.
pub(crate) async fn inventory_btc(
    config: &MakerNodeConfig,
    electrum: Option<String>,
) -> Result<(), Box<dyn std::error::Error>> {
    use rfq_store::{BtcInventoryStore, SqliteBtcInventoryStore};
    use rfq_types::BtcInventoryStatus;

    let rgb = config
        .rgb
        .as_ref()
        .ok_or("no [rgb] config: a wallet is required for --btc inventory")?;

    let electrum_url = electrum
        .filter(|s| !s.is_empty())
        .or_else(|| Some(rgb.electrum_url.clone()).filter(|s| !s.is_empty()));

    println!("colorex maker btc-inventory ({})", rgb.network);

    // --- L1/L2: on-chain wallet (sync against electrum when available) ---
    let backend = rfq_rgb::LibRgbBackend::new(
        rgb.data_dir.clone(),
        rgb.wallet_name.clone(),
        rgb.network.clone(),
        electrum_url.clone().unwrap_or_default(),
        std::path::PathBuf::new(),
        String::new(),
    );
    match &electrum_url {
        Some(url) => match backend.sync_wallet().await {
            Ok(()) => output::note(&format!("on-chain wallet — synced via {url}")),
            Err(e) => output::step_warn(&format!("sync failed (showing stale cache): {e}")),
        },
        None => output::note("on-chain wallet — STALE (no --electrum / [rgb] electrum_url)"),
    }

    let raw = backend.wallet_balance().await?;
    let (mut k0_sats, mut k0_n, mut other_sats, mut other_n) = (0u64, 0usize, 0u64, 0usize);
    for u in &raw {
        if u.keychain == 0 {
            k0_sats += u.sats;
            k0_n += 1;
        } else {
            other_sats += u.sats;
            other_n += 1;
        }
    }
    output::kv(
        "keychain 0 (BTC funding)",
        &format!("{k0_n} utxos · {k0_sats} sats"),
    );
    output::kv(
        "other keychains (RGB anchor)",
        &format!("{other_n} utxos · {other_sats} sats"),
    );
    for u in &raw {
        println!(
            "    {}:{}  {} sats  k{}/{}",
            u.txid, u.vout, u.sats, u.keychain, u.index
        );
    }

    // --- RGB-exclusion filter: which on-chain UTXOs are fundable ---
    println!();
    let assets = registered_assets(config).await;
    let filtered = if assets.is_empty() {
        output::note("RGB filter skipped — no contracts registered");
        None
    } else {
        let now = now_ms();
        let btc_only = backend.list_btc_only_utxos(&assets, now).await?;
        let only_total: u64 = btc_only.iter().map(|u| u.value_sats).sum();
        output::kv(
            "BTC-only (fundable)",
            &format!("{} utxos · {} sats", btc_only.len(), only_total),
        );
        // Anything on-chain but not in btc_only was excluded as RGB-bearing.
        let keep: std::collections::HashSet<(String, u32)> = btc_only
            .iter()
            .map(|u| (u.outpoint.txid.clone(), u.outpoint.vout))
            .collect();
        let excluded: Vec<_> = raw
            .iter()
            .filter(|u| !keep.contains(&(u.txid.clone(), u.vout)))
            .collect();
        if excluded.is_empty() {
            output::note("excluded as RGB-bearing: none");
        } else {
            output::note(&format!("excluded as RGB-bearing: {}", excluded.len()));
            for u in excluded {
                println!(
                    "    {}:{}  {} sats  k{}/{}",
                    u.txid, u.vout, u.sats, u.keychain, u.index
                );
            }
        }
        Some(only_total)
    };

    // --- L3: SQLite cache (what coin-selection reads) ---
    println!();
    let db_path = rgb.data_dir.join(&rgb.network).join("maker.db");
    let store = SqliteBtcInventoryStore::open(&db_path).await?;
    let rows = store.list_all().await;
    output::kv(
        "SQLite cache (maker.db btc_utxos)",
        &db_path.display().to_string(),
    );
    let now = now_ms();
    let (mut avail_s, mut avail_n) = (0u64, 0usize);
    let (mut resv_s, mut resv_n) = (0u64, 0usize);
    let (mut pend_s, mut pend_n) = (0u64, 0usize);
    let (mut spent_s, mut spent_n) = (0u64, 0usize);
    let (mut inval_s, mut inval_n) = (0u64, 0usize);
    let mut detail: Vec<String> = Vec::new();
    for u in &rows {
        let op = format!("{}:{}", u.outpoint.txid, u.outpoint.vout);
        match &u.status {
            BtcInventoryStatus::Available => {
                avail_s += u.value_sats;
                avail_n += 1;
            }
            BtcInventoryStatus::Reserved {
                quote_id,
                expires_at_ms,
                ..
            } => {
                resv_s += u.value_sats;
                resv_n += 1;
                let flag = if *expires_at_ms <= now {
                    "EXPIRED — should be released".to_string()
                } else {
                    format!("expires_in={}ms", expires_at_ms - now)
                };
                detail.push(format!(
                    "    RESERVED {op}  {} sats  quote={} {flag}",
                    u.value_sats, quote_id.0
                ));
            }
            BtcInventoryStatus::PendingBitcoinConfirm { witness_txid, .. } => {
                pend_s += u.value_sats;
                pend_n += 1;
                detail.push(format!(
                    "    PENDING  {op}  {} sats  witness={witness_txid}",
                    u.value_sats
                ));
            }
            BtcInventoryStatus::Spent { .. } => {
                spent_s += u.value_sats;
                spent_n += 1;
            }
            BtcInventoryStatus::Invalid { reason } => {
                inval_s += u.value_sats;
                inval_n += 1;
                detail.push(format!(
                    "    INVALID  {op}  {} sats  reason={reason}",
                    u.value_sats
                ));
            }
        }
    }
    println!("    Available:  {avail_n} utxos · {avail_s} sats   <-- selectable for funding");
    println!("    Reserved:   {resv_n} utxos · {resv_s} sats");
    println!("    Pending:    {pend_n} utxos · {pend_s} sats");
    println!("    Spent:      {spent_n} utxos · {spent_s} sats");
    println!("    Invalid:    {inval_n} utxos · {inval_s} sats");
    for line in &detail {
        println!("{line}");
    }

    // --- Drift summary: localize the fault ---
    println!();
    output::kv(
        "DRIFT — on-chain BTC-only",
        &format!("{} sats", filtered.unwrap_or(k0_sats)),
    );
    output::kv("DRIFT — SQLite Available", &format!("{avail_s} sats"));
    let chain_fundable = filtered.unwrap_or(k0_sats);
    if chain_fundable == 0 && k0_sats == 0 {
        output::note(
            "=> chain k0 empty: DEPLETION or sync failure (check `maker addresses --electrum`)",
        );
    } else if chain_fundable == 0 && k0_sats > 0 {
        output::note("=> k0 has BTC but filter returns none: RGB-FILTER over-inclusion");
    } else if avail_s == 0 && chain_fundable > 0 {
        output::note(
            "=> fundable on-chain but SQLite Available empty: RESERVATION LEAK / INGEST GAP",
        );
    } else if avail_s < chain_fundable {
        output::note("=> SQLite Available < on-chain fundable: partial ingest/reservation drift");
    } else {
        output::note("=> layers agree: inventory looks healthy");
    }

    // --- RGB allocations: stock vs wallet vs CHAIN ---
    // The stock's `FilterIncludeAll` lists every allocation it has ever seen,
    // INCLUDING spent ones — so "not in wallet" alone can't tell stranded from
    // spent. We probe the chain per outpoint to split them:
    //   - SPENT on-chain          → history, ignore
    //   - UNSPENT + in wallet      → sellable (should be Available inventory)
    //   - UNSPENT + NOT in wallet  → TRULY STRANDED (the recoverable bug)
    for asset in registered_assets(config).await {
        println!();
        output::kv("contract", &asset.id);
        let allocs = backend.debug_contract_allocations(&asset).await?;
        output::kv(
            "RGB stock allocations (incl. spent)",
            &format!("{}", allocs.len()),
        );

        let probe = match &electrum_url {
            Some(url) => rfq_btc::ElectrumClient::connect(url).ok(),
            None => None,
        };
        if probe.is_none() {
            output::step_warn(
                "no electrum — cannot probe on-chain spent status; showing wallet view only",
            );
        }

        let (mut sellable_amt, mut sellable_n) = (0u64, 0usize);
        let (mut stranded_amt, mut stranded_n) = (0u64, 0usize);
        let (mut spent_n, mut unknown_n) = (0usize, 0usize);
        for (op, amount, in_wallet) in &allocs {
            let unspent = match &probe {
                Some(c) => c.outpoint_unspent(op).await.ok(),
                None => None,
            };
            let tag = match (unspent, in_wallet) {
                (Some(true), true) => {
                    sellable_amt += amount;
                    sellable_n += 1;
                    "UNSPENT · in-wallet (sellable)"
                }
                (Some(true), false) => {
                    stranded_amt += amount;
                    stranded_n += 1;
                    "UNSPENT · NOT in wallet — STRANDED (recoverable)"
                }
                (Some(false), _) => {
                    spent_n += 1;
                    continue; // spent history — don't print the noise
                }
                (None, true) => "in-wallet (chain unknown)",
                (None, false) => {
                    unknown_n += 1;
                    "NOT in wallet (chain unknown)"
                }
            };
            println!("    {}:{}  {} units  {tag}", op.txid, op.vout, amount);
        }

        output::kv(
            "  sellable (unspent, in-wallet)",
            &format!("{sellable_n} · {sellable_amt} units"),
        );
        output::kv(
            "  STRANDED (unspent, off-wallet)",
            &format!("{stranded_n} · {stranded_amt} units"),
        );
        output::kv("  spent (history, hidden)", &format!("{spent_n}"));
        if unknown_n > 0 {
            output::kv("  chain-unknown (no probe)", &format!("{unknown_n}"));
        }
        if probe.is_some() {
            if stranded_amt > 0 {
                output::note(&format!(
                    "=> RGB: {stranded_amt} units STRANDED on tapret outputs bp-wallet never tracked — RECOVERABLE (index-reuse recognition bug)"
                ));
            } else if sellable_amt > 0 {
                output::note("=> RGB: sellable allocation present but store shows 0 available — STORE/INGEST stale (re-sync/reconcile)");
            } else {
                output::note("=> RGB: no unspent allocation on-chain — genuine DEPLETION (re-fund the maker)");
            }
        }
    }

    Ok(())
}

pub(crate) fn parse_broker_url(
    config: &MakerNodeConfig,
) -> Result<Url, Box<dyn std::error::Error>> {
    Url::parse(&config.maker.broker_url).map_err(|e| e.into())
}

pub(crate) fn print_inventory_snapshot(snapshot: &InventorySnapshot, spec: Option<&(String, u8)>) {
    let amt = |v: u64| match spec {
        Some((ticker, precision)) => {
            format!("{ticker} {}", rfq_types::format_amount(v, *precision))
        }
        None => v.to_string(),
    };
    // `total_*` is current controllable inventory (available + reserved +
    // pending); spent allocations are excluded upstream. `spent_amount` is NOT
    // printed: in a UTXO change-chain it sums superseded links and balloons to a
    // phantom multiple of real holdings — `spent_allocations` (a count of
    // settled-away allocations) is the only spent diagnostic worth surfacing.
    println!("total_amount={}", amt(snapshot.total_amount));
    println!("available_amount={}", amt(snapshot.available_amount));
    println!("reserved_amount={}", amt(snapshot.reserved_amount));
    println!("total_allocations={}", snapshot.total_allocations);
    println!("available_allocations={}", snapshot.available_allocations);
    println!("reserved_allocations={}", snapshot.reserved_allocations);
    println!("spent_allocations={}", snapshot.spent_allocations);
}
