use std::{
    collections::{HashMap, HashSet},
    env,
    io::{Read, Write},
    os::unix::net::UnixStream,
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::{PickerAction, PickerDocument, PickerItem, PickerOptions, PickerResult};

const MAX_RESPONSE_BYTES: u64 = 4 * 1024 * 1024;
const TOGGLE_SCOPE_ACTION: &str = "toggle-scope";
const REFRESH_ACTION: &str = "refresh";

/// The set of models requested from Pi.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum PiModelScope {
    All,
    Scoped,
}

impl PiModelScope {
    fn as_str(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::Scoped => "scoped",
        }
    }

    fn toggled(self) -> Self {
        match self {
            Self::All => Self::Scoped,
            Self::Scoped => Self::All,
        }
    }
}

/// A model selection request accepted by the Pi extension.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PiModelSelectionRequest {
    pub action: String,
    pub provider: String,
    #[serde(rename = "modelId")]
    pub model_id: String,
    #[serde(rename = "thinkingLevel", skip_serializing_if = "Option::is_none")]
    pub thinking_level: Option<String>,
}

impl PiModelSelectionRequest {
    fn validate(&self) -> Result<()> {
        if self.action != "select" {
            bail!("model row contains unsupported action {:?}", self.action);
        }
        if self.provider.is_empty() || self.model_id.is_empty() {
            bail!("model row contains an empty provider or model ID");
        }
        Ok(())
    }
}

#[derive(Debug, Serialize)]
struct ListRequest {
    action: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    scope: Option<PiModelScope>,
    #[serde(skip_serializing_if = "is_false")]
    refresh: bool,
}

fn is_false(value: &bool) -> bool {
    !*value
}

/// Parsed metadata from the first line of a model-list response.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PiModelMetadata {
    pub protocol: u32,
    pub scope: PiModelScope,
    pub has_scoped_models: bool,
    pub can_toggle_scope: bool,
    pub can_refresh: bool,
    pub thinking_level: String,
    pub context: String,
    pub context_overflow: bool,
    pub tiered_pricing: bool,
    pub idle: bool,
    pub refresh_status: String,
}

impl PiModelMetadata {
    fn parse(value: &str) -> Result<Self> {
        let mut fields = value.split(';');
        if fields.next() != Some("ok") {
            bail!("Pi model bridge returned invalid list metadata");
        }
        let values: HashMap<&str, &str> =
            fields.filter_map(|field| field.split_once('=')).collect();
        let protocol = values
            .get("protocol")
            .map_or(Ok(1), |value| value.parse::<u32>())
            .context("parse Pi model-picker protocol version")?;
        let scope = match values.get("scope").copied().unwrap_or("all") {
            "all" => PiModelScope::All,
            "scoped" => PiModelScope::Scoped,
            value => bail!("Pi model bridge returned invalid scope {value:?}"),
        };
        let has_scoped_models = parse_flag(&values, "scoped", false)?;
        Ok(Self {
            protocol,
            scope,
            has_scoped_models,
            can_toggle_scope: parse_flag(&values, "cap_scope", has_scoped_models)?,
            can_refresh: parse_flag(&values, "cap_refresh", true)?,
            thinking_level: values.get("thinking").copied().unwrap_or("off").to_owned(),
            context: values.get("context").copied().unwrap_or("—").to_owned(),
            context_overflow: parse_flag(&values, "overflow", false)?,
            tiered_pricing: parse_flag(&values, "tiered", false)?,
            idle: parse_flag(&values, "idle", true)?,
            refresh_status: values
                .get("refresh")
                .copied()
                .unwrap_or("current")
                .to_owned(),
        })
    }
}

fn parse_flag(values: &HashMap<&str, &str>, key: &str, default: bool) -> Result<bool> {
    match values.get(key).copied() {
        None => Ok(default),
        Some("0") => Ok(false),
        Some("1") => Ok(true),
        Some(value) => bail!("Pi model bridge returned invalid {key} flag {value:?}"),
    }
}

#[derive(Clone, Debug)]
struct PiModelRow {
    stable_id: String,
    selection: PiModelSelectionRequest,
    visible: String,
    aliases: String,
}

#[derive(Clone, Debug)]
struct PiModelList {
    metadata: PiModelMetadata,
    header: String,
    rows: Vec<PiModelRow>,
}

/// A direct Unix-socket client for the Pi model-picker extension.
#[derive(Clone, Debug)]
pub struct PiModelClient {
    socket_path: PathBuf,
    timeout: Duration,
    refresh_timeout: Duration,
}

impl PiModelClient {
    pub fn new(socket_path: impl Into<PathBuf>) -> Self {
        Self {
            socket_path: socket_path.into(),
            timeout: Duration::from_secs(3),
            refresh_timeout: Duration::from_secs(20),
        }
    }

    pub fn for_block(block_id: &str) -> Result<Self> {
        Ok(Self::new(model_picker_socket_path(block_id)?))
    }

    pub fn with_timeouts(mut self, timeout: Duration, refresh_timeout: Duration) -> Self {
        self.timeout = timeout;
        self.refresh_timeout = refresh_timeout;
        self
    }

    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    fn list(&self, scope: Option<PiModelScope>, refresh: bool) -> Result<PiModelList> {
        let response = self.exchange(
            &ListRequest {
                action: "list",
                scope,
                refresh,
            },
            if refresh {
                self.refresh_timeout
            } else {
                self.timeout
            },
        )?;
        parse_list_response(&response)
    }

    fn select(&self, selection: &PiModelSelectionRequest) -> Result<String> {
        selection.validate()?;
        let response = self.exchange(selection, self.timeout)?;
        parse_status_response(&response)
    }

    fn exchange<T: Serialize>(&self, request: &T, timeout: Duration) -> Result<String> {
        let mut stream = UnixStream::connect(&self.socket_path)
            .with_context(|| format!("connect to {}", self.socket_path.display()))?;
        stream
            .set_read_timeout(Some(timeout))
            .context("configure Pi model-picker read timeout")?;
        stream
            .set_write_timeout(Some(timeout))
            .context("configure Pi model-picker write timeout")?;

        serde_json::to_writer(&mut stream, request).context("encode Pi model-picker request")?;
        stream
            .write_all(b"\n")
            .context("send Pi model-picker request")?;
        stream.flush().context("flush Pi model-picker request")?;

        let mut response = Vec::new();
        stream
            .take(MAX_RESPONSE_BYTES + 1)
            .read_to_end(&mut response)
            .context("read Pi model-picker response")?;
        if response.len() as u64 > MAX_RESPONSE_BYTES {
            bail!("Pi model-picker response exceeded {MAX_RESPONSE_BYTES} bytes");
        }
        String::from_utf8(response).context("Pi model-picker response is not UTF-8")
    }
}

/// Stateful adapter that supports in-picker scope toggling and catalog refresh.
pub struct PiModelAdapter {
    client: PiModelClient,
    requested_scope: Option<PiModelScope>,
}

impl PiModelAdapter {
    pub fn new(client: PiModelClient, scope: Option<PiModelScope>) -> Self {
        Self {
            client,
            requested_scope: scope,
        }
    }

    pub fn load(&mut self, refresh: bool) -> Result<PickerDocument> {
        let list = self.client.list(self.requested_scope, refresh)?;
        self.requested_scope = Some(list.metadata.scope);
        Ok(list.into_document())
    }

    /// Handles an action emitted by the reusable picker.
    ///
    /// A returned document replaces the visible model list without closing the
    /// picker. `None` means the selection was applied and the picker may exit.
    pub fn handle_result(&mut self, result: &PickerResult) -> Result<Option<PickerDocument>> {
        match result.action.as_str() {
            TOGGLE_SCOPE_ACTION => {
                self.requested_scope =
                    Some(self.requested_scope.unwrap_or(PiModelScope::All).toggled());
                self.load(false).map(Some)
            }
            REFRESH_ACTION => self.load(true).map(Some),
            "accept" => {
                let selected = result
                    .selected
                    .first()
                    .context("Pi model picker accepted without a selection")?;
                let request: PiModelSelectionRequest = serde_json::from_value(
                    selected
                        .metadata
                        .get("request")
                        .cloned()
                        .context("Pi model selection is missing its request")?,
                )
                .context("decode Pi model selection request")?;
                self.client.select(&request)?;
                Ok(None)
            }
            action => bail!("unsupported Pi model-picker action {action:?}"),
        }
    }
}

impl PiModelList {
    fn into_document(self) -> PickerDocument {
        let actions = model_actions(&self.metadata);
        let footer = model_footer(&self.metadata);
        let mut seen_ids = HashSet::with_capacity(self.rows.len());
        let items = self
            .rows
            .into_iter()
            .enumerate()
            .map(|(index, row)| {
                let mut id = format!("{}\u{1f}{}", row.stable_id, selection_key(&row.selection));
                if !seen_ids.insert(id.clone()) {
                    id.push_str(&format!("\u{1f}{index}"));
                    seen_ids.insert(id.clone());
                }
                PickerItem {
                    id,
                    label: row.visible,
                    description: None,
                    search_text: Some(format!(
                        "{} {} {}/{} {}",
                        row.aliases,
                        row.selection.provider,
                        row.selection.provider,
                        row.selection.model_id,
                        row.selection.thinking_level.as_deref().unwrap_or_default(),
                    )),
                    preview: None,
                    metadata: json!({
                        "stableId": row.stable_id,
                        "request": row.selection,
                    }),
                }
            })
            .collect();

        PickerDocument {
            version: 1,
            options: PickerOptions {
                title: Some("Pi Model Picker".to_owned()),
                prompt: "› ".to_owned(),
                placeholder: "Filter models".to_owned(),
                header: Some(self.header),
                footer: Some(footer),
                initial_query: String::new(),
                multi: false,
                preview_visible: true,
                max_visible: 12,
                actions,
            },
            items,
        }
    }
}

fn selection_key(selection: &PiModelSelectionRequest) -> String {
    format!(
        "{}/{}:{}",
        selection.provider,
        selection.model_id,
        selection.thinking_level.as_deref().unwrap_or_default()
    )
}

fn model_actions(metadata: &PiModelMetadata) -> Vec<PickerAction> {
    let mut actions = Vec::with_capacity(2);
    if metadata.can_toggle_scope && metadata.has_scoped_models {
        actions.push(PickerAction {
            id: TOGGLE_SCOPE_ACTION.to_owned(),
            key: "tab".to_owned(),
            label: "scope".to_owned(),
        });
    }
    if metadata.can_refresh {
        actions.push(PickerAction {
            id: REFRESH_ACTION.to_owned(),
            key: "ctrl-r".to_owned(),
            label: "refresh".to_owned(),
        });
    }
    actions
}

fn model_footer(metadata: &PiModelMetadata) -> String {
    let mut parts = vec![
        "✓ current".to_owned(),
        "★ default".to_owned(),
        metadata.scope.as_str().to_owned(),
        format!("think {}", metadata.thinking_level),
    ];
    if metadata.context != "—" {
        parts.push(format!(
            "{} {}",
            if metadata.context_overflow {
                "!ctx"
            } else {
                "ctx"
            },
            metadata.context
        ));
    }
    if metadata.tiered_pricing {
        parts.push("tiered pricing".to_owned());
    }
    if !metadata.idle {
        parts.push("Pi busy".to_owned());
    }
    match metadata.refresh_status.as_str() {
        "partial" => parts.push("partial refresh".to_owned()),
        "timeout" => parts.push("refresh timeout".to_owned()),
        _ => {}
    }
    parts.join("   ")
}

fn parse_status_response(response: &str) -> Result<String> {
    let line = response.lines().next().unwrap_or_default();
    let (status, detail) = line.split_once('\t').unwrap_or((line, ""));
    match status {
        "ok" => Ok(detail.to_owned()),
        "error" => bail!(
            "{}",
            if detail.is_empty() {
                "Pi model-picker request failed"
            } else {
                detail
            }
        ),
        _ => bail!("Pi model bridge returned an invalid status response"),
    }
}

fn parse_list_response(response: &str) -> Result<PiModelList> {
    if response.starts_with("error\t") {
        parse_status_response(response)?;
        unreachable!();
    }

    let mut lines = response.lines();
    let header_line = lines
        .next()
        .context("Pi model bridge returned an empty list")?;
    let header_fields: Vec<&str> = header_line.splitn(3, '\t').collect();
    let metadata = PiModelMetadata::parse(header_fields.first().copied().unwrap_or_default())?;
    let header = if metadata.protocol >= 2 {
        if header_fields.len() != 3 || !header_fields[1].is_empty() {
            bail!("Pi model bridge returned an invalid protocol 2 header");
        }
        sanitize_ansi_field(header_fields[2]).0
    } else {
        let visible = header_fields
            .get(1)
            .context("Pi model bridge returned an invalid legacy header")?;
        sanitize_ansi_field(visible).0
    };

    let mut rows = Vec::new();
    for (index, line) in lines.enumerate() {
        if line.is_empty() {
            continue;
        }
        let (stable_id, selection_json, visible) = if metadata.protocol >= 2 {
            let fields: Vec<&str> = line.splitn(3, '\t').collect();
            if fields.len() != 3 {
                bail!(
                    "Pi model bridge returned an invalid protocol 2 row {}",
                    index + 1
                );
            }
            (fields[0].to_owned(), fields[1], fields[2])
        } else {
            let (selection_json, visible) = line.split_once('\t').with_context(|| {
                format!(
                    "Pi model bridge returned an invalid legacy row {}",
                    index + 1
                )
            })?;
            let selection: PiModelSelectionRequest = serde_json::from_str(selection_json)
                .with_context(|| format!("decode Pi model row {}", index + 1))?;
            let stable_id =
                serde_json::to_string(&json!([selection.provider, selection.model_id]))?;
            let (visible, aliases) = sanitize_ansi_field(visible);
            selection.validate()?;
            rows.push(PiModelRow {
                stable_id,
                selection,
                visible,
                aliases,
            });
            continue;
        };

        let selection: PiModelSelectionRequest = serde_json::from_str(selection_json)
            .with_context(|| format!("decode Pi model row {}", index + 1))?;
        selection.validate()?;
        let (visible, aliases) = sanitize_ansi_field(visible);
        rows.push(PiModelRow {
            stable_id,
            selection,
            visible,
            aliases,
        });
    }
    if rows.is_empty() {
        bail!("Pi model bridge returned no selectable models");
    }

    Ok(PiModelList {
        metadata,
        header,
        rows,
    })
}

fn sanitize_ansi_field(value: &str) -> (String, String) {
    let mut visible = String::with_capacity(value.len());
    let mut concealed = String::new();
    let mut chars = value.chars().peekable();
    let mut is_concealed = false;

    while let Some(character) = chars.next() {
        if character == '\u{1b}' {
            match chars.next() {
                Some('[') => {
                    let mut parameters = String::new();
                    let mut final_byte = None;
                    for next in chars.by_ref() {
                        if ('@'..='~').contains(&next) {
                            final_byte = Some(next);
                            break;
                        }
                        parameters.push(next);
                    }
                    if final_byte == Some('m') {
                        for parameter in parameters.split(';') {
                            match parameter {
                                "0" | "28" => is_concealed = false,
                                "8" => is_concealed = true,
                                _ => {}
                            }
                        }
                    }
                }
                Some(']') => {
                    let mut previous_escape = false;
                    for next in chars.by_ref() {
                        if next == '\u{7}' || (previous_escape && next == '\\') {
                            break;
                        }
                        previous_escape = next == '\u{1b}';
                    }
                }
                Some(_) | None => {}
            }
            continue;
        }

        if character.is_control() {
            continue;
        }
        if is_concealed {
            concealed.push(character);
        } else {
            visible.push(character);
        }
    }

    (
        visible.trim_end().to_owned(),
        concealed.split_whitespace().collect::<Vec<_>>().join(" "),
    )
}

pub fn model_picker_socket_path(block_id: &str) -> Result<PathBuf> {
    let state_home = env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/state")))
        .context("HOME is not set and XDG_STATE_HOME is unavailable")?;
    Ok(model_picker_socket_path_in(&state_home, block_id))
}

pub fn model_picker_socket_path_in(state_home: &Path, block_id: &str) -> PathBuf {
    state_home
        .join("pi/model-picker")
        .join(format!("{}.sock", safe_block_id(block_id)))
}

fn safe_block_id(block_id: &str) -> String {
    block_id
        .strip_prefix("block:")
        .unwrap_or(block_id)
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '.' | '_' | '-') {
                character
            } else {
                '_'
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const V2_RESPONSE: &str = concat!(
        "ok;protocol=2;cap_scope=1;cap_refresh=1;scope=scoped;scoped=1;thinking=high;context=72K;overflow=0;tiered=1;idle=1;refresh=current\t\t   MODEL  PROVIDER\n",
        "[\"github-copilot\",\"gpt-5.6-sol\"]\t{\"action\":\"select\",\"provider\":\"github-copilot\",\"modelId\":\"gpt-5.6-sol\",\"thinkingLevel\":\"high\"}\t✓  Sol 5.6  GitHub\u{1b}[8m                                                                                                github-copilot/gpt-5.6-sol reasoning\u{1b}[0m\n",
        "[\"anthropic\",\"claude-opus-4-6\"]\t{\"action\":\"select\",\"provider\":\"anthropic\",\"modelId\":\"claude-opus-4-6\"}\t   Opus 4.6  Anthropic\n",
    );

    #[test]
    fn parses_protocol_two_rows_and_concealed_aliases() {
        let list = parse_list_response(V2_RESPONSE).expect("list");
        assert_eq!(list.metadata.protocol, 2);
        assert_eq!(list.metadata.scope, PiModelScope::Scoped);
        assert_eq!(list.header, "   MODEL  PROVIDER");
        assert_eq!(list.rows[0].visible, "✓  Sol 5.6  GitHub");
        assert!(list.rows[0].aliases.contains("gpt-5.6-sol"));

        let document = list.into_document();
        assert_eq!(document.items.len(), 2);
        assert_eq!(document.options.actions.len(), 2);
        assert!(
            document
                .options
                .footer
                .as_deref()
                .unwrap_or_default()
                .contains("tiered")
        );
    }

    #[test]
    fn parses_legacy_rows() {
        let response = concat!(
            "ok;scope=all;scoped=0;thinking=off;context=—;overflow=0;refresh=current\tMODEL\n",
            "{\"action\":\"select\",\"provider\":\"openai\",\"modelId\":\"gpt-5\"}\tGPT 5\n",
        );
        let list = parse_list_response(response).expect("legacy list");
        assert_eq!(list.metadata.protocol, 1);
        assert_eq!(list.rows[0].selection.model_id, "gpt-5");
    }

    #[test]
    fn derives_socket_path_from_rex_block() {
        let path = model_picker_socket_path_in(Path::new("/tmp/rex-picker-state"), "block:abc/def");
        assert_eq!(
            path,
            PathBuf::from("/tmp/rex-picker-state/pi/model-picker/abc_def.sock")
        );
    }
}
