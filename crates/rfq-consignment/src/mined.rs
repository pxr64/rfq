//! Live-resolver mined-witness checker — the perf-critical, chain-only "is every
//! witness mined?" check used by the maker sell-side gate (all witnesses must be
//! mined) and the buy-side gate (all *except the swap tx*). Requires a live electrs
//! endpoint, so it lives behind the `electrs` feature; thin clients use the
//! proof-pack [`crate::verify`] path instead.
//!
//! ## Why this shape (Phase-0 measurements)
//! Against one romanz/electrs the ceiling is ~1,500 tx/s and it is **server-bound**:
//! pipelining (`bp-electrum` `batch_call`, which pipelines newline-delimited requests)
//! is ≈2× serial, and adding client connections to a single instance buys ~nothing. So:
//! - **pipeline** witness lookups on a **single connection** per endpoint, and
//! - **shard** across endpoints to scale (more electrs instances = more throughput).

use std::collections::HashSet;

use electrum::{Batch, Client, ElectrumApi, Param};

/// Outcome of a mined-witness check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MinedVerdict {
    /// Every queried (non-exempt) txid resolved to `>= min_confs` confirmations.
    pub all_mined: bool,
    /// Txids that were NOT mined to the required depth (unconfirmed, mempool, or absent).
    pub unmined: Vec<String>,
    /// How many txids were actually queried (excludes `exempt`).
    pub checked: usize,
}

/// Pipelined, multi-endpoint, chain-only mined-witness checker.
///
/// Blocking (electrum is blocking) — drive it from async via `tokio::task::spawn_blocking`.
pub struct MinedChecker {
    urls: Vec<String>,
    min_confs: u32,
    chunk: usize,
}

impl MinedChecker {
    /// `urls`: one or more electrum endpoints (`tcp://host:port`). Txids are sharded across
    /// them; add instances to scale past one electrs's ~1,500 tx/s ceiling.
    /// `min_confs`: required confirmations (K). `1` = "mined at all" (the sell-side gate's
    /// current `safe_height = MAX` behaviour); mainnet should use a deeper K.
    pub fn new(urls: Vec<String>, min_confs: u32) -> Self {
        Self {
            urls,
            min_confs: min_confs.max(1),
            chunk: 200,
        }
    }

    /// Override the per-`batch_call` chunk size (default 200).
    pub fn with_chunk(mut self, chunk: usize) -> Self {
        self.chunk = chunk.max(1);
        self
    }

    /// Check that every txid in `txids` (except those in `exempt`) is mined to `min_confs`
    /// confirmations. `exempt` carries the one hop a buy-side caller tolerates unmined
    /// (the not-yet-broadcast swap tx); pass an empty set for the sell-side (all-mined).
    pub fn check(
        &self,
        txids: &[String],
        exempt: &HashSet<String>,
    ) -> Result<MinedVerdict, String> {
        if self.urls.is_empty() {
            return Err("MinedChecker: no electrum endpoints configured".to_owned());
        }
        let wanted: Vec<&String> = txids.iter().filter(|t| !exempt.contains(*t)).collect();
        let checked = wanted.len();
        if checked == 0 {
            return Ok(MinedVerdict {
                all_mined: true,
                unmined: vec![],
                checked: 0,
            });
        }

        let n_urls = self.urls.len();
        let per = wanted.len().div_ceil(n_urls);
        let mut unmined = Vec::new();

        for (shard_idx, shard) in wanted.chunks(per).enumerate() {
            let url = &self.urls[shard_idx % n_urls];
            let client = Client::new(url).map_err(|e| format!("connect {url}: {e}"))?;

            for window in shard.chunks(self.chunk) {
                let mut batch = Batch::default();
                for txid in window {
                    batch.raw(
                        "blockchain.transaction.get".to_owned(),
                        vec![Param::String((*txid).clone()), Param::Bool(true)],
                    );
                }
                // `batch_call` returns Ok only if EVERY call succeeds. A never-broadcast
                // (forged) txid makes electrs error the whole batch, so on Err we fall back
                // to per-tx lookups to pinpoint which witnesses are unmined.
                match client.batch_call(&batch) {
                    Ok(results) => {
                        for (txid, v) in window.iter().zip(results.iter()) {
                            if confirmations_of(v) < self.min_confs as u64 {
                                unmined.push((*txid).clone());
                            }
                        }
                    }
                    Err(_) => {
                        for txid in window {
                            if !is_mined(&client, txid, self.min_confs) {
                                unmined.push((*txid).clone());
                            }
                        }
                    }
                }
            }
        }

        Ok(MinedVerdict {
            all_mined: unmined.is_empty(),
            unmined,
            checked,
        })
    }
}

/// Confirmations from a verbose `blockchain.transaction.get` result (0 if absent).
fn confirmations_of(v: &serde_json::Value) -> u64 {
    v.get("confirmations")
        .and_then(|x| x.as_u64())
        .unwrap_or(0)
}

/// Single verbose `transaction.get`; true iff mined to `min_confs` (Err/absent → false).
fn is_mined(client: &Client, txid: &str, min_confs: u32) -> bool {
    match client.raw_call(
        "blockchain.transaction.get",
        [Param::String(txid.to_owned()), Param::Bool(true)],
    ) {
        Ok(v) => confirmations_of(&v) >= min_confs as u64,
        Err(_) => false, // not found on-chain / not in mempool
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Default regtest electrs host port (override with RFQ_TEST_ELECTRUM).
    fn electrum_url() -> String {
        std::env::var("RFQ_TEST_ELECTRUM")
            .unwrap_or_else(|_| "tcp://127.0.0.1:60001".to_owned())
    }

    /// Gather `want` real txids by walking blocks down from the tip via id_from_pos.
    fn gather(client: &Client, want: usize) -> Vec<String> {
        let tip = client.block_headers_subscribe().expect("tip").height;
        let mut txids = Vec::new();
        let mut h = tip;
        let mut scanned = 0usize;
        while txids.len() < want && h > 0 && scanned < 5000 {
            let mut pos = 0usize;
            // electrs errors once `pos` is past the block's tx count → the while-let exits.
            while let Ok(v) = client.raw_call(
                "blockchain.transaction.id_from_pos",
                [Param::Usize(h), Param::Usize(pos), Param::Bool(false)],
            ) {
                // electrs returns either a bare txid string (signet build) or
                // `{"tx_hash": "..."}` (regtest build) — handle both.
                let txid = v.as_str().or_else(|| v.get("tx_hash").and_then(|x| x.as_str()));
                match txid {
                    Some(t) => {
                        txids.push(t.to_owned());
                        pos += 1;
                        if txids.len() >= want {
                            break;
                        }
                    }
                    None => break,
                }
            }
            h -= 1;
            scanned += 1;
        }
        txids
    }

    #[test]
    #[ignore = "needs the regtest electrs up (tcp://127.0.0.1:60001, or RFQ_TEST_ELECTRUM)"]
    fn accepts_mined_rejects_unknown_and_honours_exempt() {
        let url = electrum_url();
        let client = Client::new(&url).expect("connect electrs");
        let txids = gather(&client, 8);
        assert!(!txids.is_empty(), "no txids gathered — is the chain synced?");

        let checker = MinedChecker::new(vec![url], 1);

        // (1) real mined txids all pass.
        let v = checker.check(&txids, &HashSet::new()).expect("check");
        assert!(v.all_mined, "real mined txids should pass; unmined={:?}", v.unmined);
        assert_eq!(v.checked, txids.len());

        // (2) a fabricated (never-broadcast) txid is reported unmined.
        let fake = "ab".repeat(32);
        let mut with_fake = txids.clone();
        with_fake.push(fake.clone());
        let v2 = checker.check(&with_fake, &HashSet::new()).expect("check");
        assert!(!v2.all_mined, "a never-broadcast txid must be flagged");
        assert!(v2.unmined.contains(&fake));

        // (3) exempting that txid (the buy-side swap-tx case) makes it pass again.
        let v3 = checker
            .check(&with_fake, &HashSet::from([fake]))
            .expect("check");
        assert!(v3.all_mined, "exempting the unmined hop should pass; unmined={:?}", v3.unmined);
    }
}
