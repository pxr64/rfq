//! Unilateral self-send compositions: the RGB recovery sweep + the rebalance
//! splitters (asset-ladder, BTC-ladder, combined multi-contract). Split out of
//! the parent `swap` module, which keeps the two-party buy/sell composition and
//! the shared PSBT/transition helpers these reuse via `super::*`.

use super::*;

pub struct RecoveryInput {
    pub outpoint: Outpoint,
    pub value_sats: u64,
    pub script_pubkey: Vec<u8>,
}

/// Resolve the descriptor [`Terminal`] that produces `spk`, scanning plain +
/// tweaked derivations across the descriptor's OWN keychains (read from
/// `keychains()` — e.g. `0;1;10` — not a hardcoded guess) up to `INDEX_SCAN`.
/// Lets the sweep enrich + sign a stranded output the UTXO cache never tracked.
/// `None` when no terminal matches — which also means "not this wallet's output"
/// (e.g. a counterparty allocation the stock has merely seen), so the sweep
/// skips it rather than trying to sign someone else's coin.
fn resolve_terminal_for_spk(descriptor: &RgbDescr, spk: &ScriptPubkey) -> Option<Terminal> {
    const INDEX_SCAN: u16 = 1000;
    for keychain in descriptor.keychains() {
        for index in 0..INDEX_SCAN {
            let terminal = Terminal::new(keychain, NormalIndex::from(index));
            if descriptor
                .derive(terminal.keychain, terminal.index)
                .any(|d| d.to_script_pubkey() == *spk)
            {
                return Some(terminal);
            }
        }
    }
    None
}

/// Build, sign, and finalize a unilateral RECOVERY sweep: spend the maker's
/// `stranded` RGB outputs (which bp-wallet doesn't track, so the normal
/// wallet-selection path can't reach them) plus one BTC `fee_input`, into a
/// single fresh tapret host output at the PINNED index — re-homing all the RGB
/// onto a UTXO the wallet will recognise. Returns the raw witness tx + txid for
/// the caller to broadcast.
///
/// Spending the stranded tweaked outputs is mechanically identical to a normal
/// swap's RGB inputs (`enrich_psbt_input` matches the on-chain spk to the
/// tweaked derived key, so the signer uses the right key); the only difference
/// is the inputs are sourced from the chain + stock, not the UTXO cache.
pub(crate) fn build_recovery_sweep(
    wallet: &mut MakerWallet,
    account: &XprivAccount,
    contract_id: ContractId,
    stranded: &[RecoveryInput],
    fee_input: &RecoveryInput,
    fee_sats: u64,
) -> Result<(Vec<u8>, Txid, Vec<Outpoint>), RgbError> {
    if stranded.is_empty() {
        return Err(RgbError::TransferBuild(
            "recovery sweep needs at least one stranded RGB input".to_owned(),
        ));
    }
    let descriptor = wallet.wallet().descriptor().clone();

    let resolve = |inp: &RecoveryInput| -> Option<MakerRgbInput> {
        let spk = ScriptPubkey::from_unsafe(inp.script_pubkey.clone());
        let terminal = resolve_terminal_for_spk(&descriptor, &spk)?;
        Some(MakerRgbInput {
            outpoint: to_bp_outpoint(&inp.outpoint).ok()?,
            value: Sats(inp.value_sats),
            terminal,
            script: spk,
        })
    };

    // Keep only the stranded outputs THIS wallet can sign. Unresolvable ones are
    // not ours (FilterIncludeAll surfaces counterparty allocations the stock has
    // seen) — skip them rather than abort the whole sweep.
    let mut rgb_inputs: Vec<MakerRgbInput> = Vec::new();
    let mut swept: Vec<Outpoint> = Vec::new();
    for inp in stranded {
        match resolve(inp) {
            Some(mi) => {
                rgb_inputs.push(mi);
                swept.push(inp.outpoint.clone());
            }
            None => continue,
        }
    }
    if rgb_inputs.is_empty() {
        return Err(RgbError::TransferBuild(
            "none of the stranded outputs are spendable by this wallet \
             (no descriptor terminal derives them — not ours)"
                .to_owned(),
        ));
    }
    let btc_input = resolve(fee_input).ok_or_else(|| {
        RgbError::TransferBuild(format!(
            "fee input {}:{} is not derivable by this wallet",
            fee_input.outpoint.txid, fee_input.outpoint.vout
        ))
    })?;

    // The pinned host both carries the RGB seal anchor and hosts the tapret
    // commitment — the whole point of the sweep is to land the RGB here.
    let (host_spk, host_terminal) = pinned_tapret_host_addr(wallet)?;
    let maker_change_spk = wallet
        .wallet_mut()
        .next_address(Keychain::OUTER, true)
        .script_pubkey();

    // BTC math: every input's sats fund a small seal anchor on the host + the
    // fee, with the surplus returned to a maker change output.
    let total_in: u64 = rgb_inputs
        .iter()
        .map(|i| i.value.into_inner())
        .sum::<u64>()
        .saturating_add(btc_input.value.into_inner());
    let host_anchor = SEAL_ANCHOR_SATS;
    if total_in < host_anchor.saturating_add(fee_sats) {
        return Err(RgbError::TransferBuild(format!(
            "recovery inputs {total_in} sats < anchor {host_anchor} + fee {fee_sats}"
        )));
    }
    let change = total_in - host_anchor - fee_sats;

    // Assemble the unsigned tx: RGB inputs, then the BTC fee input; host output
    // first (pre-sort), maker BTC change if above dust.
    let seq = SeqNo::from_consensus_u32(SEQ_FINAL);
    let tx_inputs: Vec<UnsignedTxIn> = rgb_inputs
        .iter()
        .chain(std::iter::once(&btc_input))
        .map(|i| UnsignedTxIn {
            prev_output: i.outpoint,
            sequence: seq,
        })
        .collect();
    let mut tx_outputs = vec![BpTxOut::new(host_spk.clone(), Sats(host_anchor))];
    if change > DUST_LIMIT_SATS {
        tx_outputs.push(BpTxOut::new(maker_change_spk, Sats(change)));
    }
    let unsigned_tx = UnsignedTx {
        version: TxVer::V2,
        inputs: VarIntArray::from_iter_checked(tx_inputs),
        outputs: VarIntArray::from_iter_checked(tx_outputs),
        lock_time: LockTime::ZERO,
    };
    let mut psbt = Psbt::from_tx(unsigned_tx);

    // Enrich every input (all the maker's) so the signer recognises them.
    for (i, mi) in rgb_inputs
        .iter()
        .chain(std::iter::once(&btc_input))
        .enumerate()
    {
        enrich_psbt_input(
            &mut psbt,
            i,
            &descriptor,
            mi.terminal,
            mi.value,
            Some(&mi.script),
        )?;
    }

    psbt.set_rgb_close_method(CloseMethod::TapretFirst);
    psbt.set_rgb_tapret_host_on_change();
    enrich_tapret_host(&mut psbt, &host_spk, &descriptor, host_terminal)?;
    psbt.sort_outputs_by(|o| !o.is_tapret_host())
        .expect("PSBT outputs are modifiable");
    psbt.complete_construction();

    // Build the transition: consume all stranded allocations, assign the full
    // sum to one change seal bound to the host's post-sort vout.
    let (batch, change_seal) =
        build_recovery_transition(wallet, contract_id, &rgb_inputs, &psbt, &host_spk)?;

    // Commit (persists the tweak at the pinned terminal + records the new
    // allocation in the stock), then sign every input and finalize.
    commit_and_consign(
        wallet,
        &mut psbt,
        batch,
        contract_id,
        DeliverySeal::WitnessVout(change_seal),
    )?;
    sign_maker_inputs(&mut psbt, account)?;
    let (raw_tx, txid) = finalize_psbt_in_place(&mut psbt, &descriptor)?;
    Ok((raw_tx, txid, swept))
}

/// The recovery transition: consume every stranded allocation and re-assign the
/// entire sum to one change seal bound to the pinned host output's post-sort
/// vout. The unilateral analogue of [`build_buy_transition`] — no counterparty.
fn build_recovery_transition(
    wallet: &MakerWallet,
    contract_id: ContractId,
    rgb_inputs: &[MakerRgbInput],
    psbt: &Psbt,
    host_spk: &ScriptPubkey,
) -> Result<(Batch, GraphSeal), RgbError> {
    let host_vout = psbt
        .outputs()
        .find(|o| o.script == *host_spk)
        .map(|o| o.vout())
        .ok_or_else(|| RgbError::TransferBuild("host output missing after sort".to_owned()))?;

    let contract = wallet
        .stock()
        .contract_data(contract_id)
        .map_err(|e| RgbError::ContractNotFound(e.to_string()))?;
    let assignment_type = contract
        .schema
        .assignment_types_for_state(StateType::Fungible)
        .first()
        .map(|t| **t)
        .ok_or_else(|| RgbError::TransferBuild("contract has no fungible assignment".to_owned()))?;
    let transition_type = contract
        .schema
        .default_transition_for_assignment(&assignment_type);

    let mut builder = wallet
        .stock()
        .transition_builder_raw(contract_id, transition_type)
        .map_err(|e| RgbError::TransferBuild(format!("transition builder: {e}")))?;

    let bp_outpoints: Vec<BpOutpoint> = rgb_inputs.iter().map(|mi| mi.outpoint).collect();
    let assignments = wallet
        .stock()
        .contract_assignments_for(contract_id, bp_outpoints)
        .map_err(|e| RgbError::TransferBuild(format!("contract assignments: {e}")))?;
    let mut sum_inputs: u64 = 0;
    for (_seal, opouts) in assignments {
        for (opout, state) in opouts {
            builder = builder
                .add_input(opout, state.clone())
                .map_err(|e| RgbError::TransferBuild(format!("add_input: {e}")))?;
            if let AllocatedState::Amount(value) = state {
                sum_inputs = sum_inputs.saturating_add(value.as_inner().as_u64());
            }
        }
    }
    if sum_inputs == 0 {
        return Err(RgbError::TransferBuild(
            "no RGB allocations found for the stranded outpoints".to_owned(),
        ));
    }

    let change_seal = GraphSeal::with_blinded_vout(host_vout, rand::random());
    builder = builder
        .add_fungible_state_raw(
            assignment_type,
            BuilderSeal::Revealed(change_seal),
            Amount::from(sum_inputs),
        )
        .map_err(|e| RgbError::TransferBuild(format!("add recovery state: {e}")))?;

    let main = builder
        .complete_transition()
        .map_err(|e| RgbError::TransferBuild(format!("complete_transition: {e}")))?;
    let mut batch = Batch {
        main,
        extras: Default::default(),
    };
    batch.set_priority(u64::MAX);
    Ok((batch, change_seal))
}

/// Persist the tapret tweak `rgb_commit` wrote onto the host output, so the
/// maker's wallet still recognises the (now tweaked) host as its own. Factored

pub(crate) fn build_asset_split(
    wallet: &mut MakerWallet,
    account: &XprivAccount,
    contract_id: ContractId,
    rgb_source: &RecoveryInput,
    fee_input: &RecoveryInput,
    rungs: &[u64],
    fee_sats: u64,
) -> Result<(Vec<u8>, Txid), RgbError> {
    if rungs.is_empty() {
        return Err(RgbError::TransferBuild(
            "asset split needs at least one rung".to_owned(),
        ));
    }
    let descriptor = wallet.wallet().descriptor().clone();

    let resolve = |inp: &RecoveryInput| -> Option<MakerRgbInput> {
        let spk = ScriptPubkey::from_unsafe(inp.script_pubkey.clone());
        let terminal = resolve_terminal_for_spk(&descriptor, &spk)?;
        Some(MakerRgbInput {
            outpoint: to_bp_outpoint(&inp.outpoint).ok()?,
            value: Sats(inp.value_sats),
            terminal,
            script: spk,
        })
    };
    let rgb_in = resolve(rgb_source).ok_or_else(|| {
        RgbError::TransferBuild(format!(
            "rgb source {}:{} is not derivable by this wallet",
            rgb_source.outpoint.txid, rgb_source.outpoint.vout
        ))
    })?;
    let btc_in = resolve(fee_input).ok_or_else(|| {
        RgbError::TransferBuild(format!(
            "fee input {}:{} is not derivable by this wallet",
            fee_input.outpoint.txid, fee_input.outpoint.vout
        ))
    })?;

    // Pinned host carries the remainder + the tapret commitment; each rung gets a
    // fresh OUTER (k0) address so a plain UTXO rescan finds it.
    let (host_spk, host_terminal) = pinned_tapret_host_addr(wallet)?;
    let rung_spks: Vec<ScriptPubkey> = (0..rungs.len())
        .map(|_| {
            wallet
                .wallet_mut()
                .next_address(Keychain::OUTER, true)
                .script_pubkey()
        })
        .collect();
    let btc_change_spk = wallet
        .wallet_mut()
        .next_address(Keychain::OUTER, true)
        .script_pubkey();

    // BTC math: host anchor + one anchor per rung + fee, surplus to BTC change.
    let total_in = rgb_in
        .value
        .into_inner()
        .saturating_add(btc_in.value.into_inner());
    let anchor = SEAL_ANCHOR_SATS;
    let anchors_total = anchor.saturating_mul(1 + rungs.len() as u64);
    let needed = anchors_total.saturating_add(fee_sats);
    if total_in < needed {
        return Err(RgbError::TransferBuild(format!(
            "split inputs {total_in} sats < {} rung anchors + host anchor + fee ({needed})",
            rungs.len()
        )));
    }
    let btc_change = total_in - needed;

    // Inputs: RGB source, then BTC fee input. Outputs (pre-sort): host, then one
    // per rung, then BTC change if above dust.
    let seq = SeqNo::from_consensus_u32(SEQ_FINAL);
    let tx_inputs: Vec<UnsignedTxIn> = [&rgb_in, &btc_in]
        .iter()
        .map(|i| UnsignedTxIn {
            prev_output: i.outpoint,
            sequence: seq,
        })
        .collect();
    let mut tx_outputs = vec![BpTxOut::new(host_spk.clone(), Sats(anchor))];
    for spk in &rung_spks {
        tx_outputs.push(BpTxOut::new(spk.clone(), Sats(anchor)));
    }
    if btc_change > DUST_LIMIT_SATS {
        tx_outputs.push(BpTxOut::new(btc_change_spk, Sats(btc_change)));
    }
    let unsigned_tx = UnsignedTx {
        version: TxVer::V2,
        inputs: VarIntArray::from_iter_checked(tx_inputs),
        outputs: VarIntArray::from_iter_checked(tx_outputs),
        lock_time: LockTime::ZERO,
    };
    let mut psbt = Psbt::from_tx(unsigned_tx);

    for (i, mi) in [&rgb_in, &btc_in].iter().enumerate() {
        enrich_psbt_input(
            &mut psbt,
            i,
            &descriptor,
            mi.terminal,
            mi.value,
            Some(&mi.script),
        )?;
    }

    psbt.set_rgb_close_method(CloseMethod::TapretFirst);
    psbt.set_rgb_tapret_host_on_change();
    enrich_tapret_host(&mut psbt, &host_spk, &descriptor, host_terminal)?;
    psbt.sort_outputs_by(|o| !o.is_tapret_host())
        .expect("PSBT outputs are modifiable");
    psbt.complete_construction();

    // Build the transition: consume the source allocation, assign each rung to its
    // (post-sort) vout seal, remainder to the host vout seal.
    let main = build_split_transition(
        wallet,
        contract_id,
        &rgb_in,
        &psbt,
        &host_spk,
        &rung_spks,
        rungs,
    )?;
    let mut batch = Batch {
        main,
        extras: Default::default(),
    };
    batch.set_priority(u64::MAX);

    // Commit (rewrites the host taproot key + records the new allocations in the
    // stock via `consume_fascia` — no `stock.transfer` needed for a self-send),
    // persist the tweak, then sign + finalize.
    psbt.rgb_embed(batch)
        .map_err(|e| RgbError::TransferBuild(format!("rgb_embed: {e}")))?;
    let fascia = psbt
        .rgb_commit()
        .map_err(|e| RgbError::TransferBuild(format!("rgb_commit: {e}")))?;
    let witness_id = psbt.txid();
    persist_tapret_tweak(wallet, &mut psbt);
    wallet
        .stock_mut()
        .consume_fascia(fascia, FasciaResolver { witness_id })
        .map_err(|e| RgbError::TransferBuild(format!("consume_fascia: {e}")))?;

    sign_maker_inputs(&mut psbt, account)?;
    let (raw_tx, txid) = finalize_psbt_in_place(&mut psbt, &descriptor)?;
    Ok((raw_tx, txid))
}

/// The split transition for ONE contract: consume the source allocation and
/// re-assign it across one seal per rung (bound to each rung output's post-sort
/// vout) plus a remainder seal on the host vout. Returns the bare `Transition` so
/// callers can either wrap it in a single-contract [`Batch`] or bundle several
/// contracts' transitions into one batch (the multi-asset rebalance). Sibling of
/// [`build_recovery_transition`].
fn build_split_transition(
    wallet: &MakerWallet,
    contract_id: ContractId,
    rgb_source: &MakerRgbInput,
    psbt: &Psbt,
    host_spk: &ScriptPubkey,
    rung_spks: &[ScriptPubkey],
    rungs: &[u64],
) -> Result<rgb::Transition, RgbError> {
    let vout_of = |spk: &ScriptPubkey| {
        psbt.outputs()
            .find(|o| o.script == *spk)
            .map(|o| o.vout())
            .ok_or_else(|| RgbError::TransferBuild("split output missing after sort".to_owned()))
    };
    let host_vout = vout_of(host_spk)?;

    let contract = wallet
        .stock()
        .contract_data(contract_id)
        .map_err(|e| RgbError::ContractNotFound(e.to_string()))?;
    let assignment_type = contract
        .schema
        .assignment_types_for_state(StateType::Fungible)
        .first()
        .map(|t| **t)
        .ok_or_else(|| RgbError::TransferBuild("contract has no fungible assignment".to_owned()))?;
    let transition_type = contract
        .schema
        .default_transition_for_assignment(&assignment_type);

    let mut builder = wallet
        .stock()
        .transition_builder_raw(contract_id, transition_type)
        .map_err(|e| RgbError::TransferBuild(format!("transition builder: {e}")))?;

    let assignments = wallet
        .stock()
        .contract_assignments_for(contract_id, vec![rgb_source.outpoint])
        .map_err(|e| RgbError::TransferBuild(format!("contract assignments: {e}")))?;
    let mut sum_inputs: u64 = 0;
    for (_seal, opouts) in assignments {
        for (opout, state) in opouts {
            builder = builder
                .add_input(opout, state.clone())
                .map_err(|e| RgbError::TransferBuild(format!("add_input: {e}")))?;
            if let AllocatedState::Amount(value) = state {
                sum_inputs = sum_inputs.saturating_add(value.as_inner().as_u64());
            }
        }
    }
    let rungs_total: u64 = rungs.iter().sum();
    if sum_inputs <= rungs_total {
        return Err(RgbError::TransferBuild(format!(
            "source RGB {sum_inputs} must exceed rung total {rungs_total} (remainder funds the host)"
        )));
    }

    // One seal per rung, bound to its post-sort vout.
    for (spk, amount) in rung_spks.iter().zip(rungs) {
        let vout = vout_of(spk)?;
        let seal = GraphSeal::with_blinded_vout(vout, rand::random());
        builder = builder
            .add_fungible_state_raw(
                assignment_type,
                BuilderSeal::Revealed(seal),
                Amount::from(*amount),
            )
            .map_err(|e| RgbError::TransferBuild(format!("add rung state: {e}")))?;
    }
    // Remainder on the host.
    let remainder_seal = GraphSeal::with_blinded_vout(host_vout, rand::random());
    builder = builder
        .add_fungible_state_raw(
            assignment_type,
            BuilderSeal::Revealed(remainder_seal),
            Amount::from(sum_inputs - rungs_total),
        )
        .map_err(|e| RgbError::TransferBuild(format!("add remainder state: {e}")))?;

    builder
        .complete_transition()
        .map_err(|e| RgbError::TransferBuild(format!("complete_transition: {e}")))
}

/// One contract's slice of a combined rebalance pass: the fat source UTXO to
/// split (a wallet-tracked Available allocation) and the RGB rung amounts to
/// carve, largest→smallest. The remainder (`source − Σrungs`) re-lands on the
/// shared tapret host.
pub(crate) struct AssetSplitInput {
    pub contract_id: ContractId,
    pub source: Outpoint,
    pub rungs: Vec<u64>,
}

/// Smallest BTC change a rebalance tx emits as its own UTXO; a smaller surplus is
/// folded into the fee so a split never births a sub-floor BTC piece. Mirrors
/// `rfq_maker::DEFAULT_BTC_MIN_PIECE_SATS`.
const REBALANCE_BTC_CHANGE_FLOOR_SATS: u64 = 1000;

/// Build, sign and finalize ONE unilateral transaction that splits EVERY listed
/// RGB asset into its ladder rungs AND (optionally) the BTC pool into its rungs,
/// all anchored under a single tapret commitment with one fee. Each asset's rungs
/// land on fresh keychain-0 outputs (rescannable); every asset's remainder and
/// the MPC commitment ride the single pinned k10/0 host. BTC rungs are plain
/// keychain-0 outputs carved from `btc_source`; surplus returns as change, or
/// folds into the fee when below the BTC floor. Requires ≥1 asset (the pure-BTC
/// case has no RGB transition — see [`build_btc_split`]). All inputs are the
/// maker's, resolved from the wallet's own coin cache. Returns raw tx + txid.
#[allow(clippy::too_many_arguments)]
pub(crate) fn build_rebalance_tx(
    wallet: &mut MakerWallet,
    account: &XprivAccount,
    assets: &[AssetSplitInput],
    btc_source: Option<&Outpoint>,
    btc_rungs: &[u64],
    fee_sats: u64,
) -> Result<(Vec<u8>, Txid), RgbError> {
    if assets.is_empty() {
        return Err(RgbError::TransferBuild(
            "build_rebalance_tx needs at least one RGB asset; use build_btc_split for BTC-only"
                .to_owned(),
        ));
    }
    let descriptor = wallet.wallet().descriptor().clone();

    // Resolve every source from the wallet's coin cache (value + terminal + real
    // spk, so tweaked hosts enrich correctly). One MakerRgbInput per asset.
    let mut rgb_ins: Vec<MakerRgbInput> = Vec::with_capacity(assets.len());
    for a in assets {
        rgb_ins.push(
            resolve_maker_inputs(wallet, std::slice::from_ref(&a.source))?
                .pop()
                .expect("resolve_maker_inputs yields one per outpoint"),
        );
    }
    let btc_in = match btc_source {
        Some(op) => Some(
            resolve_maker_inputs(wallet, std::slice::from_ref(op))?
                .pop()
                .expect("one"),
        ),
        None => None,
    };
    let btc_rungs: &[u64] = if btc_in.is_some() { btc_rungs } else { &[] };

    // Host (k10/0) carries the MPC commitment + every asset's remainder. Fresh k0
    // spks for each asset's rungs, then the BTC rungs, then BTC change.
    let (host_spk, host_terminal) = pinned_tapret_host_addr(wallet)?;
    let mut asset_rung_spks: Vec<Vec<ScriptPubkey>> = Vec::with_capacity(assets.len());
    for a in assets {
        let spks = (0..a.rungs.len())
            .map(|_| {
                wallet
                    .wallet_mut()
                    .next_address(Keychain::OUTER, true)
                    .script_pubkey()
            })
            .collect();
        asset_rung_spks.push(spks);
    }
    let btc_rung_spks: Vec<ScriptPubkey> = (0..btc_rungs.len())
        .map(|_| {
            wallet
                .wallet_mut()
                .next_address(Keychain::OUTER, true)
                .script_pubkey()
        })
        .collect();
    let btc_change_spk = wallet
        .wallet_mut()
        .next_address(Keychain::OUTER, true)
        .script_pubkey();

    // BTC math: host anchor + one anchor per RGB rung + the BTC rungs + fee;
    // surplus is change (or folded to fee below the floor).
    let total_in: u64 = rgb_ins.iter().map(|i| i.value.into_inner()).sum::<u64>()
        + btc_in.as_ref().map(|i| i.value.into_inner()).unwrap_or(0);
    let anchor = SEAL_ANCHOR_SATS;
    let rgb_rung_count: u64 = assets.iter().map(|a| a.rungs.len() as u64).sum();
    let anchors_total = anchor.saturating_mul(1 + rgb_rung_count);
    let btc_rung_total: u64 = btc_rungs.iter().sum();
    let spent = anchors_total
        .saturating_add(btc_rung_total)
        .saturating_add(fee_sats);
    if total_in < spent {
        return Err(RgbError::TransferBuild(format!(
            "rebalance inputs {total_in} sats < required {spent} \
             (anchors {anchors_total} + btc rungs {btc_rung_total} + fee {fee_sats})"
        )));
    }
    let change = total_in - spent;

    // Inputs: RGB sources first, then the BTC source.
    let seq = SeqNo::from_consensus_u32(SEQ_FINAL);
    let mut tx_inputs: Vec<UnsignedTxIn> = rgb_ins
        .iter()
        .map(|i| UnsignedTxIn {
            prev_output: i.outpoint,
            sequence: seq,
        })
        .collect();
    if let Some(b) = &btc_in {
        tx_inputs.push(UnsignedTxIn {
            prev_output: b.outpoint,
            sequence: seq,
        });
    }

    // Outputs (pre-sort): host, asset rungs, btc rungs, btc change if ≥ floor.
    let mut tx_outputs = vec![BpTxOut::new(host_spk.clone(), Sats(anchor))];
    for spks in &asset_rung_spks {
        for spk in spks {
            tx_outputs.push(BpTxOut::new(spk.clone(), Sats(anchor)));
        }
    }
    for (spk, amount) in btc_rung_spks.iter().zip(btc_rungs) {
        tx_outputs.push(BpTxOut::new(spk.clone(), Sats(*amount)));
    }
    if change >= REBALANCE_BTC_CHANGE_FLOOR_SATS {
        tx_outputs.push(BpTxOut::new(btc_change_spk, Sats(change)));
    }

    let unsigned_tx = UnsignedTx {
        version: TxVer::V2,
        inputs: VarIntArray::from_iter_checked(tx_inputs),
        outputs: VarIntArray::from_iter_checked(tx_outputs),
        lock_time: LockTime::ZERO,
    };
    let mut psbt = Psbt::from_tx(unsigned_tx);

    let ordered_ins: Vec<&MakerRgbInput> = rgb_ins.iter().chain(btc_in.as_ref()).collect();
    for (i, mi) in ordered_ins.iter().enumerate() {
        enrich_psbt_input(
            &mut psbt,
            i,
            &descriptor,
            mi.terminal,
            mi.value,
            Some(&mi.script),
        )?;
    }

    psbt.set_rgb_close_method(CloseMethod::TapretFirst);
    psbt.set_rgb_tapret_host_on_change();
    enrich_tapret_host(&mut psbt, &host_spk, &descriptor, host_terminal)?;
    psbt.sort_outputs_by(|o| !o.is_tapret_host())
        .expect("PSBT outputs are modifiable");
    psbt.complete_construction();

    // One transition per asset, bundled into a single batch — the MPC commits
    // every contract under the one host.
    let mut transitions = Vec::with_capacity(assets.len());
    for (a, (rgb_in, rung_spks)) in assets.iter().zip(rgb_ins.iter().zip(&asset_rung_spks)) {
        transitions.push(build_split_transition(
            wallet,
            a.contract_id,
            rgb_in,
            &psbt,
            &host_spk,
            rung_spks,
            &a.rungs,
        )?);
    }
    let mut iter = transitions.into_iter();
    let main = iter.next().expect("≥1 asset ⇒ ≥1 transition");
    let mut extras = Confined::<Vec<_>, 0, { U24 - 1 }>::with_capacity(assets.len() - 1);
    for t in iter {
        extras
            .push(t)
            .map_err(|e| RgbError::TransferBuild(format!("bundle extras: {e}")))?;
    }
    let mut batch = Batch { main, extras };
    batch.set_priority(u64::MAX);

    // Commit one tapret host for every contract, persist the tweak, record the
    // new allocations (no `stock.transfer` — self-send), sign, finalize.
    psbt.rgb_embed(batch)
        .map_err(|e| RgbError::TransferBuild(format!("rgb_embed: {e}")))?;
    let fascia = psbt
        .rgb_commit()
        .map_err(|e| RgbError::TransferBuild(format!("rgb_commit: {e}")))?;
    let witness_id = psbt.txid();
    persist_tapret_tweak(wallet, &mut psbt);
    wallet
        .stock_mut()
        .consume_fascia(fascia, FasciaResolver { witness_id })
        .map_err(|e| RgbError::TransferBuild(format!("consume_fascia: {e}")))?;

    sign_maker_inputs(&mut psbt, account)?;
    let (raw_tx, txid) = finalize_psbt_in_place(&mut psbt, &descriptor)?;
    Ok((raw_tx, txid))
}

/// Plain BTC self-send that splits `btc_source` into `rungs` fresh keychain-0
/// outputs (largest→smallest); surplus returns as change, or folds into the fee
/// below the BTC floor. No RGB — used when only the BTC pool needs rebalancing.
/// Returns raw tx + txid.
pub(crate) fn build_btc_split(
    wallet: &mut MakerWallet,
    account: &XprivAccount,
    btc_source: &Outpoint,
    rungs: &[u64],
    fee_sats: u64,
) -> Result<(Vec<u8>, Txid), RgbError> {
    if rungs.is_empty() {
        return Err(RgbError::TransferBuild(
            "btc split needs at least one rung".to_owned(),
        ));
    }
    let descriptor = wallet.wallet().descriptor().clone();
    let src = resolve_maker_inputs(wallet, std::slice::from_ref(btc_source))?
        .pop()
        .expect("one");
    let rung_spks: Vec<ScriptPubkey> = (0..rungs.len())
        .map(|_| {
            wallet
                .wallet_mut()
                .next_address(Keychain::OUTER, true)
                .script_pubkey()
        })
        .collect();
    let change_spk = wallet
        .wallet_mut()
        .next_address(Keychain::OUTER, true)
        .script_pubkey();

    let total_in = src.value.into_inner();
    let rung_total: u64 = rungs.iter().sum();
    let spent = rung_total.saturating_add(fee_sats);
    if total_in < spent {
        return Err(RgbError::TransferBuild(format!(
            "btc split input {total_in} sats < rungs {rung_total} + fee {fee_sats}"
        )));
    }
    let change = total_in - spent;

    let seq = SeqNo::from_consensus_u32(SEQ_FINAL);
    let tx_inputs = vec![UnsignedTxIn {
        prev_output: src.outpoint,
        sequence: seq,
    }];
    let mut tx_outputs: Vec<BpTxOut> = rung_spks
        .iter()
        .zip(rungs)
        .map(|(spk, amt)| BpTxOut::new(spk.clone(), Sats(*amt)))
        .collect();
    if change >= REBALANCE_BTC_CHANGE_FLOOR_SATS {
        tx_outputs.push(BpTxOut::new(change_spk, Sats(change)));
    }
    let unsigned_tx = UnsignedTx {
        version: TxVer::V2,
        inputs: VarIntArray::from_iter_checked(tx_inputs),
        outputs: VarIntArray::from_iter_checked(tx_outputs),
        lock_time: LockTime::ZERO,
    };
    let mut psbt = Psbt::from_tx(unsigned_tx);
    enrich_psbt_input(
        &mut psbt,
        0,
        &descriptor,
        src.terminal,
        src.value,
        Some(&src.script),
    )?;
    psbt.complete_construction();
    sign_maker_inputs(&mut psbt, account)?;
    let (raw_tx, txid) = finalize_psbt_in_place(&mut psbt, &descriptor)?;
    Ok((raw_tx, txid))
}
