//! Maker-side WebSocket client — the auto-discovery counterpart to the broker's
//! `/maker-stream`. The maker dials the broker, sends a `Register` frame, then
//! serves pushed [`MakerConnector`] requests from the local [`Maker`] over the
//! same socket. Reconnects with capped exponential backoff, so the broker can
//! restart without the maker needing a restart.

use std::time::Duration;

use futures::{SinkExt, StreamExt};
use rfq_maker::Maker;
use rfq_router::ws_protocol::{MakerFrame, WsOp, WsRequest, WsResult};
use rfq_router::{MakerConnector, RouterError};
use rfq_types::{MakerId, SettlementIntent};
use tokio_tungstenite::tungstenite::Message;

/// Derive the broker's `/maker-stream` WebSocket URL from its HTTP `broker_url`
/// (`http`→`ws`, `https`→`wss`).
pub fn broker_ws_url(broker_url: &str) -> String {
    let base = broker_url.trim_end_matches('/');
    let base = base
        .strip_prefix("https://")
        .map(|rest| format!("wss://{rest}"))
        .or_else(|| base.strip_prefix("http://").map(|rest| format!("ws://{rest}")))
        .unwrap_or_else(|| base.to_owned());
    format!("{base}/maker-stream")
}

/// Run the broker stream forever: (re)connect, register, serve. Spawn this as a
/// background task from `maker up`.
pub async fn run_broker_stream(ws_url: String, maker_id: MakerId, maker: Maker) {
    let mut backoff = Duration::from_millis(500);
    loop {
        match connect_and_serve(&ws_url, &maker_id, &maker).await {
            Ok(()) => {
                println!("broker stream closed; reconnecting");
                backoff = Duration::from_millis(500);
            }
            Err(e) => eprintln!("broker stream error: {e}; reconnecting"),
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(Duration::from_secs(30));
    }
}

async fn connect_and_serve(
    ws_url: &str,
    maker_id: &MakerId,
    maker: &Maker,
) -> Result<(), Box<dyn std::error::Error>> {
    let (ws, _resp) = tokio_tungstenite::connect_async(ws_url).await?;
    let (mut write, mut read) = ws.split();

    let register = serde_json::to_string(&MakerFrame::Register {
        maker_id: maker_id.clone(),
    })?;
    write.send(Message::Text(register)).await?;
    println!("registered with broker as {}", maker_id.0);

    while let Some(msg) = read.next().await {
        let text = match msg? {
            Message::Text(t) => t,
            Message::Close(_) => break,
            Message::Ping(_) | Message::Pong(_) | Message::Binary(_) | Message::Frame(_) => continue,
        };
        let request: WsRequest = serde_json::from_str(&text)?;
        let result = dispatch(maker, request.op).await;
        let response = MakerFrame::Response {
            req_id: request.req_id,
            result,
        };
        write
            .send(Message::Text(serde_json::to_string(&response)?))
            .await?;
    }
    Ok(())
}

/// Run one pushed request against the local maker. Mirrors the `maker_app` HTTP
/// handlers, minus the local quote store — the broker forwards the full `Quote`
/// in `AcceptQuote`, and settlement methods look up their reservation/state in
/// the `Maker` itself (the same instance that quoted).
async fn dispatch(maker: &Maker, op: WsOp) -> WsResult {
    match op {
        WsOp::RequestQuote { request } => match maker.request_quote(request).await {
            Ok(quote) => WsResult::Quote { quote },
            Err(e) => err(e),
        },
        WsOp::AcceptQuote { quote, request } => settle(maker.accept_quote(quote, request).await),
        WsOp::DeliverConsignment {
            quote_id,
            consignment,
        } => settle(maker.deliver_consignment(quote_id, consignment).await),
        WsOp::SubmitSignedPsbt { quote_id, psbt } => {
            settle(maker.submit_signed_psbt(quote_id, psbt).await)
        }
    }
}

fn settle(result: Result<SettlementIntent, RouterError>) -> WsResult {
    match result {
        Ok(intent) => WsResult::Settlement { intent },
        Err(e) => err(e),
    }
}

fn err(e: RouterError) -> WsResult {
    WsResult::Err {
        message: e.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::broker_ws_url;

    #[test]
    fn derives_ws_url() {
        assert_eq!(
            broker_ws_url("http://127.0.0.1:3000"),
            "ws://127.0.0.1:3000/maker-stream"
        );
        assert_eq!(
            broker_ws_url("https://broker.example/"),
            "wss://broker.example/maker-stream"
        );
    }
}
