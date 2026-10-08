//! Turns one import specifier, as written in one file, into the
//! repo-relative file it points at — trying a relative path, a tsconfig
//! `paths` alias, and a workspace package name's entry, in that order,
//! same as a bundler would.

use std::path::{Path, PathBuf};

use crate::imports::RESOLVABLE_EXTENSIONS;
use crate::rev::Rev;
use crate::tsconfig::TsConfig;
use crate::workspace::Workspace;

/// `from_file` and the return value are both repo-relative.
pub fn resolve(
    specifier: &str,
    from_file: &Path,
    root: &Path,
    rev: &Rev,
    workspace: &Workspace,
    tsconfig: &TsConfig,
) -> Option<PathBuf> {
    if specifier.starts_with('.') {
        let from_dir = from_file.parent().unwrap_or(Path::new(""));
        return exists_with_extensions(&normalize(&from_dir.join(specifier)), root, rev);
    }
    // SvelteKit's `$lib` is `<package>/src/lib`, declared in a generated
    // tsconfig that is not in the repository.
    if let Some(rest) = specifier.strip_prefix("$lib/")
        && let Some(pkg) = workspace.owning_package(from_file)
    {
        return exists_with_extensions(&normalize(&pkg.dir.join("src/lib").join(rest)), root, rev);
    }
    for candidate in tsconfig.resolve_alias(specifier) {
        let rel = candidate
            .strip_prefix(root)
            .map(Path::to_path_buf)
            .unwrap_or(candidate);
        if let Some(found) = exists_with_extensions(&normalize(&rel), root, rev) {
            return Some(found);
        }
    }
    // Bare specifier naming a workspace package, possibly with a subpath
    // (`@scope/pkg/sub`). Only the package's declared entry is resolved —
    // an arbitrary subpath is a fine-layer concern.
    let (pkg_name, subpath) = split_subpath(specifier);
    let pkg = workspace.package_by_name(pkg_name)?;
    if let Some(sub) = subpath {
        return exists_with_extensions(&normalize(&pkg.dir.join(sub)), root, rev);
    }
    pkg.entries
        .iter()
        .find_map(|e| exists_with_extensions(e, root, rev))
}

fn split_subpath(specifier: &str) -> (&str, Option<&str>) {
    // A scoped package name is two segments (`@scope/name`); anything after
    // that is a subpath. An unscoped name is one segment.
    let mut parts = specifier.splitn(if specifier.starts_with('@') { 3 } else { 2 }, '/');
    let first = parts.next().unwrap_or(specifier);
    let name = match specifier.starts_with('@') {
        true => match parts.next() {
            Some(second) => &specifier[..first.len() + 1 + second.len()],
            None => specifier,
        },
        false => first,
    };
    let rest = specifier[name.len()..].trim_start_matches('/');
    (name, (!rest.is_empty()).then_some(rest))
}

fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for part in path.components() {
        match part {
            std::path::Component::ParentDir => {
                out.pop();
            }
            std::path::Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

fn exists_with_extensions(rel: &Path, root: &Path, rev: &Rev) -> Option<PathBuf> {
    if rev.exists(root, rel) {
        return Some(rel.to_path_buf());
    }
    // `./store.svelte` may be `store.svelte.ts` (a Svelte-runes module).
    if rel.extension().is_some() {
        for ext in ["ts", "js"] {
            let appended = PathBuf::from(format!("{}.{ext}", rel.display()));
            if rev.exists(root, &appended) {
                return Some(appended);
            }
        }
    }
    if rel.extension().is_none() {
        for ext in RESOLVABLE_EXTENSIONS {
            let with_ext = rel.with_extension(ext);
            if rev.exists(root, &with_ext) {
                return Some(with_ext);
            }
        }
        for ext in RESOLVABLE_EXTENSIONS {
            let index = rel.join(format!("index.{ext}"));
            if rev.exists(root, &index) {
                return Some(index);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_scoped_and_unscoped_subpaths() {
        assert_eq!(
            split_subpath("@acme-lib/db-pool"),
            ("@acme-lib/db-pool", None)
        );
        assert_eq!(
            split_subpath("@acme-lib/db-pool/testing"),
            ("@acme-lib/db-pool", Some("testing"))
        );
        assert_eq!(
            split_subpath("lodash/debounce"),
            ("lodash", Some("debounce"))
        );
    }

    #[test]
    fn normalizes_dot_dot_segments() {
        assert_eq!(
            normalize(Path::new("apps/shop/database/shop/src/../shared/client")),
            PathBuf::from("apps/shop/database/shop/shared/client")
        );
    }
}
