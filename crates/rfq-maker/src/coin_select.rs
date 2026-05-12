use rfq_types::{AssetId, InventoryStatus, InventoryUtxo, Outpoint};
use thiserror::Error;

/// Result of selecting UTXOs to cover a target amount. `total_input` is the
/// sum of `chosen` amounts; `expected_change = total_input - requested` is the
/// change the transfer will need to produce.
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

/// Placeholder selector for 14c that picks a single UTXO `>= amount`, mirroring
/// the whole-allocation behavior of pre-#14 MockMaker. 14d replaces this with
/// `GreedyExactFitSelector`.
#[derive(Debug, Clone, Default)]
pub struct WholeUtxoSelector;

impl CoinSelector for WholeUtxoSelector {
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

        let candidates: Vec<&InventoryUtxo> = available
            .iter()
            .filter(|u| matches!(u.status, InventoryStatus::Available) && u.amount >= amount)
            .collect();

        let total_available: u64 = available
            .iter()
            .filter(|u| matches!(u.status, InventoryStatus::Available))
            .map(|u| u.amount)
            .sum();

        let Some(picked) = candidates.into_iter().min_by_key(|u| u.amount) else {
            return Err(CoinSelectionError::Insufficient {
                wanted: amount,
                available: total_available,
            });
        };

        Ok(Selection {
            asset: asset.clone(),
            chosen: vec![picked.outpoint.clone()],
            total_input: picked.amount,
            requested: amount,
            expected_change: picked.amount - amount,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rfq_types::{AssetKind, BitcoinNetwork};

    const TXID: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    fn asset() -> AssetId {
        AssetId {
            network: BitcoinNetwork::Regtest,
            kind: AssetKind::Rgb20,
            id: "rgb-test".to_owned(),
        }
    }

    fn utxo(vout: u32, amount: u64, status: InventoryStatus) -> InventoryUtxo {
        InventoryUtxo {
            outpoint: Outpoint::new(TXID, vout),
            asset_id: asset(),
            amount,
            btc_sats: 1000,
            status,
            created_at_ms: 0,
            updated_at_ms: 0,
            pending_txid: None,
        }
    }

    #[test]
    fn whole_utxo_picks_smallest_that_fits() {
        let utxos = vec![
            utxo(0, 100, InventoryStatus::Available),
            utxo(1, 250, InventoryStatus::Available),
            utxo(2, 50, InventoryStatus::Available),
        ];
        let selection = WholeUtxoSelector
            .select(&asset(), 80, &utxos)
            .expect("should select");
        assert_eq!(selection.chosen.len(), 1);
        assert_eq!(selection.total_input, 100);
        assert_eq!(selection.expected_change, 20);
    }

    #[test]
    fn whole_utxo_insufficient_when_no_utxo_fits() {
        let utxos = vec![
            utxo(0, 50, InventoryStatus::Available),
            utxo(1, 30, InventoryStatus::Available),
        ];
        let err = WholeUtxoSelector
            .select(&asset(), 100, &utxos)
            .expect_err("should fail");
        assert!(matches!(
            err,
            CoinSelectionError::Insufficient {
                wanted: 100,
                available: 80
            }
        ));
    }

    #[test]
    fn whole_utxo_ignores_non_available_status() {
        let utxos = vec![
            utxo(
                0,
                500,
                InventoryStatus::Spent {
                    witness_txid: "wt".into(),
                    quote_id: rfq_types::QuoteId("q".into()),
                },
            ),
            utxo(1, 100, InventoryStatus::Available),
        ];
        let selection = WholeUtxoSelector
            .select(&asset(), 50, &utxos)
            .expect("should select the Available one");
        assert_eq!(selection.total_input, 100);
    }

    #[test]
    fn whole_utxo_rejects_zero_amount() {
        let utxos = vec![utxo(0, 100, InventoryStatus::Available)];
        assert!(matches!(
            WholeUtxoSelector.select(&asset(), 0, &utxos),
            Err(CoinSelectionError::ZeroAmount)
        ));
    }
}
