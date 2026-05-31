use std::net::SocketAddr;
use std::sync::Arc;

use rfq_router::{HttpMakerConnector, MakerConnector, Url};
use rfq_types::MakerId;

/// Broker entrypoint.
///
/// By default this serves the in-process **mock** makers (`rfq_api::app()`),
/// matching the original demo behavior. To point the broker at one or more
/// real `colorex maker up` daemons, set `BROKER_MAKER`:
///
/// ```text
/// BROKER_MAKER="regtest-maker@http://127.0.0.1:4000" cargo run -p rfq-api
/// ```
///
/// `BROKER_MAKER` is `<maker_id>@<url>`; repeat by comma-separating entries.
/// The `<maker_id>` MUST equal the maker daemon's `config.maker.node_id` — the
/// broker routes accept/consignment/sign by matching the stored `quote.maker_id`.
///
/// `BROKER_LISTEN` overrides the bind address (default `127.0.0.1:3000`).
#[tokio::main]
async fn main() {
    let listen: SocketAddr = std::env::var("BROKER_LISTEN")
        .unwrap_or_else(|_| "127.0.0.1:3000".to_owned())
        .parse()
        .expect("BROKER_LISTEN must be a socket address, e.g. 127.0.0.1:3000");

    let app = match std::env::var("BROKER_MAKER") {
        Ok(spec) if !spec.trim().is_empty() => {
            let makers = parse_makers(&spec);
            for m in &makers {
                println!("broker maker: {}", m.maker_id().0);
            }
            rfq_api::app_with_makers(makers)
        }
        _ => {
            println!("broker: no BROKER_MAKER set — serving in-process mock makers");
            rfq_api::app()
        }
    };

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
