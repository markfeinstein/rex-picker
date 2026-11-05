use std::{sync::Arc, thread};

use nucleo::{
    Config, Nucleo, Status, Utf32String,
    pattern::{CaseMatching, Normalization},
};

use crate::protocol::{PickerItem, PickerResult, PickerSelection};

#[derive(Clone, Debug)]
struct Candidate {
    item: PickerItem,
    matcher_text: String,
}

/// The non-visual picker state and high-performance fuzzy matcher.
///
/// Matching runs on Nucleo's worker pool, keeping the terminal event loop
/// responsive even when adapters stream thousands of candidates.
pub struct PickerEngine {
    matcher: Nucleo<Candidate>,
    query: String,
    cursor: usize,
    selected_ids: Vec<String>,
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
        let injector = matcher.injector();
        for item in items {
            let candidate = Candidate {
                matcher_text: item.matcher_text().into_owned(),
                item,
            };
            injector.push(candidate, |candidate, columns| {
                columns[0] = Utf32String::from(candidate.matcher_text.as_str());
            });
        }
        drop(injector);

        Self {
            matcher,
            query: String::new(),
            cursor: 0,
            selected_ids: Vec::new(),
            multi,
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
        self.matcher
            .snapshot()
            .get_matched_item(index.try_into().ok()?)
            .map(|candidate| &candidate.data.item)
    }

    pub fn is_selected(&self, id: &str) -> bool {
        self.selected_ids.iter().any(|selected| selected == id)
    }

    pub fn selected_count(&self) -> usize {
        self.selected_ids.len()
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
        let Some(id) = self.current_item().map(|item| item.id.clone()) else {
            return false;
        };

        if let Some(position) = self
            .selected_ids
            .iter()
            .position(|selected| selected == &id)
        {
            self.selected_ids.remove(position);
        } else {
            self.selected_ids.push(id);
        }
        true
    }

    /// Builds a result for the current or explicitly toggled selections.
    pub fn result(&self, action: impl Into<String>) -> Option<PickerResult> {
        let selected = if self.multi && !self.selected_ids.is_empty() {
            self.selected_ids
                .iter()
                .filter_map(|id| self.item_by_id(id))
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

    fn item_by_id(&self, id: &str) -> Option<&PickerItem> {
        let snapshot = self.matcher.snapshot();
        (0..snapshot.item_count()).find_map(|index| {
            let candidate = snapshot.get_item(index)?;
            (candidate.data.item.id == id).then_some(&candidate.data.item)
        })
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
}
