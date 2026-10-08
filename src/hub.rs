//! One node, its callers, its callees — the neighbourhood a reviewer
//! actually walks, computed fresh from base/head function lists rather
//! than laid out once for the whole file. Walking to a different focus
//! just calls `build` again with a different label.

use std::collections::HashSet;

use crate::align::{self, Alignment};
use crate::ts_extract::TsFunction;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Unchanged,
    Added,
    Removed,
    /// Reserved for the focus node itself: present on both sides, but its
    /// own call set differs.
    Changed,
}

#[derive(Debug, Clone)]
pub struct Node {
    pub label: String,
    pub status: Status,
    /// Whether this label resolves to a real function rather than an
    /// unresolved/external call — drilling in does nothing otherwise.
    pub drillable: bool,
}

pub struct Hub {
    pub focus: Node,
    pub callers: Vec<Node>,
    pub callees: Vec<Node>,
}

pub fn build(
    base_fns: &[TsFunction],
    head_fns: &[TsFunction],
    alignment: &[Alignment],
    focus_label: &str,
) -> Option<Hub> {
    let head_idx = head_fns.iter().position(|f| align::label(f) == focus_label);
    let base_idx_by_name = base_fns.iter().position(|f| align::label(f) == focus_label);

    let matched = alignment.iter().find_map(|a| match a {
        Alignment::Matched { base, head, .. } if head_idx == Some(*head) => Some((*base, *head)),
        _ => None,
    });

    let (base_idx, head_idx, status) = match (matched, head_idx, base_idx_by_name) {
        (Some((b, h)), ..) => {
            let changed = !align::call_changes(&base_fns[b], &head_fns[h]).is_empty();
            (
                Some(b),
                Some(h),
                if changed {
                    Status::Changed
                } else {
                    Status::Unchanged
                },
            )
        }
        (None, Some(h), _) => (None, Some(h), Status::Added),
        (None, None, Some(b)) => (Some(b), None, Status::Removed),
        (None, None, None) => return None,
    };

    let label = head_idx
        .map(|h| align::label(&head_fns[h]))
        .unwrap_or_else(|| focus_label.to_string());

    Some(Hub {
        focus: Node {
            label,
            status,
            drillable: head_idx.is_some(),
        },
        callees: callees_of(base_fns, head_fns, base_idx, head_idx),
        callers: callers_of(base_fns, head_fns, alignment, base_idx, head_idx),
    })
}

fn callees_of(
    base_fns: &[TsFunction],
    head_fns: &[TsFunction],
    base_idx: Option<usize>,
    head_idx: Option<usize>,
) -> Vec<Node> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();

    if let Some(h) = head_idx {
        for c in &head_fns[h].calls {
            if !seen.insert(c.clone()) {
                continue;
            }
            let in_base = base_idx.is_some_and(|b| base_fns[b].calls.contains(c));
            let status = if in_base {
                Status::Unchanged
            } else {
                Status::Added
            };
            out.push(Node {
                label: c.clone(),
                status,
                drillable: head_fns.iter().any(|f| &align::label(f) == c),
            });
        }
    }
    if let Some(b) = base_idx {
        for c in &base_fns[b].calls {
            if seen.contains(c) {
                continue;
            }
            seen.insert(c.clone());
            out.push(Node {
                label: c.clone(),
                status: Status::Removed,
                drillable: false,
            });
        }
    }
    out
}

/// Who calls the focus, on either side of the diff.
///
/// A caller's own identity across revisions comes from the alignment's
/// matched pairs, not from comparing labels — an anonymous caller's label
/// carries its source line (`anon@613`), which shifts under an unrelated
/// earlier edit and would otherwise look like a different caller entirely,
/// reporting one real "still calls it" relationship as an add and a
/// remove instead of nothing changed.
///
/// A call made from inside an anonymous callback — a `.map` closure, a
/// transaction body — is a call by the named (or routed) function the
/// callback sits in: that's the caller listed, and the callback is not
/// a caller of its own. A handler that swaps `xs.map((x) => toDTO(x))`
/// for `toDTOs(xs)` then reads as "stopped calling toDTO, started
/// calling toDTOs", not as an anonymous function removed beside it.
fn callers_of(
    base_fns: &[TsFunction],
    head_fns: &[TsFunction],
    alignment: &[Alignment],
    base_idx: Option<usize>,
    head_idx: Option<usize>,
) -> Vec<Node> {
    let head_to_base: std::collections::HashMap<usize, usize> = alignment
        .iter()
        .filter_map(|a| match a {
            Alignment::Matched { base, head, .. } => Some((*head, *base)),
            _ => None,
        })
        .collect();
    let base_to_head: std::collections::HashMap<usize, usize> =
        head_to_base.iter().map(|(&h, &b)| (b, h)).collect();

    // A declared function is called by its bare name; a method — a
    // property of some object — as `<anything>.name(…)`, and the
    // receiver is whatever holds it, so the suffix is the only handle
    // (a Go or Python method's label carries its type, `Store.save`:
    // the suffix is the name after it).
    let calls_target = |f: &TsFunction, target: &str, method: bool| {
        let name = target.rsplit('.').next().unwrap_or(target);
        f.calls
            .iter()
            .any(|c| c == target || (method && c.ends_with(&format!(".{name}"))))
    };
    // The named or routed functions whose body — callbacks folded in —
    // calls `focus`, in source order, the focus itself excluded.
    let callers_in = |fns: &[TsFunction], focus: usize| -> Vec<usize> {
        let target = align::label(&fns[focus]);
        let method = fns[focus].method;
        let mut out: Vec<usize> = Vec::new();
        for (i, f) in fns.iter().enumerate() {
            if i == focus || !calls_target(f, &target, method) {
                continue;
            }
            let e = enclosing(fns, i);
            if e != focus && !out.contains(&e) {
                out.push(e);
            }
        }
        out
    };
    let head_callers = head_idx
        .map(|h| callers_in(head_fns, h))
        .unwrap_or_default();
    let base_callers = base_idx
        .map(|b| callers_in(base_fns, b))
        .unwrap_or_default();

    let mut out = Vec::new();
    for &h in &head_callers {
        let still_calls_in_base = head_to_base
            .get(&h)
            .is_some_and(|b| base_callers.contains(b));
        out.push(Node {
            label: align::label(&head_fns[h]),
            status: if still_calls_in_base {
                Status::Unchanged
            } else {
                Status::Added
            },
            drillable: true,
        });
    }
    for &b in &base_callers {
        let head_counterpart = base_to_head.get(&b).copied();
        if head_counterpart.is_some_and(|h| head_callers.contains(&h)) {
            continue; // already listed above, unchanged or added
        }
        out.push(Node {
            label: head_counterpart
                .map(|h| align::label(&head_fns[h]))
                .unwrap_or_else(|| align::label(&base_fns[b])),
            status: Status::Removed,
            drillable: head_counterpart.is_some(),
        });
    }
    out
}

/// The nearest named or routed function `idx` sits in (itself when it
/// is one); an anonymous callback is part of that function's body.
fn enclosing(fns: &[TsFunction], mut i: usize) -> usize {
    while fns[i].name.is_none() && fns[i].route.is_none() {
        match fns[i].parent {
            Some(p) => i = p,
            None => break,
        }
    }
    i
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::align::align;
    use crate::ts_extract::extract;

    /// The real shape one hop out from `resetPool`: it is called by
    /// `resetPoolLater` (the fire-and-forget wrapper the six
    /// scattered triggers actually call) and, until this change, directly
    /// by `connectOrFail`. Walking further to `resetPoolLater`
    /// is a second `build` call, not this hub's problem.
    fn fixture() -> (Vec<TsFunction>, Vec<TsFunction>) {
        let base = r#"
const resetPool = async () => { await logger.warn(1); };
const resetPoolLater = (p, e) => { resetPool(p, e); };
const connectOrFail = async () => { await resetPool(p, e); };
"#;
        let head = r#"
const resetPool = async () => { await drainWaiters(pool); };
const drainWaiters = async () => {};
const resetPoolLater = (p, e) => { resetPool(p, e); };
const connectOrFail = async () => { throw e; };
"#;
        (extract(base, false).unwrap(), extract(head, false).unwrap())
    }

    #[test]
    fn one_hop_out_is_the_wrapper_and_the_removed_direct_caller() {
        let (base, head) = fixture();
        let alignment = align(&base, &head);
        let hub = build(&base, &head, &alignment, "resetPool").unwrap();

        assert_eq!(hub.focus.status, Status::Changed);

        let mut callers: Vec<(&str, Status)> = hub
            .callers
            .iter()
            .map(|n| (n.label.as_str(), n.status))
            .collect();
        callers.sort_by_key(|(label, _)| *label);
        assert_eq!(
            callers,
            vec![
                ("connectOrFail", Status::Removed),
                ("resetPoolLater", Status::Unchanged)
            ]
        );

        let mut callees: Vec<(&str, Status)> = hub
            .callees
            .iter()
            .map(|n| (n.label.as_str(), n.status))
            .collect();
        callees.sort_by_key(|(label, _)| *label);
        assert_eq!(
            callees,
            vec![
                ("drainWaiters", Status::Added),
                ("logger.warn", Status::Removed)
            ]
        );
    }

    #[test]
    fn drilling_into_the_wrapper_surfaces_the_scattered_triggers() {
        let base = r#"
const resetPoolLater = () => {};
pool.on('error', (e) => { resetPoolLater(e); });
client.release = (e) => { resetPoolLater(e); };
"#;
        let base_fns = extract(base, false).unwrap();
        let alignment = align(&base_fns, &base_fns);
        let hub = build(&base_fns, &base_fns, &alignment, "resetPoolLater").unwrap();
        assert_eq!(hub.callers.len(), 2, "{:#?}", hub.callers);
        assert!(hub.callers.iter().all(|c| c.status == Status::Unchanged));
    }

    /// An anonymous caller's label carries its source line, which an
    /// unrelated earlier edit shifts — a caller matched by body hash, not
    /// name, across revisions. Naming it a caller by re-comparing labels
    /// (the bug this guards) reports one unchanged relationship as an
    /// add and a remove instead of nothing.
    #[test]
    fn an_anonymous_caller_that_only_moved_is_not_add_plus_remove() {
        let base = r#"
const resetPoolLater = () => {};
pool.on('error', (e) => { resetPoolLater(e); });
"#;
        let head = r#"


const resetPoolLater = () => {};
pool.on('error', (e) => { resetPoolLater(e); });
"#;
        let base_fns = extract(base, false).unwrap();
        let head_fns = extract(head, false).unwrap();
        let alignment = align(&base_fns, &head_fns);
        let hub = build(&base_fns, &head_fns, &alignment, "resetPoolLater").unwrap();
        assert_eq!(hub.callers.len(), 1, "{:#?}", hub.callers);
        assert_eq!(hub.callers[0].status, Status::Unchanged);
    }
}
