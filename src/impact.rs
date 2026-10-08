//! `--impact --json`: what a diff changed, and how each change is
//! reached — from every entry point the upstream walk found, down the
//! calls to the changed function it reaches first, with the file, line
//! range and calling line of every step. Deterministic analysis only;
//! what a change *means* is for whoever reads this.
//!
//! Lines are 1-based, of the head revision (the working tree when no
//! head was given); a removed function has only base lines, and says so
//! with `side: "base"`. Paths are relative to the repository root.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Raised when a change breaks what a reader of this document relies on.
pub const IMPACT_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ImpactReport {
    pub version: u32,
    /// The commit the diff is taken from: the merge base of `--base`
    /// (default: the remote's default branch) and the head.
    pub base: String,
    /// The head commit; `null` for the working tree.
    pub head: Option<String>,
    /// Every function whose body the diff added, removed or changed.
    pub changed: Vec<Function>,
    /// One per entry point: the shortest call path from it to the
    /// nearest changed function. An entry is a function nothing else
    /// was found to call, or a route handler (an entry even when a
    /// frontend calls it, so the API surface is always listed).
    pub chains: Vec<Chain>,
    /// The per-package summary.
    pub packages: Packages,
    /// The upstream walk stopped at its hop or node limit, so some
    /// chains may end before their real entry (`entry.kind:
    /// "unexplored"`).
    pub truncated: bool,
    pub limits: Limits,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Limits {
    /// Caller hops walked up from the changed functions.
    pub hops: usize,
    /// Node count after which the walk stops.
    pub nodes: usize,
}

/// A function (or a module-level reference, or a factory-built export)
/// in the call graph.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Function {
    /// Stable within one report; chains refer to functions by it.
    pub id: String,
    /// The function's name, `METHOD /path` for a route handler,
    /// `anon@<line>` for an anonymous function.
    pub name: String,
    pub path: String,
    /// First and last line of the body.
    pub line: u32,
    pub to: u32,
    /// Which revision `line`/`to` are in: "head", or "base" for a
    /// function only the base has.
    pub side: Side,
    pub change: Change,
    /// The workspace package holding the file, when there is one.
    pub package: Option<String>,
    /// For a route handler: `METHOD /mounted/path`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub route: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Side {
    Head,
    Base,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Change {
    Added,
    Removed,
    Changed,
    Unchanged,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Chain {
    pub entry: Entry,
    /// The id (in `changed`) of the changed function this chain ends at.
    pub changed: String,
    /// Every changed function reachable from this entry, nearest first.
    pub reaches: Vec<String>,
    /// From the entry down: `hops[0]` is the entry itself, each hop's
    /// `call_site` is its line calling the next hop, and the last hop's
    /// calls the changed function. Empty when the entry is itself the
    /// changed function.
    pub hops: Vec<Hop>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Entry {
    pub kind: EntryKind,
    /// The route for a handler, the function's name otherwise.
    pub label: String,
    pub id: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum EntryKind {
    /// A route handler.
    Route,
    /// A UI component (a `.svelte` file's script).
    Component,
    /// Top-level code of a module (a step table, a `main()` call).
    Module,
    /// An exported function no caller was found for.
    Export,
    /// A function no caller was found for that isn't exported.
    Function,
    /// The walk stopped before looking for this function's callers.
    Unexplored,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Hop {
    #[serde(flatten)]
    pub function: Function,
    /// The line in this function calling the next step; `null` when the
    /// call could not be placed (a reference passed as a value, say).
    pub call_site: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Packages {
    pub changed: Vec<PackageChange>,
    pub affected: Vec<PackageAffected>,
    pub crossings: Vec<Crossing>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct PackageChange {
    pub package: String,
    pub files: usize,
    pub functions: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct PackageAffected {
    pub package: String,
    /// Functions in it that call into changed code, transitively,
    /// without changes of their own.
    pub functions: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Crossing {
    pub from: String,
    pub to: String,
    pub calls: usize,
    /// Of those, calls the diff added or removed.
    pub changed_calls: usize,
    /// Route handlers among the callees.
    pub routes: Vec<String>,
}
