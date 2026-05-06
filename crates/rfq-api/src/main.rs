use std::net::SocketAddr;

#[tokio::main]
async fn main() {
    let addr = SocketAddr::from(([127, 0, 0, 1], 3000));
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .expect("bind API listener");

    axum::serve(listener, rfq_api::app())
        .await
        .expect("run API server");
}
