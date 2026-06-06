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

use std::io::{self, IsTerminal, Write};

const RESET: &str = "\x1b[0m";
const GREEN: &str = "\x1b[32m";
const YELLOW: &str = "\x1b[33m";
const CYAN: &str = "\x1b[36m";
const DIM: &str = "\x1b[2m";

fn unicode_disabled() -> bool {
    std::env::var_os("COLOREX_NO_UNICODE").is_some()
}

/// Colorize only on a real terminal, and honor the `NO_COLOR` convention
/// (https://no-color.org) — so piped output and CI logs stay plain.
fn color_enabled() -> bool {
    std::env::var_os("NO_COLOR").is_none() && io::stdout().is_terminal()
}

fn paint(text: &str, code: &str) -> String {
    if color_enabled() {
        format!("{code}{text}{RESET}")
    } else {
        text.to_owned()
    }
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
    print!("{}{label} ", paint(chevron(unicode_disabled()), CYAN));
    let _ = io::stdout().flush();
}

pub fn step_ok() {
    println!("{}", paint("[ok]", GREEN));
}

/// Like [`step_ok`] but appends a dimmed parenthetical detail: `[ok] (detail)`.
pub fn step_ok_with(detail: &str) {
    println!("{} {}", paint("[ok]", GREEN), paint(&format!("({detail})"), DIM));
}

/// A standalone `› label` line (chevron, newline, no `[ok]`) — for static info
/// lines in a banner, as opposed to a completed step.
pub fn info(label: &str) {
    println!("{}{label}", paint(chevron(unicode_disabled()), CYAN));
}

pub fn step_warn(msg: &str) {
    println!("{}", paint(&format!("[warn: {msg}]"), YELLOW));
}

pub fn step_skip() {
    println!("{}", paint("[skipped]", DIM));
}

/// Print a single `↑ <label>` line. No companion call required.
pub fn up_arrow(label: &str) {
    println!("{}{label}", paint(arrow(unicode_disabled()), CYAN));
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
