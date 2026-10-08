//! The building blocks rules are evaluated with: what a finding is, which
//! lines a diff added, SQL split into statements, and where an `await`
//! sits relative to its loop. The rules themselves are data — see
//! [`crate::rules`] and the presets under `presets/`.

use std::collections::HashSet;

use regex::Regex;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    High,
    Medium,
    Low,
}

impl Severity {
    pub fn as_str(self) -> &'static str {
        match self {
            Severity::High => "high",
            Severity::Medium => "medium",
            Severity::Low => "low",
        }
    }
}

/// One thing the plan warns about.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Finding {
    /// The rule's id: `await-in-loop`, `migration-drop`, a custom name…
    pub rule: String,
    pub severity: Severity,
    /// One line saying what is risky, for a person.
    pub title: String,
    pub path: String,
    /// 1-based, in the head revision.
    pub line: u32,
    /// The changed function it concerns, when there is one (an id from
    /// the plan's `changed`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub function: Option<String>,
    /// The source line, trimmed.
    pub excerpt: String,
}

/// Lines of `head` that the diff from `base` added or rewrote.
pub fn added_lines(base: &str, head: &str) -> HashSet<u32> {
    let diff = similar::TextDiff::from_lines(base, head);
    let mut out = HashSet::new();
    for change in diff.iter_all_changes() {
        if change.tag() == similar::ChangeTag::Insert
            && let Some(i) = change.new_index()
        {
            out.insert(i as u32 + 1);
        }
    }
    out
}

const EXITS: &[&str] = &["return_statement", "break_statement", "throw_statement"];

/// Whether `node`'s subtree holds `kinds`, not looking into nested
/// functions (nor, for `break`, into nested loops and switches).
fn contains_exit(node: tree_sitter::Node) -> bool {
    let mut stack = vec![(node, false)];
    while let Some((n, in_inner_loop)) = stack.pop() {
        let k = n.kind();
        if k == "return_statement" || k == "throw_statement" {
            return true;
        }
        if k == "break_statement" && !in_inner_loop {
            return true;
        }
        for i in 0..n.child_count() {
            let Some(c) = n.child(i as u32) else { continue };
            if FUNCTION_KINDS.contains(&c.kind()) {
                continue;
            }
            let inner = in_inner_loop
                || matches!(
                    c.kind(),
                    "for_statement" | "for_in_statement" | "while_statement" | "do_statement" | "switch_statement"
                );
            stack.push((c, inner));
        }
    }
    false
}

/// An await on the way out: inside a `return`/`throw`, or in a statement
/// that a `return`/`break`/`throw` follows directly in the same block.
fn leaves_loop(await_node: tree_sitter::Node, body: tree_sitter::Node) -> bool {
    let mut cur = await_node;
    while let Some(parent) = cur.parent() {
        if cur.id() == body.id() {
            return false;
        }
        if cur.kind() == "return_statement" || cur.kind() == "throw_statement" {
            return true;
        }
        if parent.kind() == "statement_block" || parent.id() == body.id() {
            let mut next = cur.next_named_sibling();
            while let Some(n) = next {
                if EXITS.contains(&n.kind()) {
                    return true;
                }
                if n.kind() != "comment" {
                    break;
                }
                next = n.next_named_sibling();
            }
        }
        cur = parent;
    }
    false
}

const FUNCTION_KINDS: &[&str] = &[
    "function_declaration",
    "function_expression",
    "arrow_function",
    "method_definition",
    "generator_function",
    "generator_function_declaration",
];

/// `--` and `/* */` comments blanked, newlines kept so lines still count.
pub fn strip_sql_comments(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let b = text.as_bytes();
    let mut i = 0;
    let mut in_str = false;
    while i < b.len() {
        let c = b[i] as char;
        if in_str {
            out.push(c);
            if c == '\'' {
                in_str = false;
            }
            i += 1;
            continue;
        }
        if c == '\'' {
            in_str = true;
            out.push(c);
            i += 1;
        } else if c == '-' && b.get(i + 1) == Some(&b'-') {
            while i < b.len() && b[i] != b'\n' {
                out.push(' ');
                i += 1;
            }
        } else if c == '/' && b.get(i + 1) == Some(&b'*') {
            while i < b.len() && !(b[i] == b'*' && b.get(i + 1) == Some(&b'/')) {
                out.push(if b[i] == b'\n' { '\n' } else { ' ' });
                i += 1;
            }
            out.push_str("  ");
            i += 2;
        } else {
            // Keep multi-byte characters intact.
            let ch = text[i..].chars().next().unwrap_or(' ');
            out.push(ch);
            i += ch.len_utf8();
        }
    }
    out
}

/// One SQL statement and the line it starts on.
pub fn statements(text: &str) -> Vec<(u32, String)> {
    let text = strip_sql_comments(text);
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut start_line = 1u32;
    let mut line = 1u32;
    let mut in_str = false;
    let mut dollar: Option<String> = None;
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if cur.trim().is_empty() && !c.is_whitespace() {
            start_line = line;
        }
        if c == '\n' {
            line += 1;
        }
        if let Some(tag) = &dollar {
            cur.push(c);
            if c == '$' && cur.ends_with(tag.as_str()) && cur.len() > tag.len() {
                dollar = None;
            }
            i += 1;
            continue;
        }
        if in_str {
            cur.push(c);
            if c == '\'' {
                in_str = false;
            }
            i += 1;
            continue;
        }
        match c {
            '\'' => {
                in_str = true;
                cur.push(c);
            }
            '$' => {
                // `$tag$ … $tag$`, `$$ … $$` — a function body.
                let mut j = i + 1;
                while j < chars.len() && (chars[j].is_alphanumeric() || chars[j] == '_') {
                    j += 1;
                }
                if j < chars.len() && chars[j] == '$' {
                    let tag: String = chars[i..=j].iter().collect();
                    cur.push_str(&tag);
                    dollar = Some(tag);
                    i = j + 1;
                    continue;
                }
                cur.push(c);
            }
            ';' => {
                if !cur.trim().is_empty() {
                    out.push((start_line, cur.trim().to_string()));
                }
                cur.clear();
            }
            _ => cur.push(c),
        }
        i += 1;
    }
    if !cur.trim().is_empty() {
        out.push((start_line, cur.trim().to_string()));
    }
    out
}

/// The forward half of a migration: everything before a down marker
/// (`-- Down Migration`, `-- migrate:down`, `-- +goose Down`,
/// `-- +migrate Down`) — what follows runs on rollback only. Line
/// numbers are unchanged.
pub fn forward_half(text: &str) -> String {
    let down = Regex::new(r"(?i)^\s*--\s*(?:migrate:down|\+migrate\s+down|\+goose\s+down|down\s+migration)\b")
        .expect("valid regex");
    let mut out = String::new();
    for l in text.lines() {
        if down.is_match(l) {
            break;
        }
        out.push_str(l);
        out.push('\n');
    }
    out
}

/// The table a DDL statement acts on: the word after `TABLE` (past
/// `IF EXISTS` / `ONLY`), lowercased and unquoted.
pub fn target_table(stmt: &str) -> Option<String> {
    let words: Vec<&str> = stmt.split_whitespace().collect();
    let i = words.iter().position(|w| w.eq_ignore_ascii_case("table"))?;
    let mut j = i + 1;
    while words.get(j).is_some_and(|w| {
        w.eq_ignore_ascii_case("if") || w.eq_ignore_ascii_case("not") || w.eq_ignore_ascii_case("exists") || w.eq_ignore_ascii_case("only")
    }) {
        j += 1;
    }
    words
        .get(j)
        .map(|w| w.trim_matches(|c| c == '"' || c == '(').to_ascii_lowercase())
}

/// For an `await` (or any node): whether it runs once per iteration of
/// an enclosing loop in the same function — `Some(stops_early)` if so,
/// where `stops_early` says the loop can leave early (a `return` or
/// `break` in its body: attempts, fallbacks, reading until done).
/// `None` for an await outside any loop, in a nested callback, in a
/// loop's header, in a `for await` body, or on the way out of the loop
/// (inside `return`/`throw`, or followed in its block by
/// `return`/`break`/`throw`), which runs at most once.
pub fn per_loop_iteration(node: tree_sitter::Node) -> Option<bool> {
    let mut child = node;
    let mut cur = node.parent();
    while let Some(p) = cur {
        if FUNCTION_KINDS.contains(&p.kind()) {
            return None;
        }
        if matches!(
            p.kind(),
            "for_statement" | "for_in_statement" | "while_statement" | "do_statement"
        ) && let Some(body) = p.child_by_field_name("body")
            && body.id() == child.id()
        {
            let for_await = p.kind() == "for_in_statement"
                && (0..p.child_count())
                    .filter_map(|i| p.child(i as u32))
                    .any(|c| c.kind() == "await");
            if for_await || leaves_loop(node, body) {
                return None;
            }
            return Some(contains_exit(body));
        }
        child = p;
        cur = p.parent();
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn statements_keep_their_first_line_and_skip_dollar_bodies() {
        let sql = "-- c\nSELECT 1;\nCREATE FUNCTION f() RETURNS int AS $$ SELECT 1; $$ LANGUAGE sql;\nALTER TABLE t\n  ADD COLUMN x int;\n";
        let st = statements(sql);
        assert_eq!(st.iter().map(|(l, _)| *l).collect::<Vec<_>>(), vec![2, 3, 4]);
        assert!(st[1].1.contains("$$ SELECT 1; $$"));
    }

    #[test]
    fn the_forward_half_stops_at_a_down_marker() {
        let sql = "-- Up Migration\nALTER TABLE t ADD COLUMN x int;\n-- Down Migration\nALTER TABLE t DROP COLUMN x;\n";
        assert_eq!(forward_half(sql), "-- Up Migration\nALTER TABLE t ADD COLUMN x int;\n");
        assert_eq!(target_table("ALTER TABLE IF EXISTS \"Orders\" ADD x int"), Some("orders".into()));
    }
}
