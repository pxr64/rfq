use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Outpoint {
    pub txid: String,
    pub vout: u32,
}

impl Outpoint {
    pub fn new(txid: impl Into<String>, vout: u32) -> Self {
        Self {
            txid: txid.into(),
            vout,
        }
    }
}

impl std::fmt::Display for Outpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}", self.txid, self.vout)
    }
}

impl std::str::FromStr for Outpoint {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (txid, vout) = s
            .rsplit_once(':')
            .ok_or_else(|| format!("outpoint missing ':' separator: {s:?}"))?;

        if txid.len() != 64 {
            return Err(format!(
                "outpoint txid must be 64 hex chars, got {}: {txid:?}",
                txid.len()
            ));
        }
        if !txid
            .chars()
            .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c))
        {
            return Err(format!(
                "outpoint txid must be lowercase hex (0-9a-f): {txid:?}"
            ));
        }
        let vout: u32 = vout
            .parse()
            .map_err(|e| format!("outpoint vout parse error: {e}"))?;

        Ok(Self {
            txid: txid.to_owned(),
            vout,
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct RfqId(pub String);

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct QuoteId(pub String);

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct MakerId(pub String);

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct AssetId {
    pub network: BitcoinNetwork,
    pub kind: AssetKind,
    pub id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub enum BitcoinNetwork {
    Mainnet,
    Testnet,
    Signet,
    Regtest,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub enum AssetKind {
    Btc,
    Rgb20,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum Side {
    Buy,
    Sell,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuoteRequest {
    pub rfq_id: RfqId,
    pub base_asset: AssetId,
    pub quote_asset: AssetId,
    pub side: Side,
    pub amount: u64,
    pub created_at_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateRfqRequest {
    pub base_asset: AssetId,
    pub quote_asset: AssetId,
    pub side: Side,
    pub amount: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HealthResponse {
    pub status: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Quote {
    pub quote_id: QuoteId,
    pub rfq_id: RfqId,
    pub maker_id: MakerId,
    pub base_asset: AssetId,
    pub quote_asset: AssetId,
    pub side: Side,
    pub amount: u64,
    pub price: u64,
    pub expires_at_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AcceptQuoteRequest {
    pub quote_id: QuoteId,
    pub rgb_invoice: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SettlementIntent {
    pub quote_id: QuoteId,
    pub maker_id: MakerId,
    pub status: SettlementStatus,
    pub transfer: Option<RgbTransfer>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum SettlementStatus {
    Pending,
    Ready,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Allocation {
    pub maker_id: MakerId,
    pub asset: AssetId,
    pub available_amount: u64,
}

/// Per-UTXO inventory entry returned by `RgbBackend::list_inventory_utxos`.
/// `btc_sats` may be 0 when the backend hasn't surfaced bp-wallet UTXO data yet
/// (the value is only used by the maker's fragmentation heuristics, which fall
/// back to amount-based math when sats are unknown).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RgbInventoryUtxo {
    pub outpoint: Outpoint,
    pub asset_id: AssetId,
    pub amount: u64,
    pub btc_sats: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum AllocationState {
    Available,
    Reserved {
        quote_id: QuoteId,
        expires_at_ms: u64,
    },
    Spent {
        quote_id: QuoteId,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ManagedAllocation {
    pub allocation: Allocation,
    pub state: AllocationState,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct InventorySnapshot {
    pub total_amount: u64,
    pub available_amount: u64,
    pub reserved_amount: u64,
    pub spent_amount: u64,
    pub total_allocations: u64,
    pub available_allocations: u64,
    pub reserved_allocations: u64,
    pub spent_allocations: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RgbTransfer {
    pub psbt: String,
    pub consignment: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    const VALID_TXID: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    #[test]
    fn outpoint_display_uses_txid_colon_vout() {
        let op = Outpoint::new(VALID_TXID, 3);
        assert_eq!(op.to_string(), format!("{VALID_TXID}:3"));
    }

    #[test]
    fn outpoint_from_str_parses_valid_string() {
        let s = format!("{VALID_TXID}:7");
        let op = Outpoint::from_str(&s).unwrap();
        assert_eq!(op.txid, VALID_TXID);
        assert_eq!(op.vout, 7);
    }

    #[test]
    fn outpoint_round_trips_through_display_and_from_str() {
        let original = Outpoint::new(VALID_TXID, 42);
        let parsed = Outpoint::from_str(&original.to_string()).unwrap();
        assert_eq!(original, parsed);
    }

    #[test]
    fn outpoint_from_str_rejects_missing_colon() {
        assert!(Outpoint::from_str(VALID_TXID).is_err());
    }

    #[test]
    fn outpoint_from_str_rejects_short_txid() {
        assert!(Outpoint::from_str("abc:0").is_err());
    }

    #[test]
    fn outpoint_from_str_rejects_uppercase_hex() {
        let upper = VALID_TXID.to_uppercase();
        assert!(Outpoint::from_str(&format!("{upper}:0")).is_err());
    }

    #[test]
    fn outpoint_from_str_rejects_non_hex_txid() {
        let bad = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdeg";
        assert!(Outpoint::from_str(&format!("{bad}:0")).is_err());
    }

    #[test]
    fn outpoint_from_str_rejects_invalid_vout() {
        assert!(Outpoint::from_str(&format!("{VALID_TXID}:notanumber")).is_err());
    }

    #[test]
    fn outpoint_serde_round_trip() {
        let op = Outpoint::new(VALID_TXID, 9);
        let json = serde_json::to_string(&op).unwrap();
        let parsed: Outpoint = serde_json::from_str(&json).unwrap();
        assert_eq!(op, parsed);
    }
}
