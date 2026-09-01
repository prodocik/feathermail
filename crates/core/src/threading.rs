//! Folder-local conversation grouping (T-029, D22).
//!
//! Pure: no SQLite, no I/O, no GTK. The store loads a folder's headers,
//! this module assigns a surviving `threads.id` to each row, and the store
//! writes that assignment plus rollup. Gmail's `X-GM-THRID` beats JWZ
//! (Message-ID / In-Reply-To / References). Subject is never consulted.

use std::collections::HashMap;

/// Settings key flipped to `"1"` after the one-shot `rethread_folder` pass
/// on already-downloaded 1:1 mail. Not a user preference and not a DDL
/// migration — a live profile opened before T-029 would otherwise stay
/// one-UID-one-thread forever.
pub const RETHREAD_SETTINGS_KEY: &str = "threading_jwz_v1";

/// One message's grouping inputs. `row_id` is the current `threads.id`
/// (or the candidate that INSERT just created). The returned group id is
/// that existing id when one member of the group already has one, else a
/// stable pick from the same set.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ThreadHint {
    pub row_id: String,
    pub message_id: Option<String>,
    pub in_reply_to: Option<String>,
    pub references: Vec<String>,
    pub gm_thrid: Option<String>,
}

/// Assign each hint a surviving thread id.
///
/// Members that already share a `row_id` stay together. The same
/// `gm_thrid` forces a merge even when References disagree. Distinct
/// `gm_thrid` values never merge, even when JWZ would link them.
#[must_use]
pub fn assign_groups(hints: &[ThreadHint]) -> Vec<String> {
    let n = hints.len();
    if n == 0 {
        return Vec::new();
    }
    let mut uf = UnionFind::new_with(hints);

    // Already one thread in the store: keep them together regardless of
    // headers (a previous pass, or Gmail thrid written on 1:1 rows).
    let mut by_row: HashMap<&str, usize> = HashMap::new();
    for (i, h) in hints.iter().enumerate() {
        if h.row_id.is_empty() {
            continue;
        }
        if let Some(&j) = by_row.get(h.row_id.as_str()) {
            uf.union(i, j);
        } else {
            by_row.insert(h.row_id.as_str(), i);
        }
    }

    // Gmail: same X-GM-THRID is authoritative.
    let mut by_gm: HashMap<&str, usize> = HashMap::new();
    for (i, h) in hints.iter().enumerate() {
        let Some(thrid) = nonempty(h.gm_thrid.as_deref()) else {
            continue;
        };
        if let Some(&j) = by_gm.get(thrid) {
            uf.union(i, j);
        } else {
            by_gm.insert(thrid, i);
        }
    }

    // JWZ over Message-ID / In-Reply-To / References, but refuse to join
    // two components that already carry different gm_thrid values.
    let mut by_mid: HashMap<String, usize> = HashMap::new();
    for (i, h) in hints.iter().enumerate() {
        for id in related_ids(h) {
            if let Some(&j) = by_mid.get(&id) {
                try_union(&mut uf, i, j);
            } else {
                by_mid.insert(id, i);
            }
        }
    }

    let mut survivor: HashMap<usize, String> = HashMap::new();
    for (i, h) in hints.iter().enumerate() {
        let root = uf.find(i);
        survivor
            .entry(root)
            .and_modify(|s| {
                if h.row_id < *s {
                    *s = h.row_id.clone();
                }
            })
            .or_insert_with(|| h.row_id.clone());
    }
    (0..n).map(|i| survivor[&uf.find(i)].clone()).collect()
}

fn try_union(uf: &mut UnionFind, a: usize, b: usize) {
    if uf.find(a) == uf.find(b) {
        return;
    }
    let ta = component_gm_thrid(uf, a);
    let tb = component_gm_thrid(uf, b);
    match (ta, tb) {
        (Some(x), Some(y)) if x != y => {}
        _ => uf.union(a, b),
    }
}

/// The component's `gm_thrid`, if any member carries one -- O(alpha),
/// since each component's value now lives in the union-find itself
/// (`UnionFind::thrid`, kept in sync by `union`) instead of being
/// recomputed by scanning every hint on every call. Clones the (short)
/// string rather than borrowing it, so two calls in a row (see
/// `try_union`) don't fight over `uf`'s mutable borrow.
fn component_gm_thrid(uf: &mut UnionFind, i: usize) -> Option<String> {
    let root = uf.find(i);
    uf.thrid[root].clone()
}

fn related_ids(h: &ThreadHint) -> Vec<String> {
    let mut ids = Vec::new();
    push_id(&mut ids, h.message_id.as_deref());
    if let Some(irt) = h.in_reply_to.as_deref() {
        for tok in irt.split_whitespace() {
            push_id(&mut ids, Some(tok));
        }
    }
    for r in &h.references {
        push_id(&mut ids, Some(r.as_str()));
    }
    ids
}

fn push_id(ids: &mut Vec<String>, raw: Option<&str>) {
    if let Some(id) = nonempty(raw).map(str::to_string) {
        ids.push(id);
    }
}

fn nonempty(s: Option<&str>) -> Option<&str> {
    s.map(str::trim).filter(|s| !s.is_empty())
}

struct UnionFind {
    parent: Vec<usize>,
    /// Each component root's `gm_thrid`, if any member carries one. Kept
    /// in sync by `union` so `component_gm_thrid` is a `find` plus a
    /// lookup instead of a scan over every hint (see core-domain-03: that
    /// scan made `assign_groups` quadratic in folder size).
    thrid: Vec<Option<String>>,
}

impl UnionFind {
    fn new_with(hints: &[ThreadHint]) -> Self {
        let thrid = hints
            .iter()
            .map(|h| nonempty(h.gm_thrid.as_deref()).map(str::to_string))
            .collect();
        Self {
            parent: (0..hints.len()).collect(),
            thrid,
        }
    }

    fn find(&mut self, mut x: usize) -> usize {
        while self.parent[x] != x {
            self.parent[x] = self.parent[self.parent[x]];
            x = self.parent[x];
        }
        x
    }

    fn union(&mut self, a: usize, b: usize) {
        let pa = self.find(a);
        let pb = self.find(b);
        if pa == pb {
            return;
        }
        let (new_root, old_root) = if pa < pb { (pa, pb) } else { (pb, pa) };
        self.parent[old_root] = new_root;
        if self.thrid[new_root].is_none() {
            self.thrid[new_root] = self.thrid[old_root].take();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `assign_and_rollup` loads the *whole folder* and calls this on every
    /// 200-header batch, inside the sync write transaction. So the cost of
    /// one call has to grow with the folder, not with the folder squared.
    /// `component_gm_thrid` used to walk all `n` hints (calling `find` on
    /// each) for every JWZ id match, which made this O(n^2); it now reads
    /// the component's `gm_thrid` straight out of the union-find.
    ///
    /// Shape below: one root plus `n-1` replies to it -- an ordinary
    /// mailing-list thread. Budget is deliberately loose (a linear
    /// implementation measures ~11ms here in a debug build).
    #[test]
    fn assign_groups_is_not_quadratic_in_folder_size() {
        const N: usize = 20_000;
        let hints: Vec<ThreadHint> = (0..N)
            .map(|i| ThreadHint {
                row_id: format!("thr:{i:06}"),
                message_id: Some(format!("<m{i}@x>")),
                in_reply_to: (i > 0).then(|| "<m0@x>".to_string()),
                references: if i == 0 {
                    Vec::new()
                } else {
                    vec!["<m0@x>".to_string()]
                },
                gm_thrid: None,
            })
            .collect();

        let start = std::time::Instant::now();
        let groups = assign_groups(&hints);
        let elapsed = start.elapsed();

        assert_eq!(
            groups
                .iter()
                .collect::<std::collections::HashSet<_>>()
                .len(),
            1,
            "the whole reply chain is one conversation"
        );
        assert!(
            elapsed < std::time::Duration::from_millis(500),
            "assign_groups on {N} folder-local hints took {elapsed:?}; it is quadratic"
        );
    }

    fn hint(
        row_id: &str,
        message_id: Option<&str>,
        in_reply_to: Option<&str>,
        references: &[&str],
        gm_thrid: Option<&str>,
    ) -> ThreadHint {
        ThreadHint {
            row_id: row_id.into(),
            message_id: message_id.map(str::to_string),
            in_reply_to: in_reply_to.map(str::to_string),
            references: references.iter().map(|s| (*s).to_string()).collect(),
            gm_thrid: gm_thrid.map(str::to_string),
        }
    }

    fn groups(hints: &[ThreadHint]) -> Vec<String> {
        assign_groups(hints)
    }

    fn unique_count(assigned: &[String]) -> usize {
        let mut v = assigned.to_vec();
        v.sort();
        v.dedup();
        v.len()
    }

    #[test]
    fn three_message_in_reply_to_chain_is_one_group() {
        let hints = vec![
            hint("thr:a", Some("<a@x>"), None, &[], None),
            hint("thr:b", Some("<b@x>"), Some("<a@x>"), &[], None),
            hint("thr:c", Some("<c@x>"), Some("<b@x>"), &[], None),
        ];
        let assigned = groups(&hints);
        assert_eq!(unique_count(&assigned), 1);
        assert_eq!(assigned[0], assigned[1]);
        assert_eq!(assigned[1], assigned[2]);
        assert_eq!(assigned[0], "thr:a");
    }

    #[test]
    fn fifty_hints_in_a_reply_chain_are_one_group() {
        let hints: Vec<ThreadHint> = (0..50)
            .map(|i| {
                let mid = format!("<m{i}@x>");
                let parent = if i == 0 {
                    None
                } else {
                    Some(format!("<m{}@x>", i - 1))
                };
                ThreadHint {
                    row_id: format!("thr:{i}"),
                    message_id: Some(mid),
                    in_reply_to: parent,
                    references: Vec::new(),
                    gm_thrid: None,
                }
            })
            .collect();
        let assigned = groups(&hints);
        assert_eq!(assigned.len(), 50);
        assert_eq!(unique_count(&assigned), 1);
    }

    #[test]
    fn gm_thrid_beats_jwz_when_references_disagree() {
        let hints = vec![
            hint("thr:1", Some("<a@x>"), None, &["<other-1@x>"], Some("111")),
            hint("thr:2", Some("<b@x>"), None, &["<other-2@x>"], Some("111")),
        ];
        let assigned = groups(&hints);
        assert_eq!(unique_count(&assigned), 1);
    }

    #[test]
    fn distinct_gm_thrid_does_not_merge_even_when_jwz_would() {
        let hints = vec![
            hint("thr:1", Some("<a@x>"), None, &[], Some("111")),
            hint(
                "thr:2",
                Some("<b@x>"),
                Some("<a@x>"),
                &["<a@x>"],
                Some("222"),
            ),
        ];
        let assigned = groups(&hints);
        assert_eq!(unique_count(&assigned), 2);
        assert_ne!(assigned[0], assigned[1]);
    }

    #[test]
    fn same_message_id_groups_two_uids() {
        let hints = vec![
            hint("thr:1", Some("<dup@x>"), None, &[], None),
            hint("thr:2", Some("<dup@x>"), None, &[], None),
        ];
        let assigned = groups(&hints);
        assert_eq!(unique_count(&assigned), 1);
    }

    #[test]
    fn unrelated_hints_stay_apart() {
        let hints = vec![
            hint("thr:1", Some("<a@x>"), None, &[], None),
            hint("thr:2", Some("<b@x>"), None, &[], None),
        ];
        let assigned = groups(&hints);
        assert_eq!(unique_count(&assigned), 2);
    }

    #[test]
    fn empty_input_is_empty_output() {
        assert!(assign_groups(&[]).is_empty());
    }

    /// `by_row` merges same-`row_id` hints unconditionally, before either
    /// hint's `gm_thrid` is considered -- with the union-find now carrying
    /// one `thrid` per component (`UnionFind::thrid`, set once when a
    /// component gets its first non-empty value and never overwritten,
    /// mirroring the old scan's `for k in 0..n` "first match wins" order),
    /// that merged component's `thrid` must still correctly refuse a later
    /// JWZ link (`try_union`, via In-Reply-To) to a hint carrying a
    /// *different* `gm_thrid` -- exactly the case `distinct_gm_thrid_does_not_merge_even_when_jwz_would`
    /// covers for two single-hint components, now checked through a
    /// multi-hint one.
    #[test]
    fn a_row_id_merged_components_thrid_still_blocks_a_conflicting_jwz_link() {
        let hints = vec![
            hint("thr:x", Some("<a@x>"), None, &[], Some("111")),
            hint("thr:x", Some("<b@x>"), None, &[], None),
            hint("thr:y", Some("<c@x>"), Some("<b@x>"), &[], Some("222")),
        ];
        let assigned = groups(&hints);
        assert_eq!(assigned[0], assigned[1], "same row_id always merges");
        assert_eq!(
            unique_count(&assigned),
            2,
            "hint 2's gm_thrid (222) conflicts with the row_id-merged component's thrid (111), \
             so the In-Reply-To link to hint 1 must not merge it in"
        );
    }

    #[test]
    fn existing_shared_row_id_is_the_survivor() {
        let hints = vec![
            hint("thr:keep", Some("<a@x>"), None, &[], None),
            hint("thr:keep", Some("<b@x>"), None, &[], None),
        ];
        let assigned = groups(&hints);
        assert_eq!(
            assigned,
            vec!["thr:keep".to_string(), "thr:keep".to_string()]
        );
    }
}
