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

use amplify::confinement::{Confined, U24};
use amplify::Wrapper as _;
use base64::Engine as _;
use bpstd::psbt::{Psbt, PsbtVer, UnsignedTx, UnsignedTxIn};
use bpstd::dbc::tapret::TapretProof;
use bpstd::seals::txout::CloseMethod;
use bpstd::signers::TestnetRefSigner;
use bpstd::{
    Address, ConsensusEncode, Derive, Descriptor, Keychain, LockTime, NormalIndex,
    Outpoint as BpOutpoint, Sats, ScriptPubkey, SeqNo, SigScript, SighashType, Terminal,
    TxOut as BpTxOut, TxVer, Txid, VarIntArray, Vout,
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
    Amount, ContractId, DescriptorRgb, ExposedSeal, GraphSeal, RgbDescr, RgbKeychain, RgbWallet,
    SecretSeal, StateType,
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

/// BTC value parked on a witness-vout RGB output (the taker's "future seal") —
/// the dust limit. The fee for any later spend of this RGB is funded by inputs
/// the spender APPENDS (the taker always adds the BTC inputs that pay the tx
/// fee), not from this anchor, so it needn't exceed dust.
///
/// Public so the maker's buy-side funding selection covers it — a witness-vout
/// taker's BTC inputs must fund `gross + fee + SEAL_ANCHOR_SATS`, not just
/// `gross + fee`, or `assemble_unsigned_psbt` would overspend.
pub const SEAL_ANCHOR_SATS: u64 = 546;

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
    /// The UTXO's actual on-chain scriptPubkey, captured from the wallet's coin
    /// cache so `enrich_psbt_input` picks the right derived (base vs tweaked)
    /// script even when the terminal was reused for several differently-tweaked
    /// outputs.
    script: ScriptPubkey,
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
    // Map each known coin to its actual on-chain scriptPubkey. A tapret host
    // output's taproot key is tweaked, so its spk is NOT the base derived
    // address — `enrich_psbt_input` needs the real one to pick the matching
    // derived script and sign correctly.
    let spk_by_outpoint: std::collections::HashMap<BpOutpoint, ScriptPubkey> = wallet
        .wallet()
        .coins()
        .map(|c| (c.outpoint, c.address.addr.script_pubkey()))
        .collect();
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
        let script = spk_by_outpoint.get(&utxo.outpoint).cloned().ok_or_else(|| {
            RgbError::TransferBuild(format!(
                "no cached scriptPubkey for reserved RGB outpoint {want}"
            ))
        })?;
        resolved.push(MakerRgbInput {
            outpoint: utxo.outpoint,
            value: utxo.value,
            terminal: utxo.terminal,
            script,
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
    expected_script: Option<&ScriptPubkey>,
) -> Result<(), RgbError> {
    // A tapret terminal derives `[base, tweaked...]`, and bp-wallet can reuse a
    // terminal across differently-tweaked outputs (its `last_used` index doesn't
    // survive a wallet reload, while the descriptor's tweaks do), so neither
    // `.next()` nor `.last()` reliably picks the *specific* UTXO being spent.
    // When the caller knows the on-chain scriptPubkey (maker RGB inputs — see
    // `resolve_maker_inputs`), match it exactly so the signer uses the right
    // (tweaked or base) key; otherwise fall back to the last derived script
    // (untweaked taker inputs derive only `[base]`). Signing the wrong key gets
    // the witness rejected with "Invalid Schnorr signature".
    let derived = match expected_script {
        Some(spk) => descriptor
            .derive(terminal.keychain, terminal.index)
            .find(|d| d.to_script_pubkey() == *spk)
            .ok_or_else(|| {
                RgbError::TransferBuild(format!(
                    "no derived script at {terminal:?} matches the input scriptPubkey"
                ))
            })?,
        None => descriptor
            .derive(terminal.keychain, terminal.index)
            .last()
            .ok_or_else(|| RgbError::TransferBuild("descriptor produced no script".to_owned()))?,
    };
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

/// The maker pins EVERY tapret commitment host (buy change + sell receive) to
/// this single low index on the Tapret keychain (10), rather than calling
/// `next_address` to advance. Why: a tapret host's on-chain scriptPubkey is the
/// *tweaked* taproot key, which bp-wallet's electrum scan tracks unreliably.
/// When the host drifts to a sparse high index, the scan's gap limit (it stops
/// after N consecutive empty addresses) never reaches it and the maker's RGB
/// change is stranded — every swap's change becomes invisible/unspendable. A
/// fixed LOW index is always inside the gap window, so the host is always
/// rescanned. Each swap still produces a distinct on-chain output: the
/// commitment differs, so the tweaked key differs, even at the same terminal;
/// the descriptor accumulates all tweaks under this one terminal and the derive
/// impl emits the plain key + every stored tweak, so the scan finds them all.
const TAPRET_HOST_INDEX: u16 = 0;

/// Derive the maker's pinned Tapret-keychain host address (`TAPRET_HOST_INDEX`)
/// and its [`Terminal`], so the caller can enrich the host output with the
/// `tap_internal_key` that `rgb_commit` tweaks (and the `tap_bip32_derivation`
/// the tweak persistence needs). Returns the PLAIN (untweaked) P2TR spk —
/// `rgb_commit` rewrites it to the committed key. Pinned, NOT advanced; see
/// [`TAPRET_HOST_INDEX`].
fn pinned_tapret_host_addr(wallet: &MakerWallet) -> Result<(ScriptPubkey, Terminal), RgbError> {
    let terminal = Terminal::new(RgbKeychain::Tapret, NormalIndex::from(TAPRET_HOST_INDEX));
    let derived = wallet
        .wallet()
        .descriptor()
        .derive(terminal.keychain, terminal.index)
        // First yield is the plain `TaprootKeyOnly` key (the untweaked host);
        // any further yields are stored tweaks. We want the plain one.
        .next()
        .ok_or_else(|| {
            RgbError::TransferBuild("descriptor produced no tapret host script".to_owned())
        })?;
    Ok((derived.to_script_pubkey(), terminal))
}

/// Mark the maker-owned P2TR output at `host_spk` as the tapret commitment host,
/// enriching it with the `tap_internal_key`/`tap_bip32_derivation` that
/// `rgb_commit` tweaks to embed the commitment (mirrors [`enrich_psbt_input`],
/// but for an output). Without the internal key `rgb_commit` errors with
/// "taproot output doesn't specify internal key".
fn enrich_tapret_host(
    psbt: &mut Psbt,
    host_spk: &ScriptPubkey,
    descriptor: &RgbDescr,
    terminal: Terminal,
) -> Result<(), RgbError> {
    let derived = descriptor
        .derive(terminal.keychain, terminal.index)
        .next()
        .ok_or_else(|| RgbError::TransferBuild("descriptor produced no host script".to_owned()))?;
    let host = psbt
        .outputs_mut()
        .find(|o| o.script == *host_spk)
        .ok_or_else(|| {
            RgbError::TransferBuild("tapret host output missing after assembly".to_owned())
        })?;
    host.tap_internal_key = derived.to_internal_pk();
    host.tap_bip32_derivation = descriptor.xonly_keyset(terminal);
    host.set_tapret_host()
        .map_err(|e| RgbError::TransferBuild(format!("set tapret host: {e:?}")))?;
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
    /// Terminal of `maker_payout_spk` — lets `assemble_unsigned_psbt` enrich the
    /// tapret host output with the internal key `rgb_commit` tweaks.
    maker_payout_terminal: Terminal,
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
    // RGB-change seal (bound post-sort in `build_buy_transition`). Its terminal
    // is captured so the host output can be enriched with its internal key.
    let (maker_payout_spk, maker_payout_terminal) = pinned_tapret_host_addr(wallet)?;
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
        maker_payout_terminal,
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
    // Backstop: never emit an overspending PSBT. If the taker's selected inputs
    // don't cover gross + fee + anchor, reject here with a clear error rather than
    // letting `saturating_sub` silently zero the change (which the counterparty's
    // signer then rejects with "Outputs spends more than inputs amount").
    let required = gross_btc_sats
        .saturating_add(actual_fee_sats)
        .saturating_add(taker_anchor_sats);
    if taker_btc_total < required {
        return Err(RgbError::TransferBuild(format!(
            "taker funding {taker_btc_total} sats < required {required} \
             (gross {gross_btc_sats} + fee {actual_fee_sats} + anchor {taker_anchor_sats})"
        )));
    }
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
        enrich_psbt_input(
            &mut psbt,
            i,
            &inputs.descriptor,
            mi.terminal,
            mi.value,
            Some(&mi.script),
        )?;
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
    enrich_tapret_host(
        &mut psbt,
        &inputs.maker_payout_spk,
        &inputs.descriptor,
        inputs.maker_payout_terminal,
    )?;
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

    // Persist the tapret tweak so the maker's wallet recognises the host output
    // whose taproot key `rgb_commit` just rewrote. Without it the maker loses
    // track of the RGB it parks on the host (buy change / sell receive) — a
    // tweaked output is no longer a plain derived address, so a UTXO scan misses
    // it. Mirrors rgb-api's `pay.rs`; gated on the host-on-change flag set in
    // `assemble_*`.
    persist_tapret_tweak(wallet, psbt);

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
    /// Actual on-chain scriptPubkey (from the wallet's coin cache) so
    /// `enrich_psbt_input` picks the right derived script when this BTC input is
    /// a reused/tweaked tapret output (e.g. a prior buy payout).
    script: ScriptPubkey,
}

/// Match each maker-declared BTC outpoint against the wallet's UTXO set,
/// returning the bp outpoint + value + terminal for each. Errors if any
/// outpoint isn't in the wallet (the maker said it owns a UTXO it doesn't).
fn resolve_maker_btc_inputs(
    wallet: &MakerWallet,
    maker_btc_inputs: &[(Outpoint, TxOut)],
) -> Result<Vec<MakerBtcInput>, RgbError> {
    // Authoritative on-chain spk per coin (the declared txout can be synthetic in
    // tests, and the base derived address won't match a tweaked host) — same
    // source as `resolve_maker_inputs`.
    let spk_by_outpoint: std::collections::HashMap<BpOutpoint, ScriptPubkey> = wallet
        .wallet()
        .coins()
        .map(|c| (c.outpoint, c.address.addr.script_pubkey()))
        .collect();
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
        let script = spk_by_outpoint.get(&utxo.outpoint).cloned().ok_or_else(|| {
            RgbError::TransferBuild(format!("no cached scriptPubkey for maker BTC outpoint {want}"))
        })?;
        resolved.push(MakerBtcInput {
            outpoint: utxo.outpoint,
            value: utxo.value,
            terminal: utxo.terminal,
            script,
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
    /// Terminal of `maker_receive_spk` — enriches the tapret host output with the
    /// internal key `rgb_commit` tweaks.
    maker_receive_terminal: Terminal,
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
    let (maker_receive_spk, maker_receive_terminal) = pinned_tapret_host_addr(wallet)?;
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
        maker_receive_terminal,
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
    // Backstop (mirror of the buy side): the maker must fund the gross payout +
    // its own RGB-receive anchor. The fee is the taker's (netted from its payout),
    // so it's not part of the maker's requirement. Reject rather than overspend.
    let maker_required = gross_btc_sats.saturating_add(SEAL_ANCHOR_SATS);
    if maker_btc_total < maker_required {
        return Err(RgbError::TransferBuild(format!(
            "maker BTC {maker_btc_total} sats < required {maker_required} \
             (gross {gross_btc_sats} + anchor {SEAL_ANCHOR_SATS})"
        )));
    }
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
            Some(&mi.script),
        )?;
    }

    // Tapret host + canonical ordering. The maker's RGB-receive output (P2TR, on
    // the Tapret keychain) hosts the RGB MPC commitment in its taproot tweak —
    // no `OP_RETURN`, so the swap looks like an ordinary Taproot spend.
    // `set_rgb_tapret_host_on_change` lets `commit_and_consign` persist the tweak
    // so the maker can recognise and spend the tweaked receive output.
    psbt.set_rgb_close_method(CloseMethod::TapretFirst);
    psbt.set_rgb_tapret_host_on_change();
    enrich_tapret_host(
        &mut psbt,
        &inputs.maker_receive_spk,
        &inputs.descriptor,
        inputs.maker_receive_terminal,
    )?;
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
    finalize_psbt_in_place(&mut psbt, descriptor)
}

/// Finalize an already-signed (in-memory) PSBT against `descriptor` and extract
/// the raw witness tx + txid. Shared by the taker-round-trip path
/// ([`finalize_signed_psbt`]) and the unilateral recovery sweep (every input is
/// the maker's, so it signs + finalizes in one shot).
fn finalize_psbt_in_place(psbt: &mut Psbt, descriptor: &RgbDescr) -> Result<(Vec<u8>, Txid), RgbError> {
    psbt.finalize(descriptor);

    // A taker that finalized its input externally (e.g. the browser wallet via
    // @scure/btc-signer) sets only `final_witness` — for a native-segwit/taproot
    // input the empty scriptSig is correctly omitted per BIP-174. But this psbt
    // crate's `extract()` unconditionally `.expect()`s `final_script_sig`
    // (data.rs `to_signed_txin`), so it panics on such inputs even though they're
    // fully finalized. Backfill an empty scriptSig wherever a witness is present
    // but the scriptSig isn't. (The rust taker's `finalize` fills both, which is
    // why the regtest CLI path never hit this.)
    for input in psbt.inputs_mut() {
        if input.final_witness.is_some() && input.final_script_sig.is_none() {
            input.final_script_sig = Some(SigScript::empty());
        }
    }

    if !psbt.is_finalized() {
        return Err(RgbError::FinalizeFailed(
            "PSBT has unfinalized inputs after finalize".to_owned(),
        ));
    }

    let witness_id = psbt.txid();
    let tx = psbt
        .extract()
        .map_err(|e| RgbError::FinalizeFailed(format!("extract signed tx: {e}")))?;
    Ok((tx.consensus_serialize(), witness_id))
}

/// An on-chain output the maker controls but bp-wallet's UTXO cache doesn't
/// track — its `(value, scriptPubkey)` come from the chain (electrum) and its
/// `terminal` is recovered by matching the spk against descriptor derivations.
/// This is how the recovery sweep addresses the maker's stranded tapret outputs.
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
    for (i, mi) in rgb_inputs.iter().chain(std::iter::once(&btc_input)).enumerate() {
        enrich_psbt_input(&mut psbt, i, &descriptor, mi.terminal, mi.value, Some(&mi.script))?;
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
/// out of [`commit_and_consign`] so the unilateral self-send paths can reuse it.
fn persist_tapret_tweak(wallet: &mut MakerWallet, psbt: &mut Psbt) {
    if psbt.rgb_tapret_host_on_change() {
        let tweak = psbt
            .dbc_output::<TapretProof>()
            .and_then(|o| Some((o.terminal_derivation()?, o.tapret_commitment().ok()?)));
        if let Some((terminal, commitment)) = tweak {
            wallet.wallet_mut().descriptor_mut(|wd| {
                let _ = wd.with_descriptor_mut::<()>(|descr| {
                    descr.add_tapret_tweak(terminal, commitment);
                    Ok(())
                });
            });
        }
    }
}

/// Build, sign, and finalize a unilateral RGB **denomination-ladder split** for a
/// SINGLE contract: spend one fat maker RGB UTXO (`rgb_source`) plus one BTC
/// `fee_input`, and re-allocate the asset across `rungs` fresh **keychain-0**
/// witness-vout outputs (the ladder pieces, largest→smallest), with the
/// remainder (`sum_inputs - Σrungs`) parked back on the pinned tapret host (which
/// also carries the MPC commitment). Returns the raw witness tx + txid.
///
/// This is the de-risking spike for [[project_maker_tapret_change_stranding]]:
/// the rung outputs are PLAIN P2TR on the OUTER keychain that bp-wallet rescans
/// normally — only the single host sits on the tweaked k10/0 path. It is also the
/// single-contract kernel that the multi-contract `build_rebalance_tx` (one tx
/// for every asset + BTC) generalises. The remainder MUST be > 0 (the planner
/// always leaves change) so the host carries a real allocation.
#[allow(clippy::too_many_arguments)]
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
        enrich_psbt_input(&mut psbt, i, &descriptor, mi.terminal, mi.value, Some(&mi.script))?;
    }

    psbt.set_rgb_close_method(CloseMethod::TapretFirst);
    psbt.set_rgb_tapret_host_on_change();
    enrich_tapret_host(&mut psbt, &host_spk, &descriptor, host_terminal)?;
    psbt.sort_outputs_by(|o| !o.is_tapret_host())
        .expect("PSBT outputs are modifiable");
    psbt.complete_construction();

    // Build the transition: consume the source allocation, assign each rung to its
    // (post-sort) vout seal, remainder to the host vout seal.
    let main =
        build_split_transition(wallet, contract_id, &rgb_in, &psbt, &host_spk, &rung_spks, rungs)?;
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
            .add_fungible_state_raw(assignment_type, BuilderSeal::Revealed(seal), Amount::from(*amount))
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
        enrich_psbt_input(&mut psbt, i, &descriptor, mi.terminal, mi.value, Some(&mi.script))?;
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
    enrich_psbt_input(&mut psbt, 0, &descriptor, src.terminal, src.value, Some(&src.script))?;
    psbt.complete_construction();
    sign_maker_inputs(&mut psbt, account)?;
    let (raw_tx, txid) = finalize_psbt_in_place(&mut psbt, &descriptor)?;
    Ok((raw_tx, txid))
}
