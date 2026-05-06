use std::{collections::HashMap, sync::Arc};

use async_trait::async_trait;
use rfq_types::{Quote, QuoteId};
use tokio::sync::RwLock;

#[async_trait]
pub trait QuoteStore: Send + Sync {
    async fn save_quote(&self, quote: Quote);

    async fn get_quote(&self, quote_id: &QuoteId) -> Option<Quote>;
}

#[derive(Debug, Clone, Default)]
pub struct InMemoryQuoteStore {
    quotes: Arc<RwLock<HashMap<QuoteId, Quote>>>,
}

impl InMemoryQuoteStore {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl QuoteStore for InMemoryQuoteStore {
    async fn save_quote(&self, quote: Quote) {
        self.quotes
            .write()
            .await
            .insert(quote.quote_id.clone(), quote);
    }

    async fn get_quote(&self, quote_id: &QuoteId) -> Option<Quote> {
        self.quotes.read().await.get(quote_id).cloned()
    }
}
