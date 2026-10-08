//! Reads `compilerOptions.paths` so a bare import through an alias resolves
//! to the same file the TypeScript compiler would land on.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;

#[derive(Debug, Default, Clone)]
pub struct TsConfig {
    /// Directory `baseUrl` and `paths` are resolved relative to.
    pub base_dir: PathBuf,
    pub base_url: PathBuf,
    /// Alias pattern to its target patterns, both with the `*` still in
    /// them (e.g. `"@app/*"` -> `["src/*"]`), exactly as tsconfig writes it.
    pub paths: HashMap<String, Vec<String>>,
}

#[derive(Deserialize, Default)]
struct RawTsConfig {
    #[serde(default)]
    extends: Option<String>,
    #[serde(rename = "compilerOptions", default)]
    compiler_options: RawCompilerOptions,
}

#[derive(Deserialize, Default)]
struct RawCompilerOptions {
    #[serde(rename = "baseUrl", default)]
    base_url: Option<String>,
    #[serde(default)]
    paths: HashMap<String, Vec<String>>,
}

/// Loads the tsconfig nearest to `from_dir`, walking up to `root`, and
/// follows one `extends` chain (tsconfig.base.json and friends) merging
/// child over parent. Good enough for the layouts real monorepos use;
/// project references beyond `extends` are a fine-layer concern.
pub fn load_nearest(root: &Path, from_dir: &Path) -> TsConfig {
    let mut dir = from_dir;
    loop {
        let candidate = dir.join("tsconfig.json");
        if candidate.is_file() {
            return load_chain(&candidate);
        }
        if dir == root || dir.parent().is_none() {
            break;
        }
        match dir.strip_prefix(root) {
            Ok(rel) if rel.as_os_str().is_empty() => break,
            _ => {}
        }
        let Some(parent) = dir.parent() else { break };
        dir = parent;
    }
    TsConfig::default()
}

fn load_chain(path: &Path) -> TsConfig {
    let Ok(text) = std::fs::read_to_string(path) else {
        return TsConfig::default();
    };
    let Ok(raw) = serde_json::from_str::<RawTsConfig>(strip_jsonc_comments(&text).as_str()) else {
        return TsConfig::default();
    };
    let dir = path.parent().unwrap_or(Path::new(".")).to_path_buf();

    let mut merged = match &raw.extends {
        Some(ext) => {
            let parent_path = dir.join(ext);
            let parent_path = if parent_path.extension().is_none() {
                parent_path.with_extension("json")
            } else {
                parent_path
            };
            load_chain(&parent_path)
        }
        None => TsConfig::default(),
    };

    let base_url = raw
        .compiler_options
        .base_url
        .map(|b| dir.join(b))
        .unwrap_or_else(|| merged.base_url.clone());
    if !raw.compiler_options.paths.is_empty() {
        merged.paths = raw.compiler_options.paths;
    }
    merged.base_dir = dir;
    merged.base_url = if base_url.as_os_str().is_empty() {
        merged.base_dir.clone()
    } else {
        base_url
    };
    merged
}

/// tsconfig.json is commonly JSONC (comments, trailing commas). This strips
/// `//` and `/* */` comments outside of strings; trailing commas are left
/// for serde_json to reject on, which in practice real configs avoid.
fn strip_jsonc_comments(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    let mut in_string = false;
    while let Some(c) = chars.next() {
        if in_string {
            out.push(c);
            if c == '\\' {
                if let Some(next) = chars.next() {
                    out.push(next);
                }
            } else if c == '"' {
                in_string = false;
            }
            continue;
        }
        match c {
            '"' => {
                in_string = true;
                out.push(c);
            }
            '/' if chars.peek() == Some(&'/') => {
                for c in chars.by_ref() {
                    if c == '\n' {
                        out.push('\n');
                        break;
                    }
                }
            }
            '/' if chars.peek() == Some(&'*') => {
                chars.next();
                let mut prev = ' ';
                for c in chars.by_ref() {
                    if prev == '*' && c == '/' {
                        break;
                    }
                    prev = c;
                }
            }
            _ => out.push(c),
        }
    }
    out
}

impl TsConfig {
    /// Every file `specifier` could mean, most specific alias first. The
    /// caller tries each against the filesystem; `paths` entries are
    /// listed in priority order, same as the compiler resolves them.
    pub fn resolve_alias(&self, specifier: &str) -> Vec<PathBuf> {
        let mut out = Vec::new();
        for (pattern, targets) in &self.paths {
            let Some(prefix) = pattern.strip_suffix('*') else {
                if pattern == specifier {
                    for t in targets {
                        out.push(self.base_url.join(t));
                    }
                }
                continue;
            };
            let Some(rest) = specifier.strip_prefix(prefix) else {
                continue;
            };
            for t in targets {
                let target = t.strip_suffix('*').unwrap_or(t);
                out.push(self.base_url.join(format!("{target}{rest}")));
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_line_and_block_comments_outside_strings() {
        let src = "{ // a\n  \"a\": 1, /* b */ \"b\": \"//not a comment\" }";
        let stripped = strip_jsonc_comments(src);
        let v: serde_json::Value = serde_json::from_str(&stripped).unwrap();
        assert_eq!(v["a"], 1);
        assert_eq!(v["b"], "//not a comment");
    }

    #[test]
    fn resolves_star_alias_against_base_url() {
        let mut paths = HashMap::new();
        paths.insert("@app/*".to_string(), vec!["src/*".to_string()]);
        let cfg = TsConfig {
            base_dir: PathBuf::from("root"),
            base_url: PathBuf::from("root"),
            paths,
        };
        let out = cfg.resolve_alias("@app/shared/client");
        assert_eq!(out, vec![PathBuf::from("root/src/shared/client")]);
    }
}
