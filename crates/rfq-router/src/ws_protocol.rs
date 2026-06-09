//! Wire protocol for the persistent maker↔broker WebSocket.
//!
//! The maker dials the broker and registers; the broker then *pushes*
//! [`MakerConnector`](crate::MakerConnector) requests over the socket and the
//! maker replies. Directionality is fixed, so the two sides use distinct frame
//! types rather than one union:
//!
//! - broker → maker: [`WsRequest`] (a [`WsOp`] tagged by `req_id`)
//! - maker → broker: [`MakerFrame`] — the FIRST frame after connect MUST be
//!   [`MakerFrame::Register`]; every later frame is [`MakerFrame::Response`].
//!
//! All frames are JSON text frames.

use rfq_types::{
    AcceptQuoteRequest, AssetInfo, BitcoinNetwork, MakerId, OrderPrice, Outpoint, Quote, QuoteId,
    QuoteRequest, SettlementIntent,
};
use serde::{Deserialize, Serialize};

/// Broker → maker. Correlated back to the caller by `req_id`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WsRequest {
    pub req_id: u64,
    pub op: WsOp,
}

/// The four `MakerConnector` calls, as pushed over the socket.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum WsOp {
    RequestQuote {
        request: QuoteRequest,
    },
    AcceptQuote {
        quote: Quote,
        request: AcceptQuoteRequest,
    },
    DeliverConsignment {
        quote_id: QuoteId,
        consignment: String,
        /// The taker's RGB UTXOs being sold (provenance model — the consignment
        /// proves the asset, the taker names which outpoints it offers).
        #[serde(default)]
        outpoints: Vec<Outpoint>,
    },
    SubmitSignedPsbt {
        quote_id: QuoteId,
        psbt: String,
    },
}

/// Maker → broker.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum MakerFrame {
    /// Mandatory first frame: identifies the maker so the broker can route
    /// `quote.maker_id` back to this connection. `network` and `assets` are
    /// optional observability metadata (older makers omit them) the broker
    /// surfaces via `GET /status`; `assets` lists the RGB contracts this maker
    /// serves (each paired against BTC).
    Register {
        maker_id: MakerId,
        #[serde(default)]
        network: Option<BitcoinNetwork>,
        #[serde(default)]
        assets: Vec<AssetInfo>,
        #[serde(default)]
        prices: Vec<OrderPrice>,
    },
    /// Reply to a `WsRequest`, correlated by `req_id`.
    Response { req_id: u64, result: WsResult },
}

/// The three return shapes of the trait methods, collapsed for the wire.
/// `request_quote` → `Quote`; the three settlement methods → `Settlement`;
/// any error → `Err`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum WsResult {
    Quote { quote: Option<Quote> },
    Settlement { intent: SettlementIntent },
    Err { message: String },
}
