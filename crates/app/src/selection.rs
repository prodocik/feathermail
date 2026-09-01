use std::collections::HashSet;

/// Pure, id-based selection state for the Inbox list.
///
/// GTK's selection model stores positions, but the list is rebuilt while
/// pages and sync replies arrive.  This small state machine keeps the user
/// intent in stable thread ids and makes keyboard/mouse semantics testable
/// without constructing a GTK display.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct SelectionState {
    selected: HashSet<String>,
    anchor: Option<String>,
}

impl SelectionState {
    pub(crate) fn is_empty(&self) -> bool {
        self.selected.is_empty()
    }

    pub(crate) fn contains(&self, id: &str) -> bool {
        self.selected.contains(id)
    }

    /// T-054: whether this is a real multi-selection, and not just the
    /// mirror of the one open thread.
    ///
    /// `App::sync_selection` copies the single selected thread in here so
    /// the bulk actions can be written against one set. That makes
    /// [`Self::is_empty`] useless as the question "does the user have a
    /// multi-selection?": on an Inbox with any thread open the answer is
    /// always no. Escape asked it that way and so spent every press
    /// clearing a selection of one instead of closing the search box or
    /// the toast the user actually meant.
    pub(crate) fn is_multi(&self, single: &str) -> bool {
        match self.selected.len() {
            0 => false,
            1 => !self.selected.contains(single),
            _ => true,
        }
    }

    pub(crate) fn remove(&mut self, id: &str) {
        self.selected.remove(id);
        if self.anchor.as_deref() == Some(id) {
            self.anchor = self.selected.iter().next().cloned();
        }
    }

    pub(crate) fn clear(&mut self) {
        self.selected.clear();
        self.anchor = None;
    }

    /// Replace the set with the ids currently selected by GTK.  The first
    /// selected id becomes a useful Shift anchor when this comes from an
    /// external selection-model change.
    pub(crate) fn replace<I>(&mut self, ids: I)
    where
        I: IntoIterator<Item = String>,
    {
        self.selected = ids.into_iter().collect();
        if self
            .anchor
            .as_deref()
            .is_some_and(|anchor| self.selected.contains(anchor))
        {
            return;
        }
        self.anchor = self.selected.iter().next().cloned();
    }

    pub(crate) fn select_single(&mut self, id: &str) {
        self.selected.clear();
        self.selected.insert(id.to_string());
        self.anchor = Some(id.to_string());
    }

    /// Apply a row click against the visible, ordered thread ids.
    ///
    /// Ctrl toggles one id. Shift selects the inclusive range from the last
    /// anchor. Ctrl+Shift adds that range to the existing set, which is the
    /// least surprising composition of the two modifiers.
    pub(crate) fn click(&mut self, id: &str, visible: &[String], ctrl: bool, shift: bool) {
        if !visible.iter().any(|visible_id| visible_id == id) {
            return;
        }
        if shift {
            let anchor = self
                .anchor
                .as_deref()
                .filter(|anchor| visible.iter().any(|visible_id| visible_id == *anchor))
                .unwrap_or(id);
            let start = visible.iter().position(|visible_id| visible_id == anchor);
            let end = visible.iter().position(|visible_id| visible_id == id);
            if let (Some(start), Some(end)) = (start, end) {
                let (lo, hi) = if start <= end {
                    (start, end)
                } else {
                    (end, start)
                };
                if !ctrl {
                    self.selected.clear();
                }
                self.selected.extend(visible[lo..=hi].iter().cloned());
            }
            return;
        }
        if ctrl {
            if !self.selected.remove(id) {
                self.selected.insert(id.to_string());
            }
            self.anchor = Some(id.to_string());
            return;
        }
        self.select_single(id);
    }

    pub(crate) fn select_all<I>(&mut self, visible: I)
    where
        I: IntoIterator<Item = String>,
    {
        self.selected = visible.into_iter().collect();
        self.anchor = self.selected.iter().next().cloned();
    }

    /// Keep only rows that survived a list reload.  This is intentionally an
    /// intersection, not a clear: a sync can temporarily omit a row while a
    /// later page still brings it back.
    pub(crate) fn retain_visible(&mut self, visible: &[String]) {
        let visible_set: HashSet<&str> = visible.iter().map(String::as_str).collect();
        self.selected.retain(|id| visible_set.contains(id.as_str()));
        if self
            .anchor
            .as_deref()
            .is_some_and(|anchor| !visible_set.contains(anchor))
        {
            self.anchor = visible
                .iter()
                .find(|id| self.selected.contains(id.as_str()))
                .cloned();
        }
    }

    /// Return selected ids in the same order as the visible list.  Commands
    /// use this helper so row order is deterministic and no stale GTK index
    /// can leak into a bulk action.
    pub(crate) fn ordered_visible(&self, visible: &[String]) -> Vec<String> {
        visible
            .iter()
            .filter(|id| self.selected.contains(id.as_str()))
            .cloned()
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::SelectionState;

    fn ids(items: &[&str]) -> Vec<String> {
        items.iter().map(|id| (*id).to_string()).collect()
    }

    /// T-054: the mirror of the open thread is not a multi-selection.
    /// Escape's first branch reads this; when it read `is_empty()`
    /// instead, an Inbox with a thread open swallowed every Escape and
    /// the search box could not be closed with the keyboard at all.
    #[test]
    fn a_mirrored_single_selection_is_not_a_multi_selection() {
        let visible = ids(&["a", "b", "c"]);
        let mut state = SelectionState::default();
        assert!(!state.is_multi(""), "an empty set is nothing to clear");
        state.select_single("a");
        assert!(
            !state.is_multi("a"),
            "the one open thread mirrored into the set is not a multi-selection"
        );
        // A single row that is *not* the open thread is a selection the
        // user made on purpose, and Escape does clear it.
        assert!(state.is_multi("b"));
        state.click("b", &visible, true, false);
        assert!(state.is_multi("a"), "two rows are a multi-selection");
        state.clear();
        assert!(!state.is_multi("a"));
    }

    #[test]
    fn ctrl_toggles_without_losing_other_rows() {
        let visible = ids(&["a", "b", "c"]);
        let mut state = SelectionState::default();
        state.click("a", &visible, false, false);
        state.click("b", &visible, true, false);
        assert_eq!(state.ordered_visible(&visible), ids(&["a", "b"]));
        state.click("a", &visible, true, false);
        assert_eq!(state.ordered_visible(&visible), ids(&["b"]));
    }

    #[test]
    fn shift_selects_inclusive_ordered_range() {
        let visible = ids(&["a", "b", "c", "d"]);
        let mut state = SelectionState::default();
        state.click("b", &visible, false, false);
        state.click("d", &visible, false, true);
        assert_eq!(state.ordered_visible(&visible), ids(&["b", "c", "d"]));
        state.click("a", &visible, false, true);
        assert_eq!(state.ordered_visible(&visible), ids(&["a", "b"]));
    }

    #[test]
    fn ctrl_shift_adds_range_to_existing_selection() {
        let visible = ids(&["a", "b", "c", "d"]);
        let mut state = SelectionState::default();
        state.click("a", &visible, false, false);
        state.click("d", &visible, true, true);
        assert_eq!(state.ordered_visible(&visible), ids(&["a", "b", "c", "d"]));
    }

    #[test]
    fn select_all_and_escape_are_state_only() {
        let visible = ids(&["a", "b", "c"]);
        let mut state = SelectionState::default();
        state.select_all(visible.clone());
        assert_eq!(state.ordered_visible(&visible), visible);
        state.clear();
        assert!(state.is_empty());
    }

    #[test]
    fn reload_keeps_only_the_intersection() {
        let mut state = SelectionState::default();
        state.select_all(ids(&["a", "b", "c"]));
        state.retain_visible(&ids(&["b", "d"]));
        assert_eq!(state.ordered_visible(&ids(&["b", "d"])), ids(&["b"]));
    }

    #[test]
    fn action_ids_follow_visible_order_and_ignore_stale_rows() {
        let mut state = SelectionState::default();
        state.replace(ids(&["c", "missing", "a"]));
        assert_eq!(
            state.ordered_visible(&ids(&["a", "b", "c"])),
            ids(&["a", "c"])
        );
    }
}
