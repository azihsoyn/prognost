//! What each imported name in a file refers to — the specifier it came
//! from and how it was bound (`import * as db`, `import { eq }`, `import
//! X`) — plus the `export * as name from` re-exports a barrel file
//! offers. Together they let a call like `db.someDomain.find` be walked
//! from the binding, through the barrel, to the file that defines it.
//! Text scanning, same as [`crate::imports`]: a false match still has to
//! resolve to a real file and a real export before it shows up anywhere.

use std::collections::HashMap;
use std::sync::LazyLock;

use regex::Regex;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Binding {
    /// `import * as name from '…'`
    Namespace,
    /// `import { imported as name } from '…'` — holds the imported name.
    Named(String),
    /// `import name from '…'`
    Default,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Import {
    pub specifier: String,
    pub binding: Binding,
}

static NAMESPACE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"\bimport\s+(?:type\s+)?\*\s+as\s+(\w+)\s+from\s*['"]([^'"\n]+)['"]"#)
        .expect("static regex")
});
static NAMED: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"\bimport\s+(?:type\s+)?(?:(\w+)\s*,\s*)?\{([^}]*)\}\s*from\s*['"]([^'"\n]+)['"]"#)
        .expect("static regex")
});
static DEFAULT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"\bimport\s+(?:type\s+)?(\w+)\s+from\s*['"]([^'"\n]+)['"]"#).expect("static regex")
});
static REEXPORT_FROM: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"\bexport\s+(?:\*|\{[^}]*\})\s+from\s*['"]([^'"\n]+)['"]"#).expect("static regex")
});
static REEXPORT_NS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"\bexport\s+\*\s+as\s+(\w+)\s+from\s*['"]([^'"\n]+)['"]"#).expect("static regex")
});

/// Every binding a file's import statements introduce, by local name.
pub fn bindings(source: &str) -> HashMap<String, Import> {
    let mut out = HashMap::new();
    for c in NAMESPACE.captures_iter(source) {
        out.insert(
            c[1].to_string(),
            Import {
                specifier: c[2].to_string(),
                binding: Binding::Namespace,
            },
        );
    }
    for c in NAMED.captures_iter(source) {
        let specifier = c[3].to_string();
        if let Some(default) = c.get(1) {
            out.insert(
                default.as_str().to_string(),
                Import {
                    specifier: specifier.clone(),
                    binding: Binding::Default,
                },
            );
        }
        for entry in c[2].split(',') {
            let entry = entry.trim();
            let entry = entry.strip_prefix("type ").map(str::trim).unwrap_or(entry);
            if entry.is_empty() {
                continue;
            }
            let (imported, local) = match entry.split_once(" as ") {
                Some((i, l)) => (i.trim(), l.trim()),
                None => (entry, entry),
            };
            if !is_ident(local) || !is_ident(imported) {
                continue;
            }
            out.insert(
                local.to_string(),
                Import {
                    specifier: specifier.clone(),
                    binding: Binding::Named(imported.to_string()),
                },
            );
        }
    }
    for c in DEFAULT.captures_iter(source) {
        out.entry(c[1].to_string()).or_insert(Import {
            specifier: c[2].to_string(),
            binding: Binding::Default,
        });
    }
    out
}

/// `export * as name from '…'` re-exports, by name.
pub fn namespace_reexports(source: &str) -> HashMap<String, String> {
    REEXPORT_NS
        .captures_iter(source)
        .map(|c| (c[1].to_string(), c[2].to_string()))
        .collect()
}

/// Specifiers a barrel re-exports wholesale or by name: `export * from`
/// and `export { a, b } from` (not `export * as`, which keeps a name).
pub fn reexport_specs(source: &str) -> Vec<String> {
    REEXPORT_FROM
        .captures_iter(source)
        .filter(|c| !c[0].contains("* as "))
        .map(|c| c[1].to_string())
        .collect()
}

/// The package a bare specifier names: `@scope/name` for a scoped one,
/// the first segment otherwise; a relative specifier as-is.
pub fn package_name(specifier: &str) -> String {
    if specifier.starts_with('.') {
        return specifier.to_string();
    }
    let take = if specifier.starts_with('@') { 2 } else { 1 };
    specifier
        .split('/')
        .take(take)
        .collect::<Vec<_>>()
        .join("/")
}

fn is_ident(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_alphanumeric() || c == '_' || c == '$')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_namespace_named_and_default_bindings() {
        let src = r#"
import * as db from '@shop/database';
import { and, asc, eq as equals } from 'drizzle-orm';
import type { TypedResponse } from 'hono';
import Hono, { type Context, Hono as H } from 'hono';
import {
  DeleteFolderDeleteParam,
  type FolderData,
} from './OrderControllerData.ts';
"#;
        let b = bindings(src);
        assert_eq!(b["db"].binding, Binding::Namespace);
        assert_eq!(b["db"].specifier, "@shop/database");
        assert_eq!(b["equals"].binding, Binding::Named("eq".into()));
        assert_eq!(b["and"].specifier, "drizzle-orm");
        assert_eq!(
            b["TypedResponse"].binding,
            Binding::Named("TypedResponse".into())
        );
        assert_eq!(b["Hono"].binding, Binding::Default);
        assert_eq!(b["H"].binding, Binding::Named("Hono".into()));
        assert_eq!(
            b["DeleteFolderDeleteParam"].specifier,
            "./OrderControllerData.ts"
        );
        assert_eq!(b["FolderData"].binding, Binding::Named("FolderData".into()));
    }

    #[test]
    fn reads_namespace_reexports_and_package_names() {
        let src =
            "export * as fooDomain from './domain/foo/FooDomain.ts';\nexport { x } from './x.ts';";
        let r = namespace_reexports(src);
        assert_eq!(r["fooDomain"], "./domain/foo/FooDomain.ts");
        assert_eq!(r.len(), 1);
        let specs = reexport_specs(
            "export * from './client.ts';\nexport { a, b as c } from './x.ts';\nexport * as ns from './ns.ts';",
        );
        assert_eq!(specs, ["./client.ts", "./x.ts"]);
        assert_eq!(package_name("@shop/aws/kms"), "@shop/aws");
        assert_eq!(package_name("drizzle-orm"), "drizzle-orm");
        assert_eq!(package_name("hono/cors"), "hono");
    }
}
