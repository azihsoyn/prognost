//! Discovers the packages in a repo and what each one depends on: JS/TS
//! packages by their package.json, Go packages (one per directory) under
//! each go.mod, Python projects by their pyproject.toml or setup.py.
//!
//! This is the "declared deps" half of the coarse layer, used to narrow
//! which packages are worth scanning for import statements at all.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::rev::Rev;

/// Which ecosystem a package belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PackageKind {
    Js,
    Go,
    Python,
}

impl PackageKind {
    /// The kind whose packages own a source file with this path, `None`
    /// for anything that isn't Go or Python source (TypeScript, and
    /// non-source files, which any kind may own).
    fn of_source(file: &Path) -> Option<PackageKind> {
        match file.extension().and_then(|e| e.to_str()) {
            Some("go") => Some(PackageKind::Go),
            Some("py" | "pyi") => Some(PackageKind::Python),
            Some(e) if crate::imports::RESOLVABLE_EXTENSIONS.contains(&e) => Some(PackageKind::Js),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Package {
    pub kind: PackageKind,
    /// The `name` field in package.json, a Go package's import path, a
    /// Python project's name.
    pub name: Option<String>,
    /// Repo-relative directory this package lives in.
    pub dir: PathBuf,
    /// Names of every workspace dependency this package declares
    /// (dependencies + devDependencies + peerDependencies), regardless of
    /// whether the named package is actually part of this workspace.
    pub deps: HashSet<String>,
    /// Files this package resolves a bare `import "<name>"` to, most
    /// specific export first. Usually one entry (`main`/`exports["."]`),
    /// but a package with no entry field at all falls back to a short list
    /// of conventional guesses.
    pub entries: Vec<PathBuf>,
}

pub struct Workspace {
    pub packages: Vec<Package>,
}

impl Workspace {
    /// The package a file belongs to: the nearest ancestor directory that
    /// holds a package among the discovered packages — one of the
    /// source file's own kind when there is one (a TypeScript file in a
    /// Go module's tree belongs to its package.json, not the module).
    pub fn owning_package(&self, file: &Path) -> Option<&Package> {
        // Reversed, so that of two packages in one directory (a
        // package.json beside a go.mod) the first discovered wins.
        let deepest = |kind: Option<PackageKind>| {
            self.packages
                .iter()
                .rev()
                .filter(|p| kind.is_none_or(|k| p.kind == k))
                .filter(|p| file.starts_with(&p.dir))
                .max_by_key(|p| p.dir.components().count())
        };
        match PackageKind::of_source(file) {
            Some(kind) => deepest(Some(kind)).or_else(|| deepest(None)),
            None => deepest(None),
        }
    }

    /// The package of this kind named `name`.
    pub fn package_of_kind(&self, kind: PackageKind, name: &str) -> Option<&Package> {
        self.packages
            .iter()
            .find(|p| p.kind == kind && p.name.as_deref() == Some(name))
    }

    pub fn package_by_name(&self, name: &str) -> Option<&Package> {
        self.packages
            .iter()
            .find(|p| p.name.as_deref() == Some(name))
    }
}

#[derive(Deserialize, Default)]
struct PackageJson {
    name: Option<String>,
    #[serde(default)]
    dependencies: std::collections::HashMap<String, String>,
    #[serde(rename = "devDependencies", default)]
    dev_dependencies: std::collections::HashMap<String, String>,
    #[serde(rename = "peerDependencies", default)]
    peer_dependencies: std::collections::HashMap<String, String>,
    main: Option<String>,
    module: Option<String>,
    types: Option<String>,
    #[serde(default)]
    exports: Option<serde_json::Value>,
    #[serde(default)]
    workspaces: Option<serde_json::Value>,
}

/// Finds every package.json under the globs a root package.json or
/// pnpm-workspace.yaml declares, and reads each one just enough to know its
/// name, its declared deps, and where a bare import into it lands.
pub fn discover(root: &Path, rev: &Rev) -> anyhow::Result<Workspace> {
    let globs = workspace_globs(root, rev)?;
    let all_files = rev.list_files(root);
    let mut dirs: Vec<PathBuf> = Vec::new();
    for pattern in &globs {
        let Ok(matcher) = glob::Pattern::new(&format!("{pattern}/package.json")) else {
            continue;
        };
        for f in &all_files {
            if matcher.matches_path(f)
                && let Some(dir) = f.parent()
            {
                dirs.push(dir.to_path_buf());
            }
        }
    }
    // The root itself is a package too, when it has its own package.json.
    if rev.exists(root, Path::new("package.json")) {
        dirs.push(PathBuf::new());
    }
    dirs.sort();
    dirs.dedup();

    let mut packages = Vec::new();
    for dir in dirs {
        let Some(text) = rev.read(root, &dir.join("package.json")) else {
            continue;
        };
        let Ok(pkg) = serde_json::from_str::<PackageJson>(&text) else {
            continue;
        };
        let entries = entry_candidates(&dir, &pkg);
        let mut deps: HashSet<String> = HashSet::new();
        deps.extend(pkg.dependencies.into_keys());
        deps.extend(pkg.dev_dependencies.into_keys());
        deps.extend(pkg.peer_dependencies.into_keys());
        packages.push(Package {
            kind: PackageKind::Js,
            name: pkg.name,
            dir,
            deps,
            entries,
        });
    }
    packages.extend(go_packages(root, rev, &all_files));
    packages.extend(python_packages(root, rev, &all_files));
    Ok(Workspace { packages })
}

/// A directory no package scan should enter.
fn skipped(file: &Path) -> bool {
    file.components().any(|c| {
        let s = c.as_os_str().to_string_lossy();
        matches!(
            s.as_ref(),
            "vendor" | "testdata" | "node_modules" | "__pycache__" | "site-packages"
        ) || (s.starts_with('.') && s.len() > 1)
    })
}

/// One package per directory of non-test `.go` files under a `go.mod`,
/// named by its import path; its deps are the import paths its files
/// name. A nested module owns its own tree.
fn go_packages(root: &Path, rev: &Rev, all_files: &[PathBuf]) -> Vec<Package> {
    let mut modules: Vec<(PathBuf, String)> = all_files
        .iter()
        .filter(|f| f.file_name().is_some_and(|n| n == "go.mod") && !skipped(f))
        .filter_map(|f| {
            let dir = f.parent()?.to_path_buf();
            let module = crate::golang::module_path(&rev.read(root, f)?)?;
            Some((dir, module))
        })
        .collect();
    if modules.is_empty() {
        return Vec::new();
    }
    modules.sort_by_key(|(d, _)| std::cmp::Reverse(d.components().count()));
    let mut by_dir: std::collections::BTreeMap<PathBuf, HashSet<String>> = Default::default();
    for f in all_files {
        let name = f
            .file_name()
            .map(|n| n.to_string_lossy())
            .unwrap_or_default();
        if !name.ends_with(".go") || name.ends_with("_test.go") || skipped(f) {
            continue;
        }
        let dir = f.parent().unwrap_or(Path::new("")).to_path_buf();
        let deps = by_dir.entry(dir).or_default();
        if let Some(src) = rev.read(root, f) {
            deps.extend(crate::golang::import_specs(&src));
        }
    }
    by_dir
        .into_iter()
        .filter_map(|(dir, deps)| {
            let (mdir, module) = modules.iter().find(|(m, _)| dir.starts_with(m))?;
            let rel = dir.strip_prefix(mdir).ok()?;
            let name = if rel.as_os_str().is_empty() {
                module.clone()
            } else {
                format!("{module}/{}", rel.to_string_lossy().replace('\\', "/"))
            };
            Some(Package {
                kind: PackageKind::Go,
                name: Some(name),
                dir,
                deps,
                entries: Vec::new(),
            })
        })
        .collect()
}

#[derive(Deserialize, Default)]
struct PyProject {
    project: Option<PyProjectTable>,
    tool: Option<PyTool>,
}

#[derive(Deserialize, Default)]
struct PyProjectTable {
    name: Option<String>,
    #[serde(default)]
    dependencies: Vec<String>,
}

#[derive(Deserialize, Default)]
struct PyTool {
    poetry: Option<PoetryTable>,
}

#[derive(Deserialize, Default)]
struct PoetryTable {
    name: Option<String>,
    #[serde(default)]
    dependencies: std::collections::HashMap<String, toml::Value>,
}

/// A distribution name as declared, without its extras: `shop-db[pg]`
/// → `shop-db`.
fn normalize_dist(name: &str) -> String {
    name.trim()
        .split('[')
        .next()
        .unwrap_or("")
        .trim()
        .to_string()
}

/// One package per `pyproject.toml` (or `setup.py`/`setup.cfg` without
/// one), named by the project name; deps are the distributions it
/// declares.
fn python_packages(root: &Path, rev: &Rev, all_files: &[PathBuf]) -> Vec<Package> {
    let mut dirs: Vec<PathBuf> = all_files
        .iter()
        .filter(|f| {
            f.file_name()
                .is_some_and(|n| n == "pyproject.toml" || n == "setup.py" || n == "setup.cfg")
                && !skipped(f)
        })
        .filter_map(|f| f.parent().map(Path::to_path_buf))
        .collect();
    dirs.sort();
    dirs.dedup();
    let dist_name = |dir: &Path| {
        dir.file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| {
                root.file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default()
            })
    };
    dirs.into_iter()
        .map(|dir| {
            let mut name = None;
            let mut deps = HashSet::new();
            if let Some(text) = rev.read(root, &dir.join("pyproject.toml"))
                && let Ok(py) = toml::from_str::<PyProject>(&text)
            {
                if let Some(p) = py.project {
                    name = p.name;
                    for d in p.dependencies {
                        let end = d
                            .find(|c: char| !(c.is_alphanumeric() || "-_.".contains(c)))
                            .unwrap_or(d.len());
                        deps.insert(normalize_dist(&d[..end]));
                    }
                }
                if let Some(poetry) = py.tool.and_then(|t| t.poetry) {
                    name = name.or(poetry.name);
                    deps.extend(
                        poetry
                            .dependencies
                            .into_keys()
                            .filter(|k| k != "python")
                            .map(|k| normalize_dist(&k)),
                    );
                }
            }
            let name = normalize_dist(&name.unwrap_or_else(|| dist_name(&dir)));
            Package {
                kind: PackageKind::Python,
                name: Some(name),
                dir,
                deps,
                entries: Vec::new(),
            }
        })
        .collect()
}

/// Where a bare `import "<this package>"` lands, repo-relative. `exports`
/// is read only for its `"."` entry — subpath exports are a fine-layer
/// concern (LSP call hierarchy will get those right; the coarse layer only
/// needs to catch the common "changed the package's front door" case).
fn entry_candidates(dir: &Path, pkg: &PackageJson) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Some(serde_json::Value::Object(map)) = &pkg.exports {
        if let Some(dot) = map.get(".") {
            collect_export_strings(dot, &mut out);
        }
    } else if let Some(serde_json::Value::String(s)) = pkg.exports.as_ref() {
        out.push(s.clone());
    }
    for f in [&pkg.main, &pkg.module, &pkg.types].into_iter().flatten() {
        out.push(f.clone());
    }
    let mut resolved: Vec<PathBuf> = out.iter().map(|s| dir.join(strip_leading(s))).collect();
    if resolved.is_empty() {
        // No entry field at all: guess the conventional ones so a bare
        // import into this package is not silently dropped.
        for guess in ["src/index.ts", "src/index.tsx", "index.ts", "index.js"] {
            resolved.push(dir.join(guess));
        }
    }
    resolved
}

fn collect_export_strings(v: &serde_json::Value, out: &mut Vec<String>) {
    match v {
        serde_json::Value::String(s) => out.push(s.clone()),
        serde_json::Value::Object(map) => {
            for key in ["import", "require", "default", "types"] {
                if let Some(inner) = map.get(key) {
                    collect_export_strings(inner, out);
                }
            }
        }
        _ => {}
    }
}

fn strip_leading(s: &str) -> &str {
    s.strip_prefix("./").unwrap_or(s)
}

/// The globs a root declares for its workspace, from either package.json
/// `workspaces` (npm/yarn) or pnpm-workspace.yaml.
fn workspace_globs(root: &Path, rev: &Rev) -> anyhow::Result<Vec<String>> {
    if let Some(text) = rev.read(root, Path::new("pnpm-workspace.yaml")) {
        return Ok(parse_pnpm_workspace_yaml(&text));
    }
    if let Some(text) = rev.read(root, Path::new("package.json"))
        && let Ok(pkg) = serde_json::from_str::<PackageJson>(&text)
    {
        return Ok(match pkg.workspaces {
            Some(serde_json::Value::Array(items)) => items
                .into_iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect(),
            Some(serde_json::Value::Object(map)) => map
                .get("packages")
                .and_then(|v| v.as_array())
                .into_iter()
                .flatten()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect(),
            _ => Vec::new(),
        });
    }
    Ok(Vec::new())
}

/// A deliberately small reader for the one shape pnpm-workspace.yaml
/// actually takes: a top-level `packages:` list of glob strings. Full YAML
/// is not needed for that, and not worth a dependency.
fn parse_pnpm_workspace_yaml(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut in_packages = false;
    for line in text.lines() {
        let trimmed = line.trim_start();
        if !in_packages {
            if trimmed.starts_with("packages:") {
                in_packages = true;
            }
            continue;
        }
        if let Some(item) = trimmed.strip_prefix("- ") {
            let item = item.trim().trim_matches(['"', '\'']);
            out.push(item.to_string());
        } else if !trimmed.is_empty() && !line.starts_with(' ') {
            break;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_pnpm_workspace_glob_list() {
        let yaml = "packages:\n  - apps/*/database/*\n  - packages/backend/*\n";
        assert_eq!(
            parse_pnpm_workspace_yaml(yaml),
            vec![
                "apps/*/database/*".to_string(),
                "packages/backend/*".to_string()
            ]
        );
    }

    #[test]
    fn owning_package_is_the_deepest_ancestor() {
        let ws = Workspace {
            packages: vec![
                Package {
                    kind: PackageKind::Js,
                    name: Some("root".into()),
                    dir: PathBuf::from(""),
                    deps: HashSet::new(),
                    entries: vec![],
                },
                Package {
                    kind: PackageKind::Js,
                    name: Some("nested".into()),
                    dir: PathBuf::from("packages/backend/db-pool"),
                    deps: HashSet::new(),
                    entries: vec![],
                },
            ],
        };
        let owner = ws.owning_package(Path::new("packages/backend/db-pool/src/index.ts"));
        assert_eq!(owner.and_then(|p| p.name.as_deref()), Some("nested"));
    }
}
