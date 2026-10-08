//! Go: functions and calls by tree-sitter, imports, and import paths
//! resolved through the repository's `go.mod` modules.
//!
//! A Go package is a directory, so an import resolves to a directory, and
//! a name in it is looked up across the package's files. Within one
//! package every file sees every other's top-level names unqualified.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use tree_sitter::Node;

use crate::bindings::{Binding, Import};
use crate::rev::Rev;
use crate::ts_extract::{TsFunction, normalized_hash};
use crate::workspace::{PackageKind, Workspace};

fn text(node: Node, src: &[u8]) -> String {
    node.utf8_text(src).unwrap_or("").to_string()
}

fn parse(source: &str) -> anyhow::Result<tree_sitter::Tree> {
    let mut parser = tree_sitter::Parser::new();
    parser.set_language(&tree_sitter_go::LANGUAGE.into())?;
    parser
        .parse(source, None)
        .ok_or_else(|| anyhow::anyhow!("tree-sitter failed to parse"))
}

/// Every function, method and function literal, with the calls each
/// makes. Exported = the name starts with an upper-case letter.
pub fn extract(source: &str) -> anyhow::Result<Vec<TsFunction>> {
    let tree = parse(source)?;
    let mut out = Vec::new();
    walk(tree.root_node(), source.as_bytes(), None, &mut out);
    Ok(out)
}

fn exported(name: &str) -> bool {
    name.chars().next().is_some_and(char::is_uppercase)
}

fn walk(node: Node, src: &[u8], enclosing: Option<usize>, out: &mut Vec<TsFunction>) {
    let kind = node.kind();
    if matches!(
        kind,
        "function_declaration" | "method_declaration" | "func_literal"
    ) {
        let name = node.child_by_field_name("name").map(|n| text(n, src));
        let method = kind == "method_declaration";
        let id = out.len();
        out.push(TsFunction {
            parent: enclosing,
            exported: name.as_deref().is_some_and(exported),
            name,
            method,
            start_line: node.start_position().row as u32 + 1,
            end_line: node.end_position().row as u32 + 1,
            body_hash: normalized_hash(&src[node.byte_range()]),
            calls: Vec::new(),
            call_lines: Vec::new(),
            route: if kind == "func_literal" {
                route_info(node, src)
            } else {
                None
            },
        });
        for child in node.children(&mut node.walk()) {
            walk(child, src, Some(id), out);
        }
        return;
    }
    if let Some(e) = enclosing {
        let callee = match kind {
            "call_expression" => node.child_by_field_name("function"),
            // `pkg.New[T](x)` parses as a conversion to a generic type.
            "type_conversion_expression" => node
                .child_by_field_name("type")
                .filter(|t| t.kind() == "generic_type")
                .and_then(|t| t.child_by_field_name("type")),
            _ => None,
        };
        if let Some(c) = callee.and_then(|c| callee_text(c, src)) {
            out[e].calls.push(c);
            out[e].call_lines.push(node.start_position().row as u32 + 1);
        }
    }
    for child in node.children(&mut node.walk()) {
        walk(child, src, enclosing, out);
    }
}

/// `f`, `pkg.F`, `s.repo.Save`; a call in the chain collapses to
/// `f(...)`; anything else (a func literal called in place) is no name.
fn callee_text(node: Node, src: &[u8]) -> Option<String> {
    match node.kind() {
        "identifier" | "field_identifier" | "package_identifier" | "type_identifier" => {
            Some(text(node, src))
        }
        "selector_expression" => {
            let operand = callee_text(node.child_by_field_name("operand")?, src)?;
            let field = text(node.child_by_field_name("field")?, src);
            Some(format!("{operand}.{field}"))
        }
        "qualified_type" => {
            let pkg = text(node.child_by_field_name("package")?, src);
            let name = text(node.child_by_field_name("name")?, src);
            Some(format!("{pkg}.{name}"))
        }
        "index_expression" | "generic_type" => callee_text(
            node.child_by_field_name("operand")
                .or_else(|| node.child_by_field_name("type"))?,
            src,
        ),
        "call_expression" => Some(format!(
            "{}(...)",
            callee_text(node.child_by_field_name("function")?, src)?
        )),
        "parenthesized_expression" => callee_text(node.named_child(0)?, src),
        _ => None,
    }
}

const ROUTE_METHODS: &[(&str, &str)] = &[
    ("get", "GET"),
    ("post", "POST"),
    ("put", "PUT"),
    ("patch", "PATCH"),
    ("delete", "DELETE"),
    ("head", "HEAD"),
    ("options", "OPTIONS"),
    ("handlefunc", "ANY"),
    ("handle", "ANY"),
];

/// A function literal handed straight to a router — `r.Get("/x", func…)`,
/// `http.HandleFunc("/x", func…)` — as `"GET /x"`.
fn route_info(node: Node, src: &[u8]) -> Option<String> {
    let args = node.parent()?;
    if args.kind() != "argument_list" {
        return None;
    }
    let call = args.parent()?;
    let func = call.child_by_field_name("function")?;
    let method = match func.kind() {
        "selector_expression" => text(func.child_by_field_name("field")?, src),
        "identifier" => text(func, src),
        _ => return None,
    };
    let verb = ROUTE_METHODS
        .iter()
        .find(|(m, _)| method.eq_ignore_ascii_case(m))?
        .1;
    let first = args.named_child(0)?;
    if !matches!(
        first.kind(),
        "interpreted_string_literal" | "raw_string_literal"
    ) {
        return None;
    }
    let path = text(first, src);
    let path = path.trim_matches(|c| c == '"' || c == '`');
    path.starts_with('/').then(|| format!("{verb} {path}"))
}

/// The import paths a file names.
pub fn import_specs(source: &str) -> Vec<String> {
    imports(source).into_iter().map(|(_, path)| path).collect()
}

/// (local name or `None` for `_`/`.`, import path) per import. Read as
/// text: Go's import block has one fixed shape and always comes first,
/// and workspace discovery reads every file's imports.
fn imports(source: &str) -> Vec<(Option<String>, String)> {
    let mut out = Vec::new();
    let mut in_block = false;
    for line in source.lines() {
        let line = line.split("//").next().unwrap_or("").trim();
        let spec = if in_block {
            if line.starts_with(')') {
                in_block = false;
                continue;
            }
            line
        } else if let Some(rest) = line.strip_prefix("import") {
            let rest = rest.trim_start();
            if let Some(r) = rest.strip_prefix('(') {
                in_block = true;
                r.trim()
            } else if rest.len() < line.len() - "import".len() || rest.starts_with('"') {
                rest
            } else {
                continue;
            }
        } else if line.starts_with("func ") || line.starts_with("type ") || line.starts_with("var ")
        {
            break;
        } else {
            continue;
        };
        if spec.is_empty() {
            continue;
        }
        let (alias, quoted) = match spec.split_once(char::is_whitespace) {
            Some((a, q)) if !a.starts_with('"') && !a.starts_with('`') => (Some(a), q.trim()),
            _ => (None, spec),
        };
        let Some(path) = quoted
            .strip_prefix('"')
            .and_then(|q| q.split('"').next())
            .or_else(|| quoted.strip_prefix('`').and_then(|q| q.split('`').next()))
        else {
            continue;
        };
        let local = match alias {
            Some("_" | ".") => None,
            Some(a) => Some(a.to_string()),
            None => Some(default_name(path)),
        };
        out.push((local, path.to_string()));
    }
    out
}

/// The name an import path is used by when no alias is given: its last
/// segment, skipping a `/vN` major-version suffix and a `.vN` suffix.
fn default_name(path: &str) -> String {
    let mut segs = path.rsplit('/');
    let mut last = segs.next().unwrap_or(path);
    if last.len() > 1 && last.starts_with('v') && last[1..].chars().all(|c| c.is_ascii_digit()) {
        last = segs.next().unwrap_or(last);
    }
    let last = match last.rsplit_once(".v") {
        Some((head, v)) if v.chars().all(|c| c.is_ascii_digit()) => head,
        _ => last,
    };
    last.trim_start_matches("go-").replace('-', "_")
}

/// Every package a file imports, by the name it is used under.
pub fn bindings(source: &str) -> HashMap<String, Import> {
    imports(source)
        .into_iter()
        .filter_map(|(local, path)| {
            local.map(|l| {
                (
                    l,
                    Import {
                        specifier: path,
                        binding: Binding::Namespace,
                    },
                )
            })
        })
        .collect()
}

/// The repository directory an import path names, through the `go.mod`
/// modules in it; `None` for the standard library and third parties.
pub fn resolve(spec: &str, workspace: &Workspace) -> Option<PathBuf> {
    workspace
        .package_of_kind(PackageKind::Go, spec)
        .map(|p| p.dir.clone())
}

/// The non-test `.go` files directly in `dir`.
pub fn package_files(dir: &Path, root: &Path, rev: &Rev) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = rev
        .files_under(root, dir)
        .iter()
        .filter(|f| f.parent().unwrap_or(Path::new("")) == dir)
        .filter(|f| f.extension().is_some_and(|e| e == "go"))
        .filter(|f| !f.to_string_lossy().ends_with("_test.go"))
        .cloned()
        .collect();
    files.sort();
    files
}

/// The module path a `go.mod` declares.
pub fn module_path(go_mod: &str) -> Option<String> {
    go_mod
        .lines()
        .map(str::trim)
        .find_map(|l| l.strip_prefix("module "))
        .map(|m| m.trim().trim_matches('"').to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SRC: &str = r#"package store

import (
	"context"
	db "example.com/shop/internal/db"
	"gopkg.in/yaml.v3"
	"github.com/redis/go-redis/v9"
	_ "embed"
)

func Open(ctx context.Context) *Store {
	s := &Store{}
	for _, id := range ids {
		defer s.Close()
	}
	go func() { s.ping() }()
	r.Get("/orders", func(w http.ResponseWriter, r *http.Request) { list(w) })
	return db.Connect(ctx)
}

func (s *Store) Close() error { return helper() }

func helper() error { return nil }
"#;

    #[test]
    fn functions_methods_literals_and_calls() {
        let fns = extract(SRC).unwrap();
        let names: Vec<(Option<&str>, bool, bool, Option<usize>)> = fns
            .iter()
            .map(|f| (f.name.as_deref(), f.exported, f.method, f.parent))
            .collect();
        assert_eq!(
            names,
            vec![
                (Some("Open"), true, false, None),
                (None, false, false, Some(0)),
                (None, false, false, Some(0)),
                (Some("Close"), true, true, None),
                (Some("helper"), false, false, None),
            ]
        );
        assert_eq!(fns[0].calls, vec!["s.Close", "r.Get", "db.Connect"]);
        assert_eq!(fns[1].calls, vec!["s.ping"]);
        assert_eq!(fns[2].route.as_deref(), Some("GET /orders"));
        assert_eq!(fns[3].calls, vec!["helper"]);
    }

    #[test]
    fn imports_by_their_local_names() {
        let b = bindings(SRC);
        assert_eq!(b["db"].specifier, "example.com/shop/internal/db");
        assert_eq!(b["context"].specifier, "context");
        assert_eq!(b["yaml"].specifier, "gopkg.in/yaml.v3");
        assert_eq!(b["redis"].specifier, "github.com/redis/go-redis/v9");
        assert!(!b.contains_key("_") && !b.contains_key("embed"));
        assert_eq!(
            module_path("module example.com/shop\n\ngo 1.22\n").as_deref(),
            Some("example.com/shop")
        );
    }
}
