//! Pure, role-agnostic wallet-setup defaults. No I/O, no prompts — unit-testable.

use std::path::{Path, PathBuf};

/// Per-network default Electrum endpoint. Both bp-electrum and electrum-client
/// parse a `ssl://`/`tcp://` scheme (no prefix = plaintext tcp). Blockstream runs
/// no public signet server, so signet defaults to mempool.space's public SSL
/// electrs; the rest stay bare (tcp) by default.
pub fn default_electrum_url(network: &str) -> &'static str {
    match network {
        "regtest" => "localhost:60001",
        "signet" => "ssl://mempool.space:60602",
        "testnet" => "electrum.blockstream.info:60001",
        "mainnet" => "electrum.blockstream.info:50001",
        _ => "localhost:60001",
    }
}

/// Default per-wallet RGB data dir, `~/.local/share/colorex/<name>` (tilde left
/// unexpanded — callers expand via [`expand_tilde`]). Name-keyed so the wallet
/// commands only need a `--name`; everything else derives from it.
pub fn default_data_dir(name: &str) -> String {
    format!("~/.local/share/colorex/{name}")
}

/// Default signing-account file, `<data_dir>/account.key`, given an
/// already-expanded data dir.
pub fn default_account_file(data_dir: &Path) -> PathBuf {
    data_dir.join("account.key")
}

/// Expand a leading `~` and strip surrounding whitespace. The trim guards against
/// stray spaces in hand-edited configs (e.g. `" ssl://host:port "`), which the
/// path/URL consumers would otherwise treat literally.
pub fn expand_tilde(s: &str) -> PathBuf {
    PathBuf::from(shellexpand::tilde(s.trim()).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signet_electrum_is_public_ssl() {
        assert_eq!(default_electrum_url("signet"), "ssl://mempool.space:60602");
    }

    #[test]
    fn unknown_network_falls_back_to_local() {
        assert_eq!(default_electrum_url("whatever"), "localhost:60001");
    }

    #[test]
    fn data_dir_is_name_keyed() {
        assert_eq!(default_data_dir("taker"), "~/.local/share/colorex/taker");
    }

    #[test]
    fn account_file_sits_in_data_dir() {
        assert_eq!(
            default_account_file(Path::new("/x/y")),
            PathBuf::from("/x/y/account.key")
        );
    }

    #[test]
    fn expand_tilde_trims_surrounding_whitespace() {
        assert_eq!(expand_tilde("  /a/b  "), PathBuf::from("/a/b"));
    }
}
