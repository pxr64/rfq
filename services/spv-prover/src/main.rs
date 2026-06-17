use std::net::SocketAddr;

use spv_prover::{app, AppState};

/// SPV prover entrypoint (RFQIP-1 §2). Untrusted producer of self-verifying proof-packs.
///
/// Env:
/// - `SPV_PROVER_LISTEN` — bind address (default `127.0.0.1:3010`).
/// - `SPV_ELECTRUM_URL` — electrum/electrs URL (default `tcp://127.0.0.1:60001`).
/// - `SPV_NETWORK` — network label stamped into packs (default `regtest`).
#[tokio::main]
async fn main() {
    let listen: SocketAddr = std::env::var("SPV_PROVER_LISTEN")
        .unwrap_or_else(|_| "127.0.0.1:3010".to_owned())
        .parse()
        .expect("SPV_PROVER_LISTEN must be a socket address, e.g. 127.0.0.1:3010");

    let electrum_url =
        std::env::var("SPV_ELECTRUM_URL").unwrap_or_else(|_| "tcp://127.0.0.1:60001".to_owned());
    let network = std::env::var("SPV_NETWORK").unwrap_or_else(|_| "regtest".to_owned());

    println!("spv-prover: electrs → {electrum_url} (network {network})");

    let app = app(AppState::new(electrum_url, network));

    let listener = tokio::net::TcpListener::bind(listen)
        .await
        .expect("bind SPV prover listener");
    println!("spv-prover listening on {listen}");

    axum::serve(listener, app).await.expect("run SPV prover");
}
