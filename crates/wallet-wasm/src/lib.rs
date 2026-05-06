use rfq_wallet::{MockWalletBackend, WalletBackend};
use wasm_bindgen::prelude::*;

#[wasm_bindgen]
pub fn create_invoice(contract_id: String, amount: u64) -> String {
    MockWalletBackend::default()
        .create_rgb_invoice(&contract_id, amount)
        .unwrap_or_else(|error| format!("error:{error}"))
}

#[wasm_bindgen]
pub fn sign_psbt(psbt_base64: String) -> String {
    MockWalletBackend::default()
        .sign_psbt(&psbt_base64)
        .unwrap_or_else(|error| format!("error:{error}"))
}
