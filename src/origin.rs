//! What "changed" means: resolving the diff (or an explicit file) the
//! graph starts from. Everything here answers to a live `git`, never to a
//! cache — the origin is wrong the moment it is stale.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::imports::RESOLVABLE_EXTENSIONS;
use crate::rev::Rev;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum Scope {
    /// Working tree against HEAD: unstaged and staged together.
    #[default]
    Uncommitted,
    Staged,
    /// Against the default branch's most recent common ancestor with HEAD.
    Branch,
}

/// The JS/TS files a scope touched, repo-relative, existing files only —
/// a deletion has nothing left to open or expand from.
pub fn changed_files(root: &Path, scope: Scope) -> Result<Vec<PathBuf>> {
    let args: Vec<&str> = match scope {
        Scope::Uncommitted => vec!["diff", "--name-only", "HEAD"],
        Scope::Staged => vec!["diff", "--name-only", "--cached"],
        Scope::Branch => {
            let base = default_branch(root)?;
            return diff_against(root, &format!("{base}..."), "HEAD");
        }
    };
    run_diff(root, &args)
}

/// The JS/TS files that differ between two revisions: `base...head`
/// (from their merge base) for two commits, so a branch against its
/// trunk yields the branch's own changes; the working tree against
/// `base` when head is the checkout. Unlike [`changed_files`], a file
/// need not exist in the working tree — the caller reads it through
/// `Rev`, in whichever revision has it.
pub fn changed_files_between(root: &Path, base: &Rev, head: &Rev) -> Result<Vec<PathBuf>> {
    Ok(changed_paths_between(root, base, head)?
        .into_iter()
        .filter(|p| {
            p.extension()
                .is_some_and(|ext| RESOLVABLE_EXTENSIONS.iter().any(|e| *e == ext))
        })
        .collect())
}

/// Every path that differs between the two revisions, any kind of file.
pub fn changed_paths_between(root: &Path, base: &Rev, head: &Rev) -> Result<Vec<PathBuf>> {
    let Some(base_sha) = base.commit_sha() else {
        bail!("the base revision must be a commit, not the working tree");
    };
    let spec = match head.commit_sha() {
        Some(head_sha) => format!("{base_sha}...{head_sha}"),
        None => base_sha.to_string(),
    };
    let out = Command::new("git")
        .current_dir(root)
        .args(["diff", "--name-only", &spec])
        .output()
        .context("running git diff")?;
    if !out.status.success() {
        bail!("git diff failed: {}", String::from_utf8_lossy(&out.stderr));
    }
    let text = String::from_utf8_lossy(&out.stdout);
    Ok(text.lines().map(PathBuf::from).collect())
}

/// One file in a diff: its status letter (`A`, `M`, `D`, `R`, …), path
/// and, for a rename or copy, the path it came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileStatus {
    pub status: char,
    pub path: PathBuf,
    pub from: Option<PathBuf>,
}

/// Every file that differs between the two revisions, with how
/// (`git diff --name-status -M`, from the merge base like
/// [`changed_paths_between`]).
pub fn changed_file_statuses(root: &Path, base: &Rev, head: &Rev) -> Result<Vec<FileStatus>> {
    let Some(base_sha) = base.commit_sha() else {
        bail!("the base revision must be a commit, not the working tree");
    };
    let spec = match head.commit_sha() {
        Some(head_sha) => format!("{base_sha}...{head_sha}"),
        None => base_sha.to_string(),
    };
    let out = Command::new("git")
        .current_dir(root)
        .args(["diff", "--name-status", "-M", "-z", &spec])
        .output()
        .context("running git diff")?;
    if !out.status.success() {
        bail!("git diff failed: {}", String::from_utf8_lossy(&out.stderr));
    }
    Ok(parse_name_status(&String::from_utf8_lossy(&out.stdout)))
}

/// `git diff --name-status -z` output: `STATUS\0path\0`, or
/// `R100\0old\0new\0` for renames and copies.
fn parse_name_status(text: &str) -> Vec<FileStatus> {
    let mut out = Vec::new();
    let mut it = text.split('\0').filter(|s| !s.is_empty());
    while let Some(st) = it.next() {
        let status = st.chars().next().unwrap_or('M');
        if matches!(status, 'R' | 'C') {
            let (Some(from), Some(to)) = (it.next(), it.next()) else { break };
            out.push(FileStatus { status, path: PathBuf::from(to), from: Some(PathBuf::from(from)) });
        } else if let Some(p) = it.next() {
            out.push(FileStatus { status, path: PathBuf::from(p), from: None });
        }
    }
    out
}

#[cfg(test)]
mod status_tests {
    use super::*;

    #[test]
    fn name_status_reads_renames() {
        let got = parse_name_status("M\0a.ts\0R087\0old/b.ts\0new/b.ts\0D\0c.sql\0A\0d.ts\0");
        assert_eq!(got.len(), 4);
        assert_eq!(got[1], FileStatus { status: 'R', path: "new/b.ts".into(), from: Some("old/b.ts".into()) });
        assert_eq!(got[2].status, 'D');
    }
}

fn diff_against(root: &Path, base: &str, head: &str) -> Result<Vec<PathBuf>> {
    let spec = format!("{base}{head}");
    run_diff(root, &["diff", "--name-only", &spec])
}

fn run_diff(root: &Path, args: &[&str]) -> Result<Vec<PathBuf>> {
    let out = Command::new("git")
        .current_dir(root)
        .args(args)
        .output()
        .context("running git diff")?;
    if !out.status.success() {
        bail!("git diff failed: {}", String::from_utf8_lossy(&out.stderr));
    }
    let text = String::from_utf8_lossy(&out.stdout);
    Ok(text
        .lines()
        .map(PathBuf::from)
        .filter(|p| root.join(p).is_file())
        .filter(|p| {
            p.extension()
                .is_some_and(|ext| RESOLVABLE_EXTENSIONS.iter().any(|e| *e == ext))
        })
        .collect())
}

/// The branch a `branch` scope diffs against: the remote's default branch
/// when there is a remote, else whichever of `main`/`master` exists
/// locally — a repo like this one, with no remote pushed yet, still needs
/// an answer.
pub fn default_branch(root: &Path) -> Result<String> {
    let symbolic = Command::new("git")
        .current_dir(root)
        .args(["symbolic-ref", "-q", "refs/remotes/origin/HEAD"])
        .output();
    if let Ok(out) = symbolic
        && out.status.success()
    {
        let full = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if let Some(name) = full.strip_prefix("refs/remotes/origin/") {
            return Ok(name.to_string());
        }
    }
    for candidate in ["main", "master"] {
        let exists = Command::new("git")
            .current_dir(root)
            .args(["rev-parse", "--verify", "--quiet", candidate])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if exists {
            return Ok(candidate.to_string());
        }
    }
    bail!("no origin/HEAD and no local main or master to diff the branch scope against")
}

/// The commit a diff is taken from: the merge base of `base` (default:
/// the remote's default branch) and `head` (default: `HEAD`) — what
/// `git diff base...head` compares against, and what a pull request
/// shows. Diffing against the trunk's tip instead would count
/// every commit the trunk gained since the branch forked as a change of
/// this branch. A base that is already an ancestor of head resolves to
/// itself.
pub fn merge_base(root: &Path, base: Option<&str>, head: Option<&str>) -> Result<String> {
    let base = match base {
        Some(b) => b.to_string(),
        None => {
            let name = default_branch(root)?;
            let remote = format!("origin/{name}");
            if rev_exists(root, &remote) { remote } else { name }
        }
    };
    let head = head.unwrap_or("HEAD");
    let out = Command::new("git")
        .current_dir(root)
        .args(["merge-base", &base, head])
        .output()
        .context("running git merge-base")?;
    if !out.status.success() {
        bail!(
            "no common ancestor of {base} and {head}: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// A revision name (`HEAD`, a branch, a short sha) as its full sha.
pub fn commit_sha(root: &Path, rev: &str) -> Result<String> {
    let out = Command::new("git")
        .current_dir(root)
        .args(["rev-parse", "--verify", &format!("{rev}^{{commit}}")])
        .output()
        .context("running git rev-parse")?;
    if !out.status.success() {
        bail!("not a commit: {rev}");
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn rev_exists(root: &Path, rev: &str) -> bool {
    Command::new("git")
        .current_dir(root)
        .args(["rev-parse", "--verify", "--quiet", &format!("{rev}^{{commit}}")])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// An explicit `file` or `file:line` origin, bypassing the diff entirely.
pub fn parse_file_arg(arg: &str) -> (PathBuf, Option<u32>) {
    match arg.rsplit_once(':') {
        Some((path, line)) if line.chars().all(|c| c.is_ascii_digit()) && !line.is_empty() => {
            (PathBuf::from(path), line.parse().ok())
        }
        _ => (PathBuf::from(arg), None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_bare_path_with_no_line() {
        assert_eq!(
            parse_file_arg("src/main.rs"),
            (PathBuf::from("src/main.rs"), None)
        );
    }

    #[test]
    fn parses_a_path_with_a_trailing_line_number() {
        assert_eq!(
            parse_file_arg("src/main.rs:42"),
            (PathBuf::from("src/main.rs"), Some(42))
        );
    }

    #[test]
    fn a_colon_that_is_not_a_line_number_stays_part_of_the_path() {
        // Windows-drive-letter-shaped paths are not a concern here, but a
        // trailing non-numeric suffix should not be swallowed as a line.
        assert_eq!(
            parse_file_arg("src/weird:file"),
            (PathBuf::from("src/weird:file"), None)
        );
    }
}
