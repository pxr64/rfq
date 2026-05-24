//! Self-bootstrapping regtest harness for the `LibRgbBackend` e2e tests.
//!
//! Replaces the env-var pattern (`RGB_DATA_DIR`, `RGB_CONTRACT_ID`, ...) the
//! old `#[ignore]`d tests used. The developer brings up bitcoind + electrs
//! once (`make -C infra/regtest regtest-up` + `make -C infra/regtest
//! rgb-tools-install`); this module discovers them and bootstraps everything
//! else from Rust: per-role wallets in a tempdir, funding via bitcoin-cli,
//! NIA contract issuance, and an issuer→maker transfer so the maker stash
//! has spendable RGB allocations.
//!
//! Shared via [`tokio::sync::OnceCell`] — first test to call [`stack`] pays
//! the bootstrap cost (~10s); subsequent tests reuse the same state for the
//! rest of the process lifetime. The tempdir auto-cleans on test-process
//! exit; the dev's docker stack is never touched.
//!
//! See GitHub issue #23 for the broader e2e harness plan and deferred work
//! (testcontainers, pure-Rust issuance, etc.).

#![allow(dead_code)] // not every helper is used by both pilot tests

use std::path::{Path, PathBuf};
use std::process::Command;
use std::str::FromStr;

use bpstd::HardenedIndex;
use bpwallet::hot::{SecureIo, Seed, SeedType};
use bpwallet::Bip43;
use rfq_rgb::{ContractId, LibRgbBackend, RgbBackend};
use rfq_types::{AssetId, AssetKind, BitcoinNetwork, Outpoint};
use tempfile::TempDir;
use tokio::sync::{Mutex, MutexGuard, OnceCell};

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Bootstrap (on first call) and return a shared handle to the regtest stack.
/// Panics if the dev hasn't brought bitcoind + electrs up — bootstrap is not
/// allowed to mutate docker state.
pub async fn stack() -> &'static RegtestStack {
    STACK
        .get_or_try_init(|| async {
            tokio::task::spawn_blocking(RegtestStack::bootstrap)
                .await
                .map_err(|e| format!("bootstrap task join: {e}"))?
        })
        .await
        .expect("regtest harness bootstrap failed; see error message above")
}

static STACK: OnceCell<RegtestStack> = OnceCell::const_new();

pub struct RegtestStack {
    // Held for cleanup on drop; never accessed directly post-bootstrap.
    _tempdir: TempDir,
    tools_dir: PathBuf,
    compose_dir: PathBuf,
    electrum_url: String,
    contract_id: ContractId,
    contract_id_str: String,
    issuer: RoleHandles,
    maker: RoleHandles,
    /// Consignment bytes produced by the bootstrap issuer→maker transfer,
    /// cached for the validate test (and any future test that needs a real
    /// pre-accepted consignment).
    consignment_bytes: Vec<u8>,
    /// Maker's receive invoice from the bootstrap transfer (the same one
    /// the issuer paid). Cross-checked by `validate_incoming_consignment`.
    maker_invoice: String,
    /// All maker keychain-9 outpoints from the funding phase. The transfer
    /// landed RGB on one of them; the others are pure BTC and usable as
    /// `maker_btc_inputs` in the sell composition.
    maker_funding_outpoints: Vec<Outpoint>,
    /// Issuer keychain-0 address; reused as a fresh-looking taker payout
    /// destination so the sell test doesn't need to know about the issuer.
    taker_payout_addr: String,
    /// Serializes every backend op so the autosave-on-drop semantics of
    /// `Stock::load(_, true)` don't race when multiple tests run in parallel.
    /// `cargo test -p rfq-rgb -- --ignored` works without `--test-threads=1`
    /// because of this lock; the lock is held for the lifetime of any
    /// [`MakerGuard`] / [`IssuerGuard`].
    backend_lock: Mutex<()>,
}

struct RoleHandles {
    stash_dir: PathBuf,
    account_file: PathBuf,
}

impl RegtestStack {
    pub fn asset(&self) -> AssetId {
        AssetId {
            network: BitcoinNetwork::Regtest,
            kind: AssetKind::Rgb20,
            id: self.contract_id_str.clone(),
        }
    }

    pub fn contract_id(&self) -> ContractId {
        self.contract_id
    }

    pub fn contract_id_str(&self) -> &str {
        &self.contract_id_str
    }

    pub fn electrum_url(&self) -> &str {
        &self.electrum_url
    }

    /// Acquire the shared backend lock and return a maker-side backend
    /// handle. Held until the guard drops — keeps `Stock::load` /
    /// autosave-on-drop from racing across parallel tests.
    pub async fn maker_backend(&self) -> MakerGuard<'_> {
        let guard = self.backend_lock.lock().await;
        MakerGuard {
            _guard: guard,
            backend: LibRgbBackend::new(
                self.maker.stash_dir.clone(),
                "maker".to_owned(),
                "regtest".to_owned(),
                self.electrum_url.clone(),
                self.maker.account_file.clone(),
                String::new(),
            ),
            stack: self,
        }
    }

    /// Acquire the shared backend lock and return an issuer-side handle.
    pub async fn issuer_backend(&self) -> IssuerGuard<'_> {
        let guard = self.backend_lock.lock().await;
        IssuerGuard {
            _guard: guard,
            backend: LibRgbBackend::new(
                self.issuer.stash_dir.clone(),
                "issuer".to_owned(),
                "regtest".to_owned(),
                self.electrum_url.clone(),
                self.issuer.account_file.clone(),
                String::new(),
            ),
        }
    }

    pub fn consignment_bytes(&self) -> &[u8] {
        &self.consignment_bytes
    }

    pub fn maker_invoice(&self) -> &str {
        &self.maker_invoice
    }

    pub fn taker_payout_addr(&self) -> &str {
        &self.taker_payout_addr
    }
}

/// Maker backend handle holding the shared backend lock. Derefs to
/// [`LibRgbBackend`] so existing call sites keep working.
pub struct MakerGuard<'a> {
    _guard: MutexGuard<'a, ()>,
    backend: LibRgbBackend,
    stack: &'a RegtestStack,
}

impl<'a> std::ops::Deref for MakerGuard<'a> {
    type Target = LibRgbBackend;
    fn deref(&self) -> &Self::Target {
        &self.backend
    }
}

impl<'a> MakerGuard<'a> {
    /// A maker-owned BTC outpoint that doesn't carry an RGB allocation —
    /// usable as `maker_btc_inputs[0]` in a sell-side swap PSBT. Uses the
    /// guard's already-locked backend, so no extra lock acquisition.
    pub async fn spare_btc_outpoint(&self) -> Outpoint {
        let rgb_utxos = self
            .backend
            .list_inventory_utxos(&self.stack.asset())
            .await
            .expect("list_inventory_utxos for spare-outpoint lookup");
        let rgb_set: std::collections::HashSet<&Outpoint> =
            rgb_utxos.iter().map(|u| &u.outpoint).collect();
        self.stack
            .maker_funding_outpoints
            .iter()
            .find(|op| !rgb_set.contains(op))
            .cloned()
            .expect(
                "no spare maker BTC outpoint — bump the funding count in \
                 bootstrap if that ever happens",
            )
    }
}

/// Issuer backend handle holding the shared backend lock.
pub struct IssuerGuard<'a> {
    _guard: MutexGuard<'a, ()>,
    backend: LibRgbBackend,
}

impl<'a> std::ops::Deref for IssuerGuard<'a> {
    type Target = LibRgbBackend;
    fn deref(&self) -> &Self::Target {
        &self.backend
    }
}

// ---------------------------------------------------------------------------
// Bootstrap
// ---------------------------------------------------------------------------

impl RegtestStack {
    fn bootstrap() -> Result<RegtestStack, String> {
        let workspace_root = workspace_root();
        let tools_dir = env_or_default(
            "RGB_RFQ_TOOLS_DIR",
            workspace_root.join("infra/regtest/tools"),
        );

        let compose_dir = env_or_default(
            "RGB_RFQ_COMPOSE_DIR",
            workspace_root.join("infra/regtest"),
        );
        
        let electrum_url =
            std::env::var("ELECTRUM_URL").unwrap_or_else(|_| "localhost:50001".to_owned());

        require_tools(&tools_dir)?;
        require_stack_up(&compose_dir, &electrum_url)?;

        let tempdir = TempDir::new().map_err(|e| format!("tempdir: {e}"))?;
        let schema_file = workspace_root
            .join("crates/rfq-rgb/tests/fixtures/NonInflatableAsset.rgb");
        let template_path = workspace_root.join("infra/regtest/artifacts/rfq-nia.yaml");

        // Phase 1: per-role wallet creation (issuer + maker; taker deferred
        // until the sell/validate tests migrate — see issue #23).
        let issuer = create_role_wallet(&tools_dir, tempdir.path(), &electrum_url, "issuer")?;
        let maker = create_role_wallet(&tools_dir, tempdir.path(), &electrum_url, "maker")?;

        // Phase 2: import the vendored NIA schema into each stash.
        import_schema(&tools_dir, &issuer.stash_dir, "issuer", &electrum_url, &schema_file)?;
        import_schema(&tools_dir, &maker.stash_dir, "maker", &electrum_url, &schema_file)?;

        // Phase 3: fund each role on a keychain-9 address. Maker gets two
        // outpoints so the sell test has at least one spare BTC UTXO after
        // the issuer→maker transfer claims one for RGB.
        fund_role(&tools_dir, &compose_dir, &issuer.stash_dir, "issuer", &electrum_url)?;
        fund_role(&tools_dir, &compose_dir, &maker.stash_dir, "maker", &electrum_url)?;
        fund_role(&tools_dir, &compose_dir, &maker.stash_dir, "maker", &electrum_url)?;

        // Phase 4: issue the NIA contract.
        let contract_id_str = issue_contract(
            &tools_dir,
            &issuer.stash_dir,
            "issuer",
            &electrum_url,
            &template_path,
            tempdir.path(),
        )?;
        let contract_id = ContractId::from_str(&contract_id_str)
            .map_err(|e| format!("parse contract id `{contract_id_str}`: {e}"))?;

        // Phase 5: issuer→maker transfer so the maker has RGB allocations
        // to list / spend. Cache the consignment + invoice so the validate
        // / sell tests don't need their own transfer.
        let artifacts = transfer_to_maker(
            &tools_dir,
            &compose_dir,
            &issuer.stash_dir,
            &maker.stash_dir,
            &electrum_url,
            &contract_id_str,
            tempdir.path(),
        )?;

        // Phase 6: collect post-bootstrap state the tests consume.
        let maker_utxos_out =
            rgb_cmd(&tools_dir, &maker.stash_dir, "maker", &electrum_url, &["utxos"])?;
        let maker_funding_outpoints = parse_all_keychain9_outpoints(&maker_utxos_out);
        if maker_funding_outpoints.len() < 2 {
            return Err(format!(
                "expected ≥2 maker keychain-9 outpoints after funding, got {}:\n{maker_utxos_out}",
                maker_funding_outpoints.len()
            ));
        }
        let issuer_addr_out = rgb_cmd(
            &tools_dir,
            &issuer.stash_dir,
            "issuer",
            &electrum_url,
            &["address", "-k", "0"],
        )?;
        let taker_payout_addr = last_word(&issuer_addr_out)
            .ok_or_else(|| format!("could not parse taker payout addr:\n{issuer_addr_out}"))?;

        Ok(RegtestStack {
            _tempdir: tempdir,
            tools_dir,
            compose_dir,
            electrum_url,
            contract_id,
            contract_id_str,
            issuer,
            maker,
            consignment_bytes: artifacts.consignment_bytes,
            maker_invoice: artifacts.maker_invoice,
            maker_funding_outpoints,
            taker_payout_addr,
            backend_lock: Mutex::new(()),
        })
    }
}

/// Captured during the bootstrap issuer→maker transfer for later test use.
struct TransferArtifacts {
    consignment_bytes: Vec<u8>,
    maker_invoice: String,
}

// ---------------------------------------------------------------------------
// Bootstrap phases (mirror infra/regtest/scripts/*)
// ---------------------------------------------------------------------------

fn create_role_wallet(
    tools_dir: &Path,
    tempdir: &Path,
    electrum_url: &str,
    role: &str,
) -> Result<RoleHandles, String> {
    let account_file = tempdir.join(format!("{role}.account"));
    let stash_dir = tempdir.join(role);
    std::fs::create_dir_all(&stash_dir)
        .map_err(|e| format!("create stash dir {stash_dir:?}: {e}"))?;

    // In-Rust replacement for `bp-hot seed` + `bp-hot derive -N --scheme bip84
    // --account 0h ...`. The seed itself stays in memory — `LibRgbBackend`
    // only needs the encrypted account file (loaded via `XprivAccount::read`
    // in `lib_backend.rs:122`). `derive(_, testnet=true, _)` mirrors the
    // shell's omission of `--mainnet`; the empty account password mirrors the
    // shell's `-N` (`--no-password`) flag.
    let seed = Seed::random(SeedType::Bit128);
    let account = seed.derive(Bip43::Bip84, true, HardenedIndex::hardened(0));
    account
        .write(&account_file, "")
        .map_err(|e| format!("write account file {account_file:?}: {e}"))?;
    // `to_xpub_account().to_string()` mirrors the `Account:` line bp-hot
    // derive used to print: `[fingerprint/84h/1h/0h]tpubD...`. The
    // `/<0;1;9>/*` terminal declares external (0), change (1), and RGB
    // seal-anchor (9) keychains — `rgb address -k 9` needs keychain 9 in
    // the descriptor or it can't derive anchor addresses.
    let descriptor = account.to_xpub_account().to_string();
    let descriptor_with_terminal = format!("{descriptor}/<0;1;9>/*");

    // rgb create --wpkh <descriptor> <role>
    rgb_cmd(
        tools_dir,
        &stash_dir,
        role,
        electrum_url,
        &["create", "--wpkh", &descriptor_with_terminal, role],
    )?;

    Ok(RoleHandles {
        stash_dir,
        account_file,
    })
}

fn import_schema(
    tools_dir: &Path,
    stash_dir: &Path,
    role: &str,
    electrum_url: &str,
    schema_file: &Path,
) -> Result<(), String> {
    rgb_cmd(
        tools_dir,
        stash_dir,
        role,
        electrum_url,
        &["import", &schema_file.display().to_string()],
    )?;
    Ok(())
}

fn fund_role(
    tools_dir: &Path,
    compose_dir: &Path,
    stash_dir: &Path,
    role: &str,
    electrum_url: &str,
) -> Result<(), String> {
    // Derive a keychain-9 address.
    let addr_out = rgb_cmd(tools_dir, stash_dir, role, electrum_url, &["address", "-k", "9"])?;
    let addr =
        last_word(&addr_out).ok_or_else(|| format!("could not parse address for {role}"))?;

    ensure_miner_wallet(compose_dir)?;
    bitcoin_cli(
        compose_dir,
        &["-rpcwallet=miner", "sendtoaddress", &addr, "1"],
    )?;
    let miner_addr_out = bitcoin_cli(
        compose_dir,
        &["-rpcwallet=miner", "getnewaddress", "", "bech32"],
    )?;
    let miner_addr = miner_addr_out.trim();
    bitcoin_cli(
        compose_dir,
        &["-rpcwallet=miner", "generatetoaddress", "1", miner_addr],
    )?;

    // Sync so electrs's new UTXO shows up in the stash.
    rgb_cmd(tools_dir, stash_dir, role, electrum_url, &["utxos", "--sync"])?;
    Ok(())
}

fn issue_contract(
    tools_dir: &Path,
    issuer_stash: &Path,
    issuer_role: &str,
    electrum_url: &str,
    template_path: &Path,
    tempdir: &Path,
) -> Result<String, String> {
    // Schema id is content-addressed; read it back from the stash rather
    // than hard-coding so updates to the schema fixture flow through.
    let schemata_out = rgb_cmd(tools_dir, issuer_stash, issuer_role, electrum_url, &["schemata"])?;
    let schema_id = parse_schema_id(&schemata_out)
        .ok_or_else(|| format!("could not parse schema id from:\n{schemata_out}"))?;

    let utxos_out = rgb_cmd(tools_dir, issuer_stash, issuer_role, electrum_url, &["utxos"])?;
    let outpoint = parse_keychain9_outpoint(&utxos_out)
        .ok_or_else(|| format!("could not find a keychain-9 outpoint in:\n{utxos_out}"))?;

    let template = std::fs::read_to_string(template_path)
        .map_err(|e| format!("read template {template_path:?}: {e}"))?;
    let rendered = render_yaml_template(&template, &schema_id, &outpoint);
    let rendered_path = tempdir.join("rfq-nia.rendered.yaml");
    std::fs::write(&rendered_path, &rendered).map_err(|e| format!("write rendered yaml: {e}"))?;

    // `rgb issue` puts the contract id on stderr — read both streams.
    let mut cmd = Command::new(rgb_path(tools_dir));
    cmd.arg("-n")
        .arg("regtest")
        .arg(format!("--electrum={electrum_url}"))
        .arg("-d")
        .arg(issuer_stash)
        .arg("-w")
        .arg(issuer_role)
        .arg("issue")
        .arg("ssi:issuer")
        .arg(&rendered_path);
    let (issue_stdout, issue_stderr) = run_split("rgb issue (issuer)", &mut cmd)?;
    let issue_combined = format!("{issue_stdout}\n{issue_stderr}");

    if let Some(id) = parse_contract_id(&issue_combined) {
        return Ok(id);
    }
    // Fallback: scan `rgb contracts` for the freshly issued id, same as
    // rgb-issue-asset:99-102.
    let contracts_out =
        rgb_cmd(tools_dir, issuer_stash, issuer_role, electrum_url, &["contracts"])?;
    parse_contract_id(&contracts_out).ok_or_else(|| {
        format!(
            "could not parse contract id from `rgb issue`:\nstdout:\n{issue_stdout}\nstderr:\n{issue_stderr}\n\
             nor from `rgb contracts`:\n{contracts_out}"
        )
    })
}

fn transfer_to_maker(
    tools_dir: &Path,
    compose_dir: &Path,
    issuer_stash: &Path,
    maker_stash: &Path,
    electrum_url: &str,
    contract_id: &str,
    tempdir: &Path,
) -> Result<TransferArtifacts, String> {
    let invoice_out = rgb_cmd(
        tools_dir,
        maker_stash,
        "maker",
        electrum_url,
        &["invoice", "--amount", "1000", contract_id],
    )?;

    let invoice = parse_invoice(&invoice_out)
        .ok_or_else(|| format!("could not parse invoice from:\n{invoice_out}"))?;

    let consignment_path = tempdir.join("transfer.consignment.rgb");
    let psbt_path = tempdir.join("transfer.psbt");
    rgb_cmd(
        tools_dir,
        issuer_stash,
        "issuer",
        electrum_url,
        &[
            "transfer",
            &invoice,
            &consignment_path.display().to_string(),
            &psbt_path.display().to_string(),
        ],
    )?;

    // Confirm and sync both sides.
    let miner_addr_out = bitcoin_cli(
        compose_dir,
        &["-rpcwallet=miner", "getnewaddress", "", "bech32"],
    )?;
    let miner_addr = miner_addr_out.trim();
    bitcoin_cli(
        compose_dir,
        &["-rpcwallet=miner", "generatetoaddress", "1", miner_addr],
    )?;
    rgb_cmd(tools_dir, issuer_stash, "issuer", electrum_url, &["utxos", "--sync"])?;
    rgb_cmd(tools_dir, maker_stash, "maker", electrum_url, &["utxos", "--sync"])?;

    rgb_cmd(
        tools_dir,
        maker_stash,
        "maker",
        electrum_url,
        &["accept", &consignment_path.display().to_string()],
    )?;

    let consignment_bytes = std::fs::read(&consignment_path)
        .map_err(|e| format!("read consignment {consignment_path:?}: {e}"))?;
    Ok(TransferArtifacts {
        consignment_bytes,
        maker_invoice: invoice,
    })
}

// ---------------------------------------------------------------------------
// Subprocess wrappers
// ---------------------------------------------------------------------------

fn rgb_cmd(
    tools_dir: &Path,
    stash_dir: &Path,
    wallet_name: &str,
    electrum_url: &str,
    args: &[&str],
) -> Result<String, String> {
    // rgb-cmd 0.11.1-rc.6 wants the joined `--electrum=URL` form; the
    // space-separated form makes clap treat URL as a subcommand. The shell
    // wrappers in common.sh use the joined form for the same reason.
    let mut cmd = Command::new(rgb_path(tools_dir));
    cmd.arg("-n")
        .arg("regtest")
        .arg(format!("--electrum={electrum_url}"))
        .arg("-d")
        .arg(stash_dir)
        .arg("-w")
        .arg(wallet_name)
        .args(args);
    run(&format!("rgb {} ({wallet_name})", args.first().copied().unwrap_or("")), &mut cmd)
}

fn bitcoin_cli(compose_dir: &Path, args: &[&str]) -> Result<String, String> {
    let mut cmd = Command::new("docker");
    cmd.current_dir(compose_dir).args([
        "compose",
        "exec",
        "-T",
        "bitcoind",
        "bitcoin-cli",
        "-regtest",
        "-datadir=/home/bitcoin/.bitcoin",
    ]);
    cmd.args(args);
    run(&format!("bitcoin-cli {}", args.first().copied().unwrap_or("")), &mut cmd)
}

fn run(label: &str, cmd: &mut Command) -> Result<String, String> {
    let (stdout, _stderr) = run_split(label, cmd)?;
    Ok(stdout)
}

/// Variant of `run` that returns stdout and stderr separately — needed for
/// `rgb issue`, which emits the contract id on stderr. Most callers want
/// stdout-only (stderr typically holds "Loading descriptor..." noise that
/// would confuse last-word / token parsers).
fn run_split(label: &str, cmd: &mut Command) -> Result<(String, String), String> {
    let output = cmd
        .output()
        .map_err(|e| format!("{label}: failed to spawn: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "{label}: exit {}\nstdout:\n{}\nstderr:\n{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        ));
    }
    Ok((
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    ))
}

fn ensure_miner_wallet(compose_dir: &Path) -> Result<(), String> {
    if bitcoin_cli(compose_dir, &["-rpcwallet=miner", "getwalletinfo"]).is_ok() {
        return Ok(());
    }
    let dir = bitcoin_cli(compose_dir, &["listwalletdir"])?;
    if dir.contains("\"miner\"") {
        bitcoin_cli(compose_dir, &["loadwallet", "miner"])?;
    } else {
        bitcoin_cli(compose_dir, &["createwallet", "miner"])?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Discovery + parsing helpers
// ---------------------------------------------------------------------------

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crates/ dir")
        .parent()
        .expect("workspace root")
        .to_path_buf()
}

fn env_or_default(key: &str, default: PathBuf) -> PathBuf {
    std::env::var(key).map(PathBuf::from).unwrap_or(default)
}

fn rgb_path(tools_dir: &Path) -> PathBuf {
    tools_dir.join("rgb-cmd/bin/rgb")
}

fn require_tools(tools_dir: &Path) -> Result<(), String> {
    let rgb = rgb_path(tools_dir);
    if !rgb.is_file() {
        return Err(format!(
            "rgb-cmd missing at {rgb:?}.\n\
             Run: make -C infra/regtest rgb-tools-install",
        ));
    }
    Ok(())
}

fn require_stack_up(compose_dir: &Path, electrum_url: &str) -> Result<(), String> {
    if bitcoin_cli(compose_dir, &["getblockchaininfo"]).is_err() {
        return Err(
            "bitcoind not reachable via `docker compose exec` from \
             {compose_dir:?}. Run: make -C infra/regtest regtest-up"
                .to_owned(),
        );
    }
    let host_port = electrum_url
        .trim_start_matches("tcp://")
        .trim_start_matches("ssl://");
    let addr: std::net::SocketAddr = host_port
        .to_socket_addrs()
        .map_err(|e| format!("bad ELECTRUM_URL `{electrum_url}`: {e}"))?
        .next()
        .ok_or_else(|| format!("ELECTRUM_URL `{electrum_url}` resolved to no addresses"))?;
    if std::net::TcpStream::connect_timeout(&addr, std::time::Duration::from_secs(2)).is_err() {
        return Err(format!(
            "electrs not reachable at {electrum_url}. \
             Run: make -C infra/regtest regtest-up"
        ));
    }
    Ok(())
}

use std::net::ToSocketAddrs;

fn last_word(s: &str) -> Option<String> {
    s.lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .and_then(|l| l.split_whitespace().last())
        .map(|w| w.to_owned())
}

/// Find the first `<base>#<label>` token (stripping any `prefix:` segments,
/// e.g. `rgb:sch:`). Mirrors the awk in `rgb-issue-asset:49`.
fn parse_schema_id(schemata_out: &str) -> Option<String> {
    for line in schemata_out.lines() {
        for tok in line.split_whitespace() {
            if !tok.contains('#') {
                continue;
            }
            let id = tok.rsplit(':').next().unwrap_or(tok);
            if id.contains('#') {
                return Some(id.to_owned());
            }
        }
    }
    None
}

/// All keychain-9 outpoints in `rgb utxos` output. Entries span two lines —
/// `<addr>    &9/<idx>` then `<height>    <amount>    <txid>:<vout>` — so we
/// scan for a `&9/` marker and pull the outpoint from the following non-empty
/// line.
fn parse_all_keychain9_outpoints(utxos_out: &str) -> Vec<Outpoint> {
    let mut out = Vec::new();
    let mut lines = utxos_out.lines();
    while let Some(line) = lines.next() {
        if !line.contains("&9/") {
            continue;
        }
        for next in lines.by_ref() {
            if next.trim().is_empty() {
                continue;
            }
            for tok in next.split_whitespace() {
                if is_outpoint(tok) {
                    if let Ok(op) = tok.parse::<Outpoint>() {
                        out.push(op);
                    }
                    break;
                }
            }
            break;
        }
    }
    out
}

/// First `txid:vout` on a line that mentions keychain 9, else the first
/// `txid:vout` anywhere in the output. Mirrors `rgb-issue-asset:62-70`.
fn parse_keychain9_outpoint(utxos_out: &str) -> Option<String> {
    for line in utxos_out.lines() {
        if line.contains("keychain=9") || line.contains("&9/") || line.trim_start().starts_with("9 ") {
            for tok in line.split_whitespace() {
                if is_outpoint(tok) {
                    return Some(tok.to_owned());
                }
            }
        }
    }
    for line in utxos_out.lines() {
        for tok in line.split_whitespace() {
            if is_outpoint(tok) {
                return Some(tok.to_owned());
            }
        }
    }
    None
}

fn is_outpoint(s: &str) -> bool {
    let parts: Vec<_> = s.splitn(2, ':').collect();
    parts.len() == 2
        && parts[0].len() == 64
        && parts[0].chars().all(|c| c.is_ascii_hexdigit())
        && parts[1].parse::<u32>().is_ok()
}

/// Substitute the two dynamic lines in the contract YAML template — same
/// `sed` substitutions `rgb-issue-asset:83-86` does.
fn render_yaml_template(template: &str, schema_id: &str, seal_outpoint: &str) -> String {
    let mut out: Vec<String> = template
        .lines()
        .map(|line| {
            if line.starts_with("schema:") {
                format!("schema: {schema_id}")
            } else if line.starts_with("    seal:") {
                format!("    seal: {seal_outpoint}")
            } else {
                line.to_owned()
            }
        })
        .collect();
    if template.ends_with('\n') {
        out.push(String::new());
    }
    out.join("\n")
}

fn parse_contract_id(issue_out: &str) -> Option<String> {
    issue_out
        .split_whitespace()
        .filter(|tok| tok.starts_with("rgb:"))
        .last()
        .map(|s| s.to_owned())
}

fn parse_invoice(invoice_out: &str) -> Option<String> {
    invoice_out
        .lines()
        .rev()
        .find(|l| l.trim().starts_with("rgb:"))
        .map(|l| l.trim().to_owned())
}
