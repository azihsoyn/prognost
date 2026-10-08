//! Turns an alignment into the rows a renderer shows — one struct that
//! both the plain-text CLI output and the TUI read, so "what a node is"
//! and "how it draws" stay two different questions.

use crate::align::{self, Alignment, CallChange};
use crate::ts_extract::TsFunction;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Added,
    Removed,
    /// Matched, and its call set differs from BASE to HEAD.
    Changed,
    /// Matched, byte-for-byte the same call set — collapsed by default.
    Unchanged,
}

#[derive(Debug, Clone)]
pub struct Row {
    pub kind: Kind,
    pub label: String,
    pub exported: bool,
    pub depth: usize,
    pub base_range: Option<(u32, u32)>,
    pub head_range: Option<(u32, u32)>,
    /// The line a `hide`/report jump should land on: HEAD's, or BASE's
    /// when the node only exists there.
    pub source_line: u32,
    pub details: Vec<CallChange>,
}

pub struct Report {
    pub rows: Vec<Row>,
    pub changed: usize,
    pub unchanged: usize,
}

pub fn build(base_fns: &[TsFunction], head_fns: &[TsFunction], alignment: &[Alignment]) -> Report {
    let mut rows = Vec::new();
    let mut changed = 0;
    let mut unchanged = 0;

    for a in alignment {
        match a {
            Alignment::Matched { base, head, .. } => {
                let (b, h) = (&base_fns[*base], &head_fns[*head]);
                let details = align::call_changes(b, h);
                let kind = if details.is_empty() {
                    unchanged += 1;
                    Kind::Unchanged
                } else {
                    changed += 1;
                    Kind::Changed
                };
                rows.push(Row {
                    kind,
                    label: align::label(b),
                    exported: b.exported,
                    depth: depth(head_fns, *head),
                    base_range: Some((b.start_line, b.end_line)),
                    head_range: Some((h.start_line, h.end_line)),
                    source_line: h.start_line,
                    details,
                });
            }
            Alignment::BaseOnly(b) => {
                changed += 1;
                let f = &base_fns[*b];
                rows.push(Row {
                    kind: Kind::Removed,
                    label: align::label(f),
                    exported: f.exported,
                    depth: depth(base_fns, *b),
                    base_range: Some((f.start_line, f.end_line)),
                    head_range: None,
                    source_line: f.start_line,
                    details: Vec::new(),
                });
            }
            Alignment::HeadOnly(h) => {
                changed += 1;
                let f = &head_fns[*h];
                rows.push(Row {
                    kind: Kind::Added,
                    label: align::label(f),
                    exported: f.exported,
                    depth: depth(head_fns, *h),
                    base_range: None,
                    head_range: Some((f.start_line, f.end_line)),
                    source_line: f.start_line,
                    details: Vec::new(),
                });
            }
        }
    }

    rows.sort_by_key(|r| r.source_line);
    Report {
        rows,
        changed,
        unchanged,
    }
}

/// How deep a node sits inside enclosing functions — a trigger reads as
/// "inside `observeClient`" rather than one more line in a flat list.
fn depth(fns: &[TsFunction], mut idx: usize) -> usize {
    let mut d = 0;
    while let Some(p) = fns[idx].parent {
        d += 1;
        idx = p;
    }
    d
}
