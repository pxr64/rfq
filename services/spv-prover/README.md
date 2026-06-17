# spv-prover

Standalone **SPV proof-pack prover** (RFQIP-1 §2) — the *untrusted producer* of
self-verifying Bitcoin merkle-inclusion bundles that thin clients (the colorex-wallet
browser extension, an ICP canister) verify locally to confirm an RGB consignment's witness
transactions are actually mined, without running a node and without trusting any server.

Because a proof-pack is **self-verifying**, this service needs no trust: a lying or faulty
prover can only cause a verification *failure* downstream, never a false accept. That is
what lets it run separately from the broker and the maker, be cached, and be replicated.

It deliberately does **not** decode RGB consignments — callers already obtain the
witness-txid set from their own RGB crypto-validation (`tx_ord_map`), so the wire input is
just txids. This keeps the prover free of the heavy RGB stack; the verifier core lives in
`rfq-consignment` (`verify_pack` + `HeaderSource`).

## Run

```
SPV_PROVER_LISTEN=127.0.0.1:3010 \
SPV_ELECTRUM_URL=tcp://127.0.0.1:60001 \
SPV_NETWORK=regtest \
cargo run -p spv-prover
```

| env | default | meaning |
| --- | --- | --- |
| `SPV_PROVER_LISTEN` | `127.0.0.1:3010` | bind address |
| `SPV_ELECTRUM_URL` | `tcp://127.0.0.1:60001` | electrum/electrs endpoint |
| `SPV_NETWORK` | `regtest` | network label stamped into packs |
| `SPV_CACHE_DIR` | *(unset)* | persistent anchor cache dir; absent → memory-only |

## API

`GET /health` → `ok`.

`POST /spv/proof-pack`

```json
{ "txids": ["<witness-txid-hex>", "..."] }
```

→

```json
{
  "pack": {
    "version": 1,
    "network": "regtest",
    "anchors": { "<txid>": { "block_hash": "..", "block_height": 162, "tx_index": 0, "merkle_proof": [".."] } },
    "headers": { "<block_hash>": "<raw-80-byte-header-hex>" }
  },
  "unproven": ["<txid that isn't mined / unknown>"]
}
```

`unproven` lists txids with no inclusion proof (not mined / not found); a verifier rejects
those as `MissingAnchor`. The `headers` map is a convenience for header-less clients — a
client with its own header chain (wallet checkpoint, ICP-native headers) MUST ignore it and
verify against its own source (RFQIP-1 §3).

## Caching

Buried inclusion proofs are immutable, so anchors are cached **in memory** keyed by txid
(hot tier; a persistent KV tier is a follow-up). A reorg deeper than a shallow tx is the
only correctness caveat, and even then a stale anchor cannot cause a false accept — the
verifier re-checks confirmation depth against its own headers.
