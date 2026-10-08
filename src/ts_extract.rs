//! Pulls every function-like construct out of a TypeScript file — named
//! declarations, `const x = () => {}`, and anonymous callbacks alike — as
//! flat L2 nodes, with the direct calls each one makes in its own body
//! (not a nested function's body).
//!
//! An anonymous callback is still a node: six near-identical
//! `pool.on('error', (err) => { resetPoolLater(...) })` callbacks
//! scattered through one file are six different triggers, and the aligner
//! needs each as its own thing to match, even though none of them has a
//! name.

use tree_sitter::Node;

#[derive(Debug, Clone)]
pub struct TsFunction {
    pub parent: Option<usize>,
    /// A declared or inferred name: `const NAME = ...`, `function NAME()`,
    /// or the property a callback was assigned to (`x.NAME = () => {}`).
    /// `None` for a callback passed as a bare argument or array element.
    pub name: Option<String>,
    pub exported: bool,
    /// Named as a property or method of an object (`{ find: () => … }`,
    /// `find() { … }`, `x.find = () => …`) rather than as a declaration —
    /// reached as `something.find(…)`, never as a bare `find(…)`.
    pub method: bool,
    pub start_line: u32,
    pub end_line: u32,
    /// A fingerprint of the body's source with whitespace collapsed out —
    /// two functions with the same hash read as the same code, which is
    /// what lets an anonymous callback survive a rename-free move.
    pub body_hash: u64,
    /// Every call this function makes directly, in source order — a
    /// simple identifier ("sleep"), or a dotted chain ("pool.on").
    pub calls: Vec<String>,
    /// The line each entry of `calls` is made on, parallel to it — so a
    /// call that resolves to nothing local (`db.transaction`) still has
    /// a place in the file to jump to.
    pub call_lines: Vec<u32>,
    /// `"DELETE /:orderId"`-style label when this function is passed
    /// directly as a route handler — `.delete('/:orderId', ...,
    /// async (c) => {...})` — since "callback in top level" says
    /// nothing about what a Hono handler actually is, while the method
    /// and path are right there in the same call.
    pub route: Option<String>,
}

const FUNCTION_KINDS: &[&str] = &[
    "arrow_function",
    "function_expression",
    "function_declaration",
    "generator_function_declaration",
    "method_definition",
];

/// Extracts by file extension: `.tsx` with the TSX grammar, `.svelte`
/// by its `<script>` blocks, everything else as TypeScript.
pub fn extract_for_path(path: &std::path::Path, source: &str) -> anyhow::Result<Vec<TsFunction>> {
    match path.extension().and_then(|e| e.to_str()) {
        Some("tsx") | Some("jsx") => extract(source, true),
        Some("svelte") => {
            // `+page.svelte`, `+layout@.svelte` → `Page`, `Layout`: the
            // file header already says which route they belong to.
            let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("Component");
            let stem = stem.split('@').next().unwrap_or(stem);
            let name = match stem.strip_prefix('+') {
                Some(kind) => {
                    let mut c = kind.chars();
                    c.next().map(|f| f.to_uppercase().collect::<String>() + c.as_str()).unwrap_or_default()
                }
                None => stem.to_string(),
            }
            .replace(|c: char| !c.is_alphanumeric() && c != '_', "_");
            let name = if name.is_empty() { "Component".to_string() } else { name };
            let fns = extract(&script_only(source, &name), false);
            if let (Ok(fns), Ok(log)) = (&fns, std::env::var("PROGNOST_DEBUG")) {
                use std::io::Write;
                if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(log) {
                    for func in fns {
                        let _ = writeln!(
                            f,
                            "svelte fn {}: {:?} L{}-{} parent={:?}",
                            path.display(),
                            func.name,
                            func.start_line,
                            func.end_line,
                            func.parent
                        );
                    }
                }
            }
            fns
        }
        _ => extract(source, false),
    }
}

/// A Svelte component as one function: everything outside its
/// `<script>` blocks is blanked out — line for line, so every line
/// number still points at the real file — and each block is wrapped as
/// `export const <Name> = async () => { … }` on the `<script>` and
/// `</script>` lines themselves. The component's setup code, its
/// `$effect`s and handlers then read as one unit that calls things,
/// the way a route handler does, instead of a dozen "in top level"
/// callbacks. A `<script module>` block becomes `<Name>_module`.
/// Handlers written inline in the markup are lost.
pub fn script_only(source: &str, name: &str) -> String {
    let mut out = String::with_capacity(source.len() + 64);
    let mut in_script = false;
    // `import … from '…'` and `export let …` are not valid inside a
    // function body; the parser's error recovery would otherwise cut
    // the wrapper short and leave the callbacks below as top-level
    // functions again. Imports are blanked (bindings are read from the
    // real text elsewhere), `export ` prefixes dropped.
    let mut in_import = false;
    for line in source.lines() {
        let trimmed = line.trim_start();
        let opens = trimmed.starts_with("<script") && trimmed.contains('>') && !trimmed.contains("</script>");
        let closes = trimmed.starts_with("</script>");
        if opens {
            in_script = true;
            let is_module = trimmed.contains(" module") || trimmed.contains("context=\"module\"");
            let fn_name = if is_module { format!("{name}_module") } else { name.to_string() };
            out.push_str(&format!("export const {fn_name} = async () => {{\n"));
            continue;
        }
        if closes {
            in_script = false;
            in_import = false;
            out.push_str("};\n");
            continue;
        }
        if in_script {
            if in_import {
                if trimmed.contains(" from ") || trimmed.ends_with(';') {
                    in_import = false;
                }
            } else if trimmed.starts_with("import ") || trimmed.starts_with("import{") {
                in_import = !(trimmed.contains(" from ") || trimmed.ends_with(';'));
            } else if let Some(rest) = trimmed.strip_prefix("export ") {
                let indent = line.len() - trimmed.len();
                out.push_str(&line[..indent]);
                out.push_str("       ");
                out.push_str(rest);
            } else {
                out.push_str(line);
            }
        }
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod svelte_tests {
    use super::*;

    #[test]
    fn a_component_is_one_function_with_its_callbacks_inside() {
        let src = "<div/>\n<script lang=\"ts\">\n  import { x } from './x.ts';\n  import {\n    a,\n    type B,\n  } from './ab.ts';\n  export let title: string;\n  let { rows } = $props();\n  const load = async () => {\n    await client.api.v1.tables.$get();\n  };\n  $effect(() => {\n    load();\n  });\n</script>\n<p>{x}</p>\n";
        let fns = extract_for_path(std::path::Path::new("a/Foo.svelte"), src).unwrap();
        let comp = fns.iter().find(|f| f.name.as_deref() == Some("Foo")).expect("component fn");
        assert_eq!(comp.start_line, 2);
        assert_eq!(comp.end_line, 16);
        assert!(comp.exported);
        let load = fns.iter().find(|f| f.name.as_deref() == Some("load")).unwrap();
        assert_eq!(load.parent, Some(0));
        assert_eq!(load.start_line, 10);
        // The $effect callback is nested in the component, not a top-level function.
        let effect = fns.iter().find(|f| f.start_line == 13).expect("effect callback");
        assert_eq!(effect.parent, Some(0));
        assert!(fns.iter().all(|f| f.name.as_deref() == Some("Foo") || f.parent.is_some()));
    }
}

pub fn extract(source: &str, is_tsx: bool) -> anyhow::Result<Vec<TsFunction>> {
    let mut parser = tree_sitter::Parser::new();
    let language = if is_tsx {
        tree_sitter_typescript::LANGUAGE_TSX
    } else {
        tree_sitter_typescript::LANGUAGE_TYPESCRIPT
    };
    parser.set_language(&language.into())?;
    let tree = parser
        .parse(source, None)
        .ok_or_else(|| anyhow::anyhow!("tree-sitter failed to parse"))?;
    let bytes = source.as_bytes();
    let mut functions = Vec::new();
    walk(tree.root_node(), bytes, None, &mut functions);
    Ok(functions)
}

fn walk(node: Node, src: &[u8], enclosing: Option<usize>, out: &mut Vec<TsFunction>) {
    if FUNCTION_KINDS.contains(&node.kind()) {
        let id = out.len();
        let (name, exported, method) = infer_name(node, src);
        out.push(TsFunction {
            parent: enclosing,
            name,
            exported,
            method,
            start_line: node.start_position().row as u32 + 1,
            end_line: node.end_position().row as u32 + 1,
            body_hash: normalized_hash(&src[node.byte_range()]),
            calls: Vec::new(),
            call_lines: Vec::new(),
            route: route_info(node, src),
        });
        for child in node.children(&mut node.walk()) {
            walk(child, src, Some(id), out);
        }
        return;
    }

    if node.kind() == "call_expression"
        && let Some(enclosing) = enclosing
        && let Some(callee) = node.child_by_field_name("function")
    {
        out[enclosing].calls.push(callee_text(callee, src));
        out[enclosing]
            .call_lines
            .push(node.start_position().row as u32 + 1);
    }

    for child in node.children(&mut node.walk()) {
        walk(child, src, enclosing, out);
    }
}

/// A name for a function-like node, read off whatever it is attached to:
/// `const NAME = ...`, `function NAME() {}`, `obj.NAME = ...`, or
/// `{ NAME: ... }`. `exported` is true only for a top-level `export const`
/// or `export function` — a property assignment is never a module export.
/// (name, exported, method) — see [`TsFunction::method`].
fn infer_name(node: Node, src: &[u8]) -> (Option<String>, bool, bool) {
    if node.kind() == "function_declaration" || node.kind() == "generator_function_declaration" {
        let name = node.child_by_field_name("name").map(|n| text(n, src));
        let exported = node
            .parent()
            .is_some_and(|p| p.kind() == "export_statement");
        return (name, exported, false);
    }
    if node.kind() == "method_definition" {
        let name = node.child_by_field_name("name").map(|n| text(n, src));
        return (name, false, true);
    }
    let Some(parent) = node.parent() else {
        return (None, false, false);
    };
    match parent.kind() {
        "variable_declarator" => {
            let name = parent.child_by_field_name("name").map(|n| text(n, src));
            let exported = parent
                .parent() // lexical_declaration
                .and_then(|d| d.parent())
                .is_some_and(|gp| gp.kind() == "export_statement");
            (name, exported, false)
        }
        "assignment_expression" => {
            let left = parent.child_by_field_name("left");
            let is_member = left.is_some_and(|l| l.kind() == "member_expression");
            let name = left.map(|l| match l.kind() {
                "member_expression" => l.child_by_field_name("property").map(|p| text(p, src)),
                _ => Some(text(l, src)),
            });
            (name.flatten(), false, is_member)
        }
        "pair" => (
            parent.child_by_field_name("key").map(|n| text(n, src)),
            false,
            true,
        ),
        _ => (None, false, false),
    }
}

/// The text a `call_expression`'s `function` field resolves to: an
/// identifier as itself, `a.b.c` joined by hand (the grammar nests
/// `member_expression` right-to-left), a chained call collapsed to
/// `f(...)` rather than its full source (a callback argument inline in a
/// chain — `x().catch((e) => { ... })` — must not swallow `{ ... }` into
/// a callee name), anything else taken verbatim.
fn callee_text(node: Node, src: &[u8]) -> String {
    match node.kind() {
        "identifier" | "this" | "property_identifier" => text(node, src),
        "member_expression" => {
            let object = node
                .child_by_field_name("object")
                .map(|n| callee_text(n, src));
            let property = node.child_by_field_name("property").map(|n| text(n, src));
            match (object, property) {
                (Some(o), Some(p)) => format!("{o}.{p}"),
                (None, Some(p)) => p,
                (Some(o), None) => o,
                (None, None) => text(node, src),
            }
        }
        "call_expression" => {
            let f = node
                .child_by_field_name("function")
                .map(|n| callee_text(n, src));
            format!("{}(...)", f.unwrap_or_default())
        }
        _ => text(node, src),
    }
}

const HTTP_METHODS: &[&str] = &["get", "post", "put", "patch", "delete"];

/// Whether `node` is passed directly as an argument to a Hono-style
/// route registration — `<router>.<method>('<path>', ..., node)` — and
/// if so, its `"METHOD /path"` label. Checked structurally (parent is
/// an `arguments` node of a `call_expression` whose callee's property
/// is an HTTP method name, and whose own first argument is a string
/// starting with `/`) rather than by name, so it doesn't depend on what
/// the router variable happens to be called.
fn route_info(node: Node, src: &[u8]) -> Option<String> {
    let args = node.parent()?;
    if args.kind() != "arguments" {
        return None;
    }
    let call = args.parent()?;
    if call.kind() != "call_expression" {
        return None;
    }
    let func = call.child_by_field_name("function")?;
    if func.kind() != "member_expression" {
        return None;
    }
    let method = text(func.child_by_field_name("property")?, src).to_lowercase();
    if !HTTP_METHODS.contains(&method.as_str()) {
        return None;
    }
    let first_arg = args.named_children(&mut args.walk()).next()?;
    if first_arg.kind() != "string" {
        return None;
    }
    let path = text(first_arg, src);
    let path = path.trim_matches(|c| c == '\'' || c == '"' || c == '`');
    path.starts_with('/')
        .then(|| format!("{} {path}", method.to_uppercase()))
}

fn text(node: Node, src: &[u8]) -> String {
    String::from_utf8_lossy(&src[node.byte_range()]).into_owned()
}

/// FNV-1a over the bytes with ASCII whitespace dropped, so reindentation
/// or a shifted line number does not break a match. Hand-rolled rather
/// than `DefaultHasher` so the fingerprint is fixed forever, independent
/// of the standard library's own hasher.
fn normalized_hash(bytes: &[u8]) -> u64 {
    const OFFSET: u64 = 0xcbf29ce484222325;
    const PRIME: u64 = 0x100000001b3;
    let mut h = OFFSET;
    for &b in bytes {
        if b.is_ascii_whitespace() {
            continue;
        }
        h ^= b as u64;
        h = h.wrapping_mul(PRIME);
    }
    h
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn six_scattered_callbacks_each_become_their_own_node() {
        let src = r#"
export const outer = async () => {
  pool.on('error', (error) => {
    resetPoolLater(pool, error);
  });

  client.release = (error) => {
    resetPoolLater(pool, error);
  };

  const noteError = (error) => {
    resetPoolLater(pool, error);
  };

  try {
    await run();
  } catch (err) {
    resetPoolLater(pool, err);
    throw err;
  }
};
"#;
        let functions = extract(src, false).unwrap();
        let triggers: Vec<_> = functions
            .iter()
            .filter(|f| f.calls.iter().any(|c| c == "resetPoolLater"))
            .collect();
        assert_eq!(triggers.len(), 4, "{functions:#?}");

        let outer = functions
            .iter()
            .find(|f| f.name.as_deref() == Some("outer"))
            .unwrap();
        assert!(outer.exported);
        // The catch-block call belongs to `outer` directly: a catch clause
        // is not a function boundary.
        assert!(outer.calls.contains(&"resetPoolLater".to_string()));
    }

    #[test]
    fn property_assignment_names_the_callback_after_the_property() {
        let src = "client.release = (error) => { done(); };";
        let functions = extract(src, false).unwrap();
        assert_eq!(functions[0].name.as_deref(), Some("release"));
        assert!(!functions[0].exported);
    }

    #[test]
    fn member_call_chains_join_with_dots() {
        let src = "const f = () => { logger.warn({ err }, 'msg'); };";
        let functions = extract(src, false).unwrap();
        assert_eq!(functions[0].calls, vec!["logger.warn".to_string()]);
    }

    #[test]
    fn a_chained_call_collapses_instead_of_swallowing_its_callback_body() {
        let src =
            "const f = () => { drain().catch((e) => { logger.warn(e); }).finally(cleanup); };";
        let functions = extract(src, false).unwrap();
        // Each level of the chain is its own call_expression, visited
        // outer-to-inner; none of them include the callback's body text.
        assert_eq!(
            functions[0].calls,
            vec![
                "drain(...).catch(...).finally".to_string(),
                "drain(...).catch".to_string(),
                "drain".to_string(),
            ]
        );
    }

    #[test]
    fn identical_bodies_hash_the_same_regardless_of_indentation() {
        let a = extract("const f = () => { g(1); };", false).unwrap();
        let b = extract("const h = () => {\n  g(1);\n};", false).unwrap();
        assert_eq!(a[0].body_hash, b[0].body_hash);
    }
}
