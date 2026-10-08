//! The same hub shape again, one level coarser than file: the focus is a
//! package, callers are packages that declare it as a dependency,
//! callees are packages it depends on. Pure package.json metadata —
//! `Workspace` already has it, so unlike `file_hub` this needs no import
//! scanning at all.

use std::path::{Path, PathBuf};

use crate::hub::{Hub, Node, Status};
use crate::workspace::Workspace;

pub fn build(base_ws: &Workspace, head_ws: &Workspace, focus_dir: &Path) -> Option<Hub> {
    let base_pkg = base_ws.packages.iter().find(|p| p.dir == focus_dir);
    let head_pkg = head_ws.packages.iter().find(|p| p.dir == focus_dir);
    if base_pkg.is_none() && head_pkg.is_none() {
        return None;
    }

    let status = match (base_pkg, head_pkg) {
        (Some(b), Some(h)) => {
            let mut bd: Vec<&String> = b.deps.iter().collect();
            let mut hd: Vec<&String> = h.deps.iter().collect();
            bd.sort();
            hd.sort();
            if bd == hd {
                Status::Unchanged
            } else {
                Status::Changed
            }
        }
        (None, Some(_)) => Status::Added,
        (Some(_), None) => Status::Removed,
        (None, None) => unreachable!(),
    };

    let label = head_pkg
        .or(base_pkg)
        .and_then(|p| p.name.clone())
        .unwrap_or_else(|| display(focus_dir));

    Some(Hub {
        focus: Node {
            label,
            status,
            drillable: true,
        },
        callees: diff_names(base_pkg.map(|p| &p.deps), head_pkg.map(|p| &p.deps)),
        callers: diff_callers(base_ws, head_ws, base_pkg, head_pkg),
    })
}

/// Where drilling into a package-level node should land: that package's
/// directory, resolved by name against whichever workspace still has it.
pub fn dir_of(base_ws: &Workspace, head_ws: &Workspace, name: &str) -> Option<PathBuf> {
    head_ws
        .package_by_name(name)
        .or_else(|| base_ws.package_by_name(name))
        .map(|p| p.dir.clone())
}

fn diff_names(
    base: Option<&std::collections::HashSet<String>>,
    head: Option<&std::collections::HashSet<String>>,
) -> Vec<Node> {
    let mut out = Vec::new();
    let empty = std::collections::HashSet::new();
    let (base, head) = (base.unwrap_or(&empty), head.unwrap_or(&empty));
    let mut names: Vec<&String> = base.union(head).collect();
    names.sort();
    for name in names {
        let status = match (base.contains(name), head.contains(name)) {
            (true, true) => Status::Unchanged,
            (false, true) => Status::Added,
            (true, false) => Status::Removed,
            (false, false) => unreachable!(),
        };
        out.push(Node {
            label: name.clone(),
            status,
            drillable: true,
        });
    }
    out
}

fn diff_callers(
    base_ws: &Workspace,
    head_ws: &Workspace,
    base_pkg: Option<&crate::workspace::Package>,
    head_pkg: Option<&crate::workspace::Package>,
) -> Vec<Node> {
    let mut out = Vec::new();
    let base_name = base_pkg.and_then(|p| p.name.as_deref());
    let head_name = head_pkg.and_then(|p| p.name.as_deref());

    let mut names = std::collections::BTreeSet::new();
    if let Some(n) = base_name {
        names.extend(
            base_ws
                .packages
                .iter()
                .filter(|p| p.deps.contains(n))
                .filter_map(|p| p.name.clone()),
        );
    }
    if let Some(n) = head_name {
        names.extend(
            head_ws
                .packages
                .iter()
                .filter(|p| p.deps.contains(n))
                .filter_map(|p| p.name.clone()),
        );
    }

    for name in names {
        let in_base = base_name.is_some_and(|n| {
            base_ws
                .packages
                .iter()
                .any(|p| p.name.as_deref() == Some(&name) && p.deps.contains(n))
        });
        let in_head = head_name.is_some_and(|n| {
            head_ws
                .packages
                .iter()
                .any(|p| p.name.as_deref() == Some(&name) && p.deps.contains(n))
        });
        let status = match (in_base, in_head) {
            (true, true) => Status::Unchanged,
            (false, true) => Status::Added,
            (true, false) => Status::Removed,
            (false, false) => continue,
        };
        out.push(Node {
            label: name,
            status,
            drillable: true,
        });
    }
    out
}

fn display(dir: &Path) -> String {
    if dir.as_os_str().is_empty() {
        "(root)".to_string()
    } else {
        dir.display().to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn fixture(root: &Path) {
        fs::write(
            root.join("package.json"),
            r#"{ "name": "root", "private": true, "workspaces": ["apps/*", "packages/*"] }"#,
        )
        .unwrap();
        fs::create_dir_all(root.join("packages/db-pool")).unwrap();
        fs::write(
            root.join("packages/db-pool/package.json"),
            r#"{ "name": "@lib/db-pool" }"#,
        )
        .unwrap();
        for app in ["shop", "auth"] {
            fs::create_dir_all(root.join(format!("apps/{app}"))).unwrap();
            fs::write(
                root.join(format!("apps/{app}/package.json")),
                format!(r#"{{ "name": "@app/{app}", "dependencies": {{ "@lib/db-pool": "workspace:*" }} }}"#),
            )
            .unwrap();
        }
    }

    #[test]
    fn a_package_that_stops_depending_shows_as_a_removed_caller() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fixture(root);
        let base_ws = crate::workspace::discover(root, &crate::rev::Rev::working()).unwrap();

        // HEAD: auth no longer depends on db-pool.
        fs::write(
            root.join("apps/auth/package.json"),
            r#"{ "name": "@app/auth" }"#,
        )
        .unwrap();
        let head_ws = crate::workspace::discover(root, &crate::rev::Rev::working()).unwrap();

        let hub = build(&base_ws, &head_ws, Path::new("packages/db-pool")).unwrap();
        assert_eq!(hub.focus.status, Status::Unchanged);
        let mut callers: Vec<(&str, Status)> = hub
            .callers
            .iter()
            .map(|n| (n.label.as_str(), n.status))
            .collect();
        callers.sort_by_key(|(l, _)| *l);
        assert_eq!(
            callers,
            vec![
                ("@app/auth", Status::Removed),
                ("@app/shop", Status::Unchanged)
            ]
        );
    }
}
