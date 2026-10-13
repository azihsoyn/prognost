//! The languages prognost reads, and what differs between them: how a
//! file's functions and calls are extracted, how its imports are named
//! and bound, and how an import resolves to files in the repository.
//! Everything above this — alignment, the call graph, plans, rules —
//! is the same for every language.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::bindings::Import;
use crate::rev::Rev;
use crate::ts_extract::TsFunction;
use crate::tsconfig::TsConfig;
use crate::workspace::Workspace;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Lang {
    /// TypeScript, JavaScript and Svelte's `<script>` blocks.
    TypeScript,
    Go,
    Python,
}

/// Every file extension a language here reads.
pub const SOURCE_EXTENSIONS: &[&str] = &[
    "ts", "tsx", "js", "jsx", "mjs", "cjs", "mts", "cts", "svelte", "go", "py",
];

impl Lang {
    pub fn of(path: &Path) -> Option<Lang> {
        match path.extension()?.to_str()? {
            "go" => Some(Lang::Go),
            "py" => Some(Lang::Python),
            e if crate::imports::RESOLVABLE_EXTENSIONS.contains(&e) => Some(Lang::TypeScript),
            _ => None,
        }
    }
}

/// A file some language here reads.
pub fn is_source(path: &Path) -> bool {
    Lang::of(path).is_some()
}

/// A test by its language's naming convention: `_test.go`;
/// `test_*.py`, `*_test.py`, `conftest.py`. (TypeScript's `.spec.`
/// and `.test.` are matched where tests are filtered.)
pub fn is_test_name(path: &Path) -> bool {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy())
        .unwrap_or_default();
    match Lang::of(path) {
        Some(Lang::Go) => name.ends_with("_test.go"),
        Some(Lang::Python) => {
            (name.starts_with("test_") || name.ends_with("_test.py") || name == "conftest.py")
                && name.ends_with(".py")
        }
        _ => false,
    }
}

pub fn extract_for_path(path: &Path, source: &str) -> anyhow::Result<Vec<TsFunction>> {
    match Lang::of(path) {
        Some(Lang::Go) => crate::golang::extract(source),
        Some(Lang::Python) => crate::python::extract(source),
        _ => crate::ts_extract::extract_for_path(path, source),
    }
}

/// The specifiers a file imports, as written.
pub fn import_specs(path: &Path, source: &str) -> Vec<String> {
    match Lang::of(path) {
        Some(Lang::Go) => crate::golang::import_specs(source),
        Some(Lang::Python) => crate::python::import_specs(source),
        _ => crate::imports::scan(source),
    }
}

/// Every name a file imports, by the name it is used under.
pub fn bindings(path: &Path, source: &str) -> HashMap<String, Import> {
    match Lang::of(path) {
        Some(Lang::Go) => crate::golang::bindings(source),
        Some(Lang::Python) => crate::python::bindings(source),
        _ => crate::bindings::bindings(source),
    }
}

/// The repository files one specifier in `from_file` names: one file
/// for a TypeScript or Python import, every file of the package for a
/// Go one. Empty for anything outside the repository. `tsconfig` is
/// read only for TypeScript.
pub fn resolve_files(
    spec: &str,
    from_file: &Path,
    root: &Path,
    rev: &Rev,
    workspace: &Workspace,
    tsconfig: &TsConfig,
) -> Vec<PathBuf> {
    match Lang::of(from_file) {
        Some(Lang::Go) => crate::golang::resolve(spec, workspace)
            .map(|dir| crate::golang::package_files(&dir, root, rev))
            .unwrap_or_default(),
        Some(Lang::Python) => crate::python::resolve(spec, from_file, root, rev, workspace)
            .into_iter()
            .collect(),
        _ => crate::resolve::resolve(spec, from_file, root, rev, workspace, tsconfig)
            .into_iter()
            .collect(),
    }
}

/// What a call into something outside the repository is labelled by:
/// the npm package, the Go import path, the top-level Python package.
pub fn package_name(path: &Path, spec: &str) -> String {
    match Lang::of(path) {
        Some(Lang::Go) => spec.to_string(),
        Some(Lang::Python) => crate::python::package_name(spec),
        _ => crate::bindings::package_name(spec),
    }
}

/// A cheap text check that `file`'s source defines a top-level `name`
/// another file could import, before paying for a parse.
pub fn defines_textually(path: &Path, source: &str, name: &str) -> bool {
    let n = regex::escape(name);
    let pattern = match Lang::of(path) {
        Some(Lang::Go) => format!(r"(?m)^func\s+(?:\([^)]*\)\s*)?{n}\b"),
        Some(Lang::Python) => format!(
            r"(?m)^(?:async\s+def|def|class)\s+{n}\b|^{n}\s*=|^from\s+\S+\s+import\s+.*\b{n}\b"
        ),
        _ => format!(
            r"\bexport\s+(?:default\s+)?(?:async\s+)?(?:const|let|var|function|class)\s+{n}\b|\bexport\s+(?:const|let|var)?\s*\{{[^}}]*\b{n}\b"
        ),
    };
    regex::Regex::new(&pattern).is_ok_and(|re| re.is_match(source))
}
