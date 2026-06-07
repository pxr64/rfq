use std::net::SocketAddr;

use rfq_api::registry::MakerRegistry;

/// Broker entrypoint.
///
/// Makers **auto-register** over a WebSocket: `colorex maker up` dials
/// `ws://<this-broker>/maker-stream` and registers itself — no broker config
/// needed. The broker routes accept/consignment/sign by matching the registered
/// `maker_id` against the stored `quote.maker_id`. `BROKER_LISTEN` overrides the
/// bind (default `127.0.0.1:3000`).
#[tokio::main]
async fn main() {
    let listen: SocketAddr = std::env::var("BROKER_LISTEN")
        .unwrap_or_else(|_| "127.0.0.1:3000".to_owned())
        .parse()
        .expect("BROKER_LISTEN must be a socket address, e.g. 127.0.0.1:3000");

    // Makers self-register over /maker-stream; the registry starts empty.
    let registry = MakerRegistry::new();
    println!("broker: makers auto-register over ws://…/maker-stream");

    let app = rfq_api::app_with_registry(registry);

    let listener = tokio::net::TcpListener::bind(listen)
        .await
        .expect("bind API listener");
    println!("broker listening on {listen}");

    axum::serve(listener, app).await.expect("run API server");
}
