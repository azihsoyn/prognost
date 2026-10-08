//! Where the git repository the current directory sits in actually starts.

use std::path::PathBuf;
use std::process::Command;

use anyhow::{Result, bail};

pub fn root() -> Result<PathBuf> {
    let out = Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .output()?;
    if !out.status.success() {
        bail!("not inside a git repository");
    }
    Ok(PathBuf::from(String::from_utf8_lossy(&out.stdout).trim()))
}
