//! `GET /maker-stream` — the WebSocket a maker dials to auto-register. The
//! broker reads the mandatory `Register` frame, builds a [`WsMakerConnector`]
//! over the socket, inserts it into the [`MakerRegistry`](crate::registry::MakerRegistry),
//! and removes it when the socket closes.

use std::sync::Arc;
use std::time::Duration;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::response::Response;
use futures::stream::SplitStream;
use futures::{SinkExt, StreamExt};
use rfq_router::ws_protocol::MakerFrame;
use rfq_router::{MakerConnector, WsMakerConnector};
use rfq_types::MakerId;
use tokio::time::timeout;

use crate::AppState;

pub(crate) async fn maker_stream(ws: WebSocketUpgrade, State(state): State<AppState>) -> Response {
    ws.on_upgrade(move |socket| handle_socket(socket, state))
}

async fn handle_socket(socket: WebSocket, state: AppState) {
    let (sink, mut stream) = socket.split();

    // The first frame must identify the maker.
    let Some(maker_id) = read_register(&mut stream).await else {
        return;
    };

    // Adapt axum's `Message` frames ↔ `String` for the generic connector.
    // `Box::pin` makes the `with`/`filter_map` adapters `Unpin` (their async
    // closures aren't), satisfying `WsMakerConnector::spawn`'s bounds.
    let text_sink = Box::pin(
        sink.with(|s: String| async move { Ok::<Message, axum::Error>(Message::Text(s)) }),
    );
    let text_stream = Box::pin(stream.filter_map(|m| async move {
        match m {
            Ok(Message::Text(t)) => Some(Ok::<String, ()>(t)),
            _ => None, // close / error / ping / pong / binary
        }
    }));

    let (connector, closed) = WsMakerConnector::spawn(maker_id.clone(), text_sink, text_stream);
    let connector: Arc<dyn MakerConnector> = connector;
    state.registry.insert(connector.clone()).await;
    println!("broker: maker registered: {}", maker_id.0);

    // Hold until the socket dies, then deregister (only if still this conn).
    let _ = closed.await;
    state.registry.remove_if(&maker_id, &connector).await;
    println!("broker: maker disconnected: {}", maker_id.0);
}

/// Wait for the `Register` frame (≤5s per frame). Non-register text is ignored;
/// a close/error/timeout ends the connection.
async fn read_register(stream: &mut SplitStream<WebSocket>) -> Option<MakerId> {
    loop {
        match timeout(Duration::from_secs(5), stream.next()).await {
            Ok(Some(Ok(Message::Text(t)))) => {
                if let Ok(MakerFrame::Register { maker_id }) =
                    serde_json::from_str::<MakerFrame>(&t)
                {
                    return Some(maker_id);
                }
                // non-register text before register: keep waiting
            }
            Ok(Some(Ok(_))) => continue, // ping/pong/binary
            _ => return None,            // close, error, stream end, or timeout
        }
    }
}
