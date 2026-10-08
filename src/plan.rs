//! `prognost plan`: what a diff changes and how far that reaches — the
//! code counterpart of `terraform plan`, as far as code allows. It
//! reports, it does not judge: whether a change is risky is
//! `prognost assess`'s business, run on this report (see [`crate::assess`]).
//!
//! The report has two layers. The facts: `files` the diff touches,
//! `functions` (the changed ones and everything upstream of them, one
//! shape for both) and `calls` between them. The `summary`: counts
//! derived from those facts — how far each change reaches, per package,
//! per kind of entry point — so a reader can check every number against
//! the facts it came from.
//!
//! What it cannot be, unlike terraform: code calls itself (one line can
//! matter in thirty places), its dependencies are inferred rather than
//! declared (dynamic dispatch, DI and callbacks can hide an edge), and
//! there is no live state to compare with — only the previous code.

use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::impact::{Change, EntryKind, Limits, Side};

/// Raised when a change breaks what a reader of this document relies on.
/// 3: facts (`files`, `functions`, `calls`) and `summary` as two layers.
pub const PLAN_VERSION: u32 = 3;

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct PlanReport {
    pub version: u32,
    /// The merge base the diff is taken from.
    pub base: String,
    /// The head commit; `null` for the working tree.
    pub head: Option<String>,
    /// Every file the diff touches, whatever its kind.
    pub files: Vec<FileChange>,
    /// The functions the diff added, removed or changed, and every
    /// function upstream of them (`change: "unchanged"`).
    pub functions: Vec<Function>,
    /// Calls between those functions, caller → callee.
    pub calls: Vec<Call>,
    /// Counts derived from the facts above.
    pub summary: Summary,
    /// The caller walk stopped at its limit: upstream is a lower bound.
    pub truncated: bool,
    pub limits: Limits,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct FileChange {
    pub path: String,
    pub change: FileChangeKind,
    /// For a rename: where it was.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub previous_path: Option<String>,
    /// The workspace package holding it, when there is one.
    pub package: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum FileChangeKind {
    Added,
    Modified,
    Deleted,
    Renamed,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Function {
    /// Stable within one report; `calls` and `summary` refer to it.
    pub id: String,
    /// Its name, `METHOD /path` for a route handler, `anon@<line>` for
    /// an anonymous function.
    pub name: String,
    pub path: String,
    /// First and last line of the body, in `side`'s revision.
    pub range: LineRange,
    /// `head`, or `base` for a function only the base has (removed).
    pub side: Side,
    pub change: Change,
    pub package: Option<String>,
    /// Declared `export` at the top level of its module.
    pub exported: bool,
    /// For a route handler: `METHOD /mounted/path`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub route: Option<String>,
    /// Set when it is an entry point: nothing else was found calling
    /// it, or it is a route handler.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entry: Option<EntryKind>,
}

/// 1-based, inclusive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct LineRange {
    pub start: u32,
    pub end: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Call {
    pub caller: String,
    pub callee: String,
    /// The caller's line making the call (head); `null` when it could
    /// not be placed, or for a call the diff removed.
    pub line: Option<u32>,
    pub change: CallChange,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CallChange {
    Added,
    Removed,
    Unchanged,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Summary {
    pub changed: ChangeCounts,
    /// Everything upstream of the changed functions, together: the
    /// functions with `change: "unchanged"`, over calls the diff keeps.
    pub reach: Reach,
    /// Per changed function, widest reach first.
    pub symbols: Vec<SymbolSummary>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct ChangeCounts {
    pub added: usize,
    pub changed: usize,
    pub removed: usize,
    /// Files touched without any function-level change (types, config,
    /// SQL, markup …).
    pub files_without_function_changes: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Reach {
    pub functions: usize,
    pub files: usize,
    pub packages: Vec<PackageReach>,
    /// Entry points by kind.
    pub entries: BTreeMap<String, usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct PackageReach {
    pub package: String,
    pub functions: usize,
    pub files: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SymbolSummary {
    /// A function id.
    pub id: String,
    /// A route handler, or exported and called from another package.
    pub public: bool,
    /// Packages other than its own with a direct call to it.
    pub called_from_packages: Vec<String>,
    /// What calls it, directly or not (other changed functions excluded
    /// from `functions`).
    pub reach_functions: usize,
    pub reach_files: usize,
    pub reach_packages: usize,
    pub reach_entries: usize,
}

impl PlanReport {
    pub fn function(&self, id: &str) -> Option<&Function> {
        self.functions.iter().find(|f| f.id == id)
    }

    pub fn to_text(&self) -> String {
        use crate::color as c;
        let mut out = String::new();
        let short = |s: &str| s.chars().take(10).collect::<String>();
        out.push_str(&format!(
            "{}  {} → {}\n\n",
            c::bold("prognost plan"),
            c::dim(&short(&self.base)),
            c::dim(&self.head.as_deref().map(short).unwrap_or_else(|| "working tree".into()))
        ));

        let n = &self.summary.changed;
        out.push_str(&format!(
            "{}  ({} changed, {} added, {} removed)\n",
            c::bold(&format!("Changed symbols: {}", n.added + n.changed + n.removed)),
            c::yellow(&format!("~{}", n.changed)),
            c::green(&format!("+{}", n.added)),
            c::red(&format!("-{}", n.removed))
        ));
        for s in self.summary.symbols.iter().take(15) {
            let Some(f) = self.function(&s.id) else { continue };
            let (mark, paint): (&str, fn(&str) -> String) = match f.change {
                Change::Added => ("+", c::green),
                Change::Removed => ("-", c::red),
                _ => ("~", c::yellow),
            };
            let name = f.route.as_deref().unwrap_or(&f.name);
            let reach = if s.reach_functions == 0 {
                String::new()
            } else {
                format!(
                    "  {}{}",
                    c::cyan(&format!("← {} fn / {} pkg", s.reach_functions, s.reach_packages)),
                    if s.public { format!(" · {}", c::magenta("public")) } else { String::new() }
                )
            };
            out.push_str(&format!(
                "  {} {}  {}{reach}\n",
                paint(mark),
                paint(&c::bold(name)),
                c::dim(&format!("{}:{}", f.path, f.range.start))
            ));
        }
        if self.summary.symbols.len() > 15 {
            out.push_str(&c::dim(&format!("  … {} more (--json for all)\n", self.summary.symbols.len() - 15)));
        }

        // Files the function list says nothing about.
        let with_functions: std::collections::HashSet<&str> = self
            .functions
            .iter()
            .filter(|f| f.change != Change::Unchanged)
            .map(|f| f.path.as_str())
            .collect();
        let others: Vec<&FileChange> = self
            .files
            .iter()
            .filter(|f| !with_functions.contains(f.path.as_str()))
            .collect();
        if !others.is_empty() {
            out.push_str(&format!("\n{}\n", c::bold(&format!("Other changed files: {}", others.len()))));
            for f in others.iter().take(20) {
                let mark = match f.change {
                    FileChangeKind::Added => c::green("+"),
                    FileChangeKind::Deleted => c::red("-"),
                    FileChangeKind::Renamed => c::blue(">"),
                    FileChangeKind::Modified => c::yellow("~"),
                };
                out.push_str(&format!("  {mark} {}\n", f.path));
            }
            if others.len() > 20 {
                out.push_str(&c::dim(&format!("  … {} more (--json for all)\n", others.len() - 20)));
            }
        }

        let r = &self.summary.reach;
        out.push_str(&format!(
            "\n{}",
            c::bold(&format!(
                "Reach: {} functions in {} files / {} packages",
                r.functions,
                r.files,
                r.packages.len()
            ))
        ));
        if !r.entries.is_empty() {
            let kinds: Vec<String> = r.entries.iter().map(|(k, n)| format!("{n} {k}")).collect();
            out.push_str(&format!(", entry points: {}", kinds.join(", ")));
        }
        out.push('\n');
        let pkgs: Vec<String> = r
            .packages
            .iter()
            .take(8)
            .map(|p| format!("{} ← {}", c::cyan(&p.package), p.functions))
            .collect();
        if !pkgs.is_empty() {
            out.push_str(&format!("  {}\n", pkgs.join("   ")));
        }
        out.push_str(&format!(
            "\n{} {} to add, {} to change, {} to remove ({} files); reaching {} functions in {} packages.\n",
            c::bold("Plan:"),
            c::green(&n.added.to_string()),
            c::yellow(&n.changed.to_string()),
            c::red(&n.removed.to_string()),
            self.files.len(),
            r.functions,
            r.packages.len()
        ));
        if self.truncated {
            out.push_str(&c::yellow(&format!(
                "Note: the caller walk stopped at its limit ({} hops / {} nodes); reach is a lower bound.\n",
                self.limits.hops, self.limits.nodes
            )));
        }
        out
    }

}
