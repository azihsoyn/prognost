//! Pulls the specifier string out of every import-shaped statement in a
//! JS/TS file. This is the coarse layer's other half — cheap text scanning
//! rather than a real parser, on the theory that a stray match inside a
//! comment or string costs nothing (the specifier still has to resolve to
//! a real file to become an edge) and a real parser costs a dependency and
//! startup time this layer is meant not to have.

use std::sync::LazyLock;

static SPECIFIER: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(
        r#"(?x)
        (?:
            \bfrom\s*|
            \brequire\s*\(\s*|
            \bimport\s*\(\s*|
            \bimport\s+
        )
        (?:
            "(?P<dq>[^"\n]+)" |
            '(?P<sq>[^'\n]+)'
        )
        "#,
    )
    .expect("static regex")
});

/// Every specifier a source file imports from, in the order they appear.
/// Duplicates are kept: callers that care about uniqueness dedupe after
/// resolving, once specifiers become file paths.
pub fn scan(source: &str) -> Vec<String> {
    SPECIFIER
        .captures_iter(source)
        .map(|c| {
            c.name("dq")
                .or_else(|| c.name("sq"))
                .unwrap()
                .as_str()
                .to_string()
        })
        .collect()
}

pub const RESOLVABLE_EXTENSIONS: &[&str] = &["ts", "tsx", "js", "jsx", "mjs", "cjs", "mts", "cts", "svelte"];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_named_default_and_side_effect_imports() {
        let src = r#"
            import pg from "pg";
            import { getPool } from '../shared/client';
            import './side-effect.css';
            export * from "./reexport";
            const x = require("./legacy");
            const y = await import("./lazy");
        "#;
        assert_eq!(
            scan(src),
            vec![
                "pg",
                "../shared/client",
                "./side-effect.css",
                "./reexport",
                "./legacy",
                "./lazy"
            ]
        );
    }

    #[test]
    fn ignores_from_when_there_is_no_following_quote() {
        assert_eq!(scan("const from = 1; from(x);"), Vec::<String>::new());
    }
}
