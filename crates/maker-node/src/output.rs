//! Styled CLI output helpers. Two visual conventions:
//!
//! - `step("doing X")` + `step_ok()` / `step_warn(msg)` / `step_skip()` for
//!   inline progress (used by `colorex maker init`).
//! - `up_arrow("listening on …")` for the daemon's startup banner (used
//!   by `colorex maker up`).
//!
//! Falls back to ASCII (`>`, `^`) when `COLOREX_NO_UNICODE=1` is set so
//! `colorex` stays readable in environments where the `›` / `↑` glyphs
//! render as boxes (older terminals, basic CI logs).

use std::io::{self, Write};

fn unicode_disabled() -> bool {
    std::env::var_os("COLOREX_NO_UNICODE").is_some()
}

fn chevron(disabled: bool) -> &'static str {
    if disabled {
        "> "
    } else {
        "› "
    }
}

fn arrow(disabled: bool) -> &'static str {
    if disabled {
        "^ "
    } else {
        "↑ "
    }
}

/// Print the start of a progress line (no newline). Pair with one of
/// `step_ok` / `step_warn` / `step_skip` to terminate.
pub fn step(label: &str) {
    print!("{}{label} ", chevron(unicode_disabled()));
    let _ = io::stdout().flush();
}

pub fn step_ok() {
    println!("[ok]");
}

pub fn step_warn(msg: &str) {
    println!("[warn: {msg}]");
}

pub fn step_skip() {
    println!("[skipped]");
}

/// Print a single `↑ <label>` line. No companion call required.
pub fn up_arrow(label: &str) {
    println!("{}{label}", arrow(unicode_disabled()));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chevron_glyph_default_and_fallback() {
        assert_eq!(chevron(false), "› ");
        assert_eq!(chevron(true), "> ");
    }

    #[test]
    fn arrow_glyph_default_and_fallback() {
        assert_eq!(arrow(false), "↑ ");
        assert_eq!(arrow(true), "^ ");
    }
}
