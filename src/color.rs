//! Whether output is coloured, and the few styles it uses.
//!
//! Decided once, at start: `--color always|never` (or `--no-color`) when
//! given; otherwise `NO_COLOR` (set and non-empty) turns colour off,
//! `CLICOLOR_FORCE` (set, not `0`) turns it on, and failing both, colour
//! is on when standard output is a terminal that isn't `dumb`. Text
//! piped to a file or another program is plain unless asked for.

use std::io::IsTerminal;
use std::sync::OnceLock;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Choice {
    Auto,
    Always,
    Never,
}

impl Choice {
    pub fn parse(s: &str) -> Option<Choice> {
        match s {
            "auto" => Some(Choice::Auto),
            "always" => Some(Choice::Always),
            "never" => Some(Choice::Never),
            _ => None,
        }
    }
}

static ENABLED: OnceLock<bool> = OnceLock::new();

/// Settles the choice for the rest of the run. Later calls change nothing.
pub fn init(choice: Choice) {
    let env = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
    let on = match choice {
        Choice::Always => true,
        Choice::Never => false,
        Choice::Auto => {
            if env("NO_COLOR").is_some() {
                false
            } else if env("CLICOLOR_FORCE").is_some_and(|v| v != "0") {
                true
            } else {
                std::io::stdout().is_terminal() && env("TERM").as_deref() != Some("dumb")
            }
        }
    };
    let _ = ENABLED.set(on);
}

/// Colour is on. False until [`init`] says otherwise, so library callers
/// and tests get plain text.
pub fn enabled() -> bool {
    ENABLED.get().copied().unwrap_or(false)
}

/// `--color <when>`, `--color=<when>` and `--no-color` taken out of
/// `args`, wherever they appear; the rest returned in order.
pub fn take_option(args: Vec<String>) -> Result<(Choice, Vec<String>), String> {
    let mut choice = Choice::Auto;
    let mut rest = Vec::with_capacity(args.len());
    let mut it = args.into_iter();
    while let Some(a) = it.next() {
        if a == "--no-color" {
            choice = Choice::Never;
        } else if a == "--color" {
            let v = it.next().ok_or("--color needs auto, always or never")?;
            choice =
                Choice::parse(&v).ok_or(format!("--color {v}: expected auto, always or never"))?;
        } else if let Some(v) = a.strip_prefix("--color=") {
            choice =
                Choice::parse(v).ok_or(format!("--color={v}: expected auto, always or never"))?;
        } else {
            rest.push(a);
        }
    }
    Ok((choice, rest))
}

fn paint(s: &str, code: &str) -> String {
    if enabled() && !s.is_empty() {
        format!("\x1b[{code}m{s}\x1b[0m")
    } else {
        s.to_string()
    }
}

pub fn bold(s: &str) -> String {
    paint(s, "1")
}
pub fn dim(s: &str) -> String {
    paint(s, "2")
}
pub fn red(s: &str) -> String {
    paint(s, "31")
}
pub fn green(s: &str) -> String {
    paint(s, "32")
}
pub fn yellow(s: &str) -> String {
    paint(s, "33")
}
pub fn blue(s: &str) -> String {
    paint(s, "34")
}
pub fn magenta(s: &str) -> String {
    paint(s, "35")
}
pub fn cyan(s: &str) -> String {
    paint(s, "36")
}
pub fn bold_red(s: &str) -> String {
    paint(s, "1;31")
}
pub fn bold_yellow(s: &str) -> String {
    paint(s, "1;33")
}
pub fn bold_green(s: &str) -> String {
    paint(s, "1;32")
}

/// The logo (`assets/logo.txt`), coloured: the calls cyan, the change
/// amber, the node nothing reaches dim, the tagline dim.
pub fn logo() -> String {
    const LOGO: &str = include_str!("../assets/logo.txt");
    let mut out = String::new();
    for line in LOGO.lines() {
        let (art, tag) = match line.char_indices().nth(17) {
            Some((i, _)) if line.contains("prognosis") => line.split_at(i),
            _ => (line, ""),
        };
        for ch in art.chars() {
            let s = ch.to_string();
            out.push_str(&match ch {
                '●' | '╲' | '╱' => cyan(&s),
                '◉' => paint(&s, "1;33"),
                '○' => dim(&s),
                ' ' => s,
                _ => bold(&s),
            });
        }
        out.push_str(&dim(tag));
        out.push('\n');
    }
    out
}

/// Colours out of a drawn TUI frame, for `--color never`: every cell back
/// to the terminal's own colours, and a cell that stood out by its
/// background (the selection, a highlighted chain) reversed instead, so
/// it still stands out.
pub fn strip_buffer(buf: &mut ratatui::buffer::Buffer) {
    use ratatui::style::{Color, Modifier};
    for cell in buf.content.iter_mut() {
        if cell.bg != Color::Reset {
            cell.modifier.insert(Modifier::REVERSED);
        }
        cell.fg = Color::Reset;
        cell.bg = Color::Reset;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_option_is_found_anywhere_and_removed() {
        let args = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let (c, rest) = take_option(args(&["plan", "--no-color", "--json"])).unwrap();
        assert_eq!((c, rest), (Choice::Never, args(&["plan", "--json"])));
        let (c, rest) = take_option(args(&["assess", "--color", "always", "-"])).unwrap();
        assert_eq!((c, rest), (Choice::Always, args(&["assess", "-"])));
        let (c, _) = take_option(args(&["--color=never"])).unwrap();
        assert_eq!(c, Choice::Never);
        assert!(take_option(args(&["--color", "rainbow"])).is_err());
    }
}
