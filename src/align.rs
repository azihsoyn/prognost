//! The aligner: matches BASE and HEAD function nodes so a diff can say
//! "this is the same trigger, just shifted" instead of two unrelated
//! lists. Four tiers, most confident first, borrowed from gumtree's
//! anchor-then-propagate idea — an anchor found by name lets a positional
//! pass downstream trust "same slot under the same parent" instead of
//! guessing blind.
//!
//! Not an LLM, not fuzzy: every tier is a deterministic equality check.
//! What a human would call "obviously the same code" should fall out of
//! name equality or an exact body-hash match; anything that needs more
//! than that is left as added/removed rather than guessed at.

use std::collections::{HashMap, HashSet};

use crate::ts_extract::TsFunction;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    /// Same name (the closest single-file stand-in for "same package +
    /// export name + symbol path" — there is only one file here, so the
    /// symbol path collapses to the name). Cross-file "moved" is a later
    /// tier once this works on more than one file at a time.
    Exact,
    /// No name in common, but the normalized body text hashes identically
    /// — the classic anonymous-callback-that-didn't-change case.
    BodyHash,
    /// Neither of the above lined up; matched by being the Nth remaining
    /// child under an already-matched (or both-root) parent. Propagates
    /// downward: a parent matched this way becomes a valid anchor for
    /// its own children on the next pass, so this reaches arbitrarily
    /// deep nesting, not just one level.
    Positional,
}

#[derive(Debug, Clone)]
pub enum Alignment {
    Matched {
        base: usize,
        head: usize,
        tier: Tier,
    },
    BaseOnly(usize),
    HeadOnly(usize),
}

pub fn align(base: &[TsFunction], head: &[TsFunction]) -> Vec<Alignment> {
    let mut base_left: HashSet<usize> = (0..base.len()).collect();
    let mut head_left: HashSet<usize> = (0..head.len()).collect();
    let mut matched: HashMap<usize, usize> = HashMap::new();
    let mut out = Vec::new();

    // Tier 1: exact name match, taken in id order so a repeated name
    // resolves deterministically rather than by hash-set iteration order.
    let mut base_ids: Vec<usize> = base_left.iter().copied().collect();
    base_ids.sort_unstable();
    for bi in base_ids {
        let Some(name) = &base[bi].name else { continue };
        let mut head_ids: Vec<usize> = head_left.iter().copied().collect();
        head_ids.sort_unstable();
        let Some(hi) = head_ids
            .into_iter()
            .find(|&hi| head[hi].name.as_deref() == Some(name.as_str()))
        else {
            continue;
        };
        base_left.remove(&bi);
        head_left.remove(&hi);
        matched.insert(bi, hi);
        out.push(Alignment::Matched {
            base: bi,
            head: hi,
            tier: Tier::Exact,
        });
    }

    // Tier 3: identical normalized body, for whatever has no name in
    // common left — almost always an anonymous callback. A one-line
    // callback like `(item) => item.id` recurs all over a file,
    // so a pair whose parents already correspond is taken first (to a
    // fixed point: a callback paired here anchors its own callbacks);
    // only what is left is paired across parents, in id order.
    loop {
        let mut paired = false;
        let mut base_ids: Vec<usize> = base_left.iter().copied().collect();
        base_ids.sort_unstable();
        for bi in base_ids {
            let anchor = match base[bi].parent {
                None => None,
                Some(p) => match matched.get(&p) {
                    Some(&hp) => Some(hp),
                    None => continue,
                },
            };
            let mut head_ids: Vec<usize> = head_left.iter().copied().collect();
            head_ids.sort_unstable();
            let Some(hi) = head_ids
                .into_iter()
                .find(|&hi| head[hi].parent == anchor && head[hi].body_hash == base[bi].body_hash)
            else {
                continue;
            };
            base_left.remove(&bi);
            head_left.remove(&hi);
            matched.insert(bi, hi);
            out.push(Alignment::Matched {
                base: bi,
                head: hi,
                tier: Tier::BodyHash,
            });
            paired = true;
        }
        if !paired {
            break;
        }
    }
    let mut base_ids: Vec<usize> = base_left.iter().copied().collect();
    base_ids.sort_unstable();
    for bi in base_ids {
        let mut head_ids: Vec<usize> = head_left.iter().copied().collect();
        head_ids.sort_unstable();
        let Some(hi) = head_ids
            .into_iter()
            .find(|&hi| head[hi].body_hash == base[bi].body_hash)
        else {
            continue;
        };
        base_left.remove(&bi);
        head_left.remove(&hi);
        matched.insert(bi, hi);
        out.push(Alignment::Matched {
            base: bi,
            head: hi,
            tier: Tier::BodyHash,
        });
    }

    // Tier 4: positional, grouped by the HEAD id of an already-matched
    // parent (or by "root" when the parent is None on both sides).
    // Iterated to a fixed point rather than run once: a parent that
    // itself only gets matched positionally (an anonymous route handler
    // with no name and a changed body, say) isn't in `matched` yet on
    // the pass that would anchor its own children. A nested function
    // whose parent isn't resolved yet is left untouched for a later pass
    // rather than falling back to the shared root bucket — the root
    // bucket is exactly "every top-level declaration in the whole file",
    // and a real file has many; lumping every not-yet-anchored nested
    // function in there too invites matching a callback three levels
    // deep against some unrelated top-level handler that happens to
    // land at the same sorted position, not just failing to anchor it
    // precisely. Each pass can only newly resolve children of parents
    // matched in an earlier pass, so this always terminates — bounded by
    // the deepest nesting level actually present.
    loop {
        let matched_heads: HashSet<usize> = matched.values().copied().collect();
        let mut groups: HashMap<Option<usize>, (Vec<usize>, Vec<usize>)> = HashMap::new();
        for &bi in &base_left {
            match base[bi].parent {
                None => groups.entry(None).or_default().0.push(bi),
                Some(p) => {
                    if let Some(&anchor) = matched.get(&p) {
                        groups.entry(Some(anchor)).or_default().0.push(bi);
                    }
                    // Parent not resolved yet — wait for a later pass.
                }
            }
        }
        for &hi in &head_left {
            match head[hi].parent {
                None => groups.entry(None).or_default().1.push(hi),
                Some(p) if matched_heads.contains(&p) => {
                    groups.entry(Some(p)).or_default().1.push(hi);
                }
                Some(_) => {}
            }
        }
        let mut anchors: Vec<Option<usize>> = groups.keys().copied().collect();
        anchors.sort_unstable();
        let mut matched_this_pass = false;
        for anchor in anchors {
            let (mut bs, mut hs) = groups.remove(&anchor).unwrap_or_default();
            bs.sort_unstable();
            hs.sort_unstable();
            for (bi, hi) in bs.into_iter().zip(hs) {
                base_left.remove(&bi);
                head_left.remove(&hi);
                matched.insert(bi, hi);
                out.push(Alignment::Matched {
                    base: bi,
                    head: hi,
                    tier: Tier::Positional,
                });
                matched_this_pass = true;
            }
        }
        if !matched_this_pass {
            break;
        }
    }

    let mut base_left: Vec<usize> = base_left.into_iter().collect();
    base_left.sort_unstable();
    out.extend(base_left.into_iter().map(Alignment::BaseOnly));
    let mut head_left: Vec<usize> = head_left.into_iter().collect();
    head_left.sort_unstable();
    out.extend(head_left.into_iter().map(Alignment::HeadOnly));
    out
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CallChange {
    Added(String),
    Removed(String),
}

/// For a matched pair, which callees appear a different number of times —
/// a persisting function that quietly stopped calling something, or
/// started calling something new, without itself being added or removed.
pub fn call_changes(base: &TsFunction, head: &TsFunction) -> Vec<CallChange> {
    let mut counts: HashMap<&str, i32> = HashMap::new();
    for c in &base.calls {
        *counts.entry(c.as_str()).or_default() -= 1;
    }
    for c in &head.calls {
        *counts.entry(c.as_str()).or_default() += 1;
    }
    let mut names: Vec<&&str> = counts.keys().collect();
    names.sort();
    let mut out = Vec::new();
    for name in names {
        let n = counts[*name];
        match n.cmp(&0) {
            std::cmp::Ordering::Greater => {
                (0..n).for_each(|_| out.push(CallChange::Added((*name).to_string())))
            }
            std::cmp::Ordering::Less => {
                (0..-n).for_each(|_| out.push(CallChange::Removed((*name).to_string())))
            }
            std::cmp::Ordering::Equal => {}
        }
    }
    out
}

/// A stable display label: the declared/inferred name, or a position for
/// an anonymous node — always something a person can point at.
pub fn label(f: &TsFunction) -> String {
    match &f.name {
        Some(n) => n.clone(),
        None => format!("anon@{}", f.start_line),
    }
}

/// The label a BASE function goes by on a graph that spans both
/// revisions: its HEAD counterpart's when the aligner matched them, else
/// its own. An anonymous handler is labelled by its line, and a line
/// added above it shifts that; without this it would come back as a
/// removed handler standing next to an added one.
pub fn base_label(
    base: &[TsFunction],
    head: &[TsFunction],
    alignment: &[Alignment],
    base_idx: usize,
) -> String {
    let matched = alignment.iter().find_map(|a| match a {
        Alignment::Matched { base: b, head: h, .. } if *b == base_idx => Some(*h),
        _ => None,
    });
    match matched {
        Some(h) => label(&head[h]),
        None => label(&base[base_idx]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ts_extract::extract;

    #[test]
    fn six_scattered_triggers_align_to_the_same_node_after_a_line_shift() {
        let base = r#"
const resetPool = async () => {};
const connectOrFail = async () => {
  try {
    await run();
  } catch (err) {
    await resetPool(pool, err);
  }
};
const outer = () => {
  pool.on('error', (e) => { resetPoolLater(pool, e); });
  client.release = (e) => { resetPoolLater(pool, e); };
  const noteError = (e) => { resetPoolLater(pool, e); };
  const query = (...args) => {
    args[0] = (e, r) => { resetPoolLater(pool, e); };
    try {
      return original(...args).catch((e) => { resetPoolLater(pool, e); });
    } catch (e) {
      resetPoolLater(pool, e);
    }
  };
};
"#;
        // HEAD: two blank lines inserted up top (pure shift), the
        // connectOrFail trigger replaced by a throw, and a new
        // function + a new call inside resetPool.
        let head = r#"


const resetPool = async () => {
  await drainWaiters(pool);
};
const drainWaiters = async () => {};
const connectOrFail = async () => {
  try {
    await run();
  } catch (err) {
    throw err;
  }
};
const outer = () => {
  pool.on('error', (e) => { resetPoolLater(pool, e); });
  client.release = (e) => { resetPoolLater(pool, e); };
  const noteError = (e) => { resetPoolLater(pool, e); };
  const query = (...args) => {
    args[0] = (e, r) => { resetPoolLater(pool, e); };
    try {
      return original(...args).catch((e) => { resetPoolLater(pool, e); });
    } catch (e) {
      resetPoolLater(pool, e);
    }
  };
};
"#;
        let base_fns = extract(base, false).unwrap();
        let head_fns = extract(head, false).unwrap();
        let alignment = align(&base_fns, &head_fns);

        let matched_triggers = alignment
            .iter()
            .filter(|a| {
                let Alignment::Matched {
                    base: b, head: h, ..
                } = a
                else {
                    return false;
                };
                base_fns[*b]
                    .calls
                    .contains(&"resetPoolLater".to_string())
                    && head_fns[*h]
                        .calls
                        .contains(&"resetPoolLater".to_string())
            })
            .count();
        assert_eq!(matched_triggers, 6, "{alignment:#?}");

        // drainWaiters is new.
        assert!(alignment.iter().any(|a| matches!(a, Alignment::HeadOnly(h) if head_fns[*h].name.as_deref() == Some("drainWaiters"))));

        // resetPool persists (exact name match) and gained a call.
        let rotate_pool = alignment
            .iter()
            .find_map(|a| match a {
                Alignment::Matched { base, head, .. }
                    if base_fns[*base].name.as_deref() == Some("resetPool") =>
                {
                    Some((*base, *head))
                }
                _ => None,
            })
            .unwrap();
        let changes = call_changes(&base_fns[rotate_pool.0], &head_fns[rotate_pool.1]);
        assert_eq!(
            changes,
            vec![CallChange::Added("drainWaiters".to_string())]
        );

        // connectOrFail persists and lost its call to resetPool.
        let connect_with_retry = alignment
            .iter()
            .find_map(|a| match a {
                Alignment::Matched { base, head, .. }
                    if base_fns[*base].name.as_deref() == Some("connectOrFail") =>
                {
                    Some((*base, *head))
                }
                _ => None,
            })
            .unwrap();
        let changes = call_changes(
            &base_fns[connect_with_retry.0],
            &head_fns[connect_with_retry.1],
        );
        assert_eq!(changes, vec![CallChange::Removed("resetPool".to_string())]);
    }

    /// A callback passed to another call inside an anonymous handler —
    /// `db.transaction(..., async (client) => {...})` — has no name for
    /// tier 1, and its own body changed too (a new line added inside it)
    /// so tier 2 (body hash) can't catch it either; it can only be found
    /// by position under its parent. Its parent is itself anonymous with
    /// a changed body, so *that* also can only be found by position — at
    /// the top level, alongside an unrelated sibling that must stay out
    /// of the way. A single pass that anchors on `matched` as it stood
    /// *before* tier 4 ran (rather than iterating to a fixed point) would
    /// see the handler as unmatched when it tries to place the nested
    /// callback and fall back to pairing it against something under a
    /// different parent entirely, or leave it unmatched as if it were
    /// new. This is the real shape of a route handler wrapping its
    /// business logic in `db.transaction(async (client) => { ... })`.
    #[test]
    fn an_identical_callback_stays_with_its_own_parent() {
        // `first` loses its callback; `second` keeps an identical one.
        // The base `second`'s callback must pair with HEAD's `second`'s,
        // not with whichever identical callback comes first.
        let base = r#"
const first = () => xs.map((run) => run.id);
const second = () => ys.map((run) => run.id);
"#;
        let head = r#"
const first = () => xs.map(toId);
const second = () => ys.map((run) => run.id);
"#;
        let base_fns = extract(base, false).unwrap();
        let head_fns = extract(head, false).unwrap();
        let alignment = align(&base_fns, &head_fns);
        let removed: Vec<u32> = alignment
            .iter()
            .filter_map(|a| match a {
                Alignment::BaseOnly(b) => Some(base_fns[*b].start_line),
                _ => None,
            })
            .collect();
        assert_eq!(removed, vec![2], "the callback inside `first` is the removed one");
    }

    #[test]
    fn a_shifted_anonymous_handler_keeps_its_head_label() {
        // An import added above the router shifts every handler by one
        // line; the POST handler's body changes too (a callback inside
        // it is replaced by a call), so only the positional tier can
        // pair it. Its BASE-side label must then be the HEAD one.
        let base = r#"
export const app = new Hono()
  .get('/', async (c) => { return c.json(await list()); })
  .post('/', async (c) => {
    const rows = body.sections.map((s) => toDTO(s));
    return c.json(rows);
  });
"#;
        let head = r#"
import { toDTOs } from './dto.ts';
export const app = new Hono()
  .get('/', async (c) => { return c.json(await list()); })
  .post('/', async (c) => {
    const rows = toDTOs(body.sections);
    return c.json(rows);
  });
"#;
        let base_fns = extract(base, false).unwrap();
        let head_fns = extract(head, false).unwrap();
        let alignment = align(&base_fns, &head_fns);
        let post_base = base_fns
            .iter()
            .position(|f| f.route.as_deref() == Some("POST /"))
            .unwrap();
        let post_head = head_fns
            .iter()
            .position(|f| f.route.as_deref() == Some("POST /"))
            .unwrap();
        assert_ne!(label(&base_fns[post_base]), label(&head_fns[post_head]));
        assert_eq!(
            base_label(&base_fns, &head_fns, &alignment, post_base),
            label(&head_fns[post_head])
        );
        // The dropped `.map` callback has no counterpart: its own label.
        let cb = base_fns
            .iter()
            .position(|f| f.parent == Some(post_base) && f.route.is_none())
            .unwrap();
        assert_eq!(base_label(&base_fns, &head_fns, &alignment, cb), label(&base_fns[cb]));
    }

    #[test]
    fn a_callback_nested_two_levels_deep_still_matches_by_position() {
        let base = r#"
const other = () => {
  otherHandlerLogic();
};
const handler = async (c) => {
  await db.transaction(async (client) => {
    await client.step1();
  });
};
"#;
        let head = r#"
const other = () => {
  otherHandlerLogic();
};
const handler = async (c) => {
  await db.transaction(async (client) => {
    await client.step1();
    await client.step2();
  });
};
"#;
        let base_fns = extract(base, false).unwrap();
        let head_fns = extract(head, false).unwrap();
        let alignment = align(&base_fns, &head_fns);

        let nested_matched = alignment.iter().any(|a| {
            matches!(a, Alignment::Matched { base, head, .. }
                if base_fns[*base].calls.contains(&"client.step1".to_string())
                    && head_fns[*head].calls.contains(&"client.step1".to_string()))
        });
        assert!(nested_matched, "{alignment:#?}");

        let outer_matched = alignment.iter().any(|a| {
            matches!(a, Alignment::Matched { base, head, .. }
                if base_fns[*base].calls.contains(&"db.transaction".to_string())
                    && head_fns[*head].calls.contains(&"db.transaction".to_string()))
        });
        assert!(outer_matched, "{alignment:#?}");
    }
}
