use rfq_types::{AssetId, InventoryStatus, InventoryUtxo, Outpoint};
use thiserror::Error;

/// Result of selecting UTXOs to cover a target amount. `total_input` is the
/// sum of `chosen` amounts; `expected_change = total_input - requested` is the
/// change the transfer will need to produce.
///
/// `chosen` is sorted by outpoint for determinism — the store's
/// `reserve_utxos` is order-insensitive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Selection {
    pub asset: AssetId,
    pub chosen: Vec<Outpoint>,
    pub total_input: u64,
    pub requested: u64,
    pub expected_change: u64,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum CoinSelectionError {
    #[error("insufficient liquidity: wanted {wanted}, available {available}")]
    Insufficient { wanted: u64, available: u64 },
    #[error("asset mismatch in candidate UTXO set")]
    AssetMismatch,
    #[error("requested amount is zero")]
    ZeroAmount,
}

pub trait CoinSelector: Send + Sync {
    fn select(
        &self,
        asset: &AssetId,
        amount: u64,
        available: &[InventoryUtxo],
    ) -> Result<Selection, CoinSelectionError>;
}

/// Denomination-aware coin selector. In rank order:
///
/// 1. **Exact single** — any UTXO with `amount == requested` (zero change, 1 input).
/// 2. **Bounded subset-sum exact** — 2-of-N exact match (allowed up to ~2000
///    candidates so the dust+large hot path works). 3-of-N exact match capped
///    at 16 candidates — `C(16,3)=560` iterations.
/// 3. **Single UTXO with smallest change** — smallest UTXO `> amount`.
/// 4. **Multi-UTXO greedy** — sort descending, take largest until cumulative
///    `>= amount`.
///
/// Picks the better of (3) and (4) by smaller change first, then smaller
/// input count. When (3) doesn't exist (no single UTXO covers the request),
/// (4) is used by default.
#[derive(Debug, Clone, Default)]
pub struct GreedyExactFitSelector;

/// Above this N, skip 3-of-N exact-sum search. `C(16,3) = 560` is fine;
/// `C(50,3) = 19600` adds little fragmentation benefit per cycle spent.
const SUBSET_LIMIT_3OFN: usize = 16;

/// Above this N, skip even 2-of-N exact-sum search. `C(2000,2) ≈ 2_000_000`
/// pairs of `u64` adds is sub-millisecond and is the only thing that prevents
/// the dust+large fragmentation hot path from sweeping the large UTXO.
const SUBSET_LIMIT_2OFN: usize = 2000;

impl CoinSelector for GreedyExactFitSelector {
    fn select(
        &self,
        asset: &AssetId,
        amount: u64,
        available: &[InventoryUtxo],
    ) -> Result<Selection, CoinSelectionError> {
        if amount == 0 {
            return Err(CoinSelectionError::ZeroAmount);
        }
        if available.iter().any(|u| &u.asset_id != asset) {
            return Err(CoinSelectionError::AssetMismatch);
        }

        let mut candidates: Vec<&InventoryUtxo> = available
            .iter()
            .filter(|u| matches!(u.status, InventoryStatus::Available))
            .collect();

        let total: u64 = candidates.iter().map(|u| u.amount).sum();
        if total < amount {
            return Err(CoinSelectionError::Insufficient {
                wanted: amount,
                available: total,
            });
        }

        // Deterministic ordering: (amount asc, outpoint asc). This gives
        // step 3 the smallest-fit single and lets pair iteration produce
        // stable results when multiple exact pairs exist.
        candidates.sort_by(|a, b| a.amount.cmp(&b.amount).then(a.outpoint.cmp(&b.outpoint)));

        // Step 1: exact single.
        if let Some(u) = candidates.iter().find(|u| u.amount == amount) {
            return Ok(make_selection(asset, vec![u.outpoint.clone()], u.amount, amount));
        }

        // Step 2: bounded subset-sum exact (2-of-N then 3-of-N).
        if candidates.len() <= SUBSET_LIMIT_2OFN {
            for i in 0..candidates.len() {
                for j in (i + 1)..candidates.len() {
                    if candidates[i].amount.saturating_add(candidates[j].amount) == amount {
                        return Ok(make_selection(
                            asset,
                            vec![
                                candidates[i].outpoint.clone(),
                                candidates[j].outpoint.clone(),
                            ],
                            amount,
                            amount,
                        ));
                    }
                }
            }
        }
        if candidates.len() <= SUBSET_LIMIT_3OFN {
            for i in 0..candidates.len() {
                for j in (i + 1)..candidates.len() {
                    let pair_sum = candidates[i].amount.saturating_add(candidates[j].amount);
                    if pair_sum >= amount {
                        // Sorted ascending: pair_sum only grows for higher k.
                        // We can stop the inner loop early but only break this
                        // particular (i, j) row, not abandon i. Skip this row.
                        continue;
                    }
                    for k in (j + 1)..candidates.len() {
                        let triple = pair_sum.saturating_add(candidates[k].amount);
                        if triple == amount {
                            return Ok(make_selection(
                                asset,
                                vec![
                                    candidates[i].outpoint.clone(),
                                    candidates[j].outpoint.clone(),
                                    candidates[k].outpoint.clone(),
                                ],
                                amount,
                                amount,
                            ));
                        }
                        if triple > amount {
                            break;
                        }
                    }
                }
            }
        }

        // Step 3: single UTXO > amount with smallest change. Candidates are
        // sorted ascending, so the first one > amount is the smallest such.
        let single = candidates
            .iter()
            .find(|u| u.amount > amount)
            .map(|u| make_selection(asset, vec![u.outpoint.clone()], u.amount, amount));

        // Step 4: multi-UTXO greedy from largest down.
        let greedy = {
            let mut sorted_desc = candidates.clone();
            sorted_desc.sort_by(|a, b| b.amount.cmp(&a.amount).then(a.outpoint.cmp(&b.outpoint)));
            let mut chosen = Vec::new();
            let mut total_input: u64 = 0;
            for u in sorted_desc {
                chosen.push(u.outpoint.clone());
                total_input = total_input.saturating_add(u.amount);
                if total_input >= amount {
                    break;
                }
            }
            make_selection(asset, chosen, total_input, amount)
        };

        // Step 5: pick the better of {single, greedy}.
        match single {
            None => Ok(greedy),
            Some(s) => Ok(pick_better(s, greedy)),
        }
    }
}

fn make_selection(
    asset: &AssetId,
    mut chosen: Vec<Outpoint>,
    total_input: u64,
    amount: u64,
) -> Selection {
    chosen.sort();
    Selection {
        asset: asset.clone(),
        chosen,
        total_input,
        requested: amount,
        expected_change: total_input - amount,
    }
}

/// Tie-break: smaller change first, then smaller input count, then by sorted
/// outpoint list (already sorted by `make_selection`). `chosen` Vecs of the
/// same length compare lexicographically over their sorted outpoints — fully
/// deterministic.
fn pick_better(a: Selection, b: Selection) -> Selection {
    use std::cmp::Ordering;
    let order = a
        .expected_change
        .cmp(&b.expected_change)
        .then_with(|| a.chosen.len().cmp(&b.chosen.len()))
        .then_with(|| a.chosen.cmp(&b.chosen));
    match order {
        Ordering::Less | Ordering::Equal => a,
        Ordering::Greater => b,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rfq_types::{AssetKind, BitcoinNetwork};
    use std::time::Instant;

    fn asset() -> AssetId {
        AssetId {
            network: BitcoinNetwork::Regtest,
            kind: AssetKind::Rgb20,
            id: "rgb-test".to_owned(),
        }
    }

    fn outpoint_for(idx: u32) -> Outpoint {
        Outpoint::new(format!("{idx:064x}"), 0)
    }

    fn utxo(idx: u32, amount: u64, status: InventoryStatus) -> InventoryUtxo {
        InventoryUtxo {
            outpoint: outpoint_for(idx),
            asset_id: asset(),
            amount,
            btc_sats: 1000,
            status,
            created_at_ms: 0,
            updated_at_ms: 0,
            pending_txid: None,
        }
    }

    fn available(idx: u32, amount: u64) -> InventoryUtxo {
        utxo(idx, amount, InventoryStatus::Available)
    }

    #[test]
    fn rejects_zero_amount() {
        let utxos = vec![available(0, 100)];
        assert!(matches!(
            GreedyExactFitSelector.select(&asset(), 0, &utxos),
            Err(CoinSelectionError::ZeroAmount)
        ));
    }

    #[test]
    fn rejects_asset_mismatch() {
        let other_asset = AssetId {
            network: BitcoinNetwork::Regtest,
            kind: AssetKind::Rgb20,
            id: "other".to_owned(),
        };
        let mut wrong = available(0, 100);
        wrong.asset_id = other_asset;
        assert!(matches!(
            GreedyExactFitSelector.select(&asset(), 50, &[wrong]),
            Err(CoinSelectionError::AssetMismatch)
        ));
    }

    #[test]
    fn rejects_insufficient_liquidity() {
        let utxos = vec![available(0, 30), available(1, 20)];
        assert!(matches!(
            GreedyExactFitSelector.select(&asset(), 100, &utxos),
            Err(CoinSelectionError::Insufficient {
                wanted: 100,
                available: 50
            })
        ));
    }

    #[test]
    fn ignores_non_available_status() {
        let utxos = vec![
            utxo(
                0,
                500,
                InventoryStatus::Spent {
                    witness_txid: "wt".into(),
                    quote_id: rfq_types::QuoteId("q".into()),
                },
            ),
            available(1, 100),
        ];
        let sel = GreedyExactFitSelector.select(&asset(), 50, &utxos).unwrap();
        assert_eq!(sel.total_input, 100);
        assert_eq!(sel.chosen, vec![outpoint_for(1)]);
    }

    #[test]
    fn exact_single_wins_over_multi_combination() {
        // {100, 50, 50}, target 100. Step 1 returns {100} even though
        // {50, 50} is also exact at zero change.
        let utxos = vec![available(0, 100), available(1, 50), available(2, 50)];
        let sel = GreedyExactFitSelector.select(&asset(), 100, &utxos).unwrap();
        assert_eq!(sel.chosen.len(), 1);
        assert_eq!(sel.total_input, 100);
        assert_eq!(sel.expected_change, 0);
    }

    #[test]
    fn exact_pair_when_no_single_fits() {
        // {30, 60, 200}, target 90. No single == 90. Pair 30+60 = 90.
        let utxos = vec![available(0, 30), available(1, 60), available(2, 200)];
        let sel = GreedyExactFitSelector.select(&asset(), 90, &utxos).unwrap();
        assert_eq!(sel.chosen.len(), 2);
        assert_eq!(sel.total_input, 90);
        assert_eq!(sel.expected_change, 0);
    }

    #[test]
    fn exact_triple_for_small_sets() {
        // {10, 20, 30, 50}, target 100. No single, no pair. 20+30+50 = 100.
        let utxos = vec![
            available(0, 10),
            available(1, 20),
            available(2, 30),
            available(3, 50),
        ];
        let sel = GreedyExactFitSelector.select(&asset(), 100, &utxos).unwrap();
        assert_eq!(sel.chosen.len(), 3);
        assert_eq!(sel.total_input, 100);
        assert_eq!(sel.expected_change, 0);
    }

    #[test]
    fn no_exact_minimizes_change_via_single() {
        // {100, 60, 200}, target 150. No exact. Single > 150 = 200, change 50.
        // Greedy desc starts at 200, stops, change 50. Same result.
        let utxos = vec![available(0, 100), available(1, 60), available(2, 200)];
        let sel = GreedyExactFitSelector.select(&asset(), 150, &utxos).unwrap();
        assert_eq!(sel.chosen, vec![outpoint_for(2)]);
        assert_eq!(sel.total_input, 200);
        assert_eq!(sel.expected_change, 50);
    }

    #[test]
    fn smallest_above_beats_larger_above() {
        // {200, 300, 500}, target 100. Single = smallest > 100 = 200,
        // change 100. Greedy desc would start at 500, change 400.
        // pick_better favors single.
        let utxos = vec![available(0, 200), available(1, 300), available(2, 500)];
        let sel = GreedyExactFitSelector.select(&asset(), 100, &utxos).unwrap();
        assert_eq!(sel.chosen, vec![outpoint_for(0)]);
        assert_eq!(sel.total_input, 200);
        assert_eq!(sel.expected_change, 100);
    }

    #[test]
    fn multi_utxo_greedy_when_no_single_fits() {
        // {50, 30, 20}, target 90. No exact (subset checked: 50+30=80,
        // 50+20=70, 30+20=50, 50+30+20=100). Single > 90 = none.
        // Greedy desc: 50, 30, 20 → cumulative 100 at 3 inputs, change 10.
        let utxos = vec![available(0, 50), available(1, 30), available(2, 20)];
        let sel = GreedyExactFitSelector.select(&asset(), 90, &utxos).unwrap();
        assert_eq!(sel.chosen.len(), 3);
        assert_eq!(sel.total_input, 100);
        assert_eq!(sel.expected_change, 10);
    }

    #[test]
    fn selection_chosen_is_sorted_by_outpoint() {
        let utxos = vec![available(2, 50), available(0, 50), available(1, 50)];
        let sel = GreedyExactFitSelector.select(&asset(), 150, &utxos).unwrap();
        assert_eq!(sel.chosen.len(), 3);
        for window in sel.chosen.windows(2) {
            assert!(window[0] <= window[1], "chosen not sorted: {:?}", sel.chosen);
        }
    }

    #[test]
    fn fragmentation_hot_path_prefers_small_pair_over_large_single() {
        // 100 dust UTXOs of 10 each + 1 fat UTXO of 10000. Target 20.
        // 2-of-N at N=101 finds {10, 10} = 20 exact. The fat UTXO must NOT
        // be swept — that's the whole point of denomination-aware selection.
        let mut utxos: Vec<_> = (0..100).map(|i| available(i, 10)).collect();
        utxos.push(available(100, 10000));

        let sel = GreedyExactFitSelector.select(&asset(), 20, &utxos).unwrap();
        assert_eq!(sel.total_input, 20);
        assert_eq!(sel.chosen.len(), 2);
        assert_eq!(sel.expected_change, 0);
        // Confirm the fat one is NOT in the selection.
        let fat = outpoint_for(100);
        assert!(!sel.chosen.contains(&fat));
    }

    #[test]
    fn coin_selection_handles_1000_utxos_within_100ms() {
        let utxos: Vec<_> = (0..1000).map(|i| available(i, 10)).collect();
        let start = Instant::now();
        let sel = GreedyExactFitSelector.select(&asset(), 20, &utxos).unwrap();
        let elapsed = start.elapsed();
        assert_eq!(sel.total_input, 20);
        assert!(
            elapsed.as_millis() < 100,
            "1000-UTXO selection took {elapsed:?}; expected < 100ms"
        );
    }
}
