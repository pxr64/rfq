# Proposal: Provenance Consignments — a wallet operation for receiving via counterparty-built transactions

**Status:** Draft proposal for the RGB community — **implemented in colorex** for
the sell-leg flow (`export_provenance` in `rfq-rgb` + `create_transfer` in
rgb-wasm; the maker mints no invoice). This is the live sell-side model in
[swap-flows.md](swap-flows.md); kept here as the design write-up.
**Scope:** RGB wallet capabilities + transfer patterns. Complements, does not
replace, the invoice/`pay` model.

## Abstract

We propose a standard wallet operation — **export a provenance consignment** —
that lets the holder of an RGB allocation authorize a *counterparty* to spend that
allocation inside a transaction the **counterparty builds**, without the holder
issuing an invoice. This is required for any protocol where the RGB **receiver is
not the builder of the anchoring transaction** — most notably the seller's leg of
an atomic swap or any PSBT-coordinated transfer. The invoice model forces such a
receiver to permanently hold a spare "anchor" UTXO purely to host a seal; the
provenance model removes that requirement and is, we argue, the more natural
primitive for coordinated transactions.

This document deliberately contrasts the **invoice pattern** and the proposed
**provenance pattern** so the difference is explicit.

## 1. The standard pattern today: invoices

Canonical RGB transfer:

1. **Receiver** issues an **invoice** carrying a seal — either a **blinded seal**
   (an existing UTXO it controls) or a **witness-vout** seal (`Pay2Vout`, an
   output of the tx that will anchor the transfer).
2. **Sender** runs `pay(invoice)` — builds the anchoring transaction, commits the
   RGB transition to it, and **broadcasts** it.
3. Receiver accepts the resulting consignment.

This is clean **when the sender builds the anchoring transaction** — which is the
normal one-party transfer, and the *buyer's* leg of a swap (buyer receives; the
counterparty builds + broadcasts).

## 2. The gap: when the receiver is *not* the tx builder

In multi-party coordinated transactions — atomic swaps, PSBT coordination — there
is a leg where the RGB **receiver does not build the anchoring tx**; the
counterparty does. On that leg, *neither* seal type gives the receiver a usable
invoice:

- **Witness-vout** is *relative* — "vout N of the tx anchoring this transfer." But
  only the **builder** of that tx can bind it, and the receiver isn't the builder
  (and the tx may not exist yet). The receiver can only witness-vout-bind to a tx
  *it* is constructing, which on this leg is a throwaway, never broadcast.
- **Blinded** is *absolute* — `(txid, vout)`. It needs a confirmed UTXO whose txid
  is already fixed. So the receiver must keep a **separate, RGB-empty anchor UTXO**
  on hand solely to host the invoice seal.

### 2.1 Why a blinded seal can't just target the coordinated tx

A natural objection: *"the txid is known before signing, so blind-commit to the
coordinated tx's output."* Signing/finalization are indeed txid-neutral (witness
data is excluded from the txid). But the **RGB commitment is not** — it is a tapret
tweak on an output (or an opret `OP_RETURN`), and outputs are part of the txid.
Committing the bundle therefore changes the txid, and the ordering is fatal:

```
1. freeze seal           ← the seal is data inside the bundle
2. commit bundle → tweak an output   ← THIS fixes the final txid (T_final)
3. sign / finalize       ← do NOT change T_final
```

A blinded seal must contain `T_final`. But `T_final` only exists after step 2, and
step 2 commits to the bundle that **contains the seal frozen at step 1** —
self-referential, impossible. **Witness-vout escapes this only because its seal
carries no txid**; that is precisely why it is the only seal that can land on the
same tx that carries the commitment — and why a blinded seal always needs a
*different, already-committed* tx (the anchor).

**Consequence:** the receiver-who-isn't-the-builder has no clean seal. The invoice
model forces it to maintain a spare anchor UTXO indefinitely — operational friction
and a liveness dependency for every such transfer.

## 3. The proposed pattern: provenance consignments

Flip the direction. Instead of the receiver issuing an invoice the sender pays,
the **holder exports the provenance of its own allocation**, and the counterparty
consumes it and performs the spend.

Proposed wallet operation:

```
export_provenance(contract_id, outpoints) -> consignment
```

- Produces a consignment that **terminates on the holder's own outpoint(s)** —
  conveying history genesis → … → the allocation at `outpoint`.
- **Spends nothing, touches no bitcoin, references no counterparty seal, needs no
  anchor.** There is no future tx in the artifact, so nothing to resolve.
- Mechanically: re-derive a consignment for an already-settled receive — in
  rgb-std terms, `Stock::transfer(contract, [output_seal], [], [], None)`. The
  witness id **MUST be left unspecified**: a seal's `txid` equals its anchoring
  witness tx *only for witness-vout receives*. For an allocation **received on a
  blinded seal** bound to a pre-existing UTXO, the transition lives on a *different*
  witness tx, so pinning the seal's txid as the witness filter excludes the
  relevant bundle and yields an **empty consignment**. Leaving it unspecified lets
  the stock resolve the bundle from the outpoint and walk the full graph to genesis.
- Works for **any allocation the holder can address by its current `txid:vout`** —
  i.e. all of the holder's own allocations once revealed in its stash, **regardless
  of whether they were received via a blinded or a witness-vout invoice**. The only
  requirement is a known, revealed outpoint (which the holder always has for its own
  seals).

The **counterparty (tx builder)** then:

1. **Accepts** the provenance consignment → its stash now holds the holder's
   allocation and its full history.
2. **Builds** the coordinated transaction spending that allocation, receiving its
   own share via a **witness-vout it binds** (it *is* the builder), and routing any
   change back to a holder-supplied change seal.
3. Obtains the holder's **bitcoin signature** on the relevant input.

## 4. Invoice vs Provenance — the difference, side by side

| | **Invoice** (today) | **Provenance consignment** (proposed) |
|---|---|---|
| Issued by | the **receiver** | the **holder** of the allocation |
| Carries a seal? | yes — blinded or witness-vout | **no** — terminates on the holder's own existing outpoint |
| Needs a spare anchor UTXO? | blinded: **yes** | **no** |
| References a future tx? | witness-vout: yes | no |
| What it authorizes | nothing (sender pays into it) | conveys allocation + provenance; the **spend is authorized by the bitcoin signature** on the coordinated tx |
| Receiver can be a non-builder? | only via a separately-funded anchor | **yes, natively** |
| Wallet support today | universal | **proposed** |
| Mental model | "send RGB to my seal" | "here is my allocation + its history; you spend it" |

## 5. Security

Authorization is **the bitcoin signature** on the coordinated transaction input,
which the holder reviews (decoded balance deltas) before signing — exactly as in
the invoice flow. The provenance consignment is **data, not authority**: holding it
does not let the counterparty move the RGB without the holder's signature. So the
trust model is unchanged from a normal PSBT-coordinated transfer.

## 6. Privacy

**Equivalent to the invoice flow.** Both reveal the holder's allocation history and
input outpoints to the counterparty — inherent to the transfer, and the outpoints
become public on-chain the moment the coordinated tx broadcasts. The counterparty's
receive is a visible output of the coordinated tx in both patterns. The blinded
invoice's only privacy property — hiding the receiver's anchor UTXO from the sender
— protects a UTXO the transaction never uses, so it is effectively vestigial here.
There is no privacy regression.

## 7. Wallet capabilities required

The provenance pattern deliberately keeps a small wallet surface — only **one**
capability is new:

| Capability | Standard today? |
|---|---|
| **List RGB allocations by outpoint** (which of my UTXOs hold which asset) | yes — standard wallet read |
| **`export_provenance(contract, outpoints) → consignment`** | **the one new primitive** this proposal adds |
| **Sign a PSBT** (review decoded deltas, sign own input) | yes — standard |

The holder never co-authors the transaction and never commits an RGB transition
into someone else's PSBT — the counterparty (tx builder) does all of that. This is
what keeps third-party wallet integration light. Contrast the alternative in §8.

## 8. Alternative considered: co-built (interactive) transactions

The other anchor-free option is to have **both parties co-build the transaction**,
Lightning-style: exchange inputs, agree on a skeleton, commit the RGB transition,
exchange signatures. Because the RGB receiver then *is* a co-builder of the real
(broadcast) tx, it can bind a relative witness-vout seal to it — no anchor, no
provenance export.

This works, but it is **strictly heavier** than the provenance pattern and was not
chosen:

- It needs an **interactive transaction-construction protocol** (multiple round
  trips) instead of one consignment over the existing build→sign flow.
- It requires the non-builder to **commit an RGB transition into a shared PSBT** —
  a substantially more complex wallet operation than listing outpoints + exporting
  provenance + signing.
- It therefore presents a **larger integrator surface** for third-party wallets.

Crucially, the bilateral symmetry co-building provides is **not needed** for a
single, atomic transfer with **cleanly separated outputs** (each party's share is
its own output; nothing is shared or contested). Lightning co-builds out of
necessity — a **shared 2-of-2, long-lived channel with adversarial close and
penalty mechanics** — none of which apply to a one-shot transfer. The provenance
pattern reaches the same anchor-free result by having the **builder do all the RGB
work** and the **holder contribute the minimum** (provenance + a signature). We
therefore treat co-building as the right tool for *channels* and provenance as the
right tool for *one-shot coordinated transfers*.

## 9. Open questions for the RGB community

- Should `export_provenance` be a **first-class wallet API** and/or part of the
  RGB invoicing spec, alongside `pay`/invoice?
- Is there a **standard wire encoding** for "the outpoints being authorized" to
  accompany the consignment? The counterparty **cannot derive them from the
  consignment** — terminals carry only *secret* (blinded) seals, so explicit
  witness-vout outpoints never appear there — so an explicit list alongside the
  consignment is required, not optional.
- How does this relate to existing/anticipated **RGB swap and PSBT-coordination
  proposals**? We believe "holder-exports-provenance" vs "receiver-anchors-a-seal"
  is a general fork that any RGB DEX or coordinated-tx protocol must resolve on the
  leg where the receiver isn't the builder.
