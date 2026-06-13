
use super::*;
use rfq_btc::MockBitcoinClient;
use rfq_rgb::MockRgbBackend;
use rfq_types::{AssetId, AssetKind, BitcoinNetwork, ReservationId, RfqId, Side};

fn synth_outpoint(idx: usize) -> Outpoint {
    Outpoint::new(format!("{idx:064x}"), 0)
}

fn maker_id() -> MakerId {
    MakerId("maker-1".to_owned())
}

fn asset() -> AssetId {
    AssetId {
        network: BitcoinNetwork::Regtest,
        kind: AssetKind::Rgb20,
        id: "rgb:eejuoPHh-agACtkj-6j2JkSs-cI4PIwm-CzKGJ7v-1s~IrX0".to_owned(),
    }
}

fn quote_asset() -> AssetId {
    AssetId {
        network: BitcoinNetwork::Regtest,
        kind: AssetKind::Btc,
        id: "btc".to_owned(),
    }
}

/// Prices the test `asset()` at 101 sats/unit on both sides with a generous
/// size — matches the flat price the removed fallback used to give, so the
/// quote-flow tests still resolve (no order now means *decline*, so the
/// test maker must carry an explicit policy).
fn default_test_policy() -> PricePolicy {
    PricePolicy::from_entries(
        [Side::Buy, Side::Sell]
            .into_iter()
            .map(|side| PriceEntry {
                asset_id: asset().id,
                side,
                price_sats_per_unit: 101,
                max_size: 1_000_000_000,
            })
            .collect(),
    )
}

#[test]
fn price_policy_resolves_match_decline_and_no_order() {
    let policy = PricePolicy::from_entries(vec![PriceEntry {
        asset_id: asset().id,
        side: Side::Buy,
        price_sats_per_unit: 250,
        max_size: 1_000,
    }]);

    // Matching (asset, side) within size → the order's price.
    assert!(matches!(
        policy.unit_price(&asset(), &Side::Buy, 1_000),
        PriceLookup::Price(250)
    ));
    // Over the order's size → decline.
    assert!(matches!(
        policy.unit_price(&asset(), &Side::Buy, 1_001),
        PriceLookup::Decline
    ));
    // Order exists for Buy but not Sell → no order for Sell → decline.
    assert!(matches!(
        policy.unit_price(&asset(), &Side::Sell, 10),
        PriceLookup::NoOrder
    ));
    // No order for this asset at all → decline.
    assert!(matches!(
        policy.unit_price(&quote_asset(), &Side::Buy, 10),
        PriceLookup::NoOrder
    ));
}

fn utxo_with_amount(idx: usize, amount: u64) -> RgbInventoryUtxo {
    RgbInventoryUtxo {
        outpoint: synth_outpoint(idx),
        asset_id: asset(),
        amount,
        btc_sats: 0,
    }
}

fn utxo() -> RgbInventoryUtxo {
    utxo_with_amount(0, 100)
}

fn inv_utxo(idx: usize, amount: u64, status: InventoryStatus) -> InventoryUtxo {
    let now = now_ms();
    InventoryUtxo {
        outpoint: synth_outpoint(idx),
        asset_id: asset(),
        amount,
        btc_sats: 0,
        status,
        created_at_ms: now,
        updated_at_ms: now,
        pending_txid: None,
    }
}

fn maker() -> Maker {
    maker_with_utxos(vec![utxo()])
}

fn maker_with_utxos(utxos: Vec<RgbInventoryUtxo>) -> Maker {
    maker_with_btc(utxos, Arc::new(MockBitcoinClient::new()))
}

/// Like `maker_with_utxos` but lets a test hold the `MockBitcoinClient` it
/// passed in, so it can drive broadcast-failure / feerate scenarios.
fn maker_with_btc(utxos: Vec<RgbInventoryUtxo>, bitcoin_client: Arc<MockBitcoinClient>) -> Maker {
    // Declared-funding buy tests resolve the taker's BTC inputs from this
    // address; seed it generously so selection always covers price + fee.
    bitcoin_client.seed_address_unspent(BUY_FUNDING_ADDR, taker_funding());
    let rgb_backend = Arc::new(MockRgbBackend::new(utxos.clone()));
    Maker::new(maker_id(), utxos, rgb_backend, bitcoin_client)
        .with_price_policy(default_test_policy())
}

/// Seed the store directly so tests can pre-set Reserved / Spent statuses.
fn maker_with_store(rows: Vec<InventoryUtxo>) -> Maker {
    let rgb_backend = Arc::new(MockRgbBackend::new(vec![utxo()]));
    let bitcoin_client = Arc::new(MockBitcoinClient::new());
    bitcoin_client.seed_address_unspent(BUY_FUNDING_ADDR, taker_funding());
    Maker::with_components(
        maker_id(),
        Arc::new(InMemoryInventoryStore::with_seed(rows)),
        Arc::new(GreedyExactFitSelector),
        rgb_backend,
        bitcoin_client,
    )
    .with_price_policy(default_test_policy())
}

/// The BTC address buy-side tests declare as the taker's funding source.
const BUY_FUNDING_ADDR: &str = "bcrt1qtaker";

/// One large taker UTXO at `BUY_FUNDING_ADDR`, enough to cover any test
/// quote's price + fee.
fn taker_funding() -> Vec<(Outpoint, TxOut)> {
    vec![(
        Outpoint::new(format!("{:064x}", 0xfeedu64), 0),
        TxOut {
            value_sats: 100_000_000,
            script_pubkey: vec![0x00, 0x14],
        },
    )]
}

/// Build a `SwapLeg::Buy` accept request for `quote`.
fn buy_request(quote: &Quote, invoice: &str) -> AcceptQuoteRequest {
    AcceptQuoteRequest {
        quote_id: quote.quote_id.clone(),
        leg: SwapLeg::Buy {
            rgb_invoice: invoice.to_owned(),
            btc_funding_addr: BUY_FUNDING_ADDR.to_owned(),
        },
    }
}

fn quote_request(id: &str) -> QuoteRequest {
    QuoteRequest {
        rfq_id: RfqId(id.to_owned()),
        base_asset: asset(),
        quote_asset: quote_asset(),
        side: Side::Buy,
        amount: 100,
        created_at_ms: now_ms(),
    }
}

fn sell_quote_request(id: &str) -> QuoteRequest {
    QuoteRequest {
        side: Side::Sell,
        ..quote_request(id)
    }
}

#[tokio::test]
async fn quote_fee_floors_at_min_feerate_when_estimate_is_zero() {
    // Low-activity networks (regtest, signet) return no electrum fee
    // estimate. The maker must floor the feerate at MIN_SWAP_FEERATE_SAT_VB
    // so the swap tx clears bitcoind's `minrelaytxfee`, rather than quoting a
    // 0-sat fee that gets rejected at broadcast.
    let client = Arc::new(MockBitcoinClient::new().with_feerate(0));
    let maker = maker_with_btc(vec![utxo()], client);
    let quote = maker
        .request_quote(quote_request("rfq-fee-floor"))
        .await
        .unwrap()
        .expect("maker quotes the buy");
    assert_eq!(
        quote.estimated_fee_sats,
        MIN_SWAP_FEERATE_SAT_VB * ESTIMATED_SWAP_VBYTES,
        "a 0 electrum estimate must floor to the minimum feerate"
    );
}

#[tokio::test]
async fn quote_fee_uses_electrum_estimate_when_above_floor() {
    // Sanity counterpart: a real estimate above the floor passes through.
    let client = Arc::new(MockBitcoinClient::new().with_feerate(5));
    let maker = maker_with_btc(vec![utxo()], client);
    let quote = maker
        .request_quote(quote_request("rfq-fee-real"))
        .await
        .unwrap()
        .expect("maker quotes the buy");
    assert_eq!(quote.estimated_fee_sats, 5 * ESTIMATED_SWAP_VBYTES);
}

// --- sell-side helpers (16c) ---

/// P2WPKH script — the swap PSBT is segwit-only, so `get_outpoint` rejects
/// anything else.
fn sell_p2wpkh() -> Vec<u8> {
    let mut s = vec![0x00, 0x14];
    s.extend(std::iter::repeat_n(0x33, 20));
    s
}

/// One of the taker's RGB-bearing outpoints. `sell_bitcoin_client` seeds a
/// prevout for each so `deliver_consignment`'s lookups resolve.
fn taker_rgb_outpoint(idx: u64) -> Outpoint {
    Outpoint::new(format!("{:064x}", 0xda7a + idx), 0)
}

fn btc_inv(idx: u64, value_sats: u64) -> BtcInventoryUtxo {
    BtcInventoryUtxo {
        outpoint: Outpoint::new(format!("{:064x}", 0xb7c0 + idx), 0),
        value_sats,
        script_pubkey: sell_p2wpkh(),
        status: BtcInventoryStatus::Available,
        created_at_ms: 0,
        updated_at_ms: 0,
        pending_txid: None,
    }
}

fn sell_bitcoin_client() -> MockBitcoinClient {
    let mut client = MockBitcoinClient::new();
    for i in 0..3 {
        client = client.with_prevout(
            taker_rgb_outpoint(i),
            TxOut {
                value_sats: 50_000,
                script_pubkey: sell_p2wpkh(),
            },
        );
    }
    client
}

fn sell_maker_with_btc(client: Arc<MockBitcoinClient>) -> Maker {
    let rgb_backend = Arc::new(MockRgbBackend::new(vec![]));
    Maker::new(maker_id(), vec![], rgb_backend, client)
        .with_btc_inventory((0..3).map(|i| btc_inv(i, 1_000_000)).collect())
        .with_price_policy(default_test_policy())
}

fn sell_maker() -> Maker {
    sell_maker_with_btc(Arc::new(sell_bitcoin_client()))
}

fn sell_request(quote: &Quote) -> AcceptQuoteRequest {
    AcceptQuoteRequest {
        quote_id: quote.quote_id.clone(),
        leg: SwapLeg::Sell {
            btc_payout_addr: "bcrt1qtaker000payout".to_owned(),
            rgb_change_invoice: None,
        },
    }
}

/// A mock-valid taker **provenance** consignment for `outpoints` (the format
/// the `MockRgbBackend` parses). No maker invoice — provenance model.
fn sell_consignment(quote: &Quote, outpoints: &[Outpoint]) -> String {
    let csv = outpoints
        .iter()
        .map(|o| o.to_string())
        .collect::<Vec<_>>()
        .join(",");
    format!(
        "mock-consignment|sell|amount={}|outpoints={csv}",
        quote.amount,
    )
}

#[tokio::test]
async fn sell_quote_carries_no_maker_invoice() {
    let quote = sell_maker()
        .request_quote(sell_quote_request("rfq-sell"))
        .await
        .unwrap()
        .expect("a BTC-funded maker quotes the sell side");

    // Provenance model: the maker mints no invoice; the taker exports provenance
    // for its own outpoints and names them on the wire.
    assert!(quote.maker_rgb_invoice.is_none());
}

#[tokio::test]
async fn buy_quote_has_no_maker_rgb_invoice() {
    let quote = maker()
        .request_quote(quote_request("rfq-buy"))
        .await
        .unwrap()
        .expect("quote should be returned");

    assert!(quote.maker_rgb_invoice.is_none());
}

#[tokio::test]
async fn sell_quote_declined_without_btc_inventory() {
    // A buy-only maker has no BTC pool to fund a sell-side payout.
    let quote = maker()
        .request_quote(sell_quote_request("rfq-sell"))
        .await
        .unwrap();
    assert!(quote.is_none());
}

#[tokio::test]
async fn sell_flow_settles_through_accept_consignment_sign() {
    let maker = sell_maker();
    let quote = maker
        .request_quote(sell_quote_request("rfq-sell"))
        .await
        .unwrap()
        .expect("sell quote");

    let accepted = maker
        .accept_quote(quote.clone(), sell_request(&quote))
        .await
        .unwrap();
    assert_eq!(accepted.status, SettlementStatus::AwaitingConsignment);

    let consignment = sell_consignment(&quote, &[taker_rgb_outpoint(0), taker_rgb_outpoint(1)]);
    let delivered = maker
        .deliver_consignment(quote.quote_id.clone(), consignment, vec![])
        .await
        .unwrap();
    assert_eq!(delivered.status, SettlementStatus::AwaitingTakerSignature);
    let transfer = delivered.transfer.expect("transfer");
    assert!(!transfer.partial_psbt.is_empty());
    // Every input is committed on the sell side — the txid is known now.
    assert!(transfer.expected_witness_txid.is_some());

    let final_intent = maker
        .submit_signed_psbt(
            quote.quote_id.clone(),
            format!("{}|signed", transfer.partial_psbt),
        )
        .await
        .unwrap();
    assert_eq!(final_intent.status, SettlementStatus::PendingBitcoinConfirm);
    assert_eq!(
        final_intent.witness_txid.as_deref(),
        transfer.expected_witness_txid.as_deref(),
        "settled txid matches the one committed at consignment time",
    );
    assert!(final_intent.final_consignment.is_some());

    // The maker's BTC moved to PendingBitcoinConfirm — no longer reserved.
    let snap = maker.btc_store.snapshot().await;
    assert_eq!(snap.reserved_utxos, 0);
    assert_eq!(snap.pending_settlement_utxos, 1);
}

#[tokio::test]
async fn deliver_consignment_rejects_invalid_consignment() {
    let maker = sell_maker();
    let quote = maker
        .request_quote(sell_quote_request("rfq-sell"))
        .await
        .unwrap()
        .expect("sell quote");
    maker
        .accept_quote(quote.clone(), sell_request(&quote))
        .await
        .unwrap();

    let result = maker
        .deliver_consignment(quote.quote_id.clone(), "rgb-invalid".to_owned(), vec![])
        .await;
    assert!(matches!(result, Err(RouterError::ConsignmentRejected(_))));
    // The BTC reservation was handed back.
    assert_eq!(maker.btc_store.snapshot().await.reserved_utxos, 0);
}

#[tokio::test]
async fn deliver_consignment_after_window_lapse_errors() {
    let maker = sell_maker();
    let quote = maker
        .request_quote(sell_quote_request("rfq-sell"))
        .await
        .unwrap()
        .expect("sell quote");
    maker
        .accept_quote(quote.clone(), sell_request(&quote))
        .await
        .unwrap();

    // Stand in for the cleanup loop expiring the consignment window.
    let reservation = maker
        .btc_store
        .find_reservation_for_quote(&quote.quote_id)
        .await
        .expect("btc reservation live after accept");
    maker
        .btc_store
        .release_reservation(&reservation, now_ms())
        .await
        .unwrap();

    let consignment = sell_consignment(&quote, &[taker_rgb_outpoint(0)]);
    let result = maker
        .deliver_consignment(quote.quote_id.clone(), consignment, vec![])
        .await;
    assert!(result.is_err());
}

#[tokio::test]
async fn deliver_consignment_fee_slippage_releases_btc() {
    let maker = sell_maker();
    let mut quote = maker
        .request_quote(sell_quote_request("rfq-sell"))
        .await
        .unwrap()
        .expect("sell quote");
    // Tiny baseline + zero tolerance: the re-estimate at consignment time
    // (5 sat/vB × 200 vB = 1000) blows the cap.
    quote.estimated_fee_sats = 1;
    quote.fee_slippage_bps = 0;
    maker
        .accept_quote(quote.clone(), sell_request(&quote))
        .await
        .unwrap();

    let consignment = sell_consignment(&quote, &[taker_rgb_outpoint(0)]);
    let result = maker
        .deliver_consignment(quote.quote_id.clone(), consignment, vec![])
        .await;
    assert!(matches!(
        result,
        Err(RouterError::FeeSlippageExceeded { .. })
    ));
    assert_eq!(maker.btc_store.snapshot().await.reserved_utxos, 0);
}

#[tokio::test]
async fn sell_sign_rejects_bait_and_switch() {
    let maker = sell_maker();
    let quote = maker
        .request_quote(sell_quote_request("rfq-sell"))
        .await
        .unwrap()
        .expect("sell quote");
    maker
        .accept_quote(quote.clone(), sell_request(&quote))
        .await
        .unwrap();
    let consignment = sell_consignment(&quote, &[taker_rgb_outpoint(0), taker_rgb_outpoint(1)]);
    let delivered = maker
        .deliver_consignment(quote.quote_id.clone(), consignment, vec![])
        .await
        .unwrap();
    let psbt = delivered.transfer.expect("transfer").partial_psbt;

    // The taker drops one consigned outpoint from the signed PSBT.
    let tampered = psbt.replace(&taker_rgb_outpoint(0).to_string(), "");
    let result = maker
        .submit_signed_psbt(quote.quote_id.clone(), format!("{tampered}|signed"))
        .await;
    assert!(matches!(result, Err(RouterError::PsbtInvalid(_))));
    assert_eq!(maker.btc_store.snapshot().await.reserved_utxos, 0);
}

#[tokio::test]
async fn sell_sign_rejects_txid_mismatch() {
    let maker = sell_maker();
    let quote = maker
        .request_quote(sell_quote_request("rfq-sell"))
        .await
        .unwrap()
        .expect("sell quote");
    maker
        .accept_quote(quote.clone(), sell_request(&quote))
        .await
        .unwrap();
    let consignment = sell_consignment(&quote, &[taker_rgb_outpoint(0)]);
    let delivered = maker
        .deliver_consignment(quote.quote_id.clone(), consignment, vec![])
        .await
        .unwrap();
    let transfer = delivered.transfer.expect("transfer");
    let expected_wt = transfer.expected_witness_txid.expect("committed txid");

    // Tamper the committed witness txid — the inputs stay intact, so the
    // bait-and-switch guard passes and the txid check is what fires.
    let tampered = transfer.partial_psbt.replace(&expected_wt, &"f".repeat(64));
    let result = maker
        .submit_signed_psbt(quote.quote_id.clone(), format!("{tampered}|signed"))
        .await;
    assert!(matches!(result, Err(RouterError::PsbtInvalid(_))));
    assert_eq!(maker.btc_store.snapshot().await.reserved_utxos, 0);
}

#[tokio::test]
async fn sell_sign_broadcast_failure_releases_btc() {
    let client = Arc::new(sell_bitcoin_client());
    let maker = sell_maker_with_btc(client.clone());
    let quote = maker
        .request_quote(sell_quote_request("rfq-sell"))
        .await
        .unwrap()
        .expect("sell quote");
    maker
        .accept_quote(quote.clone(), sell_request(&quote))
        .await
        .unwrap();
    let consignment = sell_consignment(&quote, &[taker_rgb_outpoint(0)]);
    let delivered = maker
        .deliver_consignment(quote.quote_id.clone(), consignment, vec![])
        .await
        .unwrap();
    let psbt = delivered.transfer.expect("transfer").partial_psbt;

    client.fail_next_broadcast("simulated rpc failure");
    let result = maker
        .submit_signed_psbt(quote.quote_id.clone(), format!("{psbt}|signed"))
        .await;
    assert!(result.is_err());
    assert_eq!(maker.btc_store.snapshot().await.reserved_utxos, 0);
}

#[tokio::test]
async fn first_quote_reserves_allocation() {
    let maker = maker();

    let quote = maker.request_quote(quote_request("rfq-1")).await.unwrap();

    let quote = quote.expect("quote should be returned");
    let utxos = maker.utxo_snapshot().await;
    assert_eq!(utxos.len(), 1);
    assert!(matches!(
        &utxos[0].status,
        InventoryStatus::Reserved { quote_id, expires_at_ms, .. }
            if quote_id == &quote.quote_id && *expires_at_ms == quote.expires_at_ms
    ));
}

#[tokio::test]
async fn inventory_summary_counts_initial_inventory_as_available() {
    let maker = maker_with_utxos(vec![utxo_with_amount(0, 100), utxo_with_amount(1, 200)]);

    let snapshot = maker.inventory_summary().await;

    assert_eq!(
        snapshot,
        InventorySnapshot {
            total_amount: 300,
            available_amount: 300,
            reserved_amount: 0,
            spent_amount: 0,
            total_allocations: 2,
            available_allocations: 2,
            reserved_allocations: 0,
            spent_allocations: 0,
        }
    );
}

#[tokio::test]
async fn inventory_summary_counts_reserved_after_quote() {
    let maker = maker();

    let quote = maker.request_quote(quote_request("rfq-1")).await.unwrap();
    let snapshot = maker.inventory_summary().await;

    assert!(quote.is_some());
    assert_eq!(snapshot.available_amount, 0);
    assert_eq!(snapshot.reserved_amount, 100);
    assert_eq!(snapshot.spent_amount, 0);
    assert_eq!(snapshot.available_allocations, 0);
    assert_eq!(snapshot.reserved_allocations, 1);
    assert_eq!(snapshot.spent_allocations, 0);
}

#[tokio::test]
async fn inventory_summary_releases_expired_reservation() {
    let maker = maker_with_store(vec![inv_utxo(
        0,
        100,
        InventoryStatus::Reserved {
            reservation_id: ReservationId("res-0".to_owned()),
            rfq_id: RfqId("rfq-0".to_owned()),
            quote_id: QuoteId("expired-quote".to_owned()),
            expires_at_ms: 0,
        },
    )]);

    let snapshot = maker.inventory_summary().await;
    let utxos = maker.utxo_snapshot().await;

    assert_eq!(snapshot.available_amount, 100);
    assert_eq!(snapshot.reserved_amount, 0);
    assert_eq!(snapshot.available_allocations, 1);
    assert_eq!(snapshot.reserved_allocations, 0);
    assert!(matches!(utxos[0].status, InventoryStatus::Available));
}

#[tokio::test]
async fn release_expired_reservations_returns_count_and_releases() {
    let maker = maker_with_store(vec![inv_utxo(
        0,
        100,
        InventoryStatus::Reserved {
            reservation_id: ReservationId("res-0".to_owned()),
            rfq_id: RfqId("rfq-0".to_owned()),
            quote_id: QuoteId("expired-quote".to_owned()),
            expires_at_ms: 0,
        },
    )]);

    let released = maker.release_expired_reservations().await;
    let utxos = maker.utxo_snapshot().await;

    assert_eq!(released, 1);
    assert!(matches!(utxos[0].status, InventoryStatus::Available));
}

#[tokio::test]
async fn release_expired_reservations_ignores_active_and_spent() {
    let maker = maker_with_store(vec![
        inv_utxo(
            0,
            100,
            InventoryStatus::Reserved {
                reservation_id: ReservationId("res-0".to_owned()),
                rfq_id: RfqId("rfq-0".to_owned()),
                quote_id: QuoteId("active-quote".to_owned()),
                expires_at_ms: now_ms() + 30_000,
            },
        ),
        inv_utxo(
            1,
            100,
            InventoryStatus::Spent {
                witness_txid: "wt-1".to_owned(),
                quote_id: QuoteId("spent-quote".to_owned()),
            },
        ),
    ]);

    let released = maker.release_expired_reservations().await;
    let utxos = maker.utxo_snapshot().await;

    assert_eq!(released, 0);
    assert!(matches!(utxos[0].status, InventoryStatus::Reserved { .. }));
    assert!(matches!(utxos[1].status, InventoryStatus::Spent { .. }));
}

#[tokio::test]
async fn second_quote_cannot_reuse_reserved_liquidity() {
    let maker = maker();

    let first_quote = maker.request_quote(quote_request("rfq-1")).await.unwrap();
    let second_quote = maker.request_quote(quote_request("rfq-2")).await.unwrap();

    assert!(first_quote.is_some());
    assert!(second_quote.is_none());
}

#[tokio::test]
async fn expired_reservation_becomes_available() {
    let expired_quote_id = QuoteId("expired-quote".to_owned());
    let maker = maker_with_store(vec![inv_utxo(
        0,
        100,
        InventoryStatus::Reserved {
            reservation_id: ReservationId("res-0".to_owned()),
            rfq_id: RfqId("rfq-0".to_owned()),
            quote_id: expired_quote_id,
            expires_at_ms: 0,
        },
    )]);

    let quote = maker.request_quote(quote_request("rfq-1")).await.unwrap();

    assert!(quote.is_some());
    let utxos = maker.utxo_snapshot().await;
    assert!(matches!(
        &utxos[0].status,
        InventoryStatus::Reserved { quote_id, .. } if quote_id == &quote.unwrap().quote_id
    ));
}

#[tokio::test]
async fn accept_keeps_utxo_reserved_awaiting_signature() {
    // Under the swap flow, `accept` no longer spends — it builds the PSBT
    // and waits for the taker's signature. The reserved UTXO stays
    // `Reserved` (with its deadline extended to the settlement window).
    let maker = maker();
    let quote = maker
        .request_quote(quote_request("rfq-1"))
        .await
        .unwrap()
        .expect("quote should be returned");

    let intent = maker
        .accept_quote(quote.clone(), buy_request(&quote, "rgb:test_invoice"))
        .await
        .unwrap();

    assert_eq!(intent.status, SettlementStatus::AwaitingTakerSignature);
    assert!(intent.transfer.is_some());
    let utxos = maker.utxo_snapshot().await;
    assert!(matches!(
        &utxos[0].status,
        InventoryStatus::Reserved { quote_id, .. } if quote_id == &quote.quote_id
    ));
}

#[tokio::test]
async fn inventory_counts_pending_confirm_after_sign() {
    // accept → sign drives the input UTXO to `PendingBitcoinConfirm`
    // (broadcast, awaiting confirmation) — not `Spent`, which only happens
    // once the witness tx confirms.
    let maker = maker();
    let quote = maker
        .request_quote(quote_request("rfq-1"))
        .await
        .unwrap()
        .expect("quote should be returned");

    let intent = maker
        .accept_quote(quote.clone(), buy_request(&quote, "rgb:test_invoice"))
        .await
        .unwrap();
    let psbt = intent.transfer.expect("transfer").partial_psbt;
    maker
        .submit_signed_psbt(quote.quote_id.clone(), format!("{psbt}|signed"))
        .await
        .unwrap();

    let ext = maker.extended_inventory_summary().await;
    assert_eq!(ext.available_utxos, 0);
    assert_eq!(ext.reserved_utxos, 0);
    assert_eq!(ext.pending_settlement_utxos, 1);
    assert_eq!(ext.pending_settlement_amount, 100);
}

#[tokio::test]
async fn spent_allocation_cannot_be_quoted_again() {
    let maker = maker();
    let quote = maker
        .request_quote(quote_request("rfq-1"))
        .await
        .unwrap()
        .expect("quote should be returned");
    maker
        .accept_quote(
            quote.clone(),
            AcceptQuoteRequest {
                quote_id: quote.quote_id.clone(),
                leg: SwapLeg::Buy {
                    rgb_invoice: "rgb:test_invoice".to_owned(),
                    btc_funding_addr: "bcrt1qtaker".to_owned(),
                },
            },
        )
        .await
        .unwrap();

    let next_quote = maker.request_quote(quote_request("rfq-2")).await.unwrap();

    assert!(next_quote.is_none());
}

// --- new 14c tests ---

#[tokio::test]
async fn request_quote_reserves_specific_utxos_not_whole_inventory() {
    // Three available UTXOs of 100 each. A request for 80 reserves exactly
    // one of them — the other two stay Available. This is the per-UTXO
    // win over the pre-#14 whole-allocation model.
    let maker = maker_with_utxos(vec![
        utxo_with_amount(0, 100),
        utxo_with_amount(1, 100),
        utxo_with_amount(2, 100),
    ]);

    let mut request = quote_request("rfq-1");
    request.amount = 80;
    let quote = maker.request_quote(request).await.unwrap();
    assert!(quote.is_some());

    let ext = maker.extended_inventory_summary().await;
    assert_eq!(ext.available_utxos, 2);
    assert_eq!(ext.reserved_utxos, 1);
    assert_eq!(ext.available_amount, 200);
    assert_eq!(ext.reserved_amount, 100);
}

#[tokio::test]
async fn expired_reservation_releases_per_utxo() {
    let maker = maker_with_store(vec![
        inv_utxo(
            0,
            100,
            InventoryStatus::Reserved {
                reservation_id: ReservationId("res-0".to_owned()),
                rfq_id: RfqId("rfq-0".to_owned()),
                quote_id: QuoteId("expired".to_owned()),
                expires_at_ms: 0,
            },
        ),
        inv_utxo(1, 200, InventoryStatus::Available),
    ]);

    let released = maker.release_expired_reservations().await;
    let ext = maker.extended_inventory_summary().await;

    assert_eq!(released, 1);
    assert_eq!(ext.available_utxos, 2);
    assert_eq!(ext.reserved_utxos, 0);
}

#[tokio::test]
async fn concurrent_request_quotes_do_not_double_reserve() {
    // Five UTXOs of 100 each. Spawn 10 concurrent quote requests of 50;
    // at most 5 should succeed (one UTXO per quote with WholeUtxoSelector).
    let maker = Arc::new(maker_with_utxos(vec![
        utxo_with_amount(0, 100),
        utxo_with_amount(1, 100),
        utxo_with_amount(2, 100),
        utxo_with_amount(3, 100),
        utxo_with_amount(4, 100),
    ]));

    let mut handles = Vec::new();
    for i in 0..10 {
        let maker = maker.clone();
        handles.push(tokio::spawn(async move {
            let mut request = quote_request(&format!("rfq-{i}"));
            request.amount = 50;
            maker.request_quote(request).await.unwrap()
        }));
    }

    let mut successes = 0;
    for h in handles {
        if h.await.unwrap().is_some() {
            successes += 1;
        }
    }
    assert_eq!(successes, 5);

    let ext = maker.extended_inventory_summary().await;
    assert_eq!(ext.reserved_utxos, 5);
    assert_eq!(ext.available_utxos, 0);
}

// --- 14e tests ---

#[tokio::test]
async fn rebalance_emits_no_triggers_for_healthy_inventory() {
    // Single big UTXO → fragmentation 0.0, available_utxos = 1 (between
    // min and max). All triggers should be silent.
    let maker = maker_with_utxos(vec![utxo_with_amount(0, 1000)]);
    let policy = RebalancePolicy {
        fragmentation_threshold: 0.7,
        max_utxo_count: 50,
        min_utxo_count: 1,
    };
    let plan = maker.rebalance(&policy).await;
    assert!(plan.is_empty(), "expected no triggers, got {plan:?}");
}

#[tokio::test]
async fn rebalance_fires_high_fragmentation_above_threshold() {
    // 5 UTXOs of 100 each. Largest 100 / total 500 = 0.2 share →
    // fragmentation = 0.8, which crosses 0.7.
    let maker = maker_with_utxos(vec![
        utxo_with_amount(0, 100),
        utxo_with_amount(1, 100),
        utxo_with_amount(2, 100),
        utxo_with_amount(3, 100),
        utxo_with_amount(4, 100),
    ]);
    let policy = RebalancePolicy {
        fragmentation_threshold: 0.7,
        max_utxo_count: 50,
        min_utxo_count: 1,
    };
    let plan = maker.rebalance(&policy).await;
    assert!(plan
        .triggers
        .iter()
        .any(|t| matches!(t, RebalanceTrigger::HighFragmentation { .. })));
}

#[tokio::test]
async fn rebalance_fires_too_many_utxos_above_max() {
    let utxos: Vec<_> = (0..10).map(|i| utxo_with_amount(i, 10)).collect();
    let maker = maker_with_utxos(utxos);
    let policy = RebalancePolicy {
        fragmentation_threshold: 1.0, // suppress fragmentation trigger
        max_utxo_count: 5,
        min_utxo_count: 1,
    };
    let plan = maker.rebalance(&policy).await;
    assert!(plan
        .triggers
        .iter()
        .any(|t| matches!(t, RebalanceTrigger::TooManyUtxos { count: 10, max: 5 })));
}

#[tokio::test]
async fn rebalance_fires_too_few_utxos_below_min() {
    let maker = maker_with_utxos(vec![utxo_with_amount(0, 1000)]);
    let policy = RebalancePolicy {
        fragmentation_threshold: 1.0,
        max_utxo_count: 50,
        min_utxo_count: 3,
    };
    let plan = maker.rebalance(&policy).await;
    assert!(plan
        .triggers
        .iter()
        .any(|t| matches!(t, RebalanceTrigger::TooFewUtxos { count: 1, min: 3 })));
}

#[tokio::test]
async fn sign_marks_input_pending_bitcoin_confirm_change_added_by_chain_observer() {
    // Single UTXO of 100; request 60 → change = 40. After /sign, only the
    // input UTXO is PendingBitcoinConfirm — the RGB change UTXO is added
    // later by the chain observer (`ingest_rgb_change_utxos`) once
    // `sync_wallet` sees the new outpoint, mirroring the BTC change path.
    let maker = maker_with_utxos(vec![utxo_with_amount(0, 100)]);

    let mut request = quote_request("rfq-1");
    request.amount = 60;
    let quote = maker
        .request_quote(request)
        .await
        .unwrap()
        .expect("quote should be issued");

    let intent = maker
        .accept_quote(quote.clone(), buy_request(&quote, "rgb:test"))
        .await
        .unwrap();
    let psbt = intent.transfer.expect("transfer").partial_psbt;
    maker
        .submit_signed_psbt(quote.quote_id.clone(), format!("{psbt}|signed"))
        .await
        .unwrap();

    let utxos = maker.utxo_snapshot().await;
    let pending: Vec<_> = utxos
        .iter()
        .filter(|u| matches!(u.status, InventoryStatus::PendingBitcoinConfirm { .. }))
        .collect();
    assert_eq!(pending.len(), 1, "only the input UTXO should be pending");
    assert_eq!(pending[0].amount, 100);
}

#[tokio::test]
async fn buy_flow_settles_through_accept_then_sign() {
    let maker = maker();
    let quote = maker
        .request_quote(quote_request("rfq-1"))
        .await
        .unwrap()
        .expect("quote should be returned");

    let intent = maker
        .accept_quote(quote.clone(), buy_request(&quote, "rgb:invoice"))
        .await
        .unwrap();
    assert_eq!(intent.status, SettlementStatus::AwaitingTakerSignature);
    let transfer = intent.transfer.expect("transfer");
    assert!(transfer.consignment.is_some());

    let final_intent = maker
        .submit_signed_psbt(
            quote.quote_id.clone(),
            format!("{}|signed", transfer.partial_psbt),
        )
        .await
        .unwrap();
    assert_eq!(final_intent.status, SettlementStatus::PendingBitcoinConfirm);
    assert!(final_intent.witness_txid.is_some());
    assert!(final_intent.final_consignment.is_some());

    let utxos = maker.utxo_snapshot().await;
    assert!(matches!(
        utxos[0].status,
        InventoryStatus::PendingBitcoinConfirm { .. }
    ));
}

/// A single taker funding UTXO of `value_sats` at `BUY_FUNDING_ADDR`.
fn funding_utxo(value_sats: u64) -> Vec<(Outpoint, TxOut)> {
    let mut p2wpkh = vec![0x00, 0x14];
    p2wpkh.extend(std::iter::repeat_n(0x22, 20));
    vec![(
        Outpoint::new(format!("{:064x}", 0xfeedu64), 0),
        TxOut {
            value_sats,
            script_pubkey: p2wpkh,
        },
    )]
}

// Regression: the witness-vout RGB receive output costs SEAL_ANCHOR_SATS, so the
// taker's funding must cover price + fee + anchor. Funding only price + fee used
// to yield an overspending PSBT (the taker's signer then rejected it with
// "Outputs spends more than inputs amount"); the maker must now reject up front.
#[tokio::test]
async fn buy_rejects_funding_short_by_the_seal_anchor() {
    let client = Arc::new(MockBitcoinClient::new());
    let maker = maker_with_btc(vec![utxo()], Arc::clone(&client));
    let quote = maker
        .request_quote(quote_request("rfq-1"))
        .await
        .unwrap()
        .expect("quote");
    // price + fee, but NOT the seal anchor.
    let short = quote.price + quote.estimated_fee_sats;
    client.seed_address_unspent(BUY_FUNDING_ADDR, funding_utxo(short));

    let err = maker
        .accept_quote(quote.clone(), buy_request(&quote, "rgb:invoice"))
        .await
        .unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("needs") && msg.contains("anchor"),
        "expected a funding-shortfall error mentioning the anchor, got: {msg}"
    );
}

#[tokio::test]
async fn buy_accepts_funding_that_covers_price_fee_and_anchor() {
    let client = Arc::new(MockBitcoinClient::new());
    let maker = maker_with_btc(vec![utxo()], Arc::clone(&client));
    let quote = maker
        .request_quote(quote_request("rfq-1"))
        .await
        .unwrap()
        .expect("quote");
    let enough = quote.price + quote.estimated_fee_sats + rfq_rgb::SEAL_ANCHOR_SATS;
    client.seed_address_unspent(BUY_FUNDING_ADDR, funding_utxo(enough));

    let intent = maker
        .accept_quote(quote.clone(), buy_request(&quote, "rgb:invoice"))
        .await
        .unwrap();
    assert_eq!(intent.status, SettlementStatus::AwaitingTakerSignature);
}

#[tokio::test]
async fn submit_signed_psbt_unknown_quote_errors() {
    let maker = maker();
    let result = maker
        .submit_signed_psbt(QuoteId("no-such-quote".to_owned()), "psbt".to_owned())
        .await;
    assert!(result.is_err());
}

#[tokio::test]
async fn submit_signed_psbt_rejects_unsigned_psbt() {
    // MockRgbBackend::finalize_after_taker_sign rejects an empty PSBT.
    let maker = maker();
    let quote = maker
        .request_quote(quote_request("rfq-1"))
        .await
        .unwrap()
        .expect("quote");
    maker
        .accept_quote(quote.clone(), buy_request(&quote, "rgb:invoice"))
        .await
        .unwrap();

    let result = maker
        .submit_signed_psbt(quote.quote_id.clone(), String::new())
        .await;
    assert!(matches!(result, Err(RouterError::PsbtInvalid(_))));
    // Reservation released back to Available.
    let utxos = maker.utxo_snapshot().await;
    assert!(matches!(utxos[0].status, InventoryStatus::Available));
}

#[tokio::test]
async fn buy_accept_fee_slippage_releases_reservation() {
    // Default mock feerate (5 sat/vB) × 200 vB = 1000 sat actual. Crafting
    // the quote with a tiny baseline + zero slippage tolerance makes the
    // accept-time estimate blow the cap. Under declared-funding the buy
    // PSBT is fully committed at accept, so slippage is checked there (not
    // at /sign).
    let maker = maker();
    let mut quote = maker
        .request_quote(quote_request("rfq-1"))
        .await
        .unwrap()
        .expect("quote");
    quote.estimated_fee_sats = 1;
    quote.fee_slippage_bps = 0;

    let result = maker
        .accept_quote(quote.clone(), buy_request(&quote, "rgb:invoice"))
        .await;
    assert!(matches!(
        result,
        Err(RouterError::FeeSlippageExceeded { .. })
    ));
    // Reservation released back to Available.
    let utxos = maker.utxo_snapshot().await;
    assert!(matches!(utxos[0].status, InventoryStatus::Available));
}

#[tokio::test]
async fn submit_signed_psbt_broadcast_failure_releases_reservation() {
    let btc = Arc::new(MockBitcoinClient::new());
    let maker = maker_with_btc(vec![utxo()], btc.clone());
    let quote = maker
        .request_quote(quote_request("rfq-1"))
        .await
        .unwrap()
        .expect("quote");
    let intent = maker
        .accept_quote(quote.clone(), buy_request(&quote, "rgb:invoice"))
        .await
        .unwrap();
    let psbt = intent.transfer.expect("transfer").partial_psbt;

    btc.fail_next_broadcast("simulated rpc failure");
    let result = maker
        .submit_signed_psbt(quote.quote_id.clone(), format!("{psbt}|signed"))
        .await;
    assert!(result.is_err());
    let utxos = maker.utxo_snapshot().await;
    assert!(matches!(utxos[0].status, InventoryStatus::Available));
}

#[tokio::test]
async fn submit_signed_psbt_after_settlement_window_lapse_errors() {
    // Simulate the taker missing the TAKER_SIGNATURE_TTL_MS window: once
    // the cleanup loop has released the reservation, submit_signed_psbt
    // must refuse, drop the stale pending entry, and leave the maker's
    // UTXO back on Available — no broadcast.
    let maker = maker();
    let quote = maker
        .request_quote(quote_request("rfq-1"))
        .await
        .unwrap()
        .expect("quote");
    let intent = maker
        .accept_quote(quote.clone(), buy_request(&quote, "rgb:invoice"))
        .await
        .unwrap();
    let psbt = intent.transfer.expect("transfer").partial_psbt;

    // Stand in for the cleanup loop expiring the settlement window.
    let reservation_id = maker
        .store
        .find_reservation_for_quote(&quote.quote_id)
        .await
        .expect("reservation live after accept");
    maker
        .store
        .release_reservation(&reservation_id, now_ms())
        .await
        .unwrap();

    let result = maker
        .submit_signed_psbt(quote.quote_id.clone(), format!("{psbt}|signed"))
        .await;
    assert!(result.is_err());
    let utxos = maker.utxo_snapshot().await;
    assert!(matches!(utxos[0].status, InventoryStatus::Available));
}

#[tokio::test]
async fn accept_releases_reservation_on_transfer_failure() {
    // MockRgbBackend rejects invoices that don't start with "rgb:".
    let maker = maker();
    let quote = maker
        .request_quote(quote_request("rfq-1"))
        .await
        .unwrap()
        .expect("quote should be issued");

    let result = maker
        .accept_quote(
            quote.clone(),
            AcceptQuoteRequest {
                quote_id: quote.quote_id,
                leg: SwapLeg::Buy {
                    rgb_invoice: "not-an-invoice".into(),
                    btc_funding_addr: "bcrt1qtaker".to_owned(),
                },
            },
        )
        .await;
    assert!(result.is_err());

    let snapshot = maker.inventory_summary().await;
    assert_eq!(snapshot.available_amount, 100);
    assert_eq!(snapshot.reserved_amount, 0);
    assert_eq!(snapshot.spent_amount, 0);
}

#[tokio::test]
async fn sweep_confirmations_marks_pending_rgb_and_btc_spent() {
    let reservation_id = ReservationId("res-rgb-1".to_owned());
    let wt = "wt-shared-1".to_owned();

    let rgb_pending = inv_utxo(
        0,
        100,
        InventoryStatus::PendingBitcoinConfirm {
            reservation_id: reservation_id.clone(),
            witness_txid: wt.clone(),
        },
    );
    let maker = maker_with_store(vec![rgb_pending]);

    let btc_reservation_id = ReservationId("res-btc-1".to_owned());
    let btc_pending = BtcInventoryUtxo {
        outpoint: Outpoint::new(format!("{:064x}", 0xb7c1u64), 0),
        value_sats: 50_000,
        script_pubkey: sell_p2wpkh(),
        status: BtcInventoryStatus::PendingBitcoinConfirm {
            reservation_id: btc_reservation_id.clone(),
            witness_txid: wt.clone(),
        },
        created_at_ms: 0,
        updated_at_ms: 0,
        pending_txid: None,
    };
    let maker = maker.with_btc_inventory(vec![btc_pending]);

    let spent = maker.sweep_confirmations().await;
    assert_eq!(spent, 2, "one UTXO from each store should sweep");

    let rgb_status = maker
        .utxo_snapshot()
        .await
        .into_iter()
        .next()
        .unwrap()
        .status;
    assert!(matches!(
        rgb_status,
        InventoryStatus::Spent { ref witness_txid, .. } if witness_txid == &wt
    ));

    let btc_status = maker
        .btc_store
        .list_all()
        .await
        .into_iter()
        .next()
        .unwrap()
        .status;
    assert!(matches!(
        btc_status,
        BtcInventoryStatus::Spent { ref witness_txid, .. } if witness_txid == &wt
    ));
}

#[tokio::test]
async fn legacy_inventory_summary_matches_extended_downcast() {
    let maker = maker_with_store(vec![
        inv_utxo(0, 100, InventoryStatus::Available),
        inv_utxo(
            1,
            200,
            InventoryStatus::Reserved {
                reservation_id: ReservationId("res-1".to_owned()),
                rfq_id: RfqId("rfq-1".to_owned()),
                quote_id: QuoteId("q".to_owned()),
                expires_at_ms: now_ms() + 60_000,
            },
        ),
    ]);

    let legacy = maker.inventory_summary().await;
    let ext = maker.extended_inventory_summary().await;
    let derived: InventorySnapshot = (&ext).into();
    assert_eq!(legacy, derived);
}

// --- rebalance ladder planner (pure) ---

fn ladder(base: u64, ratio: f64, tiers: u32, copies: u32, min_piece: u64) -> LadderSpec {
    LadderSpec {
        base,
        ratio,
        tiers,
        copies,
        min_piece,
    }
}

/// `(synth_outpoint(i), value)` pairs for the planner's `available` input.
fn avail(values: &[u64]) -> Vec<(Outpoint, u64)> {
    values
        .iter()
        .enumerate()
        .map(|(i, &v)| (synth_outpoint(i), v))
        .collect()
}

#[test]
fn ladder_target_rungs_expands_geometric_largest_first() {
    // 100 → 50 → 25 → 12 (12.5 truncated), two copies each.
    let rungs = ladder(100, 0.5, 4, 2, 1).target_rungs();
    assert_eq!(rungs, vec![100, 100, 50, 50, 25, 25, 12, 12]);
}

#[test]
fn ladder_target_rungs_drops_subfloor_tiers() {
    // min_piece 30 prunes the 25 and 12 tiers.
    let rungs = ladder(100, 0.5, 4, 1, 30).target_rungs();
    assert_eq!(rungs, vec![100, 50]);
}

#[test]
fn ladder_deficit_reports_uncovered_targets() {
    // Pieces [100, 1000] cover two targets; [50,50,100,100] remain.
    let deficit = ladder_deficit(&[100, 1000], &[100, 100, 50, 50, 25, 25]);
    let mut d = deficit;
    d.sort_unstable();
    assert_eq!(d, vec![50, 50, 100, 100]);
}

#[test]
fn plan_ladder_none_when_already_covered() {
    let spec = ladder(100, 0.5, 3, 1, 1); // targets [100,50,25]
    assert!(plan_ladder(&avail(&[100, 50, 25]), &spec).is_none());
}

#[test]
fn plan_ladder_splits_one_fat_utxo_into_rungs() {
    let spec = ladder(100, 0.5, 3, 1, 1); // targets [100,50,25]
                                          // The lone 1000 covers the smallest target (25); the 100 and 50 tiers are
                                          // the deficit, carved off the fat UTXO largest-first (remainder 850).
    let (src, val, rungs) = plan_ladder(&avail(&[1000]), &spec).expect("a split");
    assert_eq!(src, synth_outpoint(0));
    assert_eq!(val, 1000);
    assert_eq!(rungs, vec![100, 50]);
}

#[test]
fn plan_ladder_picks_the_fattest_source() {
    let spec = ladder(100, 0.5, 3, 2, 1);
    // index 1 holds the 1000; it must be chosen over the 100.
    let (src, _, _) = plan_ladder(&avail(&[100, 1000]), &spec).expect("a split");
    assert_eq!(src, synth_outpoint(1));
}

#[test]
fn plan_ladder_stops_when_source_too_small_for_any_rung() {
    // targets [100,50]; a lone 60 covers 50 but can't fund a 100 rung while
    // leaving the min_piece remainder.
    let spec = ladder(100, 0.5, 2, 1, 50);
    assert!(plan_ladder(&avail(&[60]), &spec).is_none());
}

#[test]
fn plan_ladder_is_idempotent_after_a_split() {
    let spec = ladder(100, 0.5, 3, 1, 1);
    // Feed back the post-split distribution: remainder 850 + rungs 100, 50.
    assert!(plan_ladder(&avail(&[850, 100, 50]), &spec).is_none());
}

fn asset_split(rungs: Vec<u64>, source_btc_sats: u64) -> AssetSplit {
    AssetSplit {
        asset: asset(),
        source: synth_outpoint(0),
        source_amount: 10_000,
        source_btc_sats,
        rungs,
    }
}

#[test]
fn assemble_returns_plan_with_btc_needed_when_budget_fits() {
    let plan = assemble_rebalance_tx(vec![asset_split(vec![50, 25], 546)], None, 100_000, 300)
        .expect("a plan");
    // fee 300 + anchor 546 * (2 rungs + 1 remainder) = 300 + 1638.
    assert_eq!(plan.btc_needed, 300 + 546 * 3);
    assert_eq!(plan.assets[0].rungs, vec![50, 25]);
    assert!(plan.btc.is_none());
}

#[test]
fn assemble_scales_down_rgb_rungs_when_budget_short() {
    // avail = 1000 (pool) + 546 (source) = 1546. Full ladder of 3 rungs needs
    // 100 + 546*4 = 2284 → drop smallest rung (12) → 1738 → still short → drop
    // 25 → 100 + 546*2 = 1192 ≤ 1546. One rung survives.
    let plan = assemble_rebalance_tx(vec![asset_split(vec![50, 25, 12], 546)], None, 1000, 100)
        .expect("a scaled-down plan");
    assert_eq!(plan.assets[0].rungs, vec![50]);
    assert_eq!(plan.btc_needed, 100 + 546 * 2);
}

#[test]
fn assemble_none_when_budget_cannot_even_fund_fee() {
    // Zero pool, one 546-sat source, fee 1000: never fits → everything dropped.
    assert!(assemble_rebalance_tx(vec![asset_split(vec![50], 546)], None, 0, 1000).is_none());
}

#[test]
fn assemble_none_when_nothing_to_split() {
    assert!(assemble_rebalance_tx(vec![], None, 100_000, 300).is_none());
    // an empty-rung asset is stripped, leaving nothing.
    assert!(assemble_rebalance_tx(vec![asset_split(vec![], 546)], None, 100_000, 300).is_none());
}

#[test]
fn assemble_keeps_btc_rungs_when_they_fit() {
    let btc = BtcSplit {
        source: synth_outpoint(9),
        source_sats: 100_000,
        rungs: vec![50_000, 25_000],
    };
    let plan = assemble_rebalance_tx(vec![], Some(btc), 100_000, 300).expect("a plan");
    let b = plan.btc.expect("btc split kept");
    assert_eq!(b.rungs, vec![50_000, 25_000]);
    // fee 300 + 0 rgb outputs + (50_000 + 25_000) btc rungs.
    assert_eq!(plan.btc_needed, 300 + 75_000);
}

#[test]
fn ladder_target_rungs_empty_for_degenerate_specs() {
    assert!(
        ladder(100, 0.5, 0, 2, 1).target_rungs().is_empty(),
        "tiers=0"
    );
    assert!(
        ladder(100, 0.5, 3, 0, 1).target_rungs().is_empty(),
        "copies=0"
    );
    // base below min_piece → first tier already dropped, all dropped.
    assert!(
        ladder(5, 0.5, 3, 1, 10).target_rungs().is_empty(),
        "base<min_piece"
    );
}

#[test]
fn plan_ladder_none_for_empty_inventory() {
    // Nothing to split when there are no UTXOs at all.
    assert!(plan_ladder(&[], &ladder(100, 0.5, 3, 1, 1)).is_none());
}

#[test]
fn plan_ladder_leaves_a_positive_remainder() {
    // targets [100,50,25]; a lone 100 covers the 25 tier, deficit [50,100].
    // Carving 100 from a 100 source would leave 0 (no remainder) → skipped;
    // 50 fits (leaves 50). So the source can never be fully consumed.
    let (_, val, rungs) = plan_ladder(&avail(&[100]), &ladder(100, 0.5, 3, 1, 1)).expect("split");
    assert_eq!(val, 100);
    assert_eq!(
        rungs,
        vec![50],
        "can't carve a rung equal to the whole source"
    );
}

#[test]
fn assemble_drops_a_whole_asset_when_all_its_rungs_scale_away() {
    // Two assets; a tight budget forces dropping every rung of the smaller
    // contributor. avail = pool 0 + sources (546 each). required for the full
    // plan exceeds it, so rungs drop until it fits — possibly emptying an asset.
    let a = AssetSplit {
        asset: asset(),
        source: synth_outpoint(0),
        source_amount: 10_000,
        source_btc_sats: 546,
        rungs: vec![5, 5, 5],
    };
    let b = AssetSplit {
        asset: quote_asset(),
        source: synth_outpoint(1),
        source_amount: 10_000,
        source_btc_sats: 546,
        rungs: vec![5, 5, 5],
    };
    // Pool 0; only the two 546-sat sources fund anchors. fee 100. Full plan
    // needs 100 + 546*(6 rungs + 2 remainders) = 100 + 4368, avail = 1092 →
    // scales down hard. Whatever survives must be fundable.
    let plan = assemble_rebalance_tx(vec![a, b], None, 0, 100);
    if let Some(p) = plan {
        // Every surviving asset has at least one rung, and the budget holds.
        assert!(p.assets.iter().all(|a| !a.rungs.is_empty()));
        let outs: u64 = p.assets.iter().map(|a| a.rungs.len() as u64 + 1).sum();
        let need = 100 + REBALANCE_ANCHOR_SATS * outs;
        let avail = p.assets.iter().map(|a| a.source_btc_sats).sum::<u64>();
        assert!(avail >= need, "surviving plan must be fundable");
    }
    // (None is also acceptable — everything scaled away.)
}

#[test]
fn assemble_counts_rgb_source_sats_toward_the_budget() {
    // A fat RGB source (lots of its own sats) funds its anchors without any
    // BTC pool — the budget should accept it.
    let a = asset_split(vec![50, 25], 100_000);
    let plan = assemble_rebalance_tx(vec![a], None, 0, 300).expect("source sats fund it");
    assert_eq!(plan.assets[0].rungs, vec![50, 25]);
}

#[test]
fn plan_change_rungs_carves_deficit_into_swap_change() {
    let spec = ladder(100, 0.5, 3, 1, 1); // targets [100,50,25]
                                          // Holdings cover the smallest tier; the swap change (200) funds the
                                          // missing 100 + 50 rungs (largest-first), leaving a remainder.
    assert_eq!(plan_change_rungs(&[1000], 200, &spec), vec![100, 50]);
    // Change too small to carve any rung → skip (empty).
    assert!(plan_change_rungs(&[1000], 10, &spec).is_empty());
    // No change → empty.
    assert!(plan_change_rungs(&[1000], 0, &spec).is_empty());
}

#[test]
fn rebalance_vbytes_sizes_all_p2tr_tx() {
    // 2 inputs + 6 outputs: 11 + 2*58 + 6*43.
    assert_eq!(estimate_rebalance_vbytes(2, 6), 11 + 116 + 258);
    assert_eq!(estimate_rebalance_vbytes(1, 2), 11 + 58 + 86);
}

#[test]
fn feerate_clamp_floors_and_caps_per_network() {
    use rfq_types::BitcoinNetwork::*;
    // A missing/zero estimate floors at 1 (so the tx clears minrelaytxfee).
    assert_eq!(clamp_next_block_feerate(0, &Regtest), 1);
    // Test nets cap low (5); a wild estimate can't blow the fee up.
    assert_eq!(clamp_next_block_feerate(99_999, &Signet), 5);
    assert_eq!(clamp_next_block_feerate(99_999, &Regtest), 5);
    // Mainnet caps at 1000; an in-band estimate passes through.
    assert_eq!(clamp_next_block_feerate(99_999, &Mainnet), 1000);
    assert_eq!(clamp_next_block_feerate(42, &Mainnet), 42);
}

#[test]
fn assemble_scales_down_btc_rungs_when_source_too_small() {
    // No RGB; a 60k BTC source can't carve [50k, 25k] + 300 fee → drop the
    // smallest BTC rung (25k) so 50k + 300 fits.
    let btc = BtcSplit {
        source: synth_outpoint(9),
        source_sats: 60_000,
        rungs: vec![50_000, 25_000],
    };
    let plan = assemble_rebalance_tx(vec![], Some(btc), 60_000, 300).expect("a scaled plan");
    let b = plan.btc.expect("btc split survives");
    assert_eq!(
        b.rungs,
        vec![50_000],
        "smallest BTC rung dropped to fit the source"
    );
    assert_eq!(plan.btc_needed, 300 + 50_000);
}

#[tokio::test]
async fn reserve_for_rebalance_removes_source_from_available() {
    // The primary anti-thrash guard: a source claimed for a split immediately
    // leaves Available, so the planner can't re-pick it next tick.
    let maker = maker_with_utxos(vec![utxo_with_amount(0, 1000), utxo_with_amount(1, 500)]);
    assert_eq!(maker.available_rgb_for(&asset()).await.len(), 2);

    let source = synth_outpoint(0);
    maker
        .reserve_rgb_for_rebalance(source.clone())
        .await
        .expect("reserve source for rebalance");

    let after = maker.available_rgb_for(&asset()).await;
    assert_eq!(after.len(), 1, "reserved source drops out of Available");
    assert!(
        !after.iter().any(|(o, _, _)| *o == source),
        "the reserved source is gone from Available"
    );
}
