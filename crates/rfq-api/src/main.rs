use std::net::SocketAddr;
use std::sync::Arc;

use rfq_api::registry::MakerRegistry;
use rfq_router::{HttpMakerConnector, MakerConnector, Url};
use rfq_types::MakerId;

/// Broker entrypoint.
///
/// Makers **auto-register** over a WebSocket: `colorex maker up` dials
/// `ws://<this-broker>/maker-stream` and registers itself — no broker config
/// needed. The broker routes accept/consignment/sign by matching the registered
/// `maker_id` against the stored `quote.maker_id`.
///
/// `BROKER_MAKER` (optional, legacy) statically pre-seeds HTTP-dialed makers:
/// `<maker_id>@<url>`, comma-separated; `<maker_id>` MUST equal the maker's
/// `config.maker.node_id`. `BROKER_LISTEN` overrides the bind (default
/// `127.0.0.1:3000`).
#[tokio::main]
async fn main() {
    let listen: SocketAddr = std::env::var("BROKER_LISTEN")
        .unwrap_or_else(|_| "127.0.0.1:3000".to_owned())
        .parse()
        .expect("BROKER_LISTEN must be a socket address, e.g. 127.0.0.1:3000");

    // Optional static pre-seed; makers also self-register over /maker-stream.
    let registry = match std::env::var("BROKER_MAKER") {
        Ok(spec) if !spec.trim().is_empty() => {
            let makers = parse_makers(&spec);
            for m in &makers {
                println!("broker: static maker {}", m.maker_id().0);
            }
            MakerRegistry::with(makers)
        }
        _ => {
            println!("broker: makers auto-register over ws://…/maker-stream");
            MakerRegistry::new()
        }
    };

    let app = rfq_api::app_with_registry(registry);

    let listener = tokio::net::TcpListener::bind(listen)
        .await
        .expect("bind API listener");
    println!("broker listening on {listen}");

    axum::serve(listener, app).await.expect("run API server");
}

/// Parse `BROKER_MAKER` into HTTP connectors. Format: comma-separated
/// `<maker_id>@<url>` entries. Mirrors the connector construction in
/// `crates/maker-node/tests/broker_round_trip.rs`.
fn parse_makers(spec: &str) -> Vec<Arc<dyn MakerConnector>> {
    spec.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|entry| {
            let (id, url) = entry.split_once('@').unwrap_or_else(|| {
                panic!("BROKER_MAKER entry must be `<maker_id>@<url>`, got `{entry}`")
            });
            let url = Url::parse(url.trim())
                .unwrap_or_else(|e| panic!("BROKER_MAKER url `{url}` is invalid: {e}"));
            let connector = HttpMakerConnector::new(MakerId(id.trim().to_owned()), url);
            Arc::new(connector) as Arc<dyn MakerConnector>
        })
        .collect()
}
