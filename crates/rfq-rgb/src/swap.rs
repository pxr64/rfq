//! Two-party atomic-swap PSBT composition.
//!
//! This module is the implementation of the swap-PSBT trio that
//! [`LibRgbBackend`](crate::LibRgbBackend) exposes on the [`RgbBackend`] trait.
//! It composes a single bitcoin transaction that, in one atomic step, moves RGB
//! state one way and bitcoin the other.
//!
//! ## Why this lives below `pay()`
//!
//! `rgb-api`'s [`WalletProvider::pay`] builds a *unilateral* transfer: one
//! wallet supplies every input, pays every output, and signs the whole PSBT. An
//! atomic swap is two-party — the maker contributes RGB inputs, the taker
//! contributes BTC inputs, and each signs only its own. `pay()`'s
//! [`PsbtConstructor`] would try to fund the bitcoin side from the *maker's*
//! wallet, which is wrong here. So we reach below it and compose the same
//! primitives (`Psbt::from_tx`, the transition builder, `rgb_embed`/`rgb_commit`,
//! `stock.transfer`) by hand.
//!
//! ## Ordering invariants
//!
//! The composition mirrors `rgb-api`'s `construct_psbt_rgb`/`transfer` and the
//! decisions recorded in `docs/swap-psbt-design.md`:
//!
//! 1. **Build + sort the bitcoin tx first, then the RGB transition.** The
//!    maker's RGB-change seal binds to a *vout*, and the commitment-host sort
//!    can move outputs around — so the change vout is only known after the
//!    outputs are final.
//! 2. **Commit before signing (U4).** `rgb_commit` rewrites the host output's
//!    scriptPubkey (the tapret tweak on the maker's P2TR host output), which
//!    changes the txid. Signing must happen *after* commit so the signatures
//!    cover the final output set; the witness txid is stable from the moment
//!    `rgb_commit` returns.
//! 3. **`consume_fascia` accepts the not-yet-broadcast witness (U5)** via a
//!    [`FasciaResolver`] that reports [`WitnessOrd::Tentative`].
//!
//! [`RgbBackend`]: crate::RgbBackend
//! [`WalletProvider::pay`]: rgb::WalletProvider
//! [`PsbtConstructor`]: psrgbt::PsbtConstructor

use std::str::FromStr;

use amplify::Wrapper as _;
use base64::Engine as _;
use bpstd::psbt::{Psbt, PsbtVer, UnsignedTx, UnsignedTxIn};
use bpstd::seals::txout::CloseMethod;
use bpstd::signers::TestnetRefSigner;
use bpstd::{
    Address, ConsensusEncode, Derive, Descriptor, Keychain, LockTime, Outpoint as BpOutpoint, Sats,
    ScriptPubkey, SeqNo, SighashType, Terminal, TxOut as BpTxOut, TxVer, Txid, VarIntArray, Vout,
    XprivAccount, XpubDerivable,
};
use bpwallet::Wallet;
use psrgbt::{PsbtConstructor, RgbExt, RgbPsbt};
use rgb::containers::{Batch, BuilderSeal, FileContent};
use rgb::contract::AllocatedState;
use rgb::invoice::{Beneficiary, RgbInvoice};
use rgb::validation::{WitnessOrdProvider, WitnessResolverError};
use rgb::vm::WitnessOrd;
use rgb::{
    Amount, ContractId, ExposedSeal, GraphSeal, RgbDescr, RgbKeychain, RgbWallet, SecretSeal,
    StateType,
};

use rfq_types::{Outpoint, SwapTransfer};

use crate::{ConsignmentInfo, RgbError, TxOut};

/// The maker's RGB-enabled wallet: a `bp-wallet` wallet wrapped with its
/// `Stock`. This is what [`LibRgbBackend::load_wallet`](crate::LibRgbBackend)
/// hands the composition functions.
pub(crate) type MakerWallet = RgbWallet<Wallet<XpubDerivable, RgbDescr>>;

/// P2WPKH/P2WSH/P2TR dust threshold. Outputs at or below this are dropped
/// rather than created (they'd be unspendable / non-standard).
const DUST_LIMIT_SATS: u64 = 546;

/// BTC value parked on a witness-vout RGB output (the taker's "future seal").
/// Comfortably above the dust limit so the output is standard and, crucially,
/// still spendable later: `wallet.pay` funds a transfer only from the RGB
/// input's own sats, so this must cover a future transfer fee + change output.
/// The receiver recovers these sats when it spends the RGB-bearing UTXO.
const SEAL_ANCHOR_SATS: u64 = 2_000;

/// How the counterparty's incoming RGB binds. A [`Beneficiary::BlindedSeal`]
/// invoice rides on a pre-existing anchor UTXO (hidden from the sender); a
/// [`Beneficiary::WitnessVout`] "future seal" is an address the sender pays into
/// a fresh swap-tx output, binding the RGB to that output's post-sort vout — so
/// the receiver never needs a pre-funded anchor.
pub(crate) enum TakerSeal {
    Blinded(SecretSeal),
    Witness(ScriptPubkey),
}

/// Where [`commit_and_consign`] addresses the emitted consignment. A blinded
/// recipient is consigned via `secret_seals`; a witness-vout recipient via
/// `outputs` (the graph seal resolves to an `OutputSeal` once the witness id is
/// known).
pub(crate) enum DeliverySeal {
    Secret(SecretSeal),
    WitnessVout(GraphSeal),
}

/// `nSequence` for every swap input: final (no RBF, no relative timelock). The
/// swap either confirms as built or is abandoned; we don't fee-bump in place.
const SEQ_FINAL: u32 = 0xFFFF_FFFF;

/// Witness-ordering provider for [`Stock::consume_fascia`](rgb::persistence::Stock::consume_fascia)
/// on a not-yet-broadcast swap tx. Mirrors the inline resolver in rgb-api's
/// `pay.rs`: the fascia we just produced via `rgb_commit` is committed but not
/// yet on-chain, so it gets [`WitnessOrd::Tentative`]. The `assert_eq!` is
/// load-bearing — `consume_fascia` must only ever query our own witness id.
/// See `docs/swap-psbt-design.md` U5.
pub(crate) struct FasciaResolver {
    pub(crate) witness_id: Txid,
}

impl WitnessOrdProvider for FasciaResolver {
    fn witness_ord(&self, witness_id: Txid) -> Result<WitnessOrd, WitnessResolverError> {
        assert_eq!(witness_id, self.witness_id);
        Ok(WitnessOrd::Tentative)
    }
}

/// A maker RGB input resolved against the wallet's UTXO set: the bitcoin
/// outpoint, its sat value, and the descriptor terminal needed to re-derive its
/// signing data.
struct MakerRgbInput {
    outpoint: BpOutpoint,
    value: Sats,
    terminal: Terminal,
}

/// Convert an [`rfq_types::Outpoint`] (string txid + vout) into a bp-std
/// [`Outpoint`](BpOutpoint).
fn to_bp_outpoint(outpoint: &Outpoint) -> Result<BpOutpoint, RgbError> {
    let txid = Txid::from_str(&outpoint.txid)
        .map_err(|e| RgbError::TransferBuild(format!("bad txid {}: {e}", outpoint.txid)))?;
    Ok(BpOutpoint::new(txid, Vout::from_u32(outpoint.vout)))
}

/// Match the maker's reserved RGB outpoints against the wallet's UTXO set,
/// returning the bp outpoint + value + terminal for each. Errors if any
/// reserved outpoint isn't in the wallet (inventory and wallet disagree).
fn resolve_maker_inputs(
    wallet: &MakerWallet,
    maker_rgb_utxos: &[Outpoint],
) -> Result<Vec<MakerRgbInput>, RgbError> {
    let mut resolved = Vec::with_capacity(maker_rgb_utxos.len());
    for want in maker_rgb_utxos {
        let utxo = wallet
            .wallet()
            .utxos()
            .find(|u| {
                u.outpoint.txid.to_string() == want.txid
                    && u.outpoint.vout.into_u32() == want.vout
            })
            .ok_or_else(|| {
                RgbError::TransferBuild(format!(
                    "reserved RGB outpoint {want} is not in the maker wallet"
                ))
            })?;
        resolved.push(MakerRgbInput {
            outpoint: utxo.outpoint,
            value: utxo.value,
            terminal: utxo.terminal,
        });
    }
    Ok(resolved)
}

/// Fully enrich a descriptor-controlled PSBT input so a `TestnetRefSigner`
/// over the matching account can sign it. Populates `witness_utxo`,
/// `sighash_type`, the redeem/witness scripts, the `bip32_derivation` map
/// (the load-bearing field for ECDSA signing — without it the signer's
/// `get(origin)` returns None and `psbt.sign` matches zero inputs), and the
/// taproot equivalents.
///
/// Replicates the field-set of `psbt::Psbt::construct_input` (which we can't
/// call directly here — it only adds *new* inputs, but our PSBT was built
/// via `Psbt::from_tx` so the inputs already exist).
///
/// Public so a real taker (or a test harness simulating one) can prep its
/// own inputs after receiving the maker-built PSBT: the buy/sell composition
/// leaves taker inputs with only `witness_utxo` + `sighash_type` (the maker
/// doesn't know the taker's descriptor), so the taker has to enrich and
/// sign on its end before handing the PSBT back.
pub fn enrich_psbt_input(
    psbt: &mut Psbt,
    index: usize,
    descriptor: &RgbDescr,
    terminal: Terminal,
    value: Sats,
) -> Result<(), RgbError> {
    let derived = descriptor
        .derive(terminal.keychain, terminal.index)
        .next()
        .ok_or_else(|| RgbError::TransferBuild("descriptor produced no script".to_owned()))?;
    let input = psbt
        .input_mut(index)
        .ok_or_else(|| RgbError::TransferBuild(format!("missing PSBT input {index}")))?;
    input.witness_utxo = Some(BpTxOut::new(derived.to_script_pubkey(), value));
    input.sighash_type = Some(SighashType::all());
    input.redeem_script = derived.to_redeem_script();
    input.witness_script = derived.to_witness_script();
    input.bip32_derivation = descriptor.legacy_keyset(terminal);
    input.tap_leaf_script = derived.to_leaf_scripts();
    input.tap_bip32_derivation = descriptor.xonly_keyset(terminal);
    input.tap_internal_key = derived.to_internal_pk();
    input.tap_merkle_root = derived.to_tap_root();
    Ok(())
}

/// The maker-side data the buy composition is built from: the taker's target
/// contract + blinded seal, the maker's BTC payout scriptPubkey, the wallet
/// descriptor (for input enrichment), and the resolved RGB inputs. Produced by
/// [`prepare_buy_inputs`] and threaded through the remaining phases.
pub(crate) struct BuyInputs {
    pub(crate) contract_id: ContractId,
    pub(crate) taker_seal: TakerSeal,
    maker_payout_spk: ScriptPubkey,
    /// Fresh maker BTC change address for the seal-anchor BTC value of the
    /// maker's RGB inputs (issue #25). Without this, the consumed seal-anchor
    /// sats would be burned to fee — the maker's RGB-change *seal* is bound
    /// to `maker_payout_spk`, but no output carries the *bitcoin* surplus.
    maker_btc_change_spk: ScriptPubkey,
    descriptor: RgbDescr,
    maker_inputs: Vec<MakerRgbInput>,
}

/// **Phase 1.** Parse the taker's invoice and resolve the maker's side: the
/// receive contract + blinded seal, a fresh BTC payout address, the signing
/// descriptor, and the maker's reserved RGB inputs (matched against the synced
/// wallet UTXO set). The buy swap only supports blinded-seal invoices.
pub(crate) fn prepare_buy_inputs(
    wallet: &mut MakerWallet,
    rgb_invoice: &str,
    maker_rgb_utxos: &[Outpoint],
) -> Result<BuyInputs, RgbError> {
    let invoice = RgbInvoice::from_str(rgb_invoice).map_err(|_| RgbError::InvalidInvoice)?;
    let contract_id = invoice
        .contract
        .ok_or_else(|| RgbError::TransferBuild("invoice carries no contract id".to_owned()))?;
    let taker_seal = match invoice.beneficiary.into_inner() {
        Beneficiary::BlindedSeal(seal) => TakerSeal::Blinded(seal),
        Beneficiary::WitnessVout(pay2vout, _) => TakerSeal::Witness(pay2vout.script_pubkey()),
    };

    // Fresh external key for the maker's BTC payout (receives the price)
    // + a separate fresh external key for the seal-anchor BTC change
    // (issue #25 — keeps the payout output cleanly at `gross_btc_sats`
    // while routing the consumed seal-anchor sats back to the maker
    // instead of burning them to fee).
    // The maker payout doubles as the tapret commitment host, so it lives on the
    // Tapret keychain (10) — the RGB convention for tapret
    // (`RgbKeychain::for_method(TapretFirst)`), and the keychain bp-wallet
    // re-derives tapret-tweaked outputs from. It also carries the maker's
    // RGB-change seal (bound post-sort in `build_buy_transition`).
    let maker_payout_spk = wallet
        .wallet_mut()
        .next_address(RgbKeychain::Tapret, true)
        .script_pubkey();
    let maker_btc_change_spk = wallet
        .wallet_mut()
        .next_address(Keychain::OUTER, true)
        .script_pubkey();
    let descriptor = wallet.wallet().descriptor().clone();
    let maker_inputs = resolve_maker_inputs(wallet, maker_rgb_utxos)?;

    Ok(BuyInputs {
        contract_id,
        taker_seal,
        maker_payout_spk,
        maker_btc_change_spk,
        descriptor,
        maker_inputs,
    })
}

/// **Phase 2.** Assemble the unsigned swap transaction and return it as a PSBT
/// with every input enriched and the commitment host in place.
///
/// Inputs are the maker's RGB outpoints followed by the taker's BTC outpoints;
/// outputs are the maker's BTC payout (which doubles as the tapret commitment
/// host) and the taker's change (back to its funding address, dropped if below
/// dust). The tapret commitment rides the payout output's taproot tweak — there
/// is no `OP_RETURN`. Maker inputs get full descriptor data so the signer recognizes them;
/// taker inputs get only `witness_utxo` + sighash (the taker fills the rest at
/// `/sign`). After the output sort the txid is final.
pub(crate) fn assemble_unsigned_psbt(
    inputs: &BuyInputs,
    taker_btc_inputs: &[(Outpoint, TxOut)],
    gross_btc_sats: u64,
    actual_fee_sats: u64,
) -> Result<Psbt, RgbError> {
    let taker_btc_total: u64 = taker_btc_inputs.iter().map(|(_, t)| t.value_sats).sum();
    // A witness-vout taker receives its RGB on a fresh swap output we fund with
    // SEAL_ANCHOR_SATS; the taker pays for it out of its own change. A blinded
    // taker reuses an existing anchor, so no extra output.
    let taker_anchor_sats = match inputs.taker_seal {
        TakerSeal::Witness(_) => SEAL_ANCHOR_SATS,
        TakerSeal::Blinded(_) => 0,
    };
    let taker_change = taker_btc_total
        .saturating_sub(gross_btc_sats)
        .saturating_sub(actual_fee_sats)
        .saturating_sub(taker_anchor_sats);
    let funding_spk = ScriptPubkey::from_unsafe(
        taker_btc_inputs
            .first()
            .map(|(_, t)| t.script_pubkey.clone())
            .unwrap_or_default(),
    );

    let seq = SeqNo::from_consensus_u32(SEQ_FINAL);
    let mut tx_inputs: Vec<UnsignedTxIn> = Vec::new();
    for mi in &inputs.maker_inputs {
        tx_inputs.push(UnsignedTxIn {
            prev_output: mi.outpoint,
            sequence: seq,
        });
    }
    for (op, _) in taker_btc_inputs {
        tx_inputs.push(UnsignedTxIn {
            prev_output: to_bp_outpoint(op)?,
            sequence: seq,
        });
    }
    let mut tx_outputs = vec![BpTxOut::new(inputs.maker_payout_spk.clone(), Sats(gross_btc_sats))];
    // Route the maker's RGB-input BTC value (seal-anchor sats) back to a
    // fresh maker change output — without this, Bitcoin Core treats the
    // surplus as fee and rejects the broadcast under the default
    // `maxfeerate` cap (issue #25). The RGB-change *seal* still binds to
    // `maker_payout_spk` (set later in `build_buy_transition` against
    // `payout_vout`); this output only carries BTC.
    let maker_rgb_btc_total: u64 = inputs
        .maker_inputs
        .iter()
        .map(|i| i.value.into_inner())
        .sum();
    if maker_rgb_btc_total > DUST_LIMIT_SATS {
        tx_outputs.push(BpTxOut::new(
            inputs.maker_btc_change_spk.clone(),
            Sats(maker_rgb_btc_total),
        ));
    }
    // Witness-vout receive output: carries the taker's bought RGB (the seal binds
    // to this output's post-sort vout in `build_buy_transition`).
    if let TakerSeal::Witness(spk) = &inputs.taker_seal {
        tx_outputs.push(BpTxOut::new(spk.clone(), Sats(taker_anchor_sats)));
    }
    if taker_change > DUST_LIMIT_SATS {
        tx_outputs.push(BpTxOut::new(funding_spk, Sats(taker_change)));
    }
    let unsigned_tx = UnsignedTx {
        version: TxVer::V2,
        inputs: VarIntArray::from_iter_checked(tx_inputs),
        outputs: VarIntArray::from_iter_checked(tx_outputs),
        lock_time: LockTime::ZERO,
    };
    let mut psbt = Psbt::from_tx(unsigned_tx);

    let maker_count = inputs.maker_inputs.len();
    for (i, mi) in inputs.maker_inputs.iter().enumerate() {
        enrich_psbt_input(&mut psbt, i, &inputs.descriptor, mi.terminal, mi.value)?;
    }
    for (j, (_, txout)) in taker_btc_inputs.iter().enumerate() {
        let input = psbt
            .input_mut(maker_count + j)
            .ok_or_else(|| RgbError::TransferBuild("missing taker PSBT input".to_owned()))?;
        input.witness_utxo = Some(BpTxOut::new(
            ScriptPubkey::from_unsafe(txout.script_pubkey.clone()),
            Sats(txout.value_sats),
        ));
        input.sighash_type = Some(SighashType::all());
    }

    // Tapret host + canonical ordering. The RGB MPC commitment tweaks the
    // maker's payout output (P2TR, on the Tapret keychain) rather than riding a
    // dedicated `OP_RETURN`, so the swap is indistinguishable from an ordinary
    // Taproot spend on-chain. `set_rgb_tapret_host_on_change` lets
    // `commit_and_consign` persist the resulting tweak so the maker can still
    // recognise and spend the tweaked output.
    psbt.set_rgb_close_method(CloseMethod::TapretFirst);
    psbt.set_rgb_tapret_host_on_change();
    psbt.outputs_mut()
        .find(|o| o.script == inputs.maker_payout_spk)
        .ok_or_else(|| {
            RgbError::TransferBuild("maker payout output missing for tapret host".to_owned())
        })?
        .set_tapret_host()
        .map_err(|e| RgbError::TransferBuild(format!("set tapret host: {e:?}")))?;
    psbt.sort_outputs_by(|o| !o.is_tapret_host())
        .expect("PSBT outputs are modifiable");
    psbt.complete_construction();

    Ok(psbt)
}

/// **Phase 3.** Build the RGB state transition that spends the maker's
/// allocations, assigns `amount` to the taker's blinded seal, and routes any
/// surplus to a fresh maker change seal bound to the payout output's (post-sort)
/// vout. Returns the [`Batch`] ready to embed.
pub(crate) fn build_buy_transition(
    wallet: &MakerWallet,
    inputs: &BuyInputs,
    psbt: &Psbt,
    amount: u64,
) -> Result<(Batch, DeliverySeal), RgbError> {
    // The maker's RGB change binds to the payout output's vout, which is only
    // known after Phase 2's commitment-host sort.
    let payout_vout = psbt
        .outputs()
        .find(|o| o.script == inputs.maker_payout_spk)
        .map(|o| o.vout())
        .ok_or_else(|| RgbError::TransferBuild("payout output missing after sort".to_owned()))?;

    let contract = wallet
        .stock()
        .contract_data(inputs.contract_id)
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
        .transition_builder_raw(inputs.contract_id, transition_type)
        .map_err(|e| RgbError::TransferBuild(format!("transition builder: {e}")))?;

    let maker_bp_outpoints: Vec<BpOutpoint> =
        inputs.maker_inputs.iter().map(|mi| mi.outpoint).collect();
    let assignments = wallet
        .stock()
        .contract_assignments_for(inputs.contract_id, maker_bp_outpoints)
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
    if sum_inputs < amount {
        return Err(RgbError::TransferBuild(format!(
            "reserved RGB allocations {sum_inputs} < requested {amount}"
        )));
    }

    // Pay the taker. A blinded invoice rides on its pre-existing anchor
    // (concealed); a witness-vout invoice binds to the taker's fresh swap output
    // (revealed against its post-sort vout). The matching delivery seal is what
    // `commit_and_consign` addresses the consignment to.
    let (taker_builder_seal, delivery) = match &inputs.taker_seal {
        TakerSeal::Blinded(seal) => (BuilderSeal::Concealed(*seal), DeliverySeal::Secret(*seal)),
        TakerSeal::Witness(spk) => {
            let taker_vout = psbt
                .outputs()
                .find(|o| o.script == *spk)
                .map(|o| o.vout())
                .ok_or_else(|| {
                    RgbError::TransferBuild(
                        "taker witness-vout output missing after sort".to_owned(),
                    )
                })?;
            let seal = GraphSeal::with_blinded_vout(taker_vout, rand::random());
            (BuilderSeal::Revealed(seal), DeliverySeal::WitnessVout(seal))
        }
    };
    builder = builder
        .add_fungible_state_raw(assignment_type, taker_builder_seal, Amount::from(amount))
        .map_err(|e| RgbError::TransferBuild(format!("add beneficiary state: {e}")))?;
    if sum_inputs > amount {
        let change_seal = GraphSeal::with_blinded_vout(payout_vout, rand::random());
        builder = builder
            .add_fungible_state_raw(
                assignment_type,
                BuilderSeal::Revealed(change_seal),
                Amount::from(sum_inputs - amount),
            )
            .map_err(|e| RgbError::TransferBuild(format!("add change state: {e}")))?;
    }

    let main = builder
        .complete_transition()
        .map_err(|e| RgbError::TransferBuild(format!("complete_transition: {e}")))?;
    let mut batch = Batch {
        main,
        extras: Default::default(),
    };
    batch.set_priority(u64::MAX);
    Ok((batch, delivery))
}

/// **Phase 4.** Embed the transition into the PSBT and commit it. `rgb_commit`
/// rewrites the host output's scriptPubkey, so the witness txid is stable from
/// here on (and the maker must sign *after* this — U4). The committed fascia is
/// consumed into the stash as [`WitnessOrd::Tentative`] (U5) and the
/// witness-extended consignment is emitted, addressed to `delivery` — a blinded
/// recipient via `secret_seals`, or a witness-vout recipient via `outputs` (its
/// graph seal resolves to an `OutputSeal` against the now-known witness id).
/// Returns the consignment + witness id; both flows hand it back to the taker as
/// `final_consignment` so it can record the RGB it received.
pub(crate) fn commit_and_consign(
    wallet: &mut MakerWallet,
    psbt: &mut Psbt,
    batch: Batch,
    contract_id: ContractId,
    delivery: DeliverySeal,
) -> Result<(rgb::containers::Transfer, Txid), RgbError> {
    psbt.rgb_embed(batch)
        .map_err(|e| RgbError::TransferBuild(format!("rgb_embed: {e}")))?;
    let fascia = psbt
        .rgb_commit()
        .map_err(|e| RgbError::TransferBuild(format!("rgb_commit: {e}")))?;
    let witness_id = psbt.txid();
    wallet
        .stock_mut()
        .consume_fascia(fascia, FasciaResolver { witness_id })
        .map_err(|e| RgbError::TransferBuild(format!("consume_fascia: {e}")))?;
    let stock = wallet.stock_mut();
    let transfer = match delivery {
        DeliverySeal::Secret(seal) => stock
            .transfer(contract_id, [], [seal], [], Some(witness_id))
            .map_err(|e| RgbError::TransferBuild(format!("transfer: {e}")))?,
        DeliverySeal::WitnessVout(graph_seal) => {
            let output_seal = graph_seal.to_output_seal_or_default(witness_id);
            stock
                .transfer(contract_id, [output_seal], [], [], Some(witness_id))
                .map_err(|e| RgbError::TransferBuild(format!("transfer: {e}")))?
        }
    };
    Ok((transfer, witness_id))
}

/// **Phase 5.** Sign the maker's inputs. `psbt.sign` only touches inputs whose
/// keys the signer holds, so the taker's BTC inputs are left unsigned for
/// `/sign`. Rejects a mainnet account up front (the bundled signer is
/// testnet-only — U6) and errors if the signer matched no inputs.
pub(crate) fn sign_maker_inputs(psbt: &mut Psbt, account: &XprivAccount) -> Result<(), RgbError> {
    if !account.xpriv().is_testnet() {
        return Err(RgbError::TransferBuild(
            "mainnet signing is not supported by the bundled signer".to_owned(),
        ));
    }
    let signer = TestnetRefSigner::new(account);
    let sig_count = psbt
        .sign(&signer)
        .map_err(|e| RgbError::TransferBuild(format!("sign: {e}")))?;
    if sig_count == 0 {
        return Err(RgbError::TransferBuild(
            "signer holds no keys for the maker's RGB inputs".to_owned(),
        ));
    }
    Ok(())
}

/// **Phase 6 (buy).** Serialize the (partially-signed) PSBT and the
/// maker-emitted consignment into the base64 [`SwapTransfer`] handed back over
/// the wire. The witness txid is already committed, so it's published as
/// `expected_witness_txid`.
pub(crate) fn encode_swap_transfer_buy(
    psbt: &Psbt,
    transfer: &rgb::containers::Transfer,
    witness_id: Txid,
) -> Result<SwapTransfer, RgbError> {
    let mut consignment_bytes = Vec::new();
    transfer
        .save(&mut consignment_bytes)
        .map_err(|e| RgbError::TransferBuild(format!("serialize consignment: {e}")))?;
    Ok(SwapTransfer {
        partial_psbt: base64::engine::general_purpose::STANDARD.encode(psbt.serialize(PsbtVer::V2)),
        consignment: Some(base64::engine::general_purpose::STANDARD.encode(consignment_bytes)),
        expected_witness_txid: Some(witness_id.to_string()),
    })
}

/// **Phase 6 (sell).** Serialize the (partially-signed) PSBT into the base64
/// [`SwapTransfer`]. When the taker over-consigned, `change_consignment` is the
/// maker-emitted transfer addressed to the taker's change seal (anchored to the
/// real swap witness); it rides along so the taker can accept its RGB change
/// post-broadcast. With no surplus there's nothing for the taker to receive, so
/// `consignment` stays `None`.
pub(crate) fn encode_swap_transfer_sell(
    psbt: &Psbt,
    witness_id: Txid,
    change_consignment: Option<&rgb::containers::Transfer>,
) -> Result<SwapTransfer, RgbError> {
    let consignment = match change_consignment {
        Some(transfer) => {
            let mut bytes = Vec::new();
            transfer
                .save(&mut bytes)
                .map_err(|e| RgbError::TransferBuild(format!("serialize change consignment: {e}")))?;
            Some(base64::engine::general_purpose::STANDARD.encode(bytes))
        }
        None => None,
    };
    Ok(SwapTransfer {
        partial_psbt: base64::engine::general_purpose::STANDARD.encode(psbt.serialize(PsbtVer::V2)),
        consignment,
        expected_witness_txid: Some(witness_id.to_string()),
    })
}

// ---------------------------------------------------------------------------
// Sell composition
// ---------------------------------------------------------------------------

/// A maker BTC funding input resolved against the wallet's UTXO set. Mirrors
/// [`MakerRgbInput`] — same bp-std data needed to enrich the PSBT input via
/// [`enrich_psbt_input`].
struct MakerBtcInput {
    outpoint: BpOutpoint,
    value: Sats,
    terminal: Terminal,
}

/// Match each maker-declared BTC outpoint against the wallet's UTXO set,
/// returning the bp outpoint + value + terminal for each. Errors if any
/// outpoint isn't in the wallet (the maker said it owns a UTXO it doesn't).
fn resolve_maker_btc_inputs(
    wallet: &MakerWallet,
    maker_btc_inputs: &[(Outpoint, TxOut)],
) -> Result<Vec<MakerBtcInput>, RgbError> {
    let mut resolved = Vec::with_capacity(maker_btc_inputs.len());
    for (want, _txout) in maker_btc_inputs {
        let utxo = wallet
            .wallet()
            .utxos()
            .find(|u| {
                u.outpoint.txid.to_string() == want.txid
                    && u.outpoint.vout.into_u32() == want.vout
            })
            .ok_or_else(|| {
                RgbError::TransferBuild(format!(
                    "maker BTC outpoint {want} is not in the maker wallet"
                ))
            })?;
        resolved.push(MakerBtcInput {
            outpoint: utxo.outpoint,
            value: utxo.value,
            terminal: utxo.terminal,
        });
    }
    Ok(resolved)
}

/// The maker-side data the sell composition is built from: the maker's receive
/// contract + blinded seal + deliver amount (extracted by the caller from the
/// maker's own invoice, since `create_swap_psbt_sell` no longer parses it),
/// the optional taker RGB-change seal, the taker's BTC payout spk, a fresh
/// maker BTC change address, the wallet descriptor (for input enrichment), and
/// the resolved maker BTC inputs. Produced by [`prepare_sell_inputs`] and
/// threaded through the remaining phases.
pub(crate) struct SellInputs {
    pub(crate) contract_id: ContractId,
    deliver_amount: u64,
    pub(crate) taker_change_seal: Option<TakerSeal>,
    /// Fresh maker address that receives the bought RGB on a witness-vout
    /// (revealed) output. The maker rebuilds the transition, so it routes its
    /// own receive here instead of the blinded invoice seal — `consume_fascia`
    /// surfaces revealed seals directly (a blinded receive would need an
    /// `accept_transfer` the maker never does for itself), and no pre-funded
    /// keychain-10 anchor is required.
    maker_receive_spk: ScriptPubkey,
    maker_change_spk: ScriptPubkey,
    taker_payout_spk: ScriptPubkey,
    descriptor: RgbDescr,
    maker_btc_inputs: Vec<MakerBtcInput>,
}

/// **Phase 1 (sell).** Resolve the maker's side of the sell composition:
/// parse the optional taker RGB-change invoice for its blinded seal; resolve
/// the taker's BTC payout address into a scriptPubkey; pick a fresh maker BTC
/// change address; and match the declared maker BTC inputs against the wallet
/// UTXO set to grab their per-input terminals (needed for descriptor
/// enrichment at PSBT-build time).
pub(crate) fn prepare_sell_inputs(
    wallet: &mut MakerWallet,
    contract_id: ContractId,
    deliver_amount: u64,
    btc_payout_addr: &str,
    rgb_change_invoice: Option<&str>,
    maker_btc_inputs: &[(Outpoint, TxOut)],
) -> Result<SellInputs, RgbError> {
    let taker_change_seal = match rgb_change_invoice {
        None => None,
        Some(inv) => {
            let invoice = RgbInvoice::from_str(inv).map_err(|_| RgbError::InvalidInvoice)?;
            let seal = match invoice.beneficiary.into_inner() {
                Beneficiary::BlindedSeal(seal) => TakerSeal::Blinded(seal),
                Beneficiary::WitnessVout(pay2vout, _) => {
                    TakerSeal::Witness(pay2vout.script_pubkey())
                }
            };
            Some(seal)
        }
    };

    let taker_payout_spk = Address::from_str(btc_payout_addr)
        .map_err(|e| {
            RgbError::TransferBuild(format!("bad btc_payout_addr {btc_payout_addr}: {e}"))
        })?
        .script_pubkey();

    // The maker's RGB receive lives on the Tapret keychain (10) — the RGB
    // convention for tapret — and doubles as the tapret commitment host: it only
    // carries RGB, stays segregated from BTC payment UTXOs, composes with the
    // anchor-exclusion / `list_btc_only_utxos` logic, and is the output bp-wallet
    // re-derives once the tapret tweak rewrites its key.
    let maker_receive_spk = wallet
        .wallet_mut()
        .next_address(RgbKeychain::Tapret, true)
        .script_pubkey();
    // BTC change stays on keychain 0 (a payment output, no RGB).
    let maker_change_spk = wallet
        .wallet_mut()
        .next_address(Keychain::OUTER, true)
        .script_pubkey();
    let descriptor = wallet.wallet().descriptor().clone();
    let resolved_btc = resolve_maker_btc_inputs(wallet, maker_btc_inputs)?;

    Ok(SellInputs {
        contract_id,
        deliver_amount,
        taker_change_seal,
        maker_receive_spk,
        maker_change_spk,
        taker_payout_spk,
        descriptor,
        maker_btc_inputs: resolved_btc,
    })
}

/// **Phase 2 (sell).** Assemble the unsigned swap transaction and return it
/// as a PSBT with every input enriched and the commitment host in place.
///
/// Inputs are the taker's RGB outpoints (witness_utxo + sighash only — the
/// taker will sign these at `/sign`) followed by the maker's BTC funding
/// outpoints (full descriptor enrichment so the maker signer recognizes them).
/// Outputs are the taker's BTC payout (gross minus fee), the maker's BTC
/// change (dropped if dust), and the maker's RGB-receive output — which doubles
/// as the tapret commitment host (taproot tweak, no `OP_RETURN`). After the
/// output sort the txid is final.
pub(crate) fn assemble_sell_psbt(
    inputs: &SellInputs,
    taker_rgb_prevouts: &[(Outpoint, TxOut)],
    gross_btc_sats: u64,
    actual_fee_sats: u64,
) -> Result<Psbt, RgbError> {
    let maker_btc_total: u64 = inputs
        .maker_btc_inputs
        .iter()
        .map(|i| i.value.into_inner())
        .sum();
    // Fold the taker's RGB-input BTC value (seal-anchor sats) into the
    // taker's payout output so it doesn't get burned to fee (issue #25).
    // The taker keeps its own BTC: gets the agreed swap price minus the
    // explicit fee, plus the seal-anchor sats from the outpoints it
    // consigned. Bundling into one output keeps tx size minimal.
    let taker_rgb_btc_total: u64 = taker_rgb_prevouts.iter().map(|(_, t)| t.value_sats).sum();
    // A witness-vout change recipient gets its surplus RGB on a fresh output we
    // fund with SEAL_ANCHOR_SATS, paid out of the taker's own payout (net wash —
    // the taker owns that output too). A blinded change reuses an anchor.
    let taker_change_anchor_sats = match &inputs.taker_change_seal {
        Some(TakerSeal::Witness(_)) => SEAL_ANCHOR_SATS,
        _ => 0,
    };
    let payout = gross_btc_sats
        .saturating_sub(actual_fee_sats)
        .saturating_add(taker_rgb_btc_total)
        .saturating_sub(taker_change_anchor_sats);
    // The maker receives the bought RGB on its own witness-vout output, funded
    // with SEAL_ANCHOR_SATS out of the maker's BTC change.
    let maker_change = maker_btc_total
        .saturating_sub(gross_btc_sats)
        .saturating_sub(SEAL_ANCHOR_SATS);

    let seq = SeqNo::from_consensus_u32(SEQ_FINAL);
    let mut tx_inputs: Vec<UnsignedTxIn> = Vec::new();
    for (op, _) in taker_rgb_prevouts {
        tx_inputs.push(UnsignedTxIn {
            prev_output: to_bp_outpoint(op)?,
            sequence: seq,
        });
    }
    for mi in &inputs.maker_btc_inputs {
        tx_inputs.push(UnsignedTxIn {
            prev_output: mi.outpoint,
            sequence: seq,
        });
    }
    let mut tx_outputs = vec![BpTxOut::new(inputs.taker_payout_spk.clone(), Sats(payout))];
    if maker_change > DUST_LIMIT_SATS {
        tx_outputs.push(BpTxOut::new(
            inputs.maker_change_spk.clone(),
            Sats(maker_change),
        ));
    }
    // Maker receive output: carries the maker's bought RGB on a witness-vout
    // seal (bound to its post-sort vout in `build_sell_transition`).
    tx_outputs.push(BpTxOut::new(
        inputs.maker_receive_spk.clone(),
        Sats(SEAL_ANCHOR_SATS),
    ));
    // Witness-vout change output: carries the taker's surplus RGB (the change
    // seal binds to its post-sort vout in `build_sell_transition`).
    if let Some(TakerSeal::Witness(spk)) = &inputs.taker_change_seal {
        tx_outputs.push(BpTxOut::new(spk.clone(), Sats(taker_change_anchor_sats)));
    }
    let unsigned_tx = UnsignedTx {
        version: TxVer::V2,
        inputs: VarIntArray::from_iter_checked(tx_inputs),
        outputs: VarIntArray::from_iter_checked(tx_outputs),
        lock_time: LockTime::ZERO,
    };
    let mut psbt = Psbt::from_tx(unsigned_tx);

    let taker_count = taker_rgb_prevouts.len();
    for (j, (_, txout)) in taker_rgb_prevouts.iter().enumerate() {
        let input = psbt
            .input_mut(j)
            .ok_or_else(|| RgbError::TransferBuild("missing taker RGB PSBT input".to_owned()))?;
        input.witness_utxo = Some(BpTxOut::new(
            ScriptPubkey::from_unsafe(txout.script_pubkey.clone()),
            Sats(txout.value_sats),
        ));
        input.sighash_type = Some(SighashType::all());
    }
    for (i, mi) in inputs.maker_btc_inputs.iter().enumerate() {
        enrich_psbt_input(
            &mut psbt,
            taker_count + i,
            &inputs.descriptor,
            mi.terminal,
            mi.value,
        )?;
    }

    // Tapret host + canonical ordering. The maker's RGB-receive output (P2TR, on
    // the Tapret keychain) hosts the RGB MPC commitment in its taproot tweak —
    // no `OP_RETURN`, so the swap looks like an ordinary Taproot spend.
    // `set_rgb_tapret_host_on_change` lets `commit_and_consign` persist the tweak
    // so the maker can recognise and spend the tweaked receive output.
    psbt.set_rgb_close_method(CloseMethod::TapretFirst);
    psbt.set_rgb_tapret_host_on_change();
    psbt.outputs_mut()
        .find(|o| o.script == inputs.maker_receive_spk)
        .ok_or_else(|| {
            RgbError::TransferBuild("maker receive output missing for tapret host".to_owned())
        })?
        .set_tapret_host()
        .map_err(|e| RgbError::TransferBuild(format!("set tapret host: {e:?}")))?;
    psbt.sort_outputs_by(|o| !o.is_tapret_host())
        .expect("PSBT outputs are modifiable");
    psbt.complete_construction();

    Ok(psbt)
}

/// **Phase 3 (sell).** Build the RGB state transition that spends the taker's
/// (now stash-resident, after `validate_incoming_consignment`'s `accept_transfer`)
/// allocations, assigns `deliver_amount` to the maker's blinded seal, and
/// routes any surplus to the taker's change seal. Returns the [`Batch`], the
/// [`DeliverySeal`] the caller consigns to (the change seal when the taker
/// over-consigned, else the maker seal), and a bool that's `true` when a change
/// allocation was added.
///
/// A blinded change seal is order-independent; a witness-vout change binds to a
/// post-sort PSBT vout (`psbt` is the already-sorted sell PSBT), exactly like the
/// buy side.
pub(crate) fn build_sell_transition(
    wallet: &MakerWallet,
    inputs: &SellInputs,
    psbt: &Psbt,
    consignment_info: &ConsignmentInfo,
) -> Result<(Batch, DeliverySeal, bool), RgbError> {
    let contract = wallet
        .stock()
        .contract_data(inputs.contract_id)
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
        .transition_builder_raw(inputs.contract_id, transition_type)
        .map_err(|e| RgbError::TransferBuild(format!("transition builder: {e}")))?;

    let taker_outpoints: Vec<BpOutpoint> = consignment_info
        .outpoints
        .iter()
        .map(to_bp_outpoint)
        .collect::<Result<Vec<_>, _>>()?;
    let assignments = wallet
        .stock()
        .contract_assignments_for(inputs.contract_id, taker_outpoints)
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
    // No equality check against `consignment_info.total_amount`: that field
    // sums *all* transition outputs (including blinded destinations the
    // resolver can't surface as outpoints), so it overcounts whenever the
    // consignment has a blinded recipient — the normal taker→maker case.
    // Only sum_inputs is what's actually spendable here.
    if sum_inputs < inputs.deliver_amount {
        return Err(RgbError::TransferBuild(format!(
            "stash allocations at consigned outpoints sum to {sum_inputs}, < deliver {}",
            inputs.deliver_amount
        )));
    }

    // Maker receives on a witness-vout (revealed) seal bound to its fresh receive
    // output's post-sort vout. `consume_fascia` surfaces revealed seals directly
    // — a blinded receive would need an `accept_transfer` the maker never does
    // for itself, so the bought RGB would never show in `list_inventory_utxos`.
    let maker_receive_vout = psbt
        .outputs()
        .find(|o| o.script == inputs.maker_receive_spk)
        .map(|o| o.vout())
        .ok_or_else(|| {
            RgbError::TransferBuild("maker receive output missing after sort".to_owned())
        })?;
    let maker_receive_seal = GraphSeal::with_blinded_vout(maker_receive_vout, rand::random());
    builder = builder
        .add_fungible_state_raw(
            assignment_type,
            BuilderSeal::Revealed(maker_receive_seal),
            Amount::from(inputs.deliver_amount),
        )
        .map_err(|e| RgbError::TransferBuild(format!("add beneficiary state: {e}")))?;
    let surplus = sum_inputs - inputs.deliver_amount;
    // Route surplus back to the taker's change seal (blinded or witness-vout) and
    // remember how to address its consignment. With no surplus there's nothing to
    // hand to the taker, so we deliver the (dropped) consignment to the maker's
    // own receive seal — a valid revealed seal for `stock.transfer`.
    let delivery = if surplus > 0 {
        let change_seal = inputs.taker_change_seal.as_ref().ok_or_else(|| {
            RgbError::TransferBuild(format!(
                "taker over-consigned by {surplus}; rgb_change_invoice required"
            ))
        })?;
        let (change_builder_seal, delivery) = match change_seal {
            TakerSeal::Blinded(seal) => {
                (BuilderSeal::Concealed(*seal), DeliverySeal::Secret(*seal))
            }
            TakerSeal::Witness(spk) => {
                let change_vout = psbt
                    .outputs()
                    .find(|o| o.script == *spk)
                    .map(|o| o.vout())
                    .ok_or_else(|| {
                        RgbError::TransferBuild(
                            "taker witness-vout change output missing after sort".to_owned(),
                        )
                    })?;
                let seal = GraphSeal::with_blinded_vout(change_vout, rand::random());
                (BuilderSeal::Revealed(seal), DeliverySeal::WitnessVout(seal))
            }
        };
        builder = builder
            .add_fungible_state_raw(assignment_type, change_builder_seal, Amount::from(surplus))
            .map_err(|e| RgbError::TransferBuild(format!("add change state: {e}")))?;
        delivery
    } else {
        DeliverySeal::WitnessVout(maker_receive_seal)
    };

    let main = builder
        .complete_transition()
        .map_err(|e| RgbError::TransferBuild(format!("complete_transition: {e}")))?;
    let mut batch = Batch {
        main,
        extras: Default::default(),
    };
    batch.set_priority(u64::MAX);
    Ok((batch, delivery, surplus > 0))
}

// ---------------------------------------------------------------------------
// Finalize (both flows)
// ---------------------------------------------------------------------------

/// **Finalize.** Both buy and sell converge here once the taker has signed
/// (and finalized) its own inputs and shipped the PSBT back to the maker.
///
/// `psbt.finalize(descriptor)` walks each input's `partial_sigs` and tries to
/// satisfy the descriptor; inputs whose keys the descriptor doesn't cover are
/// left untouched. The maker's inputs were left with only `partial_sigs` by
/// [`sign_maker_inputs`] (it calls `psbt.sign` but not `psbt.finalize`), so the
/// maker descriptor satisfies and finalizes them here. The taker's inputs
/// should already carry `final_witness` from the taker's own sign+finalize
/// round; if not, [`Psbt::is_finalized`] catches it and we reject the PSBT
/// before extraction.
///
/// The witness txid is the same one [`commit_and_consign`] returned at
/// PSBT-build (D3 invariant — input set was final by then, so the txid is
/// stable through sign/finalize); we just re-read it from the PSBT and hand
/// it back so callers don't need to thread it through.
pub(crate) fn finalize_signed_psbt(
    descriptor: &RgbDescr,
    signed_psbt_base64: &str,
) -> Result<(Vec<u8>, Txid), RgbError> {
    let mut psbt = Psbt::from_base64(signed_psbt_base64)
        .map_err(|e| RgbError::FinalizeFailed(format!("decode signed PSBT: {e}")))?;

    psbt.finalize(descriptor);

    if !psbt.is_finalized() {
        return Err(RgbError::FinalizeFailed(
            "PSBT has unfinalized inputs after maker finalize; \
             taker submitted partial sigs without finalizing its own inputs"
                .to_owned(),
        ));
    }

    let witness_id = psbt.txid();
    let tx = psbt
        .extract()
        .map_err(|e| RgbError::FinalizeFailed(format!("extract signed tx: {e}")))?;
    Ok((tx.consensus_serialize(), witness_id))
}
