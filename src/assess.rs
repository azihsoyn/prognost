//! `prognost assess`: judges a plan. It reads a `prognost plan --json`
//! report and applies the rules in force (presets plus the repository's
//! own, see [`crate::rules`]) to it — to the changed functions and their
//! reach as the plan recorded them, and to the lines the diff added,
//! read from the plan's two revisions. It does not analyse the call
//! graph again: what the plan says is the input.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::impact::{Change, Side};
use crate::plan::{FileChangeKind, Function, PlanReport, SymbolSummary};
use crate::rev::Rev;
use crate::risk::{self, Finding, Severity};
use crate::rules::{ChangedFile, Input, ReachFunction, RuleSet, SymbolFacts};

/// Raised when a change breaks what a reader of this document relies on.
pub const ASSESS_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Assessment {
    pub version: u32,
    /// The plan's revisions.
    pub base: String,
    pub head: Option<String>,
    /// What the rules found, most severe first.
    pub findings: Vec<Finding>,
    pub counts: Counts,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct Counts {
    pub high: usize,
    pub medium: usize,
    pub low: usize,
}

/// The rules in force at `root`, applied to `plan`, plus the results of
/// the given SARIF logs that sit on added lines.
pub fn assess(root: &Path, plan: &PlanReport, sarif: &[PathBuf]) -> Result<Assessment> {
    if plan.version != crate::plan::PLAN_VERSION {
        bail!(
            "plan version {} — this prognost reads version {}; rerun prognost plan",
            plan.version,
            crate::plan::PLAN_VERSION
        );
    }
    let rules = RuleSet::load(root)?;
    let base = Rev::commit(plan.base.clone());
    let head = match &plan.head {
        Some(h) => Rev::commit(h.clone()),
        None => Rev::working(),
    };

    // The lines the diff added, per file the plan lists.
    let mut files: Vec<ChangedFile> = Vec::new();
    for f in &plan.files {
        if f.change == FileChangeKind::Deleted || is_test_file(Path::new(&f.path)) {
            continue;
        }
        let Some(text) = head.read(root, Path::new(&f.path)) else {
            continue;
        };
        let before = base
            .read(root, Path::new(f.previous_path.as_deref().unwrap_or(&f.path)))
            .unwrap_or_default();
        files.push(ChangedFile {
            added: risk::added_lines(&before, &text),
            head: text,
            path: f.path.clone(),
        });
    }

    // The changed functions with the facts the plan recorded about them.
    let summary_of: std::collections::HashMap<&str, &SymbolSummary> =
        plan.summary.symbols.iter().map(|s| (s.id.as_str(), s)).collect();
    let changed: Vec<&Function> = plan.functions.iter().filter(|f| f.change != Change::Unchanged).collect();
    let symbols: Vec<SymbolFacts> = changed
        .iter()
        .map(|f| {
            let s = summary_of.get(f.id.as_str());
            SymbolFacts {
                id: f.id.clone(),
                name: f.name.clone(),
                path: f.path.clone(),
                line: f.range.start,
                change: serde_json::to_value(f.change)
                    .ok()
                    .and_then(|v| v.as_str().map(str::to_string))
                    .unwrap_or_default(),
                exported: f.exported,
                route: f.route.clone(),
                importing_packages: s.map(|s| s.called_from_packages.clone()).unwrap_or_default(),
                reach_functions: s.map_or(0, |s| s.reach_functions),
                reach_files: s.map_or(0, |s| s.reach_files),
                reach_packages: s.map_or(0, |s| s.reach_packages),
                entries: s.map_or(0, |s| s.reach_entries),
            }
        })
        .collect();
    // Every function the change runs under, and the changed ones, as of the head.
    let reach: Vec<ReachFunction> = plan
        .functions
        .iter()
        .filter(|f| f.side == Side::Head)
        .map(|f| ReachFunction {
            id: f.id.clone(),
            name: f.name.clone(),
            path: f.path.clone(),
            line: f.range.start,
            to: f.range.end,
        })
        .collect();

    let read = |p: &str| head.read(root, Path::new(p));
    let tree = || {
        head.list_files(root)
            .into_iter()
            .map(|f| f.to_string_lossy().into_owned())
            .collect::<Vec<_>>()
    };
    // The changed function a head line belongs to: the innermost.
    let owner = |path: &str, line: u32| -> Option<(String, String)> {
        changed
            .iter()
            .filter(|f| f.path == path && f.side == Side::Head && f.range.start <= line && line <= f.range.end)
            .min_by_key(|f| f.range.end - f.range.start)
            .map(|f| (f.id.clone(), f.name.clone()))
    };
    let mut findings = rules.evaluate(&Input {
        files: &files,
        symbols: &symbols,
        reach: &reach,
        read: &read,
        tree: &tree,
        owner: &owner,
    });
    for log in sarif {
        let text = std::fs::read_to_string(log).with_context(|| log.display().to_string())?;
        findings.extend(
            crate::rules::sarif_findings(&text, root, &files).with_context(|| log.display().to_string())?,
        );
    }
    findings.sort_by(|a, b| (a.severity, &a.rule, &a.path, a.line).cmp(&(b.severity, &b.rule, &b.path, b.line)));

    let mut counts = Counts::default();
    for f in &findings {
        match f.severity {
            Severity::High => counts.high += 1,
            Severity::Medium => counts.medium += 1,
            Severity::Low => counts.low += 1,
        }
    }
    Ok(Assessment {
        version: ASSESS_VERSION,
        base: plan.base.clone(),
        head: plan.head.clone(),
        findings,
        counts,
    })
}

/// Spec/test files carry assertions about a change, not the change.
fn is_test_file(p: &Path) -> bool {
    let s = p.to_string_lossy();
    s.contains(".spec.") || s.contains(".test.") || s.contains("/tests/") || s.contains("/test/") || s.contains("/__tests__/")
}

impl Assessment {
    /// Whether any finding is at least as severe as `level`.
    pub fn fails(&self, level: Severity) -> bool {
        self.findings.iter().any(|f| f.severity <= level)
    }

    pub fn to_text(&self) -> String {
        use crate::color as c;
        let mut out = String::new();
        let short = |s: &str| s.chars().take(10).collect::<String>();
        out.push_str(&format!(
            "{}  {} → {}\n\n",
            c::bold("prognost assess"),
            c::dim(&short(&self.base)),
            c::dim(&self.head.as_deref().map(short).unwrap_or_else(|| "working tree".into()))
        ));
        if self.findings.is_empty() {
            out.push_str(&c::green("No rule matched.\n"));
        }
        for r in &self.findings {
            // Pad first, colour after: escape codes would throw the columns off.
            let (icon, paint): (&str, fn(&str) -> String) = match r.severity {
                Severity::High => ("⚠ high  ", c::bold_red),
                Severity::Medium => ("⚠ medium", c::bold_yellow),
                Severity::Low => ("· low   ", c::dim),
            };
            out.push_str(&format!(
                "  {} {} {}\n",
                paint(icon),
                c::bold(&format!("{:<34}", r.rule)),
                r.title
            ));
            let excerpt: String = r.excerpt.chars().take(90).collect();
            let more = if r.excerpt.chars().count() > 90 { "…" } else { "" };
            out.push_str(&format!(
                "             {}  {}\n",
                c::cyan(&format!("{}:{}", r.path, r.line)),
                c::dim(&format!("{excerpt}{more}"))
            ));
        }
        out.push_str(&format!(
            "\n{} {}, {}, {}.\n",
            c::bold("Assessment:"),
            c::bold_red(&format!("{} high", self.counts.high)),
            c::bold_yellow(&format!("{} medium", self.counts.medium)),
            c::dim(&format!("{} low", self.counts.low))
        ));
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan_json(version: u32) -> String {
        format!(
            r#"{{"version":{version},"base":"abc","head":null,"files":[],"functions":[],"calls":[],"summary":{{"changed":{{"added":0,"changed":0,"removed":0,"files_without_function_changes":0}},"reach":{{"functions":0,"files":0,"packages":[],"entries":{{}}}},"symbols":[]}},"truncated":false,"limits":{{"hops":12,"nodes":600}}}}"#
        )
    }

    #[test]
    fn a_plan_round_trips_and_an_old_one_is_refused() {
        let plan: PlanReport = serde_json::from_str(&plan_json(crate::plan::PLAN_VERSION)).unwrap();
        assert_eq!(serde_json::from_str::<PlanReport>(&serde_json::to_string(&plan).unwrap()).unwrap().base, "abc");
        let old: PlanReport = serde_json::from_str(&plan_json(1)).unwrap();
        let err = assess(Path::new("."), &old, &[]).unwrap_err().to_string();
        assert!(err.contains("rerun prognost plan"), "{err}");
    }
}
