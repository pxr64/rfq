# Atomic-swap PSBT — implementation design

This doc is the **implementation-level** companion to
[`docs/swap-flows.md`](swap-flows.md). Swap-flows specifies *what* the two
parties do at each round trip; this doc specifies *which rgb-api / bp-std
primitives* `LibRgbBackend` calls to make those round trips real, the
SIGHASH model that lets a two-party PSBT survive a sequential signing pipeline,
and the design questions that have to be resolved before any code lands.

Scope: the three stubbed methods on `RgbBackend` that `LibRgbBackend` must
implement to un-mock the swap path.

- `create_swap_psbt_buy(rgb_invoice, amount, &maker_rgb_utxos) -> SwapTransfer`
- `create_swap_psbt_sell(&consignment_info, &taker_rgb_prevouts, &maker_btc_inputs, …) -> SwapTransfer`
- `finalize_after_taker_sign(signed_psbt_base64, original_consignment_base64) -> FinalizedSwap`

## Why this is composition, not a port

`rgb-cmd 0.11.1-rc.6` is the canonical example we've mirrored everywhere
else (`create_invoice`, `validate_incoming_consignment`). It does **not** have
an atomic-swap command — that's literally what we're building. Its `Transfer`
command calls `wallet.pay(invoice, params)`: a single-call **unilateral**
transfer where one wallet supplies every input, pays every output, and signs
SIGHASH_ALL across the whole PSBT.

Atomic swap PSBTs are two-party. There's no convenience wrapper for that
shape; `pay()` covers the unilateral case and stops. But every primitive we
need *is* publicly available — bp-std exposes `Psbt::new` + manual
input/output construction + per-input `sighash_type`, bp-wallet's `psbt.sign(&signer)`
naturally signs only inputs whose keys the signer holds (so partial signing
falls out for free), and rgb-api exposes the RGB transition builder + the
commitment APIs (`stock.transition_builder_raw`, `psbt.rgb_embed`,
`psbt.rgb_commit`, `stock.consume_fascia`, `stock.transfer`).

So Group B is **glue code** that reaches below `pay()` and composes those
primitives into the two-party shape. The work is wiring + design decisions
(SIGHASH model, when to commit the fascia, when to emit the consignment),
not extending the library.

## Inputs / outputs / signing matrix

The shape each side's PSBT must have at every stage of the protocol.

### Buy side

```
PSBT after create_swap_psbt_buy (returned to taker):
  inputs:   [ maker_rgb_in_1, …, maker_rgb_in_N ]   ← maker-signed, SIGHASH_ANYONECANPAY|SINGLE
  outputs:  [ taker_rgb_seal, maker_rgb_change?, rgb_commitment ]
  sighashes: maker inputs use ANYONECANPAY so taker-added inputs don't invalidate
  witness_txid: deferred (taker still adds BTC inputs)

PSBT after taker /sign (handed back to maker):
  inputs:   [ maker_rgb_in_*, taker_btc_in_1, …, taker_btc_in_M ]
  outputs:  [ taker_rgb_seal, maker_rgb_change?, rgb_commitment, taker_btc_change? ]
  sighashes: all inputs fully signed; witness_txid now fixed
```

### Sell side

```
PSBT after create_swap_psbt_sell (returned to taker):
  inputs:   [ taker_rgb_in_1, …, taker_rgb_in_N,   ← unsigned (taker signs at /sign)
              maker_btc_in_1, …, maker_btc_in_M ]  ← maker-signed, SIGHASH_ALL
  outputs:  [ maker_rgb_seal, taker_btc_payout, maker_btc_change, taker_rgb_change? ]
  sighashes: maker SIGHASH_ALL works because all inputs present at build time
  witness_txid: stable from here — pre-published as expected_witness_txid

PSBT after taker /sign (handed back to maker):
  inputs:   same set, taker RGB inputs now signed
  outputs:  unchanged
  witness_txid: must equal expected_witness_txid (the maker rejects if it drifts)
```

The asymmetry is real: buy side needs `ANYONECANPAY` because the input set
grows after the maker signs; sell side can use `SIGHASH_ALL` everywhere
because both parties' inputs are present at PSBT-build time.

## The PSBT lifecycle per method

### `create_swap_psbt_buy`

The intent of this method is to produce **a maker-signed half-PSBT** the
taker can extend.

```
1. invoice    = RgbInvoice::from_str(rgb_invoice)
2. contract   = invoice.contract                                  // taker's RGB receive
3. wallet     = self.load_wallet()                                // maker side
4. stock      = wallet.stock_mut()
5. // Resolve maker_rgb_utxos to bp-std prevout data (script_pubkey + value_sats)
   //   — walk wallet.coins() / wallet.utxos() for each Outpoint;
   //   — error if any isn't in the wallet (the inventory store + the wallet
   //     should agree, but defensively check).
6. // Build the RGB Batch
   builder = stock.transition_builder_raw(contract_id, default_transition_type)
   for each (maker_rgb_outpoint, allocation_state) in resolved_inputs:
       builder = builder.add_input(opout, state)
   builder = builder.add_fungible_state_raw(assignment_type,
                                            BuilderSeal::Concealed(invoice_seal),
                                            amount)
   // RGB change goes back to a fresh maker-controlled seal if maker over-selected
   if sum_inputs > amount:
       maker_change_seal = GraphSeal::with_blinded_vout(change_vout, rand::random())
       builder = builder.add_fungible_state_raw(..., maker_change_seal, sum_inputs - amount)
   main = builder.complete_transition()
   batch = Batch { main, extras: empty }
7. // Build the bp-std PSBT manually
   psbt = Psbt::new(version)
   for each (outpoint, txout) in resolved_inputs:
       psbt.add_input(outpoint, txout, sighash_type = ANYONECANPAY | SINGLE_or_NONE_or_ALL)
   psbt.add_output(taker_rgb_seal_script, 0)         // RGB-only output, dust
   if change > 0:
       psbt.add_output(maker_rgb_change_seal_script, 0)
   psbt.add_output(rgb_commitment_output)            // opret or tapret
8. psbt.set_rgb_close_method(close_method)
9. psbt.rgb_embed(batch)?
10. // Sign maker RGB inputs with the chosen SIGHASH
    wallet.sign_psbt_inputs(&mut psbt, maker_input_indices, sighash_flag)
11. // DON'T call psbt.rgb_commit() yet — that produces the fascia + witness txid,
    //   which is only stable after the taker's inputs are present.
12. Emit consignment = stock.transfer(contract_id, ..., witness_id = None)?
    // Open question: rgb-api's transfer requires a witness_id (see pay.rs:563).
    // For buy side we don't have one yet. See "design decision: deferred witness id" below.
13. Return SwapTransfer {
        partial_psbt: base64(psbt.serialize()),
        consignment: Some(base64(transfer.save())),
        expected_witness_txid: None,
    }
```

### `create_swap_psbt_sell`

Produces a **fully-input-committed PSBT** (so the witness txid is stable)
where the maker has signed its BTC inputs and the taker has only its RGB
inputs left to sign.

```
1. wallet  = self.load_wallet()
2. stock   = wallet.stock_mut()
3. // Maker RGB receive seal from maker_rgb_invoice
   invoice = RgbInvoice::from_str(maker_rgb_invoice)
   maker_seal = invoice.beneficiary.into_inner()    // BlindedSeal expected
4. // Build the RGB Batch — same pattern as buy, but the "main" transition
   //   is operating on the TAKER's RGB inputs (already in consignment_info)
   //   delivering to maker_seal
   builder = stock.transition_builder_raw(contract_id, default_transition_type)
   for each outpoint in consignment_info.outpoints:
       // Pull the opout + state from the consignment we already validated
       // (we have `validated` in scope from deliver_consignment, but the trait
       //  method only has consignment_info — see "design decision: opout
       //  re-derivation" below)
       builder = builder.add_input(opout, state)
   builder = builder.add_fungible_state_raw(assignment_type,
                                            BuilderSeal::Concealed(maker_seal),
                                            consignment_info.total_amount)
   if rgb_change_invoice.is_some() && consignment_info.total_amount > requested_amount:
       // Taker over-consigned; route the surplus to taker's change invoice
       change_seal = parse(rgb_change_invoice).beneficiary
       builder = builder.add_fungible_state_raw(..., change_seal, surplus)
   main = builder.complete_transition()
   batch = Batch { main, extras: empty }
5. // Build the bp-std PSBT
   psbt = Psbt::new(version)
   // taker RGB inputs (unsigned, no sighash flag yet — taker chooses)
   for each (outpoint, txout) in taker_rgb_prevouts:
       psbt.add_input(outpoint, txout, sighash_type = ALL)
   // maker BTC inputs (will be signed below)
   for each (outpoint, txout) in maker_btc_inputs:
       psbt.add_input(outpoint, txout, sighash_type = ALL)
   psbt.add_output(maker_rgb_seal_script, 0)
   psbt.add_output(btc_payout_addr_script, gross_btc_sats - actual_fee_sats)
   psbt.add_output(maker_btc_change_script, sum(maker_btc_inputs.values) - gross_btc_sats)
   if rgb_change_invoice.is_some():
       psbt.add_output(taker_rgb_change_seal_script, 0)
   psbt.add_output(rgb_commitment_output)
6. psbt.set_rgb_close_method(close_method)
7. psbt.rgb_embed(batch)?
8. // Sign ONLY the maker's BTC inputs — the taker signs its RGB inputs at /sign
   wallet.sign_psbt_inputs(&mut psbt, maker_btc_input_indices, SIGHASH_ALL)
9. // Pre-compute the witness txid for the SwapTransfer
   expected_witness_txid = psbt.txid()
10. // Don't call rgb_commit() yet either — the fascia is finalized once
    //    *every* input is signed (because rgb_commit needs the txid stable,
    //    which it already is, but it also produces a fascia we hand to
    //    consume_fascia together with the eventually-broadcast tx)
11. Return SwapTransfer {
        partial_psbt: base64(psbt.serialize()),
        consignment: None,                         // taker built the consignment
        expected_witness_txid: Some(expected_witness_txid),
    }
```

### `finalize_after_taker_sign`

Both sides converge here. The taker has signed; the maker turns the
fully-signed PSBT into a broadcastable witness tx and emits the
witness-extended consignment.

```
1. psbt    = Psbt::deserialize(base64::decode(signed_psbt_base64))
2. consig  = Transfer::load(base64::decode(original_consignment_base64))
   // (consig is `None` on sell side — maker built the PSBT from the taker's
   //  consignment but never produced one of its own. Same parameter; on sell
   //  side it's the taker-supplied original. See "design decision: which
   //  consignment finalizes" below.)
3. fascia  = psbt.rgb_commit()
4. witness_id = psbt.txid()
5. // Update the maker's Stock with the new state
   stock.consume_fascia(fascia, witness_id)
6. // Generate the witness-extended consignment to hand to the receiver
   final_transfer = stock.transfer(contract_id, beneficiary_seals, [], [], Some(witness_id))
7. // Extract the broadcastable witness tx
   tx_bytes = psbt.extract_tx().serialize()
8. Return FinalizedSwap {
       raw_tx: tx_bytes,
       witness_txid: witness_id.to_string(),
       final_consignment_base64: base64(final_transfer.save()),
   }
```

## Design decisions

These are choices the implementation must make explicitly. Pick them here, not
in commit messages.

### D1 — Buy-side maker SIGHASH

The maker signs its RGB inputs before the taker adds BTC inputs. The signature
must commit to *its own input* but **not** to the rest of the input set. That's
`SIGHASH_ANYONECANPAY` flagged on each maker input. The output set is fixed at
PSBT-build time (taker adds inputs + change but the maker's outputs are
final), so the output-side flag is **`SIGHASH_ALL`** — the maker is happy to
commit to the full output structure it built. Combined flag per maker input:
**`SIGHASH_ALL | SIGHASH_ANYONECANPAY`**.

Open: does bp-std's `psbt::Input` let us set `sighash_type` per input? See
*Known unknowns* below.

### D2 — Sell-side SIGHASH

Both sides' inputs are present when the maker signs. The maker uses
**`SIGHASH_ALL`** on its BTC inputs. The taker also uses `SIGHASH_ALL` on its
RGB inputs at /sign time. Standard PSBT signing, no special flags. The
bait-and-switch guard at /sign (already in `MockMaker::submit_signed_psbt_sell`)
catches any taker tampering with the input set; SIGHASH_ALL by itself does
*not* prevent input swaps (the maker's signature stays valid if taker swaps
its own inputs around) — that's why we cross-check the consignment outpoints
appear in the signed PSBT.

### D3 — When `rgb_commit` is called

`Psbt::rgb_commit` produces a `Fascia` (the anchor commitment data) and
requires the PSBT's txid to be stable (i.e., all inputs / outputs final).

- **Buy side `create_swap_psbt_buy`**: txid is NOT stable (taker will add
  inputs). Do **not** call `rgb_commit`. Pass the embedded-but-uncommitted
  PSBT to the taker. The commit happens in `finalize_after_taker_sign`.
- **Sell side `create_swap_psbt_sell`**: txid IS stable. We *could* commit
  here. For symmetry with buy side and to keep `finalize_after_taker_sign`
  the single point that materializes the fascia + consignment, we **defer**:
  embed in create_swap_psbt_sell, commit in finalize_after_taker_sign.

### D4 — Consignment lifecycle

Buy side: the maker builds its own consignment in `create_swap_psbt_buy`
(via `stock.transfer(..., witness_id = None)` — but see unknown U1) and
hands it to the taker. The taker validates it locally before signing.
At `finalize_after_taker_sign`, the witness-extended consignment is the
same one with the now-known witness tx attached.

Sell side: the taker built the consignment and sent it in `/consignment`.
`create_swap_psbt_sell` doesn't produce one (`SwapTransfer.consignment =
None`). At `finalize_after_taker_sign` the **maker** produces the
witness-extended consignment for *its own* future use (the maker is the
RGB receiver). The taker doesn't need a final consignment back — they
sourced the RGB.

The `original_consignment_base64` parameter on `finalize_after_taker_sign`:

- Buy side: the consignment the maker emitted earlier (used to identify
  which transfer to extend).
- Sell side: the taker's original consignment (used to identify the same).

So both sides pass *something* meaningful into this parameter; the
implementation treats them symmetrically (replay through Stock to get the
witness-extended form out).

### D5 — `Beneficiary` shape on the invoice

`rgb-api`'s `pay.rs` (~ln 275) branches on `invoice.beneficiary` between
`BlindedSeal` (no BTC output for the RGB receive) and `WitnessVout` (a real
bitcoin address for the RGB receive, used in pay2vout RGB-on-witness-output
scenarios).

For the swap protocol our `create_invoice` always emits `BlindedSeal` (see
[`lib_backend.rs:227`](../crates/rfq-rgb/src/lib_backend.rs#L227)). So both
buy and sell `create_swap_psbt_*` can assume `BlindedSeal` and reject the
WitnessVout case. Documented constraint.

### D6 — RGB change handling

- **Buy side**: maker may over-select RGB inputs. The surplus goes to a
  fresh `GraphSeal::with_blinded_vout(maker_change_vout, rand::random())`
  (mirrors `pay.rs:356`). The change vout is the maker's BTC change vout
  *if the maker had one*, but on buy side the maker has no BTC change vout
  (taker funds BTC). So we add a dedicated RGB-change output and pin the
  seal to its vout. Adds one more output to the buy PSBT.
- **Sell side**: surplus only happens if the taker over-consigns. The
  taker's `rgb_change_invoice` (optional, on `SwapLeg::Sell`) tells us
  where to send it; if absent and over-consign happens, reject the
  consignment in `create_swap_psbt_sell`.

## Known unknowns

Things we don't know from reading source alone — they need a build-iterate
cycle to resolve, ideally with the regtest stack up so we can verify behavior
end-to-end.

### U1 — `stock.transfer(..., witness_id = None)` viability on buy side

`pay.rs:563` calls `stock.transfer(contract_id, ..., Some(witness_id))` with
the witness id always known. For buy side we don't have one until taker
signs. The Stash's `transfer()` API probably requires it. Possible answers:

- **(a)** Pass a placeholder `witness_id` (e.g., a zero-Txid or the PSBT's
  current-state txid even though it'll change), then re-issue the consignment
  in `finalize_after_taker_sign` once the real witness_id is known.
- **(b)** Defer the consignment entirely to `finalize_after_taker_sign` and
  hand the taker only the half-signed PSBT (taker validates the RGB
  transition off the embedded fascia after `rgb_commit` happens at finalize
  — but then the taker can't pre-validate before signing).
- **(c)** Reach for a different rgb-api primitive that emits a consignment
  for a not-yet-final tx. Need to grep the stash provider for one.

Decide via experiment in implementation.

### U2 — bp-std PSBT per-input SIGHASH flag

D1 needs `SIGHASH_ALL | ANYONECANPAY` on the maker's RGB inputs in buy side.
bp-std's `psbt::Input` likely has a `sighash_type: Option<SighashType>` field
(it's standard PSBT BIP 174). Need to confirm the exact API + that
`rgb_embed` / `rgb_commit` respect it.

### U3 — Manual PSBT input construction without coin-selection

`pay.rs` always calls `self.construct_psbt(prev_outpoints, beneficiaries,
params.tx)` which is bp-wallet's `PsbtConstructor` trait. That trait does
coin-selection internally. We want to skip coin-selection entirely (caller
supplied every input). Need to find the bp-std `Psbt::new` / `add_input`
path that bypasses `PsbtConstructor`.

### U4 — `psbt.rgb_commit()` shape with mixed-party inputs

Does `rgb_commit` care whether all inputs are signed? It produces a
`Fascia` with the witness id (psbt.txid()) — so it needs the txid stable
(all inputs and outputs present). Per-input *signatures* shouldn't affect
the txid in segwit (witness data isn't in the txid). So `rgb_commit` at
finalize time should work whether or not the taker's RGB inputs (sell
side) were signed at PSBT-build time. Confirm by experiment.

### U5 — `stock.consume_fascia` post-finalize state mutation

`pay.rs:559` calls `stock.consume_fascia(fascia, FasciaResolver)` to advance
the stash with the new transition before broadcast confirms. For atomic
swap finalize, we do the same — but at *finalize* time, not at PSBT-build
time. Need to check that calling consume_fascia *after* a separate
embed-only step (`rgb_embed` without `rgb_commit`) at PSBT-build is valid.

## Implementation phases

When we pick this up:

- **Phase B1 — Plumbing**. Locate bp-std's `Psbt::new`/`add_input` /
  per-input sighash; resolve U2, U3. Add any missing rfq-rgb deps (likely
  none — bp-std is direct, psrgbt is transitive).
- **Phase B2 — `create_swap_psbt_buy`**. Implement against decisions D1, D3,
  D4, D5, D6; resolve U1 via experiment. End state: returns a SwapTransfer
  with a partial PSBT + buy-side consignment.
- **Phase B3 — `create_swap_psbt_sell`**. Implement against D2, D3, D4, D5,
  D6. End state: returns a SwapTransfer with a fully-input-committed PSBT +
  pre-computed witness txid + no consignment of its own.
- **Phase B4 — `finalize_after_taker_sign`**. Implement against the spec
  above; resolve U4, U5 by running. End state: returns FinalizedSwap with
  broadcastable bytes + witness-extended consignment.
- **Phase B5 — Tests**. Flip the two `#[ignore]`d stub-assertion tests in
  [`tests/cli.rs`](../crates/rfq-rgb/tests/cli.rs) (currently asserting
  `Err(TransferBuild)` / `Err(FinalizeFailed)`) to assert `Ok` + structural
  shape. Add a third test for `create_swap_psbt_sell`. Add a small
  end-to-end test that drives the whole buy or sell round trip with two
  cooperating `LibRgbBackend`s (maker + taker) on the regtest stack.

Each phase is its own session-sized commit. B1 unblocks everything else.

## What it doesn't cover

This doc designs the **happy path**. Failure handling at each step
(rgb_embed errors, sign failures, broadcast rejection) follows the same
mark_broadcast_failed pattern `MockMaker::submit_signed_psbt_*` already
implements — no design needed, port the same handling to the lib-backed
methods.

Multi-party PSBT serialization round-trip robustness (the PSBT changes hands
twice on buy side, once on sell side) needs round-trip tests as part of B5.
