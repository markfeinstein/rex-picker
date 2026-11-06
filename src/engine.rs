use std::{collections::HashSet, sync::Arc, thread};

use nucleo::{
    Config, Nucleo, Status, Utf32String,
    pattern::{CaseMatching, Normalization},
};

use crate::protocol::{PickerItem, PickerResult, PickerSelection};

#[derive(Debug)]
struct Candidate {
    /// Position in the injected item stream, used as a stable selection key.
    index: u32,
    item: PickerItem,
}

/// The non-visual picker state and high-performance fuzzy matcher.
///
/// Matching runs on Nucleo's worker pool, keeping the terminal event loop
/// responsive even when adapters stream thousands of candidates.
pub struct PickerEngine {
    matcher: Nucleo<Candidate>,
    query: String,
    cursor: usize,
    /// Selected candidate indices in selection order.
    selected: Vec<u32>,
    /// Membership index for `selected`, keeping per-row lookups O(1).
    selected_set: HashSet<u32>,
    multi: bool,
}

impl PickerEngine {
    /// Constructs an engine and begins indexing candidates immediately.
    pub fn new(items: Vec<PickerItem>, multi: bool) -> Self {
        let matcher = Nucleo::new(
            Config::DEFAULT.match_paths(),
            Arc::new(|| {}),
            available_matcher_threads(),
            1,
        );
        let mut engine = Self {
            matcher,
            query: String::new(),
            cursor: 0,
            selected: Vec::new(),
            selected_set: HashSet::new(),
            multi,
        };
        engine.inject(items);
        engine
    }

    /// Replaces every candidate while keeping the query and worker pool.
    ///
    /// Selections and the cursor are reset because candidate identities may
    /// have changed. Reusing the matcher avoids spawning a new thread pool for
    /// each scope switch, refresh, or asynchronous load.
    pub fn replace_items(&mut self, items: Vec<PickerItem>, multi: bool) {
        self.matcher.restart(true);
        self.cursor = 0;
        self.selected.clear();
        self.selected_set.clear();
        self.multi = multi;
        self.inject(items);
    }

    fn inject(&mut self, items: Vec<PickerItem>) {
        let injector = self.matcher.injector();
        for (index, item) in items.into_iter().enumerate() {
            let Ok(index) = u32::try_from(index) else {
                break;
            };
            injector.push(Candidate { index, item }, |candidate, columns| {
                // Convert straight into Nucleo's representation; the combined
                // text is not retained, avoiding a second copy of every row.
                columns[0] = Utf32String::from(candidate.item.matcher_text());
            });
        }
    }

    /// Current search query.
    pub fn query(&self) -> &str {
        &self.query
    }

    /// Replaces the query and starts a background rematch.
    pub fn set_query(&mut self, query: impl Into<String>) {
        let query = query.into();
        if query == self.query {
            return;
        }

        let append = query.starts_with(&self.query);
        self.query = query;
        self.cursor = 0;
        self.matcher.pattern.reparse(
            0,
            &self.query,
            CaseMatching::Smart,
            Normalization::Smart,
            append,
        );
    }

    /// Advances matcher work for at most `timeout_ms` milliseconds.
    pub fn tick(&mut self, timeout_ms: u64) -> Status {
        let status = self.matcher.tick(timeout_ms);
        self.clamp_cursor();
        status
    }

    /// Waits until the current match snapshot is complete.
    ///
    /// This is intended for tests, benchmarks, and non-interactive callers.
    pub fn settle(&mut self) {
        while self.tick(10).running {
            thread::yield_now();
        }
        // A final tick publishes work that completed after the previous snapshot.
        self.tick(0);
    }

    pub fn item_count(&self) -> usize {
        self.matcher.snapshot().item_count() as usize
    }

    pub fn match_count(&self) -> usize {
        self.matcher.snapshot().matched_item_count() as usize
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    pub fn current_item(&self) -> Option<&PickerItem> {
        self.matched_item(self.cursor)
    }

    pub fn matched_item(&self, index: usize) -> Option<&PickerItem> {
        self.matched_candidate(index)
            .map(|candidate| &candidate.item)
    }

    /// Whether the `index`th matched row is part of the multi-selection.
    pub fn is_matched_selected(&self, index: usize) -> bool {
        self.matched_candidate(index)
            .is_some_and(|candidate| self.selected_set.contains(&candidate.index))
    }

    /// Whether an item with this ID is part of the multi-selection.
    pub fn is_selected(&self, id: &str) -> bool {
        self.selected
            .iter()
            .filter_map(|index| self.item_by_index(*index))
            .any(|item| item.id == id)
    }

    pub fn selected_count(&self) -> usize {
        self.selected.len()
    }

    pub fn move_next(&mut self) {
        let count = self.match_count();
        if count > 0 {
            self.cursor = (self.cursor + 1) % count;
        }
    }

    pub fn move_previous(&mut self) {
        let count = self.match_count();
        if count > 0 {
            self.cursor = if self.cursor == 0 {
                count - 1
            } else {
                self.cursor - 1
            };
        }
    }

    pub fn move_page(&mut self, delta: isize, page_size: usize) {
        let count = self.match_count();
        if count == 0 {
            self.cursor = 0;
            return;
        }

        let distance = delta.saturating_mul(page_size as isize);
        self.cursor = self
            .cursor
            .saturating_add_signed(distance)
            .min(count.saturating_sub(1));
    }

    /// Toggles the current item when multi-selection is enabled.
    /// Returns whether the state changed.
    pub fn toggle_current(&mut self) -> bool {
        if !self.multi {
            return false;
        }
        let Some(index) = self
            .matched_candidate(self.cursor)
            .map(|candidate| candidate.index)
        else {
            return false;
        };

        if self.selected_set.remove(&index) {
            self.selected.retain(|selected| *selected != index);
        } else {
            self.selected_set.insert(index);
            self.selected.push(index);
        }
        true
    }

    /// Builds a result for the current or explicitly toggled selections.
    pub fn result(&self, action: impl Into<String>) -> Option<PickerResult> {
        let selected = if self.multi && !self.selected.is_empty() {
            self.selected
                .iter()
                .filter_map(|index| self.item_by_index(*index))
                .map(PickerSelection::from)
                .collect()
        } else {
            vec![PickerSelection::from(self.current_item()?)]
        };

        Some(PickerResult {
            action: action.into(),
            selected,
        })
    }

    fn matched_candidate(&self, index: usize) -> Option<&Candidate> {
        self.matcher
            .snapshot()
            .get_matched_item(index.try_into().ok()?)
            .map(|candidate| candidate.data)
    }

    fn item_by_index(&self, index: u32) -> Option<&PickerItem> {
        self.matcher
            .snapshot()
            .get_item(index)
            .map(|candidate| &candidate.data.item)
    }

    fn clamp_cursor(&mut self) {
        self.cursor = self.cursor.min(self.match_count().saturating_sub(1));
    }
}

fn available_matcher_threads() -> Option<usize> {
    thread::available_parallelism()
        .ok()
        .map(|parallelism| parallelism.get().clamp(1, 4))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn item(
        id: &str,
        label: &str,
        description: Option<&str>,
        search_text: Option<&str>,
    ) -> PickerItem {
        PickerItem {
            id: id.to_owned(),
            label: label.to_owned(),
            description: description.map(str::to_owned),
            search_text: search_text.map(str::to_owned),
            preview: None,
            metadata: json!({ "source": id }),
        }
    }

    fn engine(multi: bool) -> PickerEngine {
        let mut engine = PickerEngine::new(
            vec![
                item(
                    "sol",
                    "Sol 5.6",
                    Some("GitHub Copilot"),
                    Some("gpt-5.6-sol"),
                ),
                item(
                    "opus",
                    "Claude Opus",
                    Some("Anthropic"),
                    Some("claude-opus-4-6"),
                ),
                item(
                    "flash",
                    "Gemini Flash",
                    Some("Google"),
                    Some("gemini-3-flash"),
                ),
            ],
            multi,
        );
        engine.settle();
        engine
    }

    #[test]
    fn matches_hidden_search_text() {
        let mut engine = engine(false);
        engine.set_query("gpt56");
        engine.settle();

        assert_eq!(engine.match_count(), 1);
        assert_eq!(
            engine.current_item().map(|item| item.id.as_str()),
            Some("sol")
        );
    }

    #[test]
    fn navigation_wraps() {
        let mut engine = engine(false);
        engine.move_previous();
        assert_eq!(engine.cursor(), 2);
        engine.move_next();
        assert_eq!(engine.cursor(), 0);
    }

    #[test]
    fn multi_selection_preserves_selection_order() {
        let mut engine = engine(true);
        engine.move_next();
        assert!(engine.toggle_current());
        engine.move_next();
        assert!(engine.toggle_current());

        let result = engine.result("accept").expect("selection");
        assert_eq!(
            result
                .selected
                .iter()
                .map(|item| item.id.as_str())
                .collect::<Vec<_>>(),
            ["opus", "flash"]
        );
    }

    #[test]
    fn single_selection_uses_current_item() {
        let mut engine = engine(false);
        engine.move_next();
        let result = engine.result("copy").expect("selection");

        assert_eq!(result.action, "copy");
        assert_eq!(result.selected[0].id, "opus");
        assert_eq!(result.selected[0].metadata, json!({ "source": "opus" }));
    }

    #[test]
    fn replacing_items_keeps_query_and_clears_selection() {
        let mut engine = engine(true);
        engine.set_query("opus");
        engine.settle();
        assert!(engine.toggle_current());
        assert!(engine.is_matched_selected(0));

        engine.replace_items(
            vec![
                item("a", "Opus Magnum", None, None),
                item("b", "Unrelated", None, None),
            ],
            true,
        );
        engine.settle();

        assert_eq!(engine.query(), "opus");
        assert_eq!(engine.item_count(), 2);
        assert_eq!(engine.match_count(), 1);
        assert_eq!(engine.selected_count(), 0);
        assert!(!engine.is_matched_selected(0));
        assert_eq!(
            engine.current_item().map(|item| item.id.as_str()),
            Some("a")
        );
    }
}
