//! Standalone SPV **prover** service (RFQIP-1 §2) — the untrusted producer of
//! [`SpvProofPack`]s. Given a set of witness txids it returns a self-verifying bundle of
//! Bitcoin merkle-inclusion proofs that a thin client (wallet / ICP canister) verifies
//! locally against its own header source. Because the output is self-verifying, this
//! service needs **no trust**: a lying or faulty prover can only cause a verification
//! *failure* downstream, never a false accept. That is what lets it be standalone,
//! cacheable, and replaceable — separate from the broker and the maker.
//!
//! It does NOT decode RGB consignments: callers already obtain the witness-txid set from
//! their own RGB crypto-validation (`tx_ord_map`), so the wire input is just txids. This
//! keeps the prover free of the heavy RGB stack.
//!
//! ## Caching
//! A buried tx's inclusion proof is immutable, so anchors are cached **in memory** keyed by
//! txid (the hot tier; a persistent KV tier is a follow-up — see the hardening plan). The
//! only correctness caveat is a reorg deeper than a shallow tx; the verifier re-checks depth
//! against its own headers, so a stale anchor still cannot cause a false accept.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use rfq_consignment::proofpack::WitnessInclusion;
use rfq_consignment::{build_proof_pack, SpvProofPack};

/// Soft cap on cached anchors — crude bound so a long-lived prover can't grow without limit.
/// When exceeded the cache is cleared (a real LRU is a follow-up).
const MAX_CACHE_ENTRIES: usize = 100_000;

/// One cached, immutable inclusion proof plus the raw header it anchors to. Serializable so
/// the optional disk tier can persist it (a buried proof never changes).
#[derive(Clone, Serialize, Deserialize)]
struct CachedAnchor {
    inclusion: WitnessInclusion,
    /// Raw 80-byte block header, hex — included in the response `headers` map.
    header_hex: String,
}

#[derive(Clone)]
pub struct AppState {
    /// Electrum endpoint the prover fetches proofs from.
    pub electrum_url: String,
    /// Network label stamped into every pack (`regtest` | `signet` | `testnet` | `mainnet`).
    pub network: String,
    /// In-memory hot tier: txid → anchor.
    cache: Arc<Mutex<HashMap<String, CachedAnchor>>>,
    /// Optional persistent tier: one `<txid>.json` per anchor under this dir. Survives
    /// restarts so a buried proof is fetched from electrs at most once, ever. `None` →
    /// memory-only.
    cache_dir: Option<PathBuf>,
}

impl AppState {
    pub fn new(electrum_url: String, network: String) -> Self {
        Self {
            electrum_url,
            network,
            cache: Arc::new(Mutex::new(HashMap::new())),
            cache_dir: None,
        }
    }

    /// Enable the persistent disk tier under `dir` (created if missing).
    pub fn with_cache_dir(mut self, dir: PathBuf) -> Self {
        if let Err(e) = std::fs::create_dir_all(&dir) {
            eprintln!("spv-prover: cache dir {} unusable, disk tier off: {e}", dir.display());
        } else {
            self.cache_dir = Some(dir);
        }
        self
    }

    /// Path for a txid's persisted anchor (`<dir>/<txid>.json`). `None` if no disk tier or
    /// the txid isn't clean hex (defensive — txids are always hex in practice).
    fn disk_path(&self, txid: &str) -> Option<PathBuf> {
        let dir = self.cache_dir.as_ref()?;
        if txid.is_empty() || !txid.bytes().all(|b| b.is_ascii_hexdigit()) {
            return None;
        }
        Some(dir.join(format!("{txid}.json")))
    }

    /// Try to load a persisted anchor for `txid` from disk.
    fn load_disk(&self, txid: &str) -> Option<CachedAnchor> {
        let path = self.disk_path(txid)?;
        let bytes = std::fs::read(path).ok()?;
        serde_json::from_slice(&bytes).ok()
    }

    /// Persist an anchor for `txid` to disk (best-effort; failures are non-fatal).
    fn store_disk(&self, txid: &str, anchor: &CachedAnchor) {
        if let Some(path) = self.disk_path(txid) {
            if let Ok(bytes) = serde_json::to_vec(anchor) {
                let _ = std::fs::write(path, bytes);
            }
        }
    }
}

/// `POST /spv/proof-pack` request: the witness txids to prove. `network` is optional and
/// advisory — the service stamps its own configured network (proofs come from its electrs).
#[derive(Debug, Deserialize)]
pub struct ProofPackRequest {
    pub txids: Vec<String>,
    #[serde(default)]
    pub network: Option<String>,
}

/// `POST /spv/proof-pack` response: the pack plus any txids that couldn't be proven
/// (not mined / unknown) and are therefore absent from `pack.anchors`.
#[derive(Debug, Serialize)]
pub struct ProofPackResponse {
    pub pack: SpvProofPack,
    pub unproven: Vec<String>,
}

/// Build the axum router for the prover.
pub fn app(state: AppState) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/spv/proof-pack", post(proof_pack))
        .with_state(state)
}

async fn health() -> impl IntoResponse {
    (StatusCode::OK, "ok")
}

async fn proof_pack(
    State(state): State<AppState>,
    Json(req): Json<ProofPackRequest>,
) -> impl IntoResponse {
    match build(state, req.txids).await {
        Ok(resp) => (StatusCode::OK, Json(resp)).into_response(),
        Err(e) => (
            StatusCode::BAD_GATEWAY,
            Json(serde_json::json!({ "error": e })),
        )
            .into_response(),
    }
}

/// Assemble a pack for `txids`, serving cached anchors and fetching only the misses.
async fn build(state: AppState, txids: Vec<String>) -> Result<ProofPackResponse, String> {
    // 1. Partition into cache hits and misses.
    let (mut hits, misses): (HashMap<String, CachedAnchor>, Vec<String>) = {
        let cache = state.cache.lock().unwrap();
        let mut hits = HashMap::new();
        let mut misses = Vec::new();
        for txid in &txids {
            match cache.get(txid) {
                Some(a) => {
                    hits.insert(txid.clone(), a.clone());
                }
                None => misses.push(txid.clone()),
            }
        }
        (hits, misses)
    };

    // 2. Disk tier: for in-memory misses, try the persistent cache before electrs. A hit
    //    here is promoted back into the hot in-memory tier.
    let mut misses = misses;
    if state.cache_dir.is_some() {
        let mut still_missing = Vec::new();
        let mut cache = state.cache.lock().unwrap();
        for txid in misses {
            match state.load_disk(&txid) {
                Some(anchor) => {
                    cache.insert(txid.clone(), anchor.clone());
                    hits.insert(txid, anchor);
                }
                None => still_missing.push(txid),
            }
        }
        misses = still_missing;
    }

    // 3. Fetch the remaining misses from electrs (blocking) off the async runtime.
    let mut unproven = Vec::new();
    if !misses.is_empty() {
        let url = state.electrum_url.clone();
        let network = state.network.clone();
        let fetched = tokio::task::spawn_blocking(move || build_proof_pack(&url, &misses, &network))
            .await
            .map_err(|e| format!("prover task join: {e}"))??;

        unproven = fetched.unproven;
        // 4. Insert freshly-proven anchors into both tiers + the working set.
        let mut cache = state.cache.lock().unwrap();
        if cache.len() + fetched.pack.anchors.len() > MAX_CACHE_ENTRIES {
            cache.clear();
        }
        for (txid, inclusion) in fetched.pack.anchors {
            let header_hex = fetched
                .pack
                .headers
                .get(&inclusion.block_hash)
                .cloned()
                .unwrap_or_default();
            let cached = CachedAnchor {
                inclusion,
                header_hex,
            };
            state.store_disk(&txid, &cached);
            cache.insert(txid.clone(), cached.clone());
            hits.insert(txid, cached);
        }
    }

    // 5. Assemble the response pack from the working set, in the requested order.
    let mut anchors = std::collections::BTreeMap::new();
    let mut headers = std::collections::BTreeMap::new();
    for txid in &txids {
        if let Some(a) = hits.get(txid) {
            if !a.header_hex.is_empty() {
                headers.insert(a.inclusion.block_hash.clone(), a.header_hex.clone());
            }
            anchors.insert(txid.clone(), a.inclusion.clone());
        }
    }

    Ok(ProofPackResponse {
        pack: SpvProofPack {
            version: 1,
            network: state.network.clone(),
            anchors,
            headers,
        },
        unproven,
    })
}
