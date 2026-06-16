use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use rfq_api::registry::MakerRegistry;
use rfq_btc::ElectrumClient;
use rfq_store::{InMemorySettlementStore, PostgresSettlementStore, SettlementStore};

/// Broker entrypoint.
///
/// Makers **auto-register** over a WebSocket: `colorex maker up` dials
/// `ws://<this-broker>/maker-stream` and registers itself — no broker config
/// needed. The broker routes accept/consignment/sign by matching the registered
/// `maker_id` against the stored `quote.maker_id`.
///
/// Env:
/// - `BROKER_LISTEN` — bind address (default `127.0.0.1:3000`).
/// - `BROKER_DATABASE_URL` — Postgres DSN for the settlements/explorer store;
///   absent → in-memory (settlements don't survive a restart).
/// - `BROKER_ELECTRUM_URL` — electrum/electrs URL for the confirmation loop
///   (e.g. `tcp://127.0.0.1:60001`); absent → no confirmation loop (settlements
///   stay `PendingBitcoinConfirm`).
/// - `BROKER_CONFIRMATIONS` — confs before a settlement is `Settled` (default 1).
#[tokio::main]
async fn main() {
    // Load a local `.env` (BROKER_DATABASE_URL / BROKER_LISTEN / BROKER_ELECTRUM_URL
    // / BROKER_CONFIRMATIONS) if present; real env vars still win.
    dotenvy::dotenv().ok();

    let listen: SocketAddr = std::env::var("BROKER_LISTEN")
        .unwrap_or_else(|_| "127.0.0.1:3000".to_owned())
        .parse()
        .expect("BROKER_LISTEN must be a socket address, e.g. 127.0.0.1:3000");

    // Settlement/explorer store: Postgres when configured, else in-memory.
    let settlement_store: Arc<dyn SettlementStore> = match std::env::var("BROKER_DATABASE_URL") {
        Ok(url) => {
            let store = PostgresSettlementStore::open(&url)
                .await
                .expect("connect BROKER_DATABASE_URL (Postgres)");
            println!("broker: settlements → Postgres");
            Arc::new(store)
        }
        Err(_) => {
            println!("broker: settlements → in-memory (set BROKER_DATABASE_URL for Postgres)");
            Arc::new(InMemorySettlementStore::new())
        }
    };

    // Confirmation loop: poll the witness txids of pending settlements over
    // electrum and promote them to Settled. Only runs when an electrum URL is set.
    if let Ok(electrum_url) = std::env::var("BROKER_ELECTRUM_URL") {
        let confs: u32 = std::env::var("BROKER_CONFIRMATIONS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(1);
        match ElectrumClient::connect(&electrum_url) {
            Ok(client) => {
                let store = Arc::clone(&settlement_store);
                println!("broker: confirmation loop → {electrum_url} ({confs} conf)");
                tokio::spawn(rfq_api::run_confirmation_loop(
                    store,
                    Arc::new(client),
                    confs,
                    Duration::from_secs(30),
                ));
            }
            Err(e) => eprintln!("broker: electrum connect failed, confirmation loop off: {e}"),
        }
    } else {
        println!("broker: no BROKER_ELECTRUM_URL — confirmation loop off");
    }

    // Makers self-register over /maker-stream; the registry starts empty.
    let registry = MakerRegistry::new();
    println!("broker: makers auto-register over ws://…/maker-stream");

    let app = rfq_api::app_with_registry_and_settlements(registry, settlement_store);

    let listener = tokio::net::TcpListener::bind(listen)
        .await
        .expect("bind API listener");
    println!("broker listening on {listen}");

    axum::serve(listener, app).await.expect("run API server");
}
