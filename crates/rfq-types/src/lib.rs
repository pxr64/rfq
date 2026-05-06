use serde::{Deserialize, Serialize};

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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RgbTransfer {
    pub psbt: String,
    pub consignment: String,
}
