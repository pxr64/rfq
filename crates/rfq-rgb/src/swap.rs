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
//!    scriptPubkey (the opret payload), which changes the txid. Signing must
//!    happen *after* commit so the signatures cover the final output set; the
//!    witness txid is stable from the moment `rgb_commit` returns.
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
    Derive, Descriptor, Keychain, LockTime, Outpoint as BpOutpoint, Sats, ScriptPubkey, SeqNo,
    SighashType, Terminal, TxOut as BpTxOut, TxVer, Txid, VarIntArray, Vout, XprivAccount,
    XpubDerivable,
};
use bpwallet::Wallet;
use psrgbt::{PsbtConstructor, RgbExt, RgbPsbt};
use rgb::containers::{Batch, BuilderSeal, FileContent};
use rgb::contract::AllocatedState;
use rgb::invoice::{Beneficiary, RgbInvoice};
use rgb::validation::{WitnessOrdProvider, WitnessResolverError};
use rgb::vm::WitnessOrd;
use rgb::{Amount, ContractId, GraphSeal, RgbDescr, RgbWallet, SecretSeal, StateType};

use rfq_types::{Outpoint, SwapTransfer};

use crate::{RgbError, TxOut};

/// The maker's RGB-enabled wallet: a `bp-wallet` wallet wrapped with its
/// `Stock`. This is what [`LibRgbBackend::load_wallet`](crate::LibRgbBackend)
/// hands the composition functions.
pub(crate) type MakerWallet = RgbWallet<Wallet<XpubDerivable, RgbDescr>>;

/// P2WPKH/P2WSH/P2TR dust threshold. Outputs at or below this are dropped
/// rather than created (they'd be unspendable / non-standard).
const DUST_LIMIT_SATS: u64 = 546;

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

/// Fully enrich a maker-controlled PSBT input from the wallet descriptor, so the
/// maker's signer recognizes it. Replicates the field-set of
/// `psbt::Psbt::construct_input` (which we can't call directly — it only adds
/// new inputs, and our taker inputs aren't on the maker descriptor, forcing the
/// `Psbt::from_tx` path).
fn enrich_maker_input(
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
    contract_id: ContractId,
    taker_seal: SecretSeal,
    maker_payout_spk: ScriptPubkey,
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
        Beneficiary::BlindedSeal(seal) => seal,
        Beneficiary::WitnessVout(..) => {
            return Err(RgbError::TransferBuild(
                "buy swap requires a blinded-seal invoice".to_owned(),
            ));
        }
    };

    // Fresh external key for the maker's BTC payout (receives the price).
    let maker_payout_spk = wallet
        .wallet_mut()
        .next_address(Keychain::OUTER, true)
        .script_pubkey();
    let descriptor = wallet.wallet().descriptor().clone();
    let maker_inputs = resolve_maker_inputs(wallet, maker_rgb_utxos)?;

    Ok(BuyInputs {
        contract_id,
        taker_seal,
        maker_payout_spk,
        descriptor,
        maker_inputs,
    })
}

/// **Phase 2.** Assemble the unsigned swap transaction and return it as a PSBT
/// with every input enriched and the commitment host in place.
///
/// Inputs are the maker's RGB outpoints followed by the taker's BTC outpoints;
/// outputs are the maker's BTC payout, the taker's change (back to its funding
/// address, dropped if below dust), and an opret `OP_RETURN` commitment host.
/// Maker inputs get full descriptor data so the maker signer recognizes them;
/// taker inputs get only `witness_utxo` + sighash (the taker fills the rest at
/// `/sign`). After the output sort the txid is final.
pub(crate) fn assemble_unsigned_psbt(
    inputs: &BuyInputs,
    taker_btc_inputs: &[(Outpoint, TxOut)],
    gross_btc_sats: u64,
    actual_fee_sats: u64,
) -> Result<Psbt, RgbError> {
    let taker_btc_total: u64 = taker_btc_inputs.iter().map(|(_, t)| t.value_sats).sum();
    let taker_change = taker_btc_total
        .saturating_sub(gross_btc_sats)
        .saturating_sub(actual_fee_sats);
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
        enrich_maker_input(&mut psbt, i, &inputs.descriptor, mi.terminal, mi.value)?;
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

    // Opret OP_RETURN host (the wpkh maker's close method) + canonical ordering.
    psbt.set_rgb_close_method(CloseMethod::OpretFirst);
    psbt.construct_output_expect(ScriptPubkey::op_return(&[]), Sats::ZERO)
        .set_opret_host()
        .expect("freshly created opret output");
    psbt.sort_outputs_by(|o| !o.is_opret_host())
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
) -> Result<Batch, RgbError> {
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

    builder = builder
        .add_fungible_state_raw(
            assignment_type,
            BuilderSeal::Concealed(inputs.taker_seal),
            Amount::from(amount),
        )
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
    Ok(batch)
}

/// **Phase 4.** Embed the transition into the PSBT and commit it. `rgb_commit`
/// rewrites the host output's scriptPubkey, so the witness txid is stable from
/// here on (and the maker must sign *after* this — U4). The committed fascia is
/// consumed into the stash as [`WitnessOrd::Tentative`] (U5) and the
/// witness-extended consignment is emitted. Returns the consignment + witness id.
pub(crate) fn commit_and_consign(
    wallet: &mut MakerWallet,
    psbt: &mut Psbt,
    batch: Batch,
    inputs: &BuyInputs,
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
    let transfer = wallet
        .stock_mut()
        .transfer(inputs.contract_id, [], [inputs.taker_seal], [], Some(witness_id))
        .map_err(|e| RgbError::TransferBuild(format!("transfer: {e}")))?;
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

/// **Phase 6.** Serialize the (partially-signed) PSBT and the consignment into
/// the base64 [`SwapTransfer`] handed back over the wire. The witness txid is
/// already committed, so it's published as `expected_witness_txid`.
pub(crate) fn encode_swap_transfer(
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
