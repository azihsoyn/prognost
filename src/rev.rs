//! Reading a file as of some revision without touching the checkout.
//! HEAD's own default is the working tree, so an uncommitted edit is
//! still visible; BASE reads straight from git's object store.
//!
//! `Commit` materializes the whole tree once, into a scratch directory,
//! the first time anything asks it to read or check a file — import
//! resolution tries several candidate paths per specifier, and a `git
//! show`/`cat-file` subprocess per candidate measured over twenty seconds
//! on a real monorepo. `git archive | tar` once, then plain filesystem
//! reads, is close to instant by comparison. The directory is a plain
//! scratch copy, not a checkout the rest of the tool ever writes to, and
//! is removed when the `Rev` is dropped.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

#[derive(Debug)]
pub enum Rev {
    /// The checkout as it is now. Read once per run and remembered: an
    /// impact walk lists the tree and reads the same few thousand files
    /// again and again, and doing that against the disk every time cost
    /// minutes of system time on a real monorepo. A long-lived view
    /// (the TUI, `--serve`) therefore shows the tree as of its launch.
    Working(Cache),
    Commit(Commit),
}

/// What a revision has been asked so far.
#[derive(Debug, Default)]
pub struct Cache {
    files: RefCell<Option<Vec<PathBuf>>>,
    /// The same list as a set, for existence checks without a stat.
    file_set: RefCell<Option<HashSet<PathBuf>>>,
    /// File texts read so far.
    texts: RefCell<HashMap<PathBuf, Option<String>>>,
    /// The files under one directory, per directory asked: a package's
    /// files are asked for once per function whose callers are sought,
    /// and filtering the whole tree's list each time dominated a walk.
    under: RefCell<HashMap<PathBuf, std::rc::Rc<Vec<PathBuf>>>>,
}

#[derive(Debug)]
pub struct Commit {
    sha: String,
    extracted: RefCell<Option<Option<PathBuf>>>,
    cache: Cache,
}

impl Rev {
    pub fn working() -> Self {
        Rev::Working(Cache::default())
    }

    pub fn commit(sha: impl Into<String>) -> Self {
        Rev::Commit(Commit {
            sha: sha.into(),
            extracted: RefCell::new(None),
            cache: Cache::default(),
        })
    }

    fn cache(&self) -> &Cache {
        match self {
            Rev::Working(c) => c,
            Rev::Commit(c) => &c.cache,
        }
    }

    /// Where this revision's files sit on disk: the checkout itself, or
    /// the commit's extracted tree.
    fn dir(&self, root: &Path) -> Option<PathBuf> {
        match self {
            Rev::Working(_) => Some(root.to_path_buf()),
            Rev::Commit(c) => self.tree(c, root),
        }
    }

    pub fn read(&self, root: &Path, rel: &Path) -> Option<String> {
        let cache = self.cache();
        if let Some(hit) = cache.texts.borrow().get(rel) {
            return hit.clone();
        }
        let text = std::fs::read_to_string(self.dir(root)?.join(rel)).ok();
        cache
            .texts
            .borrow_mut()
            .insert(rel.to_path_buf(), text.clone());
        text
    }

    /// The commit this reads from, when it isn't the working tree —
    /// for building a `git diff` spec, which needs the raw sha.
    pub fn commit_sha(&self) -> Option<&str> {
        match self {
            Rev::Working(_) => None,
            Rev::Commit(c) => Some(&c.sha),
        }
    }

    pub fn label(&self) -> String {
        match self {
            Rev::Working(_) => "working tree".to_string(),
            Rev::Commit(c) => c.sha.clone(),
        }
    }

    pub fn exists(&self, root: &Path, rel: &Path) -> bool {
        let cache = self.cache();
        if cache.file_set.borrow().is_none() {
            let set = self.list_files(root).into_iter().collect();
            *cache.file_set.borrow_mut() = Some(set);
        }
        cache
            .file_set
            .borrow()
            .as_ref()
            .is_some_and(|set| set.contains(rel))
    }

    /// Every file in the tree, repo-relative. Cached: the coarse layer
    /// asks this once per candidate package while narrowing a search, and
    /// a walk over a real monorepo is not free to repeat dozens of times.
    /// The checkout is listed by git (tracked plus untracked, ignored
    /// files left out), a commit's extracted tree by walking it.
    pub fn list_files(&self, root: &Path) -> Vec<PathBuf> {
        let cache = self.cache();
        if let Some(cached) = cache.files.borrow().as_ref() {
            return cached.clone();
        }
        let files = match self {
            Rev::Working(_) => git_listed(root).unwrap_or_else(|| walk_tree(root)),
            Rev::Commit(c) => self
                .tree(c, root)
                .map(|dir| walk_tree(&dir))
                .unwrap_or_default(),
        };
        *cache.files.borrow_mut() = Some(files.clone());
        files
    }

    /// The files under `dir` (repo-relative), from the cached list.
    pub fn files_under(&self, root: &Path, dir: &Path) -> std::rc::Rc<Vec<PathBuf>> {
        let cache = self.cache();
        if let Some(hit) = cache.under.borrow().get(dir) {
            return hit.clone();
        }
        if cache.files.borrow().is_none() {
            self.list_files(root);
        }
        let files: Vec<PathBuf> = cache
            .files
            .borrow()
            .as_ref()
            .map(|all| all.iter().filter(|f| f.starts_with(dir)).cloned().collect())
            .unwrap_or_default();
        let files = std::rc::Rc::new(files);
        cache
            .under
            .borrow_mut()
            .insert(dir.to_path_buf(), files.clone());
        files
    }

    /// The scratch directory this revision is materialized into,
    /// extracting it on the first call and reusing it after. `None` means
    /// the extraction itself failed (bad sha, no git, …); callers already
    /// treat a missing file as "not found", so this folds into the same
    /// path rather than needing its own error case everywhere.
    fn tree(&self, c: &Commit, root: &Path) -> Option<PathBuf> {
        if let Some(cached) = c.extracted.borrow().as_ref() {
            return cached.clone();
        }
        let dir = extract(root, &c.sha);
        *c.extracted.borrow_mut() = Some(dir.clone());
        dir
    }
}

/// The checkout's files as git sees them: tracked and untracked, minus
/// what .gitignore excludes and minus tracked files deleted from disk.
/// The same directories [`walk_tree`] skips are skipped here too.
fn git_listed(root: &Path) -> Option<Vec<PathBuf>> {
    let out = Command::new("git")
        .current_dir(root)
        .args([
            "ls-files",
            "-z",
            "--cached",
            "--others",
            "--exclude-standard",
            "--deduplicate",
        ])
        .output()
        .ok()
        .filter(|o| o.status.success())?;
    let deleted: HashSet<PathBuf> = Command::new("git")
        .current_dir(root)
        .args(["ls-files", "-z", "--deleted"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| {
            o.stdout
                .split(|b| *b == 0)
                .filter(|s| !s.is_empty())
                .map(|s| PathBuf::from(String::from_utf8_lossy(s).into_owned()))
                .collect()
        })
        .unwrap_or_default();
    Some(
        out.stdout
            .split(|b| *b == 0)
            .filter(|s| !s.is_empty())
            .map(|s| PathBuf::from(String::from_utf8_lossy(s).into_owned()))
            .filter(|p| !deleted.contains(p))
            .filter(|p| {
                p.components().all(|c| {
                    let s = c.as_os_str().to_string_lossy();
                    !s.starts_with('.') && s != "node_modules" && s != "dist" && s != "target"
                })
            })
            .collect(),
    )
}

/// Where extracted commit trees are kept between runs: one directory
/// per full sha under the system temp directory.
pub fn cache_dir() -> PathBuf {
    std::env::temp_dir().join("prognost-cache")
}

/// (trees, bytes) in the cache.
pub fn cache_usage() -> (usize, u64) {
    let Ok(entries) = std::fs::read_dir(cache_dir()) else {
        return (0, 0);
    };
    let mut trees = 0;
    let mut bytes = 0;
    for e in entries.flatten() {
        if e.file_type().is_ok_and(|t| t.is_dir()) {
            trees += 1;
            bytes += dir_size(&e.path());
        }
    }
    (trees, bytes)
}

/// Removes every extracted tree; they are rebuilt from git on the next
/// run that needs one. Returns (trees, bytes) removed.
pub fn clean_cache() -> std::io::Result<(usize, u64)> {
    let usage = cache_usage();
    match std::fs::remove_dir_all(cache_dir()) {
        Ok(()) => Ok(usage),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok((0, 0)),
        Err(e) => Err(e),
    }
}

/// Bytes under `dir`, not following symlinks.
fn dir_size(dir: &Path) -> u64 {
    let mut total = 0;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&d) else {
            continue;
        };
        for e in entries.flatten() {
            let Ok(meta) = e.path().symlink_metadata() else {
                continue;
            };
            if meta.is_dir() {
                stack.push(e.path());
            } else {
                total += meta.len();
            }
        }
    }
    total
}

/// A commit's tree, materialized once per machine: keyed by the full
/// sha under the temp dir and kept after the run, so the next launch on
/// the same revisions skips the archive+untar of the whole repository
/// (the single largest cost of starting up). A `.complete` marker
/// guards against reusing a half-extracted tree.
fn extract(root: &Path, sha: &str) -> Option<PathBuf> {
    let full = Command::new("git")
        .current_dir(root)
        .args(["rev-parse", "--verify", &format!("{sha}^{{commit}}")])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())?;
    let dir = cache_dir().join(&full);
    let marker = dir.join(".complete");
    if marker.is_file() {
        return Some(dir);
    }
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).ok()?;

    let mut archive = Command::new("git")
        .current_dir(root)
        .args(["archive", sha])
        .stdout(Stdio::piped())
        .spawn()
        .ok()?;
    let archive_stdout = archive.stdout.take()?;

    let mut tar = Command::new("tar")
        .args(["-x", "-f", "-", "-C"])
        .arg(&dir)
        .stdin(Stdio::piped())
        .spawn()
        .ok()?;
    // Ferry archive's stdout into tar's stdin ourselves rather than
    // handing tar the pipe directly, so a `git archive` failure (a bad
    // sha) is still observed instead of tar just seeing EOF and
    // "succeeding" over an empty stream.
    let mut tar_stdin = tar.stdin.take()?;
    std::thread::spawn(move || {
        let mut reader = archive_stdout;
        let _ = std::io::copy(&mut reader, &mut tar_stdin);
    });

    let archive_ok = archive.wait().ok()?.success();
    let tar_ok = tar.wait().ok()?.success();
    if !(archive_ok && tar_ok) {
        let _ = std::fs::remove_dir_all(&dir);
        return None;
    }
    std::fs::write(&marker, full).ok()?;
    Some(dir)
}

fn walk_tree(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![PathBuf::new()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(root.join(&dir)) else {
            continue;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            if name_str.starts_with('.')
                || name_str == "node_modules"
                || name_str == "dist"
                || name_str == "target"
            {
                continue;
            }
            let rel = dir.join(&name);
            match entry.file_type() {
                Ok(t) if t.is_dir() => stack.push(rel),
                Ok(t) if t.is_file() => out.push(rel),
                _ => {}
            }
        }
    }
    out
}
