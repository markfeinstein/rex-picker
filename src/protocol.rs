use std::borrow::Cow;

use serde::{Deserialize, Serialize};
use serde_json::Value;

fn default_version() -> u32 {
    1
}

fn default_prompt() -> String {
    "› ".to_owned()
}

fn default_placeholder() -> String {
    "Filter items".to_owned()
}

fn default_max_visible() -> usize {
    12
}

/// A complete picker invocation.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct PickerDocument {
    /// Protocol version. Version 1 is the only supported version.
    #[serde(default = "default_version")]
    pub version: u32,
    /// Presentation and interaction options.
    #[serde(default)]
    pub options: PickerOptions,
    /// Candidate rows.
    pub items: Vec<PickerItem>,
}

impl PickerDocument {
    /// Validates invariants required by the picker engine.
    pub fn validate(&self) -> Result<(), String> {
        if self.version != 1 {
            return Err(format!(
                "unsupported picker protocol version {}",
                self.version
            ));
        }
        if self.options.max_visible == 0 {
            return Err("max_visible must be greater than zero".to_owned());
        }

        let mut ids = std::collections::HashSet::with_capacity(self.items.len());
        for item in &self.items {
            if item.id.is_empty() {
                return Err("item IDs must not be empty".to_owned());
            }
            if !ids.insert(item.id.as_str()) {
                return Err(format!("duplicate item ID {:?}", item.id));
            }
        }

        let mut action_ids = std::collections::HashSet::with_capacity(self.options.actions.len());
        let mut action_keys = std::collections::HashSet::with_capacity(self.options.actions.len());
        for action in &self.options.actions {
            if action.id.is_empty() {
                return Err("action IDs must not be empty".to_owned());
            }
            if !action_ids.insert(action.id.as_str()) {
                return Err(format!("duplicate action ID {:?}", action.id));
            }
            if !action_keys.insert(action.key.as_str()) {
                return Err(format!("duplicate action key {:?}", action.key));
            }
        }
        Ok(())
    }
}

/// Picker-wide display and behavior settings.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct PickerOptions {
    /// Optional heading above the query.
    #[serde(default)]
    pub title: Option<String>,
    /// Prompt rendered before the query.
    #[serde(default = "default_prompt")]
    pub prompt: String,
    /// Dimmed hint rendered while the query is empty.
    #[serde(default = "default_placeholder")]
    pub placeholder: String,
    /// Optional footer. Action hints are appended automatically.
    #[serde(default)]
    pub footer: Option<String>,
    /// Query present when the picker opens.
    #[serde(default)]
    pub initial_query: String,
    /// Allow selecting more than one item.
    #[serde(default)]
    pub multi: bool,
    /// Maximum rows shown at once.
    #[serde(default = "default_max_visible")]
    pub max_visible: usize,
    /// Additional accept actions, such as copy or open in the background.
    #[serde(default)]
    pub actions: Vec<PickerAction>,
}

impl Default for PickerOptions {
    fn default() -> Self {
        Self {
            title: None,
            prompt: default_prompt(),
            placeholder: default_placeholder(),
            footer: None,
            initial_query: String::new(),
            multi: false,
            max_visible: default_max_visible(),
            actions: Vec::new(),
        }
    }
}

/// An alternate action that accepts the current selection.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PickerAction {
    /// Stable machine-readable action name.
    pub id: String,
    /// Key name understood by the CLI, such as `ctrl-y` or `alt-o`.
    pub key: String,
    /// Human-readable footer label.
    pub label: String,
}

/// One searchable row.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct PickerItem {
    /// Stable identifier returned after selection.
    pub id: String,
    /// Primary visible text.
    pub label: String,
    /// Secondary visible text.
    #[serde(default)]
    pub description: Option<String>,
    /// Additional searchable text that is not necessarily displayed.
    #[serde(default)]
    pub search_text: Option<String>,
    /// Static preview content. Dynamic preview providers can be layered on later.
    #[serde(default)]
    pub preview: Option<String>,
    /// Adapter-defined data returned unchanged with the selection.
    #[serde(default)]
    pub metadata: Value,
}

impl PickerItem {
    /// Returns the text indexed by the fuzzy matcher.
    pub fn matcher_text(&self) -> Cow<'_, str> {
        match (&self.description, &self.search_text) {
            (None, None) => Cow::Borrowed(&self.label),
            (description, search_text) => {
                let mut text = String::with_capacity(
                    self.label.len()
                        + description.as_ref().map_or(0, |value| value.len() + 1)
                        + search_text.as_ref().map_or(0, |value| value.len() + 1),
                );
                text.push_str(&self.label);
                if let Some(description) = description {
                    text.push(' ');
                    text.push_str(description);
                }
                if let Some(search_text) = search_text {
                    text.push(' ');
                    text.push_str(search_text);
                }
                Cow::Owned(text)
            }
        }
    }
}

/// Machine-readable result emitted after the user accepts a selection.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct PickerResult {
    /// `accept` for Enter, or the configured alternate action ID.
    pub action: String,
    /// Selected rows in stable selection order.
    pub selected: Vec<PickerSelection>,
}

/// A selected row returned to an adapter.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct PickerSelection {
    pub id: String,
    pub label: String,
    pub metadata: Value,
}

impl From<&PickerItem> for PickerSelection {
    fn from(item: &PickerItem) -> Self {
        Self {
            id: item.id.clone(),
            label: item.label.clone(),
            metadata: item.metadata.clone(),
        }
    }
}
