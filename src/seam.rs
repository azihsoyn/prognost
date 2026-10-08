//! Seams: the places where a call and its target are joined by
//! something other than an import — a route path a frontend client
//! spells out and a router registers, an event name emitted here and
//! handled there, a queue name, a DI token. Both sides carry the same
//! string, so a seam is a pair of patterns: one that pulls the key out
//! of a *call site*, one that pulls it out of a *definition*. Every
//! call site whose key matches a definition's is an edge from the
//! function around the call to the function around the definition.
//!
//! Built-in presets (Hono lives in [`crate::hono`], with its own mount
//! logic) cover what the tool knows; a repository adds its own in
//! `prognost.toml`:
//!
//! ```toml
//! [[seam]]
//! name = "domain events"
//! call = '''publish\(\s*['"]([^'"]+)['"]'''
//! definition = '''subscribe\(\s*['"]([^'"]+)['"]'''
//! ```
//!
//! Capture groups form the key (joined with a space when there are
//! several); a `:param`-style segment matches any other in a `/`-path.
//! TOML's `'''…'''` strings take the regex as written, quotes included.

use std::path::{Path, PathBuf};

use regex::Regex;
use serde::Deserialize;

/// A definition side: `key` at `file:line`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Def {
    pub key: String,
    pub file: PathBuf,
    pub line: u32,
}

/// A call side: `key` at `file:line`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallSite {
    pub key: String,
    pub file: PathBuf,
    pub line: u32,
}

/// One seam: how to find its definitions and its call sites in a file,
/// and when two keys mean the same thing.
pub trait Seam {
    fn name(&self) -> &str;
    fn scan(&self, file: &Path, text: &str) -> (Vec<Def>, Vec<CallSite>);
    fn same_key(&self, a: &str, b: &str) -> bool {
        same_key(a, b)
    }
}

/// Keys equal, or equal as `/`-paths with any parameter segment
/// (`:id`, `{id}`, `[id]`) matching any other.
pub fn same_key(a: &str, b: &str) -> bool {
    if a == b {
        return true;
    }
    if !(a.contains('/') && b.contains('/')) {
        return false;
    }
    let is_param = |s: &str| {
        s.starts_with(':')
            || (s.starts_with('{') && s.ends_with('}'))
            || (s.starts_with('[') && s.ends_with(']'))
    };
    let sa: Vec<&str> = a.split('/').filter(|s| !s.is_empty()).collect();
    let sb: Vec<&str> = b.split('/').filter(|s| !s.is_empty()).collect();
    sa.len() == sb.len()
        && sa
            .iter()
            .zip(&sb)
            .all(|(x, y)| x == y || (is_param(x) && is_param(y)) || *x == "*" || *y == "*")
}

/// A seam defined by two regular expressions.
#[derive(Debug, Clone)]
pub struct StringKeySeam {
    pub name: String,
    call: Regex,
    definition: Regex,
}

impl StringKeySeam {
    pub fn new(name: &str, call: &str, definition: &str) -> anyhow::Result<Self> {
        Ok(Self {
            name: name.to_string(),
            call: Regex::new(call)?,
            definition: Regex::new(definition)?,
        })
    }
}

fn key_of(caps: &regex::Captures) -> String {
    let parts: Vec<&str> = (1..caps.len())
        .filter_map(|i| caps.get(i).map(|m| m.as_str().trim()))
        .filter(|s| !s.is_empty())
        .collect();
    if parts.is_empty() {
        caps.get(0).map(|m| m.as_str()).unwrap_or("").to_string()
    } else {
        parts.join(" ")
    }
}

fn line_of(text: &str, byte: usize) -> u32 {
    text[..byte].matches('\n').count() as u32 + 1
}

impl Seam for StringKeySeam {
    fn name(&self) -> &str {
        &self.name
    }

    fn scan(&self, file: &Path, text: &str) -> (Vec<Def>, Vec<CallSite>) {
        let defs = self
            .definition
            .captures_iter(text)
            .map(|c| Def {
                key: key_of(&c),
                file: file.to_path_buf(),
                line: line_of(text, c.get(0).unwrap().start()),
            })
            .collect();
        let calls = self
            .call
            .captures_iter(text)
            .map(|c| CallSite {
                key: key_of(&c),
                file: file.to_path_buf(),
                line: line_of(text, c.get(0).unwrap().start()),
            })
            .collect();
        (defs, calls)
    }
}

#[derive(Debug, Deserialize)]
struct Config {
    #[serde(default)]
    seam: Vec<SeamConfig>,
}

#[derive(Debug, Deserialize)]
struct SeamConfig {
    name: String,
    call: String,
    definition: String,
}

/// The repository's prognost configuration (seams, risk rules): the file
/// `PROGNOST_CONFIG` (or, as before, `PROGNOST_SEAMS`) points at, else
/// `prognost.toml` at its root — so a repository that doesn't carry the
/// file can still be given one from outside.
pub fn config_path(root: &Path) -> PathBuf {
    for var in ["PROGNOST_CONFIG", "PROGNOST_SEAMS"] {
        if let Ok(p) = std::env::var(var)
            && !p.is_empty()
        {
            return PathBuf::from(p);
        }
    }
    root.join("prognost.toml")
}

/// The seams a repository declares in its configuration. No file, no
/// seams.
pub fn load(root: &Path) -> anyhow::Result<Vec<StringKeySeam>> {
    let path = config_path(root);
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Ok(Vec::new());
    };
    parse(&text).map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))
}

pub fn parse(text: &str) -> anyhow::Result<Vec<StringKeySeam>> {
    let config: Config = toml::from_str(text)?;
    config
        .seam
        .iter()
        .map(|s| StringKeySeam::new(&s.name, &s.call, &s.definition))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_string_key_seam_joins_emit_to_on() {
        let seam = StringKeySeam::new(
            "events",
            r#"emit\(\s*['"]([^'"]+)['"]"#,
            r#"on\(\s*['"]([^'"]+)['"]"#,
        )
        .unwrap();
        let src = "bus.on('order.created', handle);\n\nfunction place() {\n  bus.emit('order.created', o);\n}\n";
        let (defs, calls) = seam.scan(Path::new("a.ts"), src);
        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0].key, "order.created");
        assert_eq!(defs[0].line, 1);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].line, 4);
        assert!(seam.same_key(&defs[0].key, &calls[0].key));
    }

    #[test]
    fn keys_compare_as_paths_with_params() {
        assert!(same_key(
            "GET /api/v1/orders/:orderId",
            "GET /api/v1/orders/:id"
        ));
        assert!(same_key("/x/{id}", "/x/[slug]"));
        assert!(!same_key("/x/a", "/x/a/b"));
        assert!(!same_key("order.created", "order.deleted"));
    }

    #[test]
    fn config_parses_seams() {
        let seams = parse(
            r#"
[[seam]]
name = "queues"
call = '''sendToQueue\(\s*["']([^"']+)'''
definition = '''consumeQueue\(\s*["']([^"']+)'''
"#,
        )
        .unwrap();
        assert_eq!(seams.len(), 1);
        assert_eq!(seams[0].name, "queues");
        assert!(parse("[[seam]]\nname = 'x'\ncall = '('\ndefinition = 'y'").is_err());
    }
}
