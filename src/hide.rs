//! Opening a node's place in `hide`, the other end of the Enter key.
//!
//! Shelled out to rather than linked: `hide` decides for itself whether to
//! reopen inside herdr, and that decision does not belong to this tool.

use std::path::Path;
use std::process::Command;

pub fn open(root: &Path, path: &str, line: Option<u32>) -> anyhow::Result<()> {
    let mut cmd = Command::new("hide");
    cmd.current_dir(root).arg(path);
    if let Some(line) = line {
        cmd.arg("--line").arg(line.to_string());
    }
    cmd.status()
        .map_err(|e| anyhow::anyhow!("could not run hide: {e}"))?;
    Ok(())
}
