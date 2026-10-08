//! Rules for `prognost assess`, written as data.
//!
//! A rule is a `[[risk]]` table. Built-in rules ship as presets
//! (`presets/*.toml`, compiled in); a repository picks presets and adds
//! or overrides rules in its `prognost.toml` (or the file
//! `PROGNOST_CONFIG` names):
//!
//! ```toml
//! presets = ["graph", "typescript", "postgres"]   # the default: all of them
//! disable = ["wide-reach"]                        # drop rules by name
//!
//! [[risk]]                                        # same name as a preset rule: replaces it
//! name = "rls-bypass"
//! kind = "line"
//! pattern = '''runWithAdminPrivileges'''
//! severity = "high"
//! title = "query runs with row-level security bypassed"
//! ```
//!
//! Four kinds, each judging something different:
//!
//! - `symbol`: a changed function, by facts from the call graph
//!   (`where = ["exported", "importing_packages >= 2"]`).
//! - `line`: a regex on the lines the diff added (`scope = "added"`), or
//!   on any line of a function the change reaches (`scope = "reach"`).
//! - `sql`: a regex on whole SQL statements of migrations, the ones the
//!   diff added, forward half only.
//! - `ast`: a tree-sitter query on TypeScript, matches on added lines,
//!   optionally narrowed by a named filter (`per-loop-iteration`).
//!
//! Everything is deterministic: the same diff always gives the same
//! findings.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, anyhow, bail};
use regex::Regex;
use serde::{Deserialize, Serialize};
use tree_sitter::StreamingIterator;

use crate::risk::{self, Finding, Severity};

/// The presets compiled into the binary, by name.
pub const PRESETS: &[(&str, &str)] = &[
    ("graph", include_str!("../presets/graph.toml")),
    ("typescript", include_str!("../presets/typescript.toml")),
    ("postgres", include_str!("../presets/postgres.toml")),
];

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Symbol,
    #[default]
    Line,
    Sql,
    Ast,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Scope {
    #[default]
    Added,
    Reach,
}

/// One `[[risk]]` table as written.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RuleConfig {
    pub name: String,
    #[serde(default)]
    pub kind: Kind,
    /// What a hit means. `{placeholders}`: named regex captures; for
    /// symbols `{name}`, `{route}`, `{importing_packages}`,
    /// `{importing_packages_list}`, `{called_from}`, `{reach_functions}`,
    /// `{reach_packages}`, `{entries}`; for lines `{in_function}`.
    pub title: String,
    #[serde(default = "default_severity")]
    pub severity: Severity,
    /// Fold findings of this rule in one file into one line.
    #[serde(default)]
    pub fold: bool,
    /// Only files whose repo-relative path matches. `sql` rules default
    /// to `.sql` files under a directory named `migration…`.
    #[serde(default)]
    pub paths: Option<String>,
    /// `line`, `sql`: the regex. `ast`: optional regex the hit's line
    /// must match.
    #[serde(default)]
    pub pattern: Option<String>,
    /// `line`, `sql`: skip a match whose text matches this.
    #[serde(default)]
    pub unless: Option<String>,
    /// `line`: added lines (default) or the whole reach.
    #[serde(default)]
    pub scope: Scope,
    /// `symbol`: conditions that must all hold.
    #[serde(default, rename = "where")]
    pub conditions: Vec<String>,
    /// `sql`: a named capture whose value must be in a `[[set]]`.
    #[serde(default)]
    pub in_set: Option<InSet>,
    /// `sql`: skip statements on a table the same file creates (default true).
    #[serde(default = "yes")]
    pub skip_new_tables: bool,
    /// `ast`: the language (`typescript`).
    #[serde(default)]
    pub language: Option<String>,
    /// `ast`: a tree-sitter query; the `@hit` capture (else the first) is reported.
    #[serde(default)]
    pub query: Option<String>,
    /// `ast`: built-in narrowing: `per-loop-iteration`.
    #[serde(default)]
    pub filters: Vec<String>,
    /// Exceptions that keep the hit but change how bad it is: the first
    /// whose `pattern` matches the line, or whose `when` holds, wins.
    #[serde(default, rename = "variant")]
    pub variants: Vec<VariantConfig>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct InSet {
    pub capture: String,
    pub set: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct VariantConfig {
    #[serde(default)]
    pub pattern: Option<String>,
    /// A fact a filter established: `loop-stops-early`.
    #[serde(default)]
    pub when: Option<String>,
    pub severity: Severity,
    pub title: String,
}

/// Values collected from the head revision: every capture named `name`
/// (else group 1) of `patterns` in files matching `paths`, lowercased.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SetConfig {
    pub name: String,
    pub paths: String,
    pub patterns: Vec<String>,
}

fn default_severity() -> Severity {
    Severity::Medium
}

fn yes() -> bool {
    true
}

#[derive(Debug, Default, Deserialize)]
struct FileConfig {
    #[serde(default)]
    presets: Option<Vec<String>>,
    #[serde(default)]
    disable: Vec<String>,
    #[serde(default)]
    set: Vec<SetConfig>,
    #[serde(default)]
    risk: Vec<RuleConfig>,
    // The seams live in the same file; they're read elsewhere.
    #[serde(default)]
    #[allow(dead_code)]
    seam: Vec<toml::Value>,
}

/// A rule ready to evaluate.
pub struct Rule {
    pub config: RuleConfig,
    /// Where it came from: `preset:graph`, or the config file.
    pub source: String,
    paths: Option<Regex>,
    pattern: Option<Regex>,
    unless: Option<Regex>,
    conditions: Vec<Condition>,
    variants: Vec<(Option<Regex>, VariantConfig)>,
}

pub struct Set {
    pub config: SetConfig,
    paths: Regex,
    patterns: Vec<Regex>,
}

/// The rules and sets in force.
pub struct RuleSet {
    pub rules: Vec<Rule>,
    pub sets: Vec<Set>,
}

fn compile(re: &str, rule: &str, field: &str) -> Result<Regex> {
    Regex::new(re).map_err(|e| anyhow!("rule {rule}: {field}: {e}"))
}

impl Rule {
    fn new(config: RuleConfig, source: &str) -> Result<Rule> {
        let n = config.name.clone();
        let opt = |r: &Option<String>, f: &str| r.as_deref().map(|r| compile(r, &n, f)).transpose();
        match config.kind {
            Kind::Line | Kind::Sql if config.pattern.is_none() => bail!("rule {n}: a {:?} rule needs a pattern", config.kind),
            Kind::Ast if config.query.is_none() => bail!("rule {n}: an ast rule needs a query"),
            Kind::Symbol if config.conditions.is_empty() => bail!("rule {n}: a symbol rule needs `where`"),
            _ => {}
        }
        for f in &config.filters {
            if f != "per-loop-iteration" {
                bail!("rule {n}: unknown filter {f:?} (known: per-loop-iteration)");
            }
        }
        if let Some(lang) = &config.language
            && lang != "typescript"
        {
            bail!("rule {n}: unknown language {lang:?} (known: typescript)");
        }
        if let Some(q) = &config.query {
            tree_sitter::Query::new(&tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(), q)
                .map_err(|e| anyhow!("rule {n}: query: {e}"))?;
        }
        let conditions = config
            .conditions
            .iter()
            .map(|c| Condition::parse(c).map_err(|e| anyhow!("rule {n}: where {c:?}: {e}")))
            .collect::<Result<Vec<_>>>()?;
        let variants = config
            .variants
            .iter()
            .map(|v| {
                if let Some(w) = &v.when
                    && w != "loop-stops-early"
                {
                    bail!("rule {n}: variant: unknown `when` {w:?} (known: loop-stops-early)");
                }
                Ok((opt(&v.pattern, "variant pattern")?, v.clone()))
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(Rule {
            paths: opt(&config.paths, "paths")?,
            pattern: opt(&config.pattern, "pattern")?,
            unless: opt(&config.unless, "unless")?,
            conditions,
            variants,
            source: source.to_string(),
            config,
        })
    }

    fn applies_to(&self, path: &str) -> bool {
        match &self.paths {
            Some(re) => re.is_match(path),
            None => self.config.kind != Kind::Sql || is_migration(path),
        }
    }
}

/// `.sql` under a directory whose name starts with `migration`.
fn is_migration(path: &str) -> bool {
    path.ends_with(".sql")
        && !path.ends_with(".down.sql")
        && path
            .split('/')
            .any(|c| c.to_ascii_lowercase().starts_with("migration"))
}

impl RuleSet {
    /// Presets as the config chooses (all of them by default), then the
    /// config's own sets and rules; a config rule replaces every preset
    /// rule of its name; `disable` drops by name.
    pub fn from_config_text(text: Option<&str>, source: &str) -> Result<RuleSet> {
        let config: FileConfig = match text {
            Some(t) => toml::from_str(t).with_context(|| source.to_string())?,
            None => FileConfig::default(),
        };
        let chosen: Vec<String> = config
            .presets
            .clone()
            .unwrap_or_else(|| PRESETS.iter().map(|(n, _)| n.to_string()).collect());
        let mut rules: Vec<Rule> = Vec::new();
        let mut sets: Vec<Set> = Vec::new();
        for name in &chosen {
            let Some((_, text)) = PRESETS.iter().find(|(n, _)| n == name) else {
                bail!(
                    "{source}: unknown preset {name:?} (known: {})",
                    PRESETS.iter().map(|(n, _)| *n).collect::<Vec<_>>().join(", ")
                );
            };
            let preset: FileConfig =
                toml::from_str(text).with_context(|| format!("preset {name}"))?;
            for s in preset.set {
                sets.push(Set::new(s)?);
            }
            for r in preset.risk {
                rules.push(Rule::new(r, &format!("preset:{name}"))?);
            }
        }
        let own: HashSet<String> = config.risk.iter().map(|r| r.name.clone()).collect();
        rules.retain(|r| !own.contains(&r.config.name));
        for s in config.set {
            sets.retain(|x| x.config.name != s.name);
            sets.push(Set::new(s)?);
        }
        for r in config.risk {
            rules.push(Rule::new(r, source).with_context(|| source.to_string())?);
        }
        rules.retain(|r| !config.disable.contains(&r.config.name));
        for r in &rules {
            if let Some(s) = &r.config.in_set
                && !sets.iter().any(|x| x.config.name == s.set)
            {
                bail!("rule {}: in_set names an unknown set {:?}", r.config.name, s.set);
            }
        }
        Ok(RuleSet { rules, sets })
    }

    /// The repository's rules: its config file (see
    /// [`crate::seam::config_path`]) over the presets.
    pub fn load(root: &Path) -> Result<RuleSet> {
        let path = crate::seam::config_path(root);
        let text = std::fs::read_to_string(&path).ok();
        let label = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.display().to_string());
        RuleSet::from_config_text(text.as_deref(), &label).with_context(|| path.display().to_string())
    }
}

impl Set {
    fn new(config: SetConfig) -> Result<Set> {
        let n = config.name.clone();
        Ok(Set {
            paths: compile(&config.paths, &n, "set paths")?,
            patterns: config
                .patterns
                .iter()
                .map(|p| compile(p, &n, "set pattern"))
                .collect::<Result<_>>()?,
            config,
        })
    }

    fn collect<'a>(&self, files: impl Iterator<Item = (&'a str, &'a str)>) -> HashSet<String> {
        let mut out = HashSet::new();
        for (path, text) in files {
            if !self.paths.is_match(path) {
                continue;
            }
            let text = if path.ends_with(".sql") {
                risk::strip_sql_comments(text)
            } else {
                text.to_string()
            };
            for re in &self.patterns {
                for c in re.captures_iter(&text) {
                    if let Some(m) = c.name("name").or_else(|| c.get(1)) {
                        out.insert(m.as_str().to_ascii_lowercase());
                    }
                }
            }
        }
        out
    }
}

// ---------------------------------------------------------------- symbol

/// What the call graph says about one changed function.
#[derive(Debug, Clone, Default)]
pub struct SymbolFacts {
    pub id: String,
    pub name: String,
    pub path: String,
    pub line: u32,
    /// `added`, `removed`, `changed`.
    pub change: String,
    pub exported: bool,
    pub route: Option<String>,
    pub importing_packages: Vec<String>,
    pub reach_functions: usize,
    pub reach_files: usize,
    pub reach_packages: usize,
    pub entries: usize,
}

impl SymbolFacts {
    /// Other code depends on it: a route, or an export another package imports.
    pub fn public(&self) -> bool {
        self.route.is_some() || (self.exported && !self.importing_packages.is_empty())
    }

    fn get(&self, field: &str) -> Option<Value> {
        Some(match field {
            "change" => Value::Text(self.change.clone()),
            "exported" => Value::Bool(self.exported),
            "route" => Value::Bool(self.route.is_some()),
            "public" => Value::Bool(self.public()),
            "importing_packages" => Value::Num(self.importing_packages.len() as f64),
            "reach_functions" => Value::Num(self.reach_functions as f64),
            "reach_files" => Value::Num(self.reach_files as f64),
            "reach_packages" => Value::Num(self.reach_packages as f64),
            "entries" => Value::Num(self.entries as f64),
            _ => return None,
        })
    }

    fn placeholders(&self) -> HashMap<String, String> {
        let list = self.importing_packages.join(", ");
        HashMap::from([
            ("name".into(), self.name.clone()),
            ("route".into(), self.route.clone().unwrap_or_default()),
            ("change".into(), self.change.clone()),
            ("importing_packages".into(), self.importing_packages.len().to_string()),
            ("importing_packages_list".into(), list.clone()),
            (
                "called_from".into(),
                if list.is_empty() { String::new() } else { format!("; called from {list}") },
            ),
            ("reach_functions".into(), self.reach_functions.to_string()),
            ("reach_files".into(), self.reach_files.to_string()),
            ("reach_packages".into(), self.reach_packages.to_string()),
            ("entries".into(), self.entries.to_string()),
        ])
    }
}

pub const SYMBOL_FIELDS: &[&str] = &[
    "change",
    "exported",
    "route",
    "public",
    "importing_packages",
    "reach_functions",
    "reach_files",
    "reach_packages",
    "entries",
];

#[derive(Debug, Clone, PartialEq)]
enum Value {
    Bool(bool),
    Num(f64),
    Text(String),
}

/// `field`, `!field`, or `field <op> value` with `==`, `!=`, `>=`,
/// `<=`, `>`, `<`.
#[derive(Debug, Clone)]
struct Condition {
    field: String,
    op: String,
    value: Option<String>,
}

impl Condition {
    fn parse(text: &str) -> Result<Condition> {
        let t = text.trim();
        let check = |f: &str| -> Result<()> {
            if SYMBOL_FIELDS.contains(&f) {
                Ok(())
            } else {
                bail!("unknown field {f:?} (known: {})", SYMBOL_FIELDS.join(", "))
            }
        };
        for op in ["==", "!=", ">=", "<=", ">", "<"] {
            if let Some((f, v)) = t.split_once(op) {
                let f = f.trim();
                check(f)?;
                return Ok(Condition {
                    field: f.to_string(),
                    op: op.to_string(),
                    value: Some(v.trim().trim_matches('"').to_string()),
                });
            }
        }
        let (neg, f) = match t.strip_prefix('!') {
            Some(f) => (true, f.trim()),
            None => (false, t),
        };
        check(f)?;
        Ok(Condition {
            field: f.to_string(),
            op: if neg { "!" } else { "" }.to_string(),
            value: None,
        })
    }

    fn holds(&self, facts: &SymbolFacts) -> bool {
        let Some(actual) = facts.get(&self.field) else {
            return false;
        };
        let truthy = |v: &Value| match v {
            Value::Bool(b) => *b,
            Value::Num(n) => *n != 0.0,
            Value::Text(s) => !s.is_empty(),
        };
        let Some(want) = &self.value else {
            return truthy(&actual) != (self.op == "!");
        };
        match actual {
            Value::Num(n) => {
                let Ok(w) = want.parse::<f64>() else { return false };
                match self.op.as_str() {
                    "==" => n == w,
                    "!=" => n != w,
                    ">=" => n >= w,
                    "<=" => n <= w,
                    ">" => n > w,
                    "<" => n < w,
                    _ => false,
                }
            }
            Value::Text(s) => match self.op.as_str() {
                "==" => s.eq_ignore_ascii_case(want),
                "!=" => !s.eq_ignore_ascii_case(want),
                _ => false,
            },
            Value::Bool(b) => {
                let w = want == "true";
                match self.op.as_str() {
                    "==" => b == w,
                    "!=" => b != w,
                    _ => false,
                }
            }
        }
    }
}

/// `{key}` replaced from `values`; unknown keys become empty.
fn render(template: &str, values: &HashMap<String, String>) -> String {
    let re = Regex::new(r"\{(\w+)\}").expect("valid regex");
    re.replace_all(template, |c: &regex::Captures| values.get(&c[1]).cloned().unwrap_or_default())
        .into_owned()
}

// ----------------------------------------------------------------- input

/// One file the diff touched, as of the head.
pub struct ChangedFile {
    pub path: String,
    pub head: String,
    pub added: HashSet<u32>,
}

/// A function upstream of the change (or a changed one), for
/// `scope = "reach"` rules.
pub struct ReachFunction {
    pub id: String,
    pub name: String,
    pub path: String,
    pub line: u32,
    pub to: u32,
}

/// Everything the rules look at.
pub struct Input<'a> {
    pub files: &'a [ChangedFile],
    pub symbols: &'a [SymbolFacts],
    pub reach: &'a [ReachFunction],
    /// A head file's text.
    pub read: &'a dyn Fn(&str) -> Option<String>,
    /// Every file in the head tree, for sets.
    pub tree: &'a dyn Fn() -> Vec<String>,
    /// The changed function (id, name) a head line belongs to.
    pub owner: &'a dyn Fn(&str, u32) -> Option<(String, String)>,
}

// ------------------------------------------------------------- evaluation

impl RuleSet {
    pub fn evaluate(&self, input: &Input) -> Vec<Finding> {
        let mut out: Vec<Finding> = Vec::new();

        // Sets are collected only if a rule in force uses one.
        let mut sets: HashMap<String, HashSet<String>> = HashMap::new();
        let wanted: HashSet<&str> = self
            .rules
            .iter()
            .filter_map(|r| r.config.in_set.as_ref().map(|s| s.set.as_str()))
            .collect();
        if !wanted.is_empty() {
            let tree = (input.tree)();
            for set in self.sets.iter().filter(|s| wanted.contains(s.config.name.as_str())) {
                let texts: Vec<(String, String)> = tree
                    .iter()
                    .filter(|p| set.paths.is_match(p))
                    .filter_map(|p| (input.read)(p).map(|t| (p.clone(), t)))
                    .collect();
                sets.insert(
                    set.config.name.clone(),
                    set.collect(texts.iter().map(|(p, t)| (p.as_str(), t.as_str()))),
                );
            }
        }

        // Symbol rules: per changed function, the first tier of each
        // name that holds.
        for s in input.symbols {
            let mut fired: HashSet<&str> = HashSet::new();
            for r in self.rules.iter().filter(|r| r.config.kind == Kind::Symbol) {
                if fired.contains(r.config.name.as_str()) || !r.conditions.iter().all(|c| c.holds(s)) {
                    continue;
                }
                fired.insert(&r.config.name);
                out.push(Finding {
                    rule: r.config.name.clone(),
                    severity: r.config.severity,
                    title: render(&r.config.title, &s.placeholders()),
                    path: s.path.clone(),
                    line: s.line,
                    function: Some(s.id.clone()),
                    excerpt: (input.read)(&s.path)
                        .and_then(|t| t.lines().nth(s.line.saturating_sub(1) as usize).map(|l| l.trim().to_string()))
                        .unwrap_or_default(),
                });
            }
        }

        for f in input.files {
            let lines: Vec<&str> = f.head.lines().collect();
            let excerpt = |l: u32| lines.get(l as usize - 1).map(|s| s.trim().to_string()).unwrap_or_default();
            let in_function = |l: u32| -> (Option<String>, String) {
                match (input.owner)(&f.path, l) {
                    Some((id, name)) => (Some(id), format!(" in {name}")),
                    None => (None, String::new()),
                }
            };
            for r in self.rules.iter().filter(|r| r.applies_to(&f.path)) {
                match r.config.kind {
                    Kind::Line if r.config.scope == Scope::Added => {
                        let re = r.pattern.as_ref().expect("checked");
                        for (i, text) in lines.iter().enumerate() {
                            let line = i as u32 + 1;
                            if !f.added.contains(&line) {
                                continue;
                            }
                            let Some(c) = re.captures(text) else { continue };
                            if r.unless.as_ref().is_some_and(|u| u.is_match(&c[0])) {
                                continue;
                            }
                            let (function, within) = in_function(line);
                            let mut values = captures_map(re, &c);
                            values.insert("in_function".into(), within);
                            let (severity, title) = r.variant(text, &[], &values);
                            out.push(Finding {
                                rule: r.config.name.clone(),
                                severity,
                                title,
                                path: f.path.clone(),
                                line,
                                function,
                                excerpt: text.trim().to_string(),
                            });
                        }
                    }
                    Kind::Sql => out.extend(sql_findings(r, f, &sets)),
                    Kind::Ast => {
                        for (line, facts) in ast_hits(r, &f.path, &f.head) {
                            if !f.added.contains(&line) {
                                continue;
                            }
                            let text = excerpt(line);
                            if r.pattern.as_ref().is_some_and(|p| !p.is_match(&text)) {
                                continue;
                            }
                            let (function, within) = in_function(line);
                            let values = HashMap::from([("in_function".to_string(), within)]);
                            let (severity, title) = r.variant(&text, &facts, &values);
                            out.push(Finding {
                                rule: r.config.name.clone(),
                                severity,
                                title,
                                path: f.path.clone(),
                                line,
                                function,
                                excerpt: text,
                            });
                        }
                    }
                    _ => {}
                }
            }
        }

        // Reach-scoped line rules: anywhere in a function the change
        // runs under, once per function.
        for r in self
            .rules
            .iter()
            .filter(|r| r.config.kind == Kind::Line && r.config.scope == Scope::Reach)
        {
            let re = r.pattern.as_ref().expect("checked");
            for f in input.reach {
                if !r.applies_to(&f.path) {
                    continue;
                }
                let Some(text) = (input.read)(&f.path) else { continue };
                let hit = text
                    .lines()
                    .enumerate()
                    .map(|(i, l)| (i as u32 + 1, l))
                    .filter(|(n, _)| f.line <= *n && *n <= f.to)
                    .find(|(_, l)| {
                        re.captures(l)
                            .is_some_and(|c| !r.unless.as_ref().is_some_and(|u| u.is_match(&c[0])))
                    });
                if let Some((line, l)) = hit {
                    let mut values = HashMap::from([("in_function".to_string(), format!(" in {}", f.name))]);
                    if let Some(c) = re.captures(l) {
                        values.extend(captures_map(re, &c));
                    }
                    let (severity, title) = r.variant(l, &[], &values);
                    out.push(Finding {
                        rule: r.config.name.clone(),
                        severity,
                        title,
                        path: f.path.clone(),
                        line,
                        function: Some(f.id.clone()),
                        excerpt: l.trim().to_string(),
                    });
                }
            }
        }

        out.sort_by(|a, b| (a.severity, &a.rule, &a.path, a.line).cmp(&(b.severity, &b.rule, &b.path, b.line)));
        out.dedup_by(|a, b| a.rule == b.rule && a.path == b.path && a.line == b.line && a.function == b.function);
        self.fold(out)
    }

    /// Findings of a `fold` rule with one severity in one file become one.
    fn fold(&self, findings: Vec<Finding>) -> Vec<Finding> {
        let folded: HashSet<&str> = self
            .rules
            .iter()
            .filter(|r| r.config.fold)
            .map(|r| r.config.name.as_str())
            .collect();
        let mut out: Vec<Finding> = Vec::new();
        let mut more: HashMap<usize, usize> = HashMap::new();
        for f in findings {
            if folded.contains(f.rule.as_str())
                && let Some(i) = out
                    .iter()
                    .position(|o| o.rule == f.rule && o.path == f.path && o.severity == f.severity)
            {
                *more.entry(i).or_default() += 1;
                continue;
            }
            out.push(f);
        }
        for (i, n) in more {
            out[i].title = format!("{} (+{n} more like it in this file)", out[i].title);
        }
        out
    }
}

impl Rule {
    /// Severity and title for one hit: the first variant whose pattern
    /// matches the line or whose `when` fact holds, else the rule's own.
    fn variant(&self, line: &str, facts: &[&str], values: &HashMap<String, String>) -> (Severity, String) {
        for (re, v) in &self.variants {
            let by_pattern = re.as_ref().is_some_and(|re| re.is_match(line));
            let by_fact = v.when.as_deref().is_some_and(|w| facts.contains(&w));
            if by_pattern || by_fact {
                return (v.severity, render(&v.title, values));
            }
        }
        (self.config.severity, render(&self.config.title, values))
    }
}

fn captures_map(re: &Regex, c: &regex::Captures) -> HashMap<String, String> {
    let mut m = HashMap::new();
    for name in re.capture_names().flatten() {
        m.insert(name.to_string(), c.name(name).map(|x| x.as_str().trim().to_string()).unwrap_or_default());
    }
    for i in 0..c.len() {
        m.insert(i.to_string(), c.get(i).map(|x| x.as_str().trim().to_string()).unwrap_or_default());
    }
    m
}

fn sql_findings(r: &Rule, f: &ChangedFile, sets: &HashMap<String, HashSet<String>>) -> Vec<Finding> {
    let re = r.pattern.as_ref().expect("checked");
    let head = risk::forward_half(&f.head);
    let lines: Vec<&str> = head.lines().collect();
    let all = risk::statements(&head);
    let created: HashSet<String> = all
        .iter()
        .filter(|(_, s)| s.trim_start().to_ascii_lowercase().starts_with("create table"))
        .filter_map(|(_, s)| risk::target_table(s))
        .collect();
    let mut out = Vec::new();
    for (line, stmt) in &all {
        let span = stmt.lines().count().max(1) as u32;
        if !(*line..line + span).any(|l| f.added.contains(&l)) {
            continue;
        }
        for c in re.captures_iter(stmt) {
            if r.unless.as_ref().is_some_and(|u| u.is_match(&c[0])) {
                continue;
            }
            if let Some(s) = &r.config.in_set {
                let v = c.name(&s.capture).map(|m| m.as_str().to_ascii_lowercase()).unwrap_or_default();
                if !sets.get(&s.set).is_some_and(|set| set.contains(&v)) {
                    continue;
                }
            }
            if r.config.skip_new_tables {
                let table = c
                    .name("table")
                    .map(|m| m.as_str().trim_matches('"').to_ascii_lowercase())
                    .or_else(|| risk::target_table(stmt));
                if table.is_some_and(|t| created.contains(&t)) {
                    continue;
                }
            }
            let values = captures_map(re, &c);
            let (severity, title) = r.variant(stmt, &[], &values);
            out.push(Finding {
                rule: r.config.name.clone(),
                severity,
                title,
                path: f.path.clone(),
                line: *line,
                function: None,
                excerpt: lines.get(*line as usize - 1).map(|s| s.trim().to_string()).unwrap_or_default(),
            });
        }
    }
    out
}

const TS_EXTENSIONS: &[&str] = &["ts", "tsx", "mts", "cts", "js", "jsx", "mjs", "cjs", "svelte"];

/// Lines of `@hit` captures (else each match's first) that pass the
/// rule's filters, with the facts the filters established.
fn ast_hits(r: &Rule, path: &str, source: &str) -> Vec<(u32, Vec<&'static str>)> {
    let ext = Path::new(path).extension().and_then(|e| e.to_str()).unwrap_or("");
    if !TS_EXTENSIONS.contains(&ext) {
        return Vec::new();
    }
    let (text, tsx) = match ext {
        "svelte" => (crate::ts_extract::script_only(source, "Component"), false),
        "tsx" | "jsx" => (source.to_string(), true),
        _ => (source.to_string(), false),
    };
    let language: tree_sitter::Language = if tsx {
        tree_sitter_typescript::LANGUAGE_TSX.into()
    } else {
        tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into()
    };
    let mut parser = tree_sitter::Parser::new();
    if parser.set_language(&language).is_err() {
        return Vec::new();
    }
    let Some(tree) = parser.parse(&text, None) else { return Vec::new() };
    let Ok(query) = tree_sitter::Query::new(&language, r.config.query.as_deref().unwrap_or_default()) else {
        return Vec::new();
    };
    let hit = query.capture_index_for_name("hit");
    let mut cursor = tree_sitter::QueryCursor::new();
    let mut matches = cursor.matches(&query, tree.root_node(), text.as_bytes());
    let mut out: Vec<(u32, Vec<&'static str>)> = Vec::new();
    while let Some(m) = matches.next() {
        let node = match hit {
            Some(i) => m.captures().iter().find(|c| c.index == i).map(|c| c.node),
            None => m.captures().first().map(|c| c.node),
        };
        let Some(node) = node else { continue };
        let mut facts: Vec<&'static str> = Vec::new();
        let mut keep = true;
        for f in &r.config.filters {
            if f == "per-loop-iteration" {
                match risk::per_loop_iteration(node) {
                    Some(stops_early) => {
                        if stops_early {
                            facts.push("loop-stops-early");
                        }
                    }
                    None => keep = false,
                }
            }
        }
        if keep {
            out.push((node.start_position().row as u32 + 1, facts));
        }
    }
    out.sort_by_key(|(l, _)| *l);
    out.dedup_by_key(|(l, _)| *l);
    out
}

// ---------------------------------------------------------------- SARIF

/// Results of an external analyser (ESLint, semgrep, a SQL linter, …)
/// from a SARIF 2.1 log, kept when they sit on a line the diff added:
/// the analyser judges the code, prognost narrows it to the change.
/// `error` → medium, `warning`/`note` → low. The rule is
/// `<tool>/<ruleId>`.
pub fn sarif_findings(text: &str, root: &Path, files: &[ChangedFile]) -> Result<Vec<Finding>> {
    let log: serde_json::Value = serde_json::from_str(text).context("not JSON")?;
    let runs = log.get("runs").and_then(|r| r.as_array()).ok_or_else(|| anyhow!("no `runs`"))?;
    let by_path: HashMap<&str, &ChangedFile> = files.iter().map(|f| (f.path.as_str(), f)).collect();
    let mut out = Vec::new();
    for run in runs {
        let tool = run
            .pointer("/tool/driver/name")
            .and_then(|v| v.as_str())
            .unwrap_or("sarif")
            .to_ascii_lowercase();
        for res in run.get("results").and_then(|r| r.as_array()).into_iter().flatten() {
            let Some(loc) = res.pointer("/locations/0/physicalLocation") else { continue };
            let Some(uri) = loc.pointer("/artifactLocation/uri").and_then(|v| v.as_str()) else { continue };
            let Some(line) = loc.pointer("/region/startLine").and_then(|v| v.as_u64()) else { continue };
            let path = relative_uri(uri, root);
            let Some(f) = by_path.get(path.as_str()) else { continue };
            let line = line as u32;
            if !f.added.contains(&line) {
                continue;
            }
            let rule_id = res.get("ruleId").and_then(|v| v.as_str()).unwrap_or("result");
            let severity = match res.get("level").and_then(|v| v.as_str()).unwrap_or("warning") {
                "error" => Severity::Medium,
                _ => Severity::Low,
            };
            let title = res
                .pointer("/message/text")
                .and_then(|v| v.as_str())
                .unwrap_or(rule_id)
                .to_string();
            out.push(Finding {
                rule: format!("{tool}/{rule_id}"),
                severity,
                title,
                path: path.clone(),
                line,
                function: None,
                excerpt: f.head.lines().nth(line as usize - 1).map(|l| l.trim().to_string()).unwrap_or_default(),
            });
        }
    }
    Ok(out)
}

fn relative_uri(uri: &str, root: &Path) -> String {
    let raw = uri.strip_prefix("file://").unwrap_or(uri);
    let p = PathBuf::from(raw);
    let rel = p.strip_prefix(root).map(Path::to_path_buf).unwrap_or(p);
    rel.to_string_lossy().trim_start_matches("./").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input_for<'a>(
        files: &'a [ChangedFile],
        symbols: &'a [SymbolFacts],
        tree: &'a [(&'a str, &'a str)],
        read: &'a dyn Fn(&str) -> Option<String>,
        list: &'a dyn Fn() -> Vec<String>,
        owner: &'a dyn Fn(&str, u32) -> Option<(String, String)>,
    ) -> Input<'a> {
        let _ = tree;
        Input { files, symbols, reach: &[], read, tree: list, owner }
    }

    fn all_lines(text: &str) -> HashSet<u32> {
        (1..=text.lines().count() as u32).collect()
    }

    fn run(rules: &RuleSet, files: Vec<ChangedFile>, symbols: Vec<SymbolFacts>, tree: Vec<(&str, &str)>) -> Vec<String> {
        let tree_owned: Vec<(String, String)> = tree.iter().map(|(p, t)| (p.to_string(), t.to_string())).collect();
        let files_text: Vec<(String, String)> = files.iter().map(|f| (f.path.clone(), f.head.clone())).collect();
        let read = move |p: &str| -> Option<String> {
            tree_owned
                .iter()
                .chain(files_text.iter())
                .find(|(q, _)| q == p)
                .map(|(_, t)| t.clone())
        };
        let names: Vec<String> = tree.iter().map(|(p, _)| p.to_string()).collect();
        let list = move || names.clone();
        let owner = |_: &str, _: u32| None;
        let input = input_for(&files, &symbols, &[], &read, &list, &owner);
        rules
            .evaluate(&input)
            .into_iter()
            .map(|f| format!("{}@{}:{}", f.rule, f.line, f.severity.as_str()))
            .collect()
    }

    fn defaults() -> RuleSet {
        RuleSet::from_config_text(None, "test").unwrap()
    }

    #[test]
    fn every_preset_compiles() {
        let rules = defaults();
        assert!(rules.rules.len() >= 10);
        assert!(rules.rules.iter().any(|r| r.config.kind == Kind::Ast));
    }

    #[test]
    fn await_in_loop_from_the_preset() {
        let src = "\
async function f(xs, reader, plans, ids) {
  for (const x of xs) {
    await save(x);
  }
  for await (const y of stream) {
    await use(y);
  }
  while (more()) {
    await Promise.all(xs.map(async (x) => await g(x)));
  }
  for (const z of await load()) { z; }
  while (true) {
    const { done } = await reader.read();
    if (done) return await stop('end');
  }
  for (const plan of plans) {
    const r = await attempt(plan);
    if (r.ok) return r;
  }
  for (const id of ids) {
    await sleep(10);
  }
}
";
        let files = vec![ChangedFile { path: "a.ts".into(), head: src.into(), added: all_lines(src) }];
        assert_eq!(
            run(&defaults(), files, vec![], vec![]),
            vec![
                "await-in-loop@3:medium",
                "await-in-loop@9:medium",
                "await-in-loop@13:low",
                "await-in-loop@17:low",
                "await-in-loop@21:low",
            ]
        );
    }

    #[test]
    fn migration_rules_from_the_preset() {
        let domains = "CREATE DOMAIN short_code AS CHAR(26) CHECK (VALUE ~ '^x');\nCREATE DOMAIN plain_time AS TIME(3);\nCREATE DOMAIN later AS TEXT;\nALTER DOMAIN later ADD CONSTRAINT c CHECK (VALUE <> '');\n";
        let head = "\
-- Up Migration
ALTER TABLE users DROP COLUMN legacy;
ALTER TABLE orders ADD COLUMN owner_id short_code;
ALTER TABLE orders ADD COLUMN note text NOT NULL;
ALTER TABLE orders ADD COLUMN at plain_time NOT NULL DEFAULT now();
CREATE INDEX orders_owner ON orders (owner_id);
CREATE TABLE fresh (id short_code);
ALTER TABLE fresh ADD COLUMN x text NOT NULL;
CREATE INDEX CONCURRENTLY o2 ON orders (note);
ALTER POLICY p ON orders USING (true);
CREATE POLICY q ON fresh USING (true);
ALTER TABLE orders DISABLE ROW LEVEL SECURITY;
ALTER TABLE orders ADD COLUMN tag later;
CREATE INDEX fresh_x ON fresh (x);
-- Down Migration
ALTER TABLE orders DROP COLUMN owner_id;
";
        let files = vec![ChangedFile {
            path: "db/migrations/1.sql".into(),
            head: head.into(),
            added: all_lines(head),
        }];
        let mut got = run(&defaults(), files, vec![], vec![("db/migrations/0.sql", domains)]);
        got.sort();
        let mut want = vec![
            "migration-drop@2:high",
            "migration-domain-rewrite@3:high",
            "migration-not-null-without-default@4:high",
            "migration-index-lock@6:medium",
            "migration-rls-policy@10:medium",
            "migration-rls-disabled@12:high",
            "migration-domain-rewrite@13:high",
        ];
        want.sort();
        assert_eq!(got, want);
    }

    #[test]
    fn symbol_tiers_pick_the_first_that_holds() {
        let s = |name: &str, exported: bool, importers: usize, reach: usize, route: Option<&str>| SymbolFacts {
            id: name.into(),
            name: name.into(),
            path: "x.ts".into(),
            line: 1,
            change: "changed".into(),
            exported,
            route: route.map(str::to_string),
            importing_packages: (0..importers).map(|i| format!("p{i}")).collect(),
            reach_functions: reach,
            reach_packages: importers + 1,
            ..Default::default()
        };
        let symbols = vec![
            s("lib", true, 3, 40, None),
            s("domain", true, 1, 5, None),
            s("handler", false, 0, 0, Some("POST /x")),
            s("helper", false, 0, 25, None),
            s("quiet", false, 0, 2, None),
        ];
        let got = run(&defaults(), vec![], symbols, vec![]);
        assert_eq!(
            got,
            vec![
                "public-api-change@1:high",
                "public-api-change@1:medium",
                "public-api-change@1:low",
                "wide-reach@1:low",
            ]
        );
    }

    #[test]
    fn config_picks_presets_overrides_and_disables() {
        let text = r#"
presets = ["graph", "postgres"]
disable = ["wide-reach"]

[[risk]]
name = "migration-index-lock"
kind = "sql"
severity = "low"
pattern = '''(?is)^\s*create\s+index\b'''
title = "index"

[[risk]]
name = "admin-privileges"
pattern = '''runWithAdminPrivileges'''
severity = "high"
title = "runs with admin privileges{in_function}"
"#;
        let rules = RuleSet::from_config_text(Some(text), "prognost.toml").unwrap();
        let names: Vec<(&str, &str)> = rules
            .rules
            .iter()
            .map(|r| (r.config.name.as_str(), r.source.as_str()))
            .collect();
        assert!(!names.iter().any(|(n, _)| *n == "await-in-loop"), "typescript preset not chosen");
        assert!(!names.iter().any(|(n, _)| *n == "wide-reach"), "disabled");
        assert_eq!(
            names.iter().filter(|(n, _)| *n == "migration-index-lock").collect::<Vec<_>>(),
            vec![&("migration-index-lock", "prognost.toml")]
        );
        let src = "x();\nawait runWithAdminPrivileges(c, q);\n";
        let files = vec![ChangedFile { path: "a.ts".into(), head: src.into(), added: [2].into() }];
        assert_eq!(run(&rules, files, vec![], vec![]), vec!["admin-privileges@2:high"]);
    }

    #[test]
    fn bad_rules_are_refused_with_the_reason() {
        for (text, needle) in [
            ("[[risk]]\nname = \"a\"\ntitle = \"t\"\n", "needs a pattern"),
            ("[[risk]]\nname = \"a\"\nkind = \"symbol\"\nwhere = [\"colour > 2\"]\ntitle = \"t\"\n", "unknown field"),
            ("[[risk]]\nname = \"a\"\nkind = \"ast\"\nquery = \"(nope\"\ntitle = \"t\"\n", "query"),
            ("presets = [\"cobol\"]\n", "unknown preset"),
            ("[[risk]]\nname = \"a\"\npattern = \"x\"\ntitel = \"t\"\n", "unknown field"),
        ] {
            let e = RuleSet::from_config_text(Some(text), "cfg").err().map(|e| format!("{e:#}")).unwrap_or_default();
            assert!(e.contains(needle), "{text:?} → {e}");
        }
    }

    #[test]
    fn sarif_results_are_kept_on_added_lines_only() {
        let sarif = r#"{"runs":[{"tool":{"driver":{"name":"ESLint"}},"results":[
            {"ruleId":"no-await-in-loop","level":"error","message":{"text":"Unexpected await inside a loop."},
             "locations":[{"physicalLocation":{"artifactLocation":{"uri":"file:///repo/src/a.ts"},"region":{"startLine":2}}}]},
            {"ruleId":"no-await-in-loop","level":"error","message":{"text":"old"},
             "locations":[{"physicalLocation":{"artifactLocation":{"uri":"src/a.ts"},"region":{"startLine":1}}}]}
        ]}]}"#;
        let files = vec![ChangedFile { path: "src/a.ts".into(), head: "a\nb\n".into(), added: [2].into() }];
        let got = sarif_findings(sarif, Path::new("/repo"), &files).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].rule, "eslint/no-await-in-loop");
        assert_eq!(got[0].line, 2);
        assert_eq!(got[0].severity, Severity::Medium);
    }
}
