//! [`WsMakerConnector`] — a [`MakerConnector`] backed by a persistent WebSocket
//! the maker dialed to the broker. The broker pushes requests over the socket
//! and awaits correlated responses.
//!
//! Generic over a `Sink<String>` + `Stream<Item = Result<String, ()>>` of JSON
//! text frames, so the broker (axum) and any client (tungstenite) supply the
//! transport adapter — no WS library leaks into this crate.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures::{Sink, SinkExt, Stream, StreamExt};
use tokio::sync::{mpsc, oneshot, Mutex};
use tokio::time::timeout;

use rfq_types::{
    AcceptQuoteRequest, MakerId, OrderPrice, Outpoint, Quote, QuoteId, QuoteRequest,
    SettlementIntent,
};

use crate::ws_protocol::{MakerFrame, WsOp, WsRequest, WsResult};
use crate::{MakerConnector, RouterError};

/// Hard ceiling on a single settlement call (accept / consignment / sign). The
/// fanout `request_quote` path is additionally capped at 500ms by `fanout_quote`.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<WsResult>>>>;

pub struct WsMakerConnector {
    maker_id: MakerId,
    next_req_id: AtomicU64,
    /// Outgoing frames go through an mpsc to a single writer task, so the shared
    /// `Arc<dyn MakerConnector>` can be called concurrently without locking the
    /// sink (concurrent `Sink::send` would be unsound).
    to_socket: mpsc::Sender<String>,
    pending: Pending,
}

impl WsMakerConnector {
    /// Spawn the reader/writer tasks over an already-split socket. Returns the
    /// connector plus a `closed` receiver that fires when the socket's stream
    /// ends (so the broker can deregister this maker).
    pub fn spawn<Si, St>(
        maker_id: MakerId,
        mut sink: Si,
        mut stream: St,
    ) -> (Arc<Self>, oneshot::Receiver<()>)
    where
        Si: Sink<String> + Send + Unpin + 'static,
        St: Stream<Item = Result<String, ()>> + Send + Unpin + 'static,
    {
        let (tx, mut rx) = mpsc::channel::<String>(64);
        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let (closed_tx, closed_rx) = oneshot::channel();

        // Writer: serialize outgoing frames onto the sink, one at a time.
        tokio::spawn(async move {
            while let Some(text) = rx.recv().await {
                if sink.send(text).await.is_err() {
                    break;
                }
            }
            let _ = sink.close().await;
        });

        // Reader: demux `Response`s into the pending oneshots; on stream end,
        // fail every outstanding request and signal `closed`.
        let pending_reader = Arc::clone(&pending);
        tokio::spawn(async move {
            while let Some(item) = stream.next().await {
                let Ok(text) = item else { break };
                if let Ok(MakerFrame::Response { req_id, result }) =
                    serde_json::from_str::<MakerFrame>(&text)
                {
                    if let Some(tx) = pending_reader.lock().await.remove(&req_id) {
                        let _ = tx.send(result);
                    }
                }
                // `Register`-after-register or malformed frames are ignored.
            }
            let mut map = pending_reader.lock().await;
            for (_, tx) in map.drain() {
                let _ = tx.send(WsResult::Err {
                    message: "maker disconnected".to_owned(),
                });
            }
            let _ = closed_tx.send(());
        });

        let connector = Arc::new(Self {
            maker_id,
            next_req_id: AtomicU64::new(1),
            to_socket: tx,
            pending,
        });
        (connector, closed_rx)
    }

    async fn call(&self, op: WsOp) -> Result<WsResult, RouterError> {
        let req_id = self.next_req_id.fetch_add(1, Ordering::Relaxed);
        // Serialize before registering the pending entry so a serde error can't
        // leak it.
        let frame = serde_json::to_string(&WsRequest { req_id, op })
            .map_err(|e| RouterError::Maker(format!("encode request: {e}")))?;

        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(req_id, tx);

        if self.to_socket.send(frame).await.is_err() {
            self.pending.lock().await.remove(&req_id);
            return Err(RouterError::Maker("maker connection closed".to_owned()));
        }

        match timeout(REQUEST_TIMEOUT, rx).await {
            Ok(Ok(result)) => Ok(result),
            Ok(Err(_)) => Err(RouterError::Maker("maker connection dropped".to_owned())),
            Err(_) => {
                self.pending.lock().await.remove(&req_id);
                Err(RouterError::Maker("maker request timed out".to_owned()))
            }
        }
    }

    async fn settlement(&self, op: WsOp) -> Result<SettlementIntent, RouterError> {
        match self.call(op).await? {
            WsResult::Settlement { intent } => Ok(intent),
            WsResult::Err { message } => Err(RouterError::Maker(message)),
            WsResult::Quote { .. } | WsResult::Prices { .. } => {
                Err(RouterError::Maker("unexpected non-settlement response".to_owned()))
            }
        }
    }
}

#[async_trait]
impl MakerConnector for WsMakerConnector {
    fn maker_id(&self) -> MakerId {
        self.maker_id.clone()
    }

    async fn request_quote(&self, request: QuoteRequest) -> Result<Option<Quote>, RouterError> {
        match self.call(WsOp::RequestQuote { request }).await? {
            WsResult::Quote { quote } => Ok(quote),
            WsResult::Err { message } => Err(RouterError::Maker(message)),
            WsResult::Settlement { .. } | WsResult::Prices { .. } => {
                Err(RouterError::Maker("unexpected non-quote response".to_owned()))
            }
        }
    }

    async fn request_prices(&self) -> Result<Vec<OrderPrice>, RouterError> {
        match self.call(WsOp::RequestPrices).await? {
            WsResult::Prices { prices } => Ok(prices),
            WsResult::Err { message } => Err(RouterError::Maker(message)),
            WsResult::Quote { .. } | WsResult::Settlement { .. } => {
                Err(RouterError::Maker("unexpected non-prices response".to_owned()))
            }
        }
    }

    async fn accept_quote(
        &self,
        quote: Quote,
        request: AcceptQuoteRequest,
    ) -> Result<SettlementIntent, RouterError> {
        self.settlement(WsOp::AcceptQuote { quote, request }).await
    }

    async fn deliver_consignment(
        &self,
        quote_id: QuoteId,
        consignment_base64: String,
        consigned_outpoints: Vec<Outpoint>,
    ) -> Result<SettlementIntent, RouterError> {
        self.settlement(WsOp::DeliverConsignment {
            quote_id,
            consignment: consignment_base64,
            outpoints: consigned_outpoints,
        })
        .await
    }

    async fn submit_signed_psbt(
        &self,
        quote_id: QuoteId,
        signed_psbt_base64: String,
    ) -> Result<SettlementIntent, RouterError> {
        self.settlement(WsOp::SubmitSignedPsbt {
            quote_id,
            psbt: signed_psbt_base64,
        })
        .await
    }
}
