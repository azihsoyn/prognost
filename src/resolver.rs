//! External call resolvers: a command a repository names in its
//! `prognost.toml` that prints the calls of one revision, for what
//! prognost's own resolution cannot see — calls a type checker resolves
//! (`golang.org/x/tools/cmd/callgraph`, a Python call-graph tool).
//!
//! ```toml
//! [[resolver]]
//! language = "go"
//! command = ["sh", "-c", "callgraph -algo=vta -format='{{.Caller.Pos}} {{.Callee.Pos}}' ./..."]
//! ```
//!
//! The command runs once per revision, in that revision's tree (the
//! working tree, or the commit extracted to prognost's cache), with
//! `PROGNOST_REVISION` (`base` or `head`), `PROGNOST_TREE` (that
//! directory) and `PROGNOST_ROOT` (the working tree) set. It prints one
//! call per line: where the call is made and where the called function
//! is, each as `path:line` (or `path:line:column`), separated by
//! whitespace — or a tab, when paths hold spaces. Paths are relative to
//! the tree or absolute inside it; other lines are skipped. A commit's
//! output is kept in the cache with its tree.
//!
//! Where it reports a call, that answer is taken over prognost's own;
//! where it reports none, prognost resolves the call as it would
//! without it.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::lang::Lang;

#[derive(Debug, Clone)]
pub struct Resolver {
    pub language: Lang,
    pub command: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct Config {
    #[serde(default)]
    resolver: Vec<ResolverConfig>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ResolverConfig {
    language: String,
    command: Vec<String>,
}

/// The resolvers in the repository's configuration
/// ([`crate::seam::config_path`]).
pub fn load(root: &Path) -> Result<Vec<Resolver>> {
    let path = crate::seam::config_path(root);
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Ok(Vec::new());
    };
    parse(&text).with_context(|| path.display().to_string())
}

pub fn parse(text: &str) -> Result<Vec<Resolver>> {
    let config: Config = toml::from_str(text)?;
    config
        .resolver
        .into_iter()
        .map(|r| {
            let language = match r.language.as_str() {
                "typescript" => Lang::TypeScript,
                "go" => Lang::Go,
                "python" => Lang::Python,
                other => {
                    bail!("resolver: unknown language {other:?} (known: typescript, go, python)")
                }
            };
            if r.command.is_empty() {
                bail!("resolver for {}: empty command", r.language);
            }
            Ok(Resolver {
                language,
                command: r.command,
            })
        })
        .collect()
}

/// The calls a resolver reported for one revision, repo-relative.
#[derive(Debug, Default)]
pub struct Calls {
    /// (file, line of the call) → (file, line) of each function called.
    pub by_site: HashMap<(PathBuf, u32), Vec<(PathBuf, u32)>>,
    /// A file → the files with calls into it.
    pub into: HashMap<PathBuf, BTreeSet<PathBuf>>,
}

/// Runs `resolver` in `tree` for one revision (`"base"`/`"head"`). With
/// `commit`, the output is cached under that commit. A failing command
/// is reported on stderr and reports no calls: prognost carries on with
/// its own resolution.
pub fn run(
    resolver: &Resolver,
    tree: &Path,
    root: &Path,
    revision: &str,
    commit: Option<&str>,
) -> Calls {
    let cached = commit.map(|sha| {
        crate::rev::cache_dir()
            .join("resolvers")
            .join(format!("{sha}-{:016x}.txt", fingerprint(resolver)))
    });
    if let Some(path) = &cached
        && let Ok(text) = std::fs::read_to_string(path)
    {
        return parse_output(&text, tree);
    }
    let out = Command::new(&resolver.command[0])
        .args(&resolver.command[1..])
        .current_dir(tree)
        .env("PROGNOST_REVISION", revision)
        .env("PROGNOST_TREE", tree)
        .env("PROGNOST_ROOT", root)
        .output();
    let text = match out {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).into_owned(),
        Ok(o) => {
            eprintln!(
                "prognost: resolver {:?} ({revision}) failed ({}): {}",
                resolver.command[0],
                o.status,
                String::from_utf8_lossy(&o.stderr)
                    .lines()
                    .next()
                    .unwrap_or("")
                    .trim()
            );
            return Calls::default();
        }
        Err(e) => {
            eprintln!(
                "prognost: resolver {:?} ({revision}): {e}",
                resolver.command[0]
            );
            return Calls::default();
        }
    };
    if let Some(path) = cached {
        let _ = std::fs::create_dir_all(path.parent().unwrap_or(Path::new(".")));
        let _ = std::fs::write(path, &text);
    }
    parse_output(&text, tree)
}

/// A stable hash of what the resolver is, so a changed command
/// doesn't reuse an old answer.
fn fingerprint(r: &Resolver) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    let lang = format!("{:?}", r.language);
    for part in std::iter::once(lang.as_str()).chain(r.command.iter().map(String::as_str)) {
        for b in part.bytes().chain([0]) {
            h ^= b as u64;
            h = h.wrapping_mul(0x100000001b3);
        }
    }
    h
}

pub fn parse_output(text: &str, tree: &Path) -> Calls {
    let prefixes: Vec<PathBuf> = [Some(tree.to_path_buf()), tree.canonicalize().ok()]
        .into_iter()
        .flatten()
        .collect();
    let mut calls = Calls::default();
    for line in text.lines() {
        let fields: Vec<&str> = if line.contains('\t') {
            line.split('\t').map(str::trim).collect()
        } else {
            line.split_whitespace().collect()
        };
        let [from, to, ..] = fields.as_slice() else {
            continue;
        };
        let (Some(from), Some(to)) = (position(from, &prefixes), position(to, &prefixes)) else {
            continue;
        };
        calls
            .into
            .entry(to.0.clone())
            .or_default()
            .insert(from.0.clone());
        let targets = calls.by_site.entry(from).or_default();
        if !targets.contains(&to) {
            targets.push(to);
        }
    }
    calls
}

/// `path:line[:column]` → (repo-relative path, line).
fn position(field: &str, prefixes: &[PathBuf]) -> Option<(PathBuf, u32)> {
    let mut parts = field.rsplitn(3, ':');
    let (a, b) = (parts.next()?, parts.next()?);
    let (path, line) = match (parts.next(), b.parse::<u32>()) {
        (Some(path), Ok(line)) if a.parse::<u32>().is_ok() => (path, line),
        _ => (field.rsplit_once(':')?.0, a.parse::<u32>().ok()?),
    };
    let path = Path::new(path);
    let rel = if path.is_absolute() {
        prefixes
            .iter()
            .find_map(|p| path.strip_prefix(p).ok())?
            .to_path_buf()
    } else {
        path.strip_prefix("./").unwrap_or(path).to_path_buf()
    };
    (line > 0 && !rel.as_os_str().is_empty()).then_some((rel, line))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_positions_relative_or_inside_the_tree() {
        let tree = Path::new("/work/shop");
        let calls = parse_output(
            "/work/shop/orders/service.go:10:18 /work/shop/postgres/store.go:5:18\n\
             orders/service.go:10 memory/store.go:5\n\
             /elsewhere/go/src/fmt/print.go:1 orders/service.go:3\n\
             - -\n\
             noise\n",
            tree,
        );
        assert_eq!(
            calls.by_site[&(PathBuf::from("orders/service.go"), 10)],
            vec![
                (PathBuf::from("postgres/store.go"), 5),
                (PathBuf::from("memory/store.go"), 5)
            ]
        );
        assert_eq!(calls.by_site.len(), 1);
        assert!(
            calls.into[Path::new("postgres/store.go")].contains(Path::new("orders/service.go"))
        );
    }

    #[test]
    fn config_names_language_and_command() {
        let r = parse("[[resolver]]\nlanguage = \"go\"\ncommand = [\"callgraph\", \"./...\"]\n")
            .unwrap();
        assert_eq!(r[0].language, Lang::Go);
        assert!(parse("[[resolver]]\nlanguage = \"cobol\"\ncommand = [\"x\"]\n").is_err());
        assert!(parse("[[risk]]\nname = \"x\"\n").unwrap().is_empty());
    }
}
