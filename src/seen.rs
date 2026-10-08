//! "I have looked at this": which changed lines of the diff the reader
//! has reviewed, kept in the same store slidiff uses — [`diffseen`]'s
//! content-addressed map under `<git-dir>/slidiff/seen.json` — so a file
//! marked here is marked there and vice versa, and the same file-level
//! "done" state can be mirrored to GitHub's Viewed checkbox.
//!
//! Keys follow slidiff exactly: the repo-relative path, one FNV-1a hash
//! per `git diff -U3` hunk over its (mark, text) lines, `#n` suffixes for
//! identical hunks in one file, and the index of a line among the
//! hunk's changed lines.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use diffseen::{Mark, Store};

/// One hunk of a file's diff: its content key and how many changed
/// lines it holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HunkKey {
    pub key: String,
    pub changed: usize,
}

pub struct SeenStore {
    store: Store,
    /// Hunk keys per file, computed once per session.
    hunks: HashMap<String, Vec<HunkKey>>,
    loaded: bool,
    root: PathBuf,
    base: String,
    head: Option<String>,
}

impl SeenStore {
    /// Opens the store shared with slidiff for this repository; `base`
    /// and `head` are the revisions whose diff the hunks come from
    /// (`head` `None` = the working tree).
    pub fn open(root: &Path, base: &str, head: Option<&str>) -> SeenStore {
        let git_dir = Command::new("git")
            .current_dir(root)
            .args(["rev-parse", "--absolute-git-dir"])
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| PathBuf::from(String::from_utf8_lossy(&o.stdout).trim()));
        let store = match git_dir {
            Some(dir) => Store::open(dir.join("slidiff/seen.json")),
            None => Store::in_memory(),
        };
        SeenStore {
            store,
            hunks: HashMap::new(),
            loaded: false,
            root: root.to_path_buf(),
            base: base.to_string(),
            head: head.map(str::to_string),
        }
    }

    /// The hunks of one file's diff, keyed the slidiff way. The whole
    /// diff is taken once, on the first ask, and split per file: one
    /// `git diff` per file cost seconds on a graph of a few hundred files.
    pub fn hunks_of(&mut self, path: &str) -> Vec<HunkKey> {
        if !self.loaded {
            self.loaded = true;
            let mut cmd = Command::new("git");
            cmd.current_dir(&self.root)
                .args(["diff", "--no-color", "--no-ext-diff", "--find-renames", "-U3", &self.base]);
            if let Some(head) = &self.head {
                cmd.arg(head);
            }
            let text = cmd
                .output()
                .ok()
                .filter(|o| o.status.success())
                .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
                .unwrap_or_default();
            for (file, unified) in split_files(&text) {
                self.hunks.insert(file, hunk_keys(&unified));
            }
        }
        self.hunks.get(path).cloned().unwrap_or_default()
    }

    /// (seen, total) changed lines of a file.
    pub fn progress(&mut self, path: &str) -> (usize, usize) {
        let hunks = self.hunks_of(path);
        self.store
            .progress(path, hunks.iter().map(|h| (h.key.as_str(), h.changed)))
    }

    /// Fully seen and carrying no flags — the state worth mirroring to
    /// GitHub's Viewed checkbox.
    pub fn is_done(&mut self, path: &str) -> bool {
        let hunks = self.hunks_of(path);
        let (s, t) = self
            .store
            .progress(path, hunks.iter().map(|h| (h.key.as_str(), h.changed)));
        let pairs: Vec<(String, usize)> = hunks.iter().map(|h| (h.key.clone(), h.changed)).collect();
        t > 0 && s == t && self.store.flag_count(path, &pairs) == 0
    }

    /// Every changed line of the file seen (or, when it already is, none).
    /// Returns the new state.
    pub fn toggle_file(&mut self, path: &str) -> bool {
        let (s, t) = self.progress(path);
        let make_seen = !(t > 0 && s == t);
        self.set_file(path, make_seen);
        make_seen
    }

    pub fn set_file(&mut self, path: &str, seen: bool) {
        for h in self.hunks_of(path) {
            self.store.set_hunk(path, &h.key, h.changed, seen);
        }
        self.store.save();
    }
}

/// slidiff's keys for the hunks of one unified diff: the content hash,
/// with `#n` on the nth duplicate.
pub fn hunk_keys(unified: &str) -> Vec<HunkKey> {
    let mut counts: HashMap<String, usize> = HashMap::new();
    parse_hunks(unified)
        .into_iter()
        .map(|lines| {
            let changed = lines.iter().filter(|(m, _)| *m != Mark::Context).count();
            let raw = diffseen::hunk_hash(lines.iter().map(|(m, t)| (*m, t.as_str())));
            let n = counts.entry(raw.clone()).or_insert(0);
            let key = if *n == 0 { raw.clone() } else { format!("{raw}#{n}") };
            *n += 1;
            HunkKey { key, changed }
        })
        .collect()
}

/// A multi-file unified diff cut into (path, that file's diff): the
/// path is the new one (`+++ b/…`), or the old one for a deletion.
fn split_files(unified: &str) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    let mut cur: Option<(Option<String>, Option<String>, String)> = None;
    let finish = |cur: Option<(Option<String>, Option<String>, String)>, out: &mut Vec<(String, String)>| {
        if let Some((old, new, text)) = cur
            && let Some(path) = new.or(old)
        {
            out.push((path, text));
        }
    };
    for line in unified.lines() {
        if line.starts_with("diff --git ") {
            finish(cur.take(), &mut out);
            cur = Some((None, None, String::new()));
        }
        let Some((old, new, text)) = cur.as_mut() else {
            continue;
        };
        if old.is_none() && new.is_none() {
            if let Some(p) = line.strip_prefix("--- a/") {
                *old = Some(p.to_string());
            } else if let Some(p) = line.strip_prefix("+++ b/") {
                *new = Some(p.to_string());
            }
        } else if new.is_none()
            && let Some(p) = line.strip_prefix("+++ b/")
        {
            *new = Some(p.to_string());
        }
        text.push_str(line);
        text.push('\n');
    }
    finish(cur, &mut out);
    out
}

/// The hunks of a unified diff as (mark, text) lines, headers dropped.
fn parse_hunks(unified: &str) -> Vec<Vec<(Mark, String)>> {
    let mut hunks: Vec<Vec<(Mark, String)>> = Vec::new();
    let mut in_hunk = false;
    for line in unified.lines() {
        if line.starts_with("@@") {
            hunks.push(Vec::new());
            in_hunk = true;
            continue;
        }
        if !in_hunk {
            continue;
        }
        if line.starts_with("diff --git") {
            in_hunk = false;
            continue;
        }
        if line.starts_with("\\ No newline") {
            continue;
        }
        let (mark, text) = match line.chars().next() {
            Some('+') => (Mark::Add, &line[1..]),
            Some('-') => (Mark::Del, &line[1..]),
            Some(' ') => (Mark::Context, &line[1..]),
            _ => (Mark::Context, line),
        };
        if let Some(h) = hunks.last_mut() {
            h.push((mark, text.to_string()));
        }
    }
    hunks
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "\
diff --git a/f.rs b/f.rs
--- a/f.rs
+++ b/f.rs
@@ -1,3 +1,4 @@
 ctx
-old
+new
+more
 tail
@@ -10,2 +11,2 @@
 a
-b
+c
@@ -20,2 +21,2 @@
 a
-b
+c
";

    #[test]
    fn a_multi_file_diff_splits_per_file() {
        let two = format!(
            "diff --git a/f.rs b/f.rs\n--- a/f.rs\n+++ b/f.rs\n@@ -1 +1 @@\n-a\n+b\n\
             diff --git a/gone.rs b/gone.rs\ndeleted file mode 100644\n--- a/gone.rs\n+++ /dev/null\n@@ -1 +0,0 @@\n-x\n"
        );
        let files = split_files(&two);
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].0, "f.rs");
        assert_eq!(files[1].0, "gone.rs");
        assert_eq!(hunk_keys(&files[0].1), hunk_keys("@@ -1 +1 @@\n-a\n+b\n").iter().cloned().collect::<Vec<_>>());
    }

    #[test]
    fn keys_match_slidiffs_scheme() {
        let keys = hunk_keys(SAMPLE);
        assert_eq!(keys.len(), 3);
        assert_eq!(keys[0].changed, 3);
        // The first hunk hashes exactly as slidiff / diffseen would.
        let expected = diffseen::hunk_hash([
            (Mark::Context, "ctx"),
            (Mark::Del, "old"),
            (Mark::Add, "new"),
            (Mark::Add, "more"),
            (Mark::Context, "tail"),
        ]);
        assert_eq!(keys[0].key, expected);
        // Two identical hunks: the second gets a #1 suffix.
        assert_eq!(keys[2].key, format!("{}#1", keys[1].key));
    }
}
