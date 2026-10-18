//! Port of `util/AnsiColors.java`: utilities for styling terminal output.
//!
//! Colors are disabled when the `NO_COLOR` environment variable is set (<https://no-color.org/>) or after calling
//! [`set_use_colors`] with `false`.

use std::sync::LazyLock;
use std::sync::atomic::{AtomicBool, Ordering};

static USE_COLORS: LazyLock<AtomicBool> = LazyLock::new(|| {
    let no_color = std::env::var("NO_COLOR").ok();
    AtomicBool::new(no_color.is_none_or(|v| v == "\0"))
});

const COLOR_RESET: &str = "\u{1b}[0m";
const FG_RED: &str = "\u{1b}[31m";
const FG_GREEN: &str = "\u{1b}[32m";
const FG_YELLOW: &str = "\u{1b}[33m";
const FG_BLUE: &str = "\u{1b}[34m";
const REVERSE: &str = "\u{1b}[7m";
const BOLD: &str = "\u{1b}[1m";

/// Globally enable or disable colored output.
pub fn set_use_colors(colors: bool) {
    USE_COLORS.store(colors, Ordering::Relaxed);
}

/// Returns true if colored output is enabled.
pub fn use_colors() -> bool {
    USE_COLORS.load(Ordering::Relaxed)
}

fn color(fg: &str, string: &str) -> String {
    if use_colors() { format!("{fg}{string}{COLOR_RESET}") } else { string.to_string() }
}

pub fn red(string: &str) -> String {
    color(FG_RED, string)
}

pub fn green(string: &str) -> String {
    color(FG_GREEN, string)
}

pub fn yellow(string: &str) -> String {
    color(FG_YELLOW, string)
}

pub fn blue(string: &str) -> String {
    color(FG_BLUE, string)
}

pub fn red_background(string: &str) -> String {
    color(&format!("{REVERSE}{BOLD}{FG_RED}"), string)
}

pub fn green_background(string: &str) -> String {
    color(&format!("{REVERSE}{BOLD}{FG_GREEN}"), string)
}

pub fn red_bold(string: &str) -> String {
    color(&format!("{BOLD}{FG_RED}"), string)
}

pub fn green_bold(string: &str) -> String {
    color(&format!("{BOLD}{FG_GREEN}"), string)
}
