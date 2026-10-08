//! The model shown on screen and returned over `--api`: files as nodes,
//! their package as the group they sit in, and the coarse-layer edges
//! between them — plus the one query the whole tool is built around,
//! "what is one hop from this file".

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::lang::{self, Lang};
use crate::rev::Rev;
use crate::tsconfig;
use crate::workspace::{PackageKind, Workspace};

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct Node {
    /// Repo-relative path; doubles as the node's id.
    pub path: String,
    pub group: String,
    /// True for a file the diff actually touched — everything reached by
    /// expanding from it is false.
    pub origin: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct Group {
    /// Repo-relative directory; doubles as the group's id.
    pub dir: String,
    /// The package's own name, when it declares one; the directory
    /// otherwise.
    pub label: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    /// Who imports this file (←).
    Callers,
    /// What this file imports, within the workspace (→).
    Dependencies,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct Edge {
    /// The importer.
    pub from: String,
    /// The imported.
    pub to: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct Graph {
    pub nodes: Vec<Node>,
    pub groups: Vec<Group>,
    pub edges: Vec<Edge>,
}

impl Graph {
    pub fn merge(&mut self, other: Graph) {
        for n in other.nodes {
            if !self.nodes.iter().any(|e| e.path == n.path) {
                self.nodes.push(n);
            }
        }
        for g in other.groups {
            if !self.groups.iter().any(|e| e.dir == g.dir) {
                self.groups.push(g);
            }
        }
        for e in other.edges {
            if !self.edges.contains(&e) {
                self.edges.push(e);
            }
        }
    }
}

/// The graph's starting nodes: the files the diff touched, with no edges
/// yet. Nothing is reached until something is expanded.
pub fn origin(root: &Path, changed: &[PathBuf], workspace: &Workspace) -> Graph {
    let mut g = Graph::default();
    for file in changed {
        let group = group_for(root, file, workspace);
        let group_dir = group.dir.clone();
        g.groups_push_unique(group);
        g.nodes.push(Node {
            path: to_slash(file),
            group: group_dir,
            origin: true,
        });
    }
    g
}

impl Graph {
    fn groups_push_unique(&mut self, group: Group) {
        if !self.groups.iter().any(|e| e.dir == group.dir) {
            self.groups.push(group);
        }
    }
}

/// Every file one hop from `target` in `direction`, as a graph fragment
/// ready to merge into what is already on screen.
pub fn reach(
    root: &Path,
    rev: &Rev,
    target: &Path,
    direction: Direction,
    workspace: &Workspace,
) -> Graph {
    match direction {
        Direction::Callers => callers_of(root, rev, target, workspace),
        Direction::Dependencies => dependencies_of(root, rev, target, workspace),
    }
}

/// Expands from `starting` by `hops` steps in both directions, folding
/// every reached file's fragment into one graph. Used by `--api graph` and
/// by tests that want the whole picture in one call; the TUI instead calls
/// [`reach`] one node at a time, on request.
pub fn build(
    root: &Path,
    rev: &Rev,
    starting: &[PathBuf],
    hops: u32,
    workspace: &Workspace,
) -> Graph {
    let mut g = origin(root, starting, workspace);
    let mut frontier: Vec<PathBuf> = starting.to_vec();
    for _ in 0..hops {
        let mut next = Vec::new();
        for file in &frontier {
            for direction in [Direction::Callers, Direction::Dependencies] {
                let fragment = reach(root, rev, file, direction, workspace);
                for n in &fragment.nodes {
                    let p = PathBuf::from(&n.path);
                    if !g.nodes.iter().any(|e| e.path == n.path) && !next.contains(&p) {
                        next.push(p);
                    }
                }
                g.merge(fragment);
            }
        }
        if next.is_empty() {
            break;
        }
        frontier = next;
    }
    g
}

fn callers_of(root: &Path, rev: &Rev, target: &Path, workspace: &Workspace) -> Graph {
    let mut g = Graph::default();
    let Some(target_pkg) = workspace.owning_package(target) else {
        return g;
    };

    let candidates: Vec<_> = workspace
        .packages
        .iter()
        .filter(|p| {
            p.dir == target_pkg.dir
                || target_pkg
                    .name
                    .as_deref()
                    .is_some_and(|name| p.deps.contains(name))
        })
        .collect();

    // A file can only import the target by naming it: its stem in a
    // relative path, or the package name when the target is the
    // package's entry. Files whose text has neither are skipped before
    // any specifier is resolved — resolution is the expensive part.
    let mut needles: Vec<String> = Vec::new();
    let lang = Lang::of(target);
    if let Some(stem) = target.file_stem().and_then(|s| s.to_str()) {
        // A Go import names the directory; so does a Python one
        // reaching a package's `__init__.py`.
        let dir_named = lang == Some(Lang::Go) || stem == "__init__";
        match target
            .parent()
            .and_then(|d| d.file_name())
            .and_then(|d| d.to_str())
        {
            Some(dir) if dir_named => needles.push(dir.to_string()),
            _ => needles.push(stem.split('.').next().unwrap_or(stem).to_string()),
        }
    }
    if target_pkg.entries.iter().any(|e| e == target)
        && let Some(name) = &target_pkg.name
    {
        needles.push(name.clone());
    }

    for pkg in candidates {
        let tsconfig = match pkg.kind {
            PackageKind::Js => tsconfig::load_nearest(root, &root.join(&pkg.dir)),
            _ => Default::default(),
        };
        for file in source_files(root, rev, &pkg.dir) {
            if file == target {
                continue;
            }
            let Some(text) = rev.read(root, &file) else {
                continue;
            };
            if !needles.iter().any(|n| text.contains(n.as_str())) {
                continue;
            }
            for spec in lang::import_specs(&file, &text) {
                if !needles.iter().any(|n| spec.contains(n.as_str())) {
                    continue;
                }
                let resolved = lang::resolve_files(&spec, &file, root, rev, workspace, &tsconfig);
                if resolved.iter().any(|r| r == target) {
                    let group = group_for(root, &file, workspace);
                    let node = Node {
                        path: to_slash(&file),
                        group: group.dir.clone(),
                        origin: false,
                    };
                    g.groups_push_unique(group);
                    if !g.nodes.iter().any(|n| n.path == node.path) {
                        g.nodes.push(node);
                    }
                    g.edges.push(Edge {
                        from: to_slash(&file),
                        to: to_slash(target),
                    });
                    break;
                }
            }
        }
    }
    g
}

fn dependencies_of(root: &Path, rev: &Rev, target: &Path, workspace: &Workspace) -> Graph {
    let mut g = Graph::default();
    let Some(text) = rev.read(root, target) else {
        return g;
    };
    let tsconfig = match Lang::of(target) {
        Some(Lang::Go | Lang::Python) => Default::default(),
        _ => tsconfig::load_nearest(root, root.join(target).parent().unwrap_or(root)),
    };
    let mut seen: HashSet<PathBuf> = HashSet::new();
    for spec in lang::import_specs(target, &text) {
        for resolved in lang::resolve_files(&spec, target, root, rev, workspace, &tsconfig) {
            if !seen.insert(resolved.clone()) {
                continue;
            }
            let group = group_for(root, &resolved, workspace);
            g.groups_push_unique(group.clone());
            g.nodes.push(Node {
                path: to_slash(&resolved),
                group: group.dir,
                origin: false,
            });
            g.edges.push(Edge {
                from: to_slash(target),
                to: to_slash(&resolved),
            });
        }
    }
    g
}

fn group_for(_root: &Path, file: &Path, workspace: &Workspace) -> Group {
    match workspace.owning_package(file) {
        Some(pkg) => Group {
            dir: to_slash(&pkg.dir),
            label: pkg.name.clone().unwrap_or_else(|| to_slash(&pkg.dir)),
        },
        None => {
            let dir = file.parent().unwrap_or(Path::new("")).to_path_buf();
            Group {
                label: to_slash(&dir),
                dir: to_slash(&dir),
            }
        }
    }
}

/// Every source file under a package directory, skipping the
/// directories a coarse scan has no business entering.
fn source_files(root: &Path, rev: &Rev, pkg_dir: &Path) -> Vec<PathBuf> {
    rev.files_under(root, pkg_dir)
        .iter()
        .filter(|f| {
            f.components().all(|c| {
                let s = c.as_os_str().to_string_lossy();
                !s.starts_with('.') && s != "node_modules" && s != "dist" && s != "target"
            })
        })
        .filter(|f| {
            let name = f
                .file_name()
                .map(|n| n.to_string_lossy())
                .unwrap_or_default();
            lang::SOURCE_EXTENSIONS
                .iter()
                .any(|ext| f.extension().is_some_and(|e| e == *ext))
                && !lang::is_test_name(f)
                && !name.ends_with(".spec.ts")
                && !name.ends_with(".test.ts")
                && !name.ends_with(".spec.tsx")
                && !name.ends_with(".test.tsx")
        })
        .cloned()
        .collect()
}

fn to_slash(p: &Path) -> String {
    p.to_string_lossy().replace('\\', "/")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    use crate::rev::Rev;

    /// One shared library package changed
    /// (`packages/backend/db-pool`), imported by the `client.ts` of
    /// three apps that each declare it as a dependency.
    fn shared_pool_fixture() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        fs::write(
            root.join("package.json"),
            r#"{ "name": "root", "private": true, "workspaces": ["apps/*/database/*", "packages/backend/*"] }"#,
        )
        .unwrap();

        fs::create_dir_all(root.join("packages/backend/db-pool/src")).unwrap();
        fs::write(
            root.join("packages/backend/db-pool/package.json"),
            r#"{ "name": "@acme-lib/db-pool", "main": "src/index.ts" }"#,
        )
        .unwrap();
        fs::write(
            root.join("packages/backend/db-pool/src/index.ts"),
            "export function createPool() {}\n",
        )
        .unwrap();

        for app in ["shop", "billing", "auth"] {
            let app_dir = format!("apps/{app}/database/{app}");
            fs::create_dir_all(root.join(&app_dir).join("src/shared")).unwrap();
            fs::write(
                root.join(&app_dir).join("package.json"),
                format!(
                    r#"{{ "name": "@acme-app/{app}-database", "dependencies": {{ "@acme-lib/db-pool": "workspace:*" }} }}"#
                ),
            )
            .unwrap();
            fs::write(
                root.join(&app_dir).join("src/shared/client.ts"),
                "import { createPool } from '@acme-lib/db-pool';\nexport const pool = createPool();\n",
            )
            .unwrap();
        }
        dir
    }

    #[test]
    fn callers_of_the_changed_package_are_the_three_client_files() {
        let dir = shared_pool_fixture();
        let root = dir.path();
        let workspace = crate::workspace::discover(root, &Rev::working()).unwrap();
        let target = PathBuf::from("packages/backend/db-pool/src/index.ts");

        let fragment = reach(
            root,
            &Rev::working(),
            &target,
            Direction::Callers,
            &workspace,
        );
        let mut paths: Vec<&str> = fragment.nodes.iter().map(|n| n.path.as_str()).collect();
        paths.sort();
        assert_eq!(
            paths,
            vec![
                "apps/auth/database/auth/src/shared/client.ts",
                "apps/billing/database/billing/src/shared/client.ts",
                "apps/shop/database/shop/src/shared/client.ts",
            ]
        );
        assert_eq!(fragment.edges.len(), 3);
        for edge in &fragment.edges {
            assert_eq!(edge.to, "packages/backend/db-pool/src/index.ts");
        }

        let groups: HashSet<&str> = fragment.groups.iter().map(|g| g.label.as_str()).collect();
        assert!(groups.contains("@acme-app/shop-database"));
        assert!(groups.contains("@acme-app/billing-database"));
        assert!(groups.contains("@acme-app/auth-database"));
    }

    #[test]
    fn origin_marks_the_changed_file_and_groups_it_by_package() {
        let dir = shared_pool_fixture();
        let root = dir.path();
        let workspace = crate::workspace::discover(root, &Rev::working()).unwrap();
        let target = PathBuf::from("packages/backend/db-pool/src/index.ts");

        let g = origin(root, std::slice::from_ref(&target), &workspace);
        assert_eq!(g.nodes.len(), 1);
        assert!(g.nodes[0].origin);
        assert_eq!(g.nodes[0].group, "packages/backend/db-pool");
        assert_eq!(g.groups[0].label, "@acme-lib/db-pool");
    }

    #[test]
    fn dependencies_of_a_client_file_reach_back_into_the_pool_package() {
        let dir = shared_pool_fixture();
        let root = dir.path();
        let workspace = crate::workspace::discover(root, &Rev::working()).unwrap();
        let target = PathBuf::from("apps/shop/database/shop/src/shared/client.ts");

        let fragment = reach(
            root,
            &Rev::working(),
            &target,
            Direction::Dependencies,
            &workspace,
        );
        assert_eq!(fragment.nodes.len(), 1);
        assert_eq!(
            fragment.nodes[0].path,
            "packages/backend/db-pool/src/index.ts"
        );
    }

    #[test]
    fn one_hop_build_from_the_package_reaches_all_three_callers() {
        let dir = shared_pool_fixture();
        let root = dir.path();
        let workspace = crate::workspace::discover(root, &Rev::working()).unwrap();
        let target = PathBuf::from("packages/backend/db-pool/src/index.ts");

        let g = build(root, &Rev::working(), &[target], 1, &workspace);
        // 1 origin + 3 callers
        assert_eq!(g.nodes.len(), 4);
        assert_eq!(g.edges.len(), 3);
    }

    /// The reason this whole layer takes a `Rev`: a caller one hop out
    /// found by reading straight from a git commit, working tree never
    /// touched — deletes every file on disk right after committing them,
    /// so a fallback to the filesystem would find nothing at all.
    #[test]
    fn callers_are_found_from_a_git_commit_with_no_working_tree_to_fall_back_to() {
        let dir = shared_pool_fixture();
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
        run(&["commit", "-q", "-m", "fixture"]);
        let out = std::process::Command::new("git")
            .current_dir(root)
            .args(["rev-parse", "HEAD"])
            .output()
            .unwrap();
        let sha = String::from_utf8_lossy(&out.stdout).trim().to_string();

        for entry in fs::read_dir(root).unwrap() {
            let entry = entry.unwrap();
            if entry.file_name() == ".git" {
                continue;
            }
            if entry.file_type().unwrap().is_dir() {
                fs::remove_dir_all(entry.path()).unwrap();
            } else {
                fs::remove_file(entry.path()).unwrap();
            }
        }

        let rev = Rev::commit(sha);
        let workspace = crate::workspace::discover(root, &rev).unwrap();
        let target = PathBuf::from("packages/backend/db-pool/src/index.ts");
        let fragment = reach(root, &rev, &target, Direction::Callers, &workspace);
        assert_eq!(fragment.nodes.len(), 3, "{fragment:#?}");
    }
}
