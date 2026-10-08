//! Python: functions and calls by tree-sitter, imports, and module paths
//! resolved to the repository's files.
//!
//! A module path resolves against the importing file's package for a
//! relative import (`from .db import pool`), and against the source
//! roots for an absolute one: the repository root, `src/`, and each
//! project's directory and its `src/`. A module is `name.py`, a package
//! `name/__init__.py`.

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
    parser.set_language(&tree_sitter_python::LANGUAGE.into())?;
    parser
        .parse(source, None)
        .ok_or_else(|| anyhow::anyhow!("tree-sitter failed to parse"))
}

/// Every `def` and `lambda`, with the calls each makes. A `def` inside a
/// class is a method; a top-level one whose name has no leading
/// underscore is exported.
pub fn extract(source: &str) -> anyhow::Result<Vec<TsFunction>> {
    let tree = parse(source)?;
    let mut out = Vec::new();
    walk(tree.root_node(), source.as_bytes(), None, false, &mut out);
    Ok(out)
}

fn walk(
    node: Node,
    src: &[u8],
    enclosing: Option<usize>,
    in_class: bool,
    out: &mut Vec<TsFunction>,
) {
    match node.kind() {
        // `lambda` is also the keyword's own (unnamed) node.
        "function_definition" | "lambda" if node.is_named() => {
            let lambda = node.kind() == "lambda";
            let name = if lambda {
                lambda_name(node, src)
            } else {
                node.child_by_field_name("name").map(|n| text(n, src))
            };
            let method = in_class && !lambda;
            let id = out.len();
            // A decorated function starts at its first decorator.
            let span = node
                .parent()
                .filter(|p| p.kind() == "decorated_definition")
                .unwrap_or(node);
            out.push(TsFunction {
                parent: enclosing,
                exported: enclosing.is_none()
                    && !in_class
                    && name.as_deref().is_some_and(|n| !n.starts_with('_')),
                name,
                method,
                start_line: span.start_position().row as u32 + 1,
                end_line: span.end_position().row as u32 + 1,
                body_hash: normalized_hash(&src[node.byte_range()]),
                calls: Vec::new(),
                call_lines: Vec::new(),
                route: if lambda { None } else { route_info(node, src) },
            });
            for child in node.children(&mut node.walk()) {
                walk(child, src, Some(id), false, out);
            }
            return;
        }
        "class_definition" => {
            if let Some(body) = node.child_by_field_name("body") {
                for child in body.children(&mut body.walk()) {
                    walk(child, src, enclosing, true, out);
                }
            }
            for field in ["superclasses", "name"] {
                if let Some(c) = node.child_by_field_name(field) {
                    walk(c, src, enclosing, false, out);
                }
            }
            return;
        }
        "decorated_definition" => {
            // Decorators are evaluated where the definition is, not
            // inside it.
            for child in node.children(&mut node.walk()) {
                walk(
                    child,
                    src,
                    enclosing,
                    in_class && child.kind() != "decorator",
                    out,
                );
            }
            return;
        }
        "call" => {
            if let Some(e) = enclosing
                && let Some(c) = node
                    .child_by_field_name("function")
                    .and_then(|f| callee_text(f, src))
            {
                out[e].calls.push(c);
                out[e].call_lines.push(node.start_position().row as u32 + 1);
            }
        }
        _ => {}
    }
    for child in node.children(&mut node.walk()) {
        walk(
            child,
            src,
            enclosing,
            in_class && node.kind() == "block",
            out,
        );
    }
}

/// `handler = lambda e: …` names the lambda `handler`.
fn lambda_name(node: Node, src: &[u8]) -> Option<String> {
    let parent = node.parent()?;
    match parent.kind() {
        "assignment" => {
            let left = parent.child_by_field_name("left")?;
            matches!(left.kind(), "identifier").then(|| text(left, src))
        }
        "keyword_argument" => parent.child_by_field_name("name").map(|n| text(n, src)),
        _ => None,
    }
}

/// `f`, `mod.f`, `self.repo.save`; a call in the chain collapses to
/// `f(...)`; anything else (a subscript, a literal) is no name.
fn callee_text(node: Node, src: &[u8]) -> Option<String> {
    match node.kind() {
        "identifier" => Some(text(node, src)),
        "attribute" => {
            let object = callee_text(node.child_by_field_name("object")?, src)?;
            let attr = text(node.child_by_field_name("attribute")?, src);
            Some(format!("{object}.{attr}"))
        }
        "call" => Some(format!(
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
    ("route", "ANY"),
    ("api_route", "ANY"),
    ("websocket", "WS"),
];

/// A function decorated as a route handler — `@app.get("/x")`,
/// `@router.post("/x")`, `@bp.route("/x", methods=["POST"])` — as
/// `"GET /x"`.
fn route_info(def: Node, src: &[u8]) -> Option<String> {
    let decorated = def
        .parent()
        .filter(|p| p.kind() == "decorated_definition")?;
    decorated.children(&mut decorated.walk()).find_map(|d| {
        if d.kind() != "decorator" {
            return None;
        }
        let call = d.named_child(0).filter(|c| c.kind() == "call")?;
        let func = call.child_by_field_name("function")?;
        if func.kind() != "attribute" {
            return None;
        }
        let method = text(func.child_by_field_name("attribute")?, src);
        let mut verb = ROUTE_METHODS
            .iter()
            .find(|(m, _)| method == *m)?
            .1
            .to_string();
        let args = call.child_by_field_name("arguments")?;
        let path = args
            .named_children(&mut args.walk())
            .find(|a| a.kind() == "string")
            .map(|a| string_value(a, src))?;
        if verb == "ANY"
            && let Some(methods) = args
                .named_children(&mut args.walk())
                .filter(|a| a.kind() == "keyword_argument")
                .find(|a| {
                    a.child_by_field_name("name")
                        .is_some_and(|n| text(n, src) == "methods")
                })
                .and_then(|a| a.child_by_field_name("value"))
        {
            let listed: Vec<String> = methods
                .named_children(&mut methods.walk())
                .filter(|m| m.kind() == "string")
                .map(|m| string_value(m, src).to_uppercase())
                .collect();
            if listed.len() == 1 {
                verb = listed[0].clone();
            }
        }
        path.starts_with('/').then(|| format!("{verb} {path}"))
    })
}

fn string_value(node: Node, src: &[u8]) -> String {
    node.named_children(&mut node.walk())
        .filter(|c| c.kind() == "string_content")
        .map(|c| text(c, src))
        .collect()
}

/// One `import`/`from … import` line: (local name, module path,
/// binding).
fn imports(source: &str) -> Vec<(String, String, Binding)> {
    let Ok(tree) = parse(source) else {
        return Vec::new();
    };
    let src = source.as_bytes();
    let mut out = Vec::new();
    let mut stack = vec![tree.root_node()];
    while let Some(n) = stack.pop() {
        match n.kind() {
            "import_statement" => {
                for name in n.children_by_field_name("name", &mut n.walk()) {
                    match name.kind() {
                        // `import a.b` binds `a`; `a.b.f()` walks to `b`.
                        "dotted_name" => {
                            let path = text(name, src);
                            let first = path.split('.').next().unwrap_or(&path).to_string();
                            out.push((first.clone(), first, Binding::Namespace));
                        }
                        "aliased_import" => {
                            let (Some(path), Some(alias)) = (
                                name.child_by_field_name("name"),
                                name.child_by_field_name("alias"),
                            ) else {
                                continue;
                            };
                            out.push((text(alias, src), text(path, src), Binding::Namespace));
                        }
                        _ => {}
                    }
                }
            }
            "import_from_statement" => {
                let Some(module) = n.child_by_field_name("module_name") else {
                    continue;
                };
                let module = text(module, src).replace(char::is_whitespace, "");
                for name in n.children_by_field_name("name", &mut n.walk()) {
                    let (imported, local) = match name.kind() {
                        "dotted_name" => (text(name, src), text(name, src)),
                        "aliased_import" => {
                            let (Some(path), Some(alias)) = (
                                name.child_by_field_name("name"),
                                name.child_by_field_name("alias"),
                            ) else {
                                continue;
                            };
                            (text(path, src), text(alias, src))
                        }
                        _ => continue,
                    };
                    out.push((local, module.clone(), Binding::Named(imported)));
                }
            }
            // Imports inside a function or a `try`/`if TYPE_CHECKING`
            // block count too.
            _ => {
                for c in n.children(&mut n.walk()) {
                    stack.push(c);
                }
            }
        }
    }
    out
}

/// The module paths a file names: each `import`'s, and for `from M
/// import n` both `M` and `M.n` (`n` may itself be a submodule).
pub fn import_specs(source: &str) -> Vec<String> {
    let mut out = Vec::new();
    for (_, module, binding) in imports(source) {
        if let Binding::Named(n) = &binding {
            out.push(join(&module, n));
        }
        out.push(module);
    }
    out
}

/// `M` + `n` as one module path: `.` + `db` is `.db`, `a` + `b` `a.b`.
pub fn join(module: &str, name: &str) -> String {
    if module.ends_with('.') {
        format!("{module}{name}")
    } else {
        format!("{module}.{name}")
    }
}

/// Every name a file imports, by the name it is used under.
pub fn bindings(source: &str) -> HashMap<String, Import> {
    imports(source)
        .into_iter()
        .map(|(local, specifier, binding)| (local, Import { specifier, binding }))
        .collect()
}

/// The repository file a module path names, as written in `from_file`;
/// `None` for the standard library and third parties.
pub fn resolve(
    spec: &str,
    from_file: &Path,
    root: &Path,
    rev: &Rev,
    workspace: &Workspace,
) -> Option<PathBuf> {
    let dots = spec.chars().take_while(|c| *c == '.').count();
    let rest = &spec[dots..];
    let rel: PathBuf = rest.split('.').filter(|s| !s.is_empty()).collect();
    let module_at = |base: &Path| -> Option<PathBuf> {
        let stem = base.join(&rel);
        let as_module = stem.with_extension("py");
        if !rel.as_os_str().is_empty() && rev.exists(root, &as_module) {
            return Some(as_module);
        }
        let init = stem.join("__init__.py");
        rev.exists(root, &init).then_some(init)
    };
    if dots > 0 {
        let mut dir = from_file.parent().unwrap_or(Path::new("")).to_path_buf();
        for _ in 1..dots {
            dir = dir.parent()?.to_path_buf();
        }
        return module_at(&dir);
    }
    if rel.as_os_str().is_empty() {
        return None;
    }
    let mut roots: Vec<PathBuf> = vec![PathBuf::new(), PathBuf::from("src")];
    for p in workspace
        .packages
        .iter()
        .filter(|p| p.kind == PackageKind::Python)
    {
        roots.push(p.dir.clone());
        roots.push(p.dir.join("src"));
    }
    // The importing file's own project first.
    if let Some(own) = workspace
        .owning_package(from_file)
        .filter(|p| p.kind == PackageKind::Python)
    {
        roots.insert(0, own.dir.join("src"));
        roots.insert(0, own.dir.clone());
    }
    roots.iter().find_map(|r| module_at(r))
}

/// The top-level package a module path is in: what a call into a module
/// outside the repository is labelled by.
pub fn package_name(spec: &str) -> String {
    spec.trim_start_matches('.')
        .split('.')
        .next()
        .unwrap_or(spec)
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SRC: &str = r#"import os
import shop.db.pool
import numpy as np
from . import models
from ..core.cache import get_cache as cache, warm

@app.get("/orders")
async def list_orders(req):
    async for row in rows():
        await shop.db.pool.query(row)
    on_done = lambda r: notify(r)
    return [helper(x) for x in models.Order.all()]

class Repo(Base):
    def save(self, item):
        self.validate(item)
        return cache().set(item)

    @staticmethod
    def _key(item):
        return item.id

def _helper(x):
    return np.sum(x)
"#;

    #[test]
    fn functions_methods_lambdas_and_calls() {
        let fns = extract(SRC).unwrap();
        let summary: Vec<(Option<&str>, bool, bool, Option<usize>)> = fns
            .iter()
            .map(|f| (f.name.as_deref(), f.exported, f.method, f.parent))
            .collect();
        assert_eq!(
            summary,
            vec![
                (Some("list_orders"), true, false, None),
                (Some("on_done"), false, false, Some(0)),
                (Some("save"), false, true, None),
                (Some("_key"), false, true, None),
                (Some("_helper"), false, false, None),
            ]
        );
        assert_eq!(fns[0].route.as_deref(), Some("GET /orders"));
        assert_eq!(fns[0].start_line, 7);
        assert_eq!(
            fns[0].calls,
            vec!["rows", "shop.db.pool.query", "helper", "models.Order.all"]
        );
        assert_eq!(fns[1].calls, vec!["notify"]);
        assert_eq!(
            fns[2].calls,
            vec!["self.validate", "cache(...).set", "cache"]
        );
    }

    #[test]
    fn imports_by_their_local_names() {
        let b = bindings(SRC);
        assert_eq!(b["shop"].specifier, "shop");
        assert_eq!(b["np"].specifier, "numpy");
        assert_eq!(b["models"].specifier, ".");
        assert_eq!(b["models"].binding, Binding::Named("models".into()));
        assert_eq!(b["cache"].specifier, "..core.cache");
        assert_eq!(b["cache"].binding, Binding::Named("get_cache".into()));
        assert!(import_specs(SRC).contains(&".models".to_string()));
        assert_eq!(package_name("shop.db.pool"), "shop");
    }
}
