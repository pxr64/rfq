use rfq_types::{Quote, QuoteRequest, Side};
use thiserror::Error;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum RfqCoreError {
    #[error("amount must be greater than zero")]
    ZeroAmount,
    #[error("base and quote assets must differ")]
    SameAsset,
    #[error("base and quote assets must be on the same Bitcoin network")]
    NetworkMismatch,
}

pub fn validate_quote_request(request: &QuoteRequest) -> Result<(), RfqCoreError> {
    if request.amount == 0 {
        return Err(RfqCoreError::ZeroAmount);
    }

    if request.base_asset == request.quote_asset {
        return Err(RfqCoreError::SameAsset);
    }

    if request.base_asset.network != request.quote_asset.network {
        return Err(RfqCoreError::NetworkMismatch);
    }

    Ok(())
}

pub fn is_quote_expired(quote: &Quote, now_ms: u64) -> bool {
    quote.expires_at_ms <= now_ms
}

pub fn sort_quotes_best_price(quotes: &mut [Quote]) {
    quotes.sort_by(|left, right| match left.side {
        Side::Buy => left.price.cmp(&right.price),
        Side::Sell => right.price.cmp(&left.price),
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use rfq_types::{AssetId, AssetKind, BitcoinNetwork, MakerId, QuoteId, RfqId};

    fn asset(kind: AssetKind, id: &str) -> AssetId {
        AssetId {
            network: BitcoinNetwork::Regtest,
            kind,
            id: id.to_owned(),
        }
    }

    fn request(amount: u64) -> QuoteRequest {
        QuoteRequest {
            rfq_id: RfqId("rfq-1".to_owned()),
            base_asset: asset(AssetKind::Rgb20, "rgb-asset"),
            quote_asset: asset(AssetKind::Btc, "btc"),
            side: Side::Buy,
            amount,
            created_at_ms: 1_000,
        }
    }

    fn quote(side: Side, price: u64) -> Quote {
        Quote {
            quote_id: QuoteId(format!("quote-{price}")),
            rfq_id: RfqId("rfq-1".to_owned()),
            maker_id: MakerId("maker-1".to_owned()),
            base_asset: asset(AssetKind::Rgb20, "rgb-asset"),
            quote_asset: asset(AssetKind::Btc, "btc"),
            side,
            amount: 10,
            price,
            expires_at_ms: 2_000,
        }
    }

    #[test]
    fn validates_positive_request() {
        assert_eq!(validate_quote_request(&request(10)), Ok(()));
    }

    #[test]
    fn rejects_zero_amount() {
        assert_eq!(
            validate_quote_request(&request(0)),
            Err(RfqCoreError::ZeroAmount)
        );
    }

    #[test]
    fn rejects_same_asset() {
        let mut request = request(10);
        request.quote_asset = request.base_asset.clone();

        assert_eq!(
            validate_quote_request(&request),
            Err(RfqCoreError::SameAsset)
        );
    }

    #[test]
    fn rejects_network_mismatch() {
        let mut request = request(10);
        request.quote_asset.network = BitcoinNetwork::Testnet;

        assert_eq!(
            validate_quote_request(&request),
            Err(RfqCoreError::NetworkMismatch)
        );
    }

    #[test]
    fn detects_expired_quotes() {
        let quote = quote(Side::Buy, 100);

        assert!(is_quote_expired(&quote, 2_000));
        assert!(!is_quote_expired(&quote, 1_999));
    }

    #[test]
    fn sorts_buy_quotes_by_lowest_price() {
        let mut quotes = vec![quote(Side::Buy, 120), quote(Side::Buy, 100)];

        sort_quotes_best_price(&mut quotes);

        assert_eq!(quotes[0].price, 100);
    }

    #[test]
    fn sorts_sell_quotes_by_highest_price() {
        let mut quotes = vec![quote(Side::Sell, 100), quote(Side::Sell, 120)];

        sort_quotes_best_price(&mut quotes);

        assert_eq!(quotes[0].price, 120);
    }
}
