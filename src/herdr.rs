//! Working inside [herdr](https://herdr.dev), the terminal workspace
//! manager: when prognost runs in a herdr pane, a view it can't show in
//! place — the graph asked for by something without a terminal (an agent,
//! a script), a file to edit — opens in a pane beside the caller instead.
//! Outside herdr nothing here does anything.
//!
//! Shelled out to the `herdr` CLI, so the session, socket and permissions
//! are whatever the calling pane inherited.

use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, bail};

/// Running in a herdr-managed pane, with the `herdr` CLI at hand.
pub fn inside() -> bool {
    std::env::var("HERDR_ENV").is_ok_and(|v| v == "1")
        && Command::new("herdr")
            .arg("--version")
            .output()
            .is_ok_and(|o| o.status.success())
}

fn herdr(args: &[&str]) -> Result<serde_json::Value> {
    let out = Command::new("herdr")
        .args(args)
        .output()
        .context("running herdr")?;
    if !out.status.success() {
        bail!(
            "herdr {}: {}",
            args.first().copied().unwrap_or(""),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let text = String::from_utf8_lossy(&out.stdout);
    Ok(serde_json::from_str(&text).unwrap_or(serde_json::Value::Null))
}

/// The side to split the calling pane on: right when it is wide, down
/// when it is narrow or tall (a terminal cell is about twice as tall as
/// it is wide).
fn split_direction() -> &'static str {
    let Ok(layout) = herdr(&["pane", "layout", "--current"]) else {
        return "right";
    };
    let me = std::env::var("HERDR_PANE_ID").unwrap_or_default();
    let rect = layout
        .pointer("/result/layout/panes")
        .and_then(|p| p.as_array())
        .and_then(|panes| panes.iter().find(|p| p["pane_id"] == me.as_str()))
        .map(|p| {
            (
                p["rect"]["width"].as_u64().unwrap_or(0),
                p["rect"]["height"].as_u64().unwrap_or(0),
            )
        });
    match rect {
        Some((w, h)) if w < 2 * h => "down",
        _ => "right",
    }
}

/// Runs `command` (a shell command line) in a new pane split off the
/// caller's, working in `cwd`, labelled `label`. Returns the new pane's id.
pub fn open_beside(cwd: &Path, command: &str, label: &str, focus: bool) -> Result<String> {
    let cwd = cwd.to_string_lossy();
    let split = herdr(&[
        "pane",
        "split",
        "--current",
        "--direction",
        split_direction(),
        "--cwd",
        &cwd,
        if focus { "--focus" } else { "--no-focus" },
    ])?;
    let pane = split
        .pointer("/result/pane/pane_id")
        .and_then(|v| v.as_str())
        .context("herdr pane split returned no pane id")?
        .to_string();
    let _ = herdr(&["pane", "rename", &pane, label]);
    herdr(&["pane", "run", &pane, command])?;
    Ok(pane)
}

/// `args` as one POSIX shell command line.
pub fn shell_line<S: AsRef<str>>(args: &[S]) -> String {
    args.iter()
        .map(|a| {
            let a = a.as_ref();
            if !a.is_empty()
                && a.chars()
                    .all(|c| c.is_ascii_alphanumeric() || "-_./:=@+,".contains(c))
            {
                a.to_string()
            } else {
                format!("'{}'", a.replace('\'', r"'\''"))
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// The editor command for `path` at `line`: `$VISUAL`, else `$EDITOR`,
/// else `vi`, with `+<line>` (understood by vi, vim, nvim, nano, emacs,
/// helix, micro; `code` and `zed` get `--goto path:line`).
pub fn editor_line(path: &Path, line: u32) -> String {
    let editor = std::env::var("VISUAL")
        .ok()
        .filter(|v| !v.is_empty())
        .or_else(|| std::env::var("EDITOR").ok().filter(|v| !v.is_empty()))
        .unwrap_or_else(|| "vi".to_string());
    let p = path.to_string_lossy();
    let name = editor.split_whitespace().next().unwrap_or("");
    let base = name.rsplit('/').next().unwrap_or(name);
    // The editor variable is itself a command line (`code -w`): kept as is.
    if matches!(base, "code" | "codium" | "cursor" | "zed") {
        format!("{editor} --goto {}", shell_line(&[format!("{p}:{line}")]))
    } else {
        format!("{editor} +{line} {}", shell_line(&[p.as_ref()]))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_lines_quote_only_what_needs_it() {
        assert_eq!(
            shell_line(&["prognost", "graph", "--base", "main"]),
            "prognost graph --base main"
        );
        assert_eq!(shell_line(&["a b", "it's"]), r#"'a b' 'it'\''s'"#);
    }
}
