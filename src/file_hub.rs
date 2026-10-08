//! The same hub shape as `hub.rs`, one level coarser: the focus is a
//! file, its callers are the files that import it, its callees are the
//! files it imports — built on reachhop's original coarse-layer graph
//! engine (`graph.rs`), now revision-aware, diffed the same way a
//! function's calls are.
//!
//! Identity here is simpler than at the function level: a file path is
//! stable across a revision unless the file itself moved, so there is no
//! anonymous-node problem to work around.

use std::path::Path;

use crate::graph::{self, Direction};
use crate::hub::{Hub, Node, Status};
use crate::rev::Rev;
use crate::workspace::Workspace;

pub fn build(
    root: &Path,
    base_rev: &Rev,
    head_rev: &Rev,
    base_ws: &Workspace,
    head_ws: &Workspace,
    focus_path: &Path,
) -> Option<Hub> {
    let base_exists = base_rev.exists(root, focus_path);
    let head_exists = head_rev.exists(root, focus_path);
    if !base_exists && !head_exists {
        return None;
    }

    let base_callers = if base_exists {
        graph::reach(root, base_rev, focus_path, Direction::Callers, base_ws).nodes
    } else {
        Default::default()
    };
    let head_callers = if head_exists {
        graph::reach(root, head_rev, focus_path, Direction::Callers, head_ws).nodes
    } else {
        Default::default()
    };
    let base_deps = if base_exists {
        graph::reach(root, base_rev, focus_path, Direction::Dependencies, base_ws).nodes
    } else {
        Default::default()
    };
    let head_deps = if head_exists {
        graph::reach(root, head_rev, focus_path, Direction::Dependencies, head_ws).nodes
    } else {
        Default::default()
    };

    let status = match (base_exists, head_exists) {
        (true, true) => {
            let same = |a: &[graph::Node], b: &[graph::Node]| {
                let mut a: Vec<&str> = a.iter().map(|n| n.path.as_str()).collect();
                let mut b: Vec<&str> = b.iter().map(|n| n.path.as_str()).collect();
                a.sort_unstable();
                b.sort_unstable();
                a == b
            };
            if same(&base_callers, &head_callers) && same(&base_deps, &head_deps) {
                Status::Unchanged
            } else {
                Status::Changed
            }
        }
        (false, true) => Status::Added,
        (true, false) => Status::Removed,
        (false, false) => unreachable!(),
    };

    let label = focus_path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();

    Some(Hub {
        focus: Node {
            label,
            status,
            drillable: true,
        },
        callers: diff_nodes(&base_callers, &head_callers),
        callees: diff_nodes(&base_deps, &head_deps),
    })
}

fn diff_nodes(base: &[graph::Node], head: &[graph::Node]) -> Vec<Node> {
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for n in head {
        seen.insert(n.path.clone());
        let in_base = base.iter().any(|b| b.path == n.path);
        let status = if in_base {
            Status::Unchanged
        } else {
            Status::Added
        };
        out.push(Node {
            label: basename(&n.path),
            status,
            drillable: true,
        });
    }
    for n in base {
        if seen.contains(&n.path) {
            continue;
        }
        out.push(Node {
            label: basename(&n.path),
            status: Status::Removed,
            drillable: true,
        });
    }
    out
}

fn basename(path: &str) -> String {
    Path::new(path)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// Same fixture shape as graph.rs's own tests: `db-pool` changed,
    /// three app `client.ts` files import it. Here the app that stops
    /// importing it in HEAD is the interesting case — the caller's own
    /// status is a plain path comparison, no aligner needed.
    fn fixture() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::write(
            root.join("package.json"),
            r#"{ "name": "root", "private": true, "workspaces": ["apps/*", "packages/*"] }"#,
        )
        .unwrap();
        fs::create_dir_all(root.join("packages/db-pool/src")).unwrap();
        fs::write(
            root.join("packages/db-pool/package.json"),
            r#"{ "name": "@lib/db-pool", "main": "src/index.ts" }"#,
        )
        .unwrap();
        fs::write(
            root.join("packages/db-pool/src/index.ts"),
            "export const x = 1;\n",
        )
        .unwrap();

        for app in ["shop", "auth"] {
            fs::create_dir_all(root.join(format!("apps/{app}/src"))).unwrap();
            fs::write(
                root.join(format!("apps/{app}/package.json")),
                format!(r#"{{ "name": "@app/{app}", "dependencies": {{ "@lib/db-pool": "workspace:*" }} }}"#),
            )
            .unwrap();
            fs::write(
                root.join(format!("apps/{app}/src/client.ts")),
                "import { x } from '@lib/db-pool';\nexport const y = x;\n",
            )
            .unwrap();
        }
        dir
    }

    #[test]
    fn a_caller_that_stops_importing_shows_as_removed() {
        let dir = fixture();
        let root = dir.path();
        let run = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .current_dir(root)
                .args(args)
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "{args:?}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        };
        run(&["init", "-q"]);
        run(&["config", "user.email", "test@example.com"]);
        run(&["config", "user.name", "test"]);
        run(&["add", "-A"]);
        run(&["commit", "-q", "-m", "base"]);
        let out = std::process::Command::new("git")
            .current_dir(root)
            .args(["rev-parse", "HEAD"])
            .output()
            .unwrap();
        let base_sha = String::from_utf8_lossy(&out.stdout).trim().to_string();

        // HEAD (uncommitted): auth's client.ts no longer imports db-pool.
        fs::write(
            root.join("apps/auth/src/client.ts"),
            "export const y = 2;\n",
        )
        .unwrap();

        let base_rev = Rev::commit(base_sha);
        let head_rev = Rev::working();
        let base_ws = crate::workspace::discover(root, &base_rev).unwrap();
        let head_ws = crate::workspace::discover(root, &head_rev).unwrap();
        let target = Path::new("packages/db-pool/src/index.ts");
        let hub = build(root, &base_rev, &head_rev, &base_ws, &head_ws, target).unwrap();

        assert_eq!(hub.focus.status, Status::Changed);
        assert_eq!(hub.callers.len(), 2, "{:#?}", hub.callers);
        let removed = hub
            .callers
            .iter()
            .filter(|n| n.status == Status::Removed)
            .count();
        let unchanged = hub
            .callers
            .iter()
            .filter(|n| n.status == Status::Unchanged)
            .count();
        assert_eq!((removed, unchanged), (1, 1), "{:#?}", hub.callers);
    }
}
