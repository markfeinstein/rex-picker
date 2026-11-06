use std::{
    cell::RefCell,
    collections::HashSet,
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    rc::Rc,
};

use anyhow::{Context, Result, bail};
use html5ever::{
    tendril::StrTendril,
    tokenizer::{
        BufferQueue, CharacterTokens, EndTag, StartTag, TagToken, Token, TokenSink,
        TokenSinkResult, Tokenizer,
    },
};
use regex::Regex;
use serde_json::{Value, json};
use url::Url;

use crate::{PickerAction, PickerDocument, PickerItem, PickerOptions, PickerResult};

const LINE_LIMIT: usize = 20_000;
const FORMAT_PROBE_BYTES: usize = 1_024;
const BLOB_CHUNK_BYTES: usize = 900_000;
const INITIAL_TAIL_BYTES: usize = 2_000_000;
const COPY_ACTION: &str = "copy";
const BACKGROUND_ACTION: &str = "background";

/// One unique HTTP(S) destination extracted from terminal scrollback.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UrlCandidate {
    pub url: String,
    pub label: Option<String>,
}

/// Captures recent Rex scrollback and extracts URL candidates.
#[derive(Clone, Debug)]
pub struct RexUrlSource {
    rex_bin: PathBuf,
    session: String,
    block: String,
    line_limit: usize,
}

impl RexUrlSource {
    pub fn new(
        rex_bin: impl Into<PathBuf>,
        session: impl Into<String>,
        block: impl Into<String>,
    ) -> Self {
        Self {
            rex_bin: rex_bin.into(),
            session: session.into(),
            block: block.into(),
            line_limit: LINE_LIMIT,
        }
    }

    pub fn load_document(&self) -> Result<PickerDocument> {
        let html = self.capture_recent_html()?;
        Ok(url_document(extract_candidates(&html)?, self.line_limit))
    }

    fn capture_recent_html(&self) -> Result<String> {
        self.capture_recent_html_api()
            .or_else(|_| self.capture_recent_html_fallback())
    }

    fn capture_recent_html_api(&self) -> Result<String> {
        let response = self.rex_api_call(
            "com.superlogical.terminal.format",
            json!({
                "format": "html",
                "trim": true,
                "unwrap": true,
                "max_bytes": FORMAT_PROBE_BYTES,
            }),
        )?;
        let content = response
            .get("content")
            .and_then(Value::as_str)
            .context("Rex terminal format returned invalid content")?;
        let Some(blob_id) = response.get("id").and_then(Value::as_u64) else {
            return Ok(last_lines(content, self.line_limit));
        };
        let Some(total) = response.get("total").and_then(Value::as_u64) else {
            return Ok(last_lines(content, self.line_limit));
        };
        let total = usize::try_from(total).context("Rex terminal blob is too large")?;

        let mut window_bytes = total.min(INITIAL_TAIL_BYTES);
        loop {
            let start = total.saturating_sub(window_bytes);
            let suffix = self.read_blob_suffix(blob_id, start, total)?;
            if start == 0 || suffix.bytes().filter(|byte| *byte == b'\n').count() >= self.line_limit
            {
                return Ok(last_lines(&suffix, self.line_limit));
            }
            let next_window = total.min(window_bytes.saturating_mul(2));
            if next_window == window_bytes {
                return Ok(last_lines(&suffix, self.line_limit));
            }
            window_bytes = next_window;
        }
    }

    fn read_blob_suffix(&self, blob_id: u64, start: usize, total: usize) -> Result<String> {
        let mut offset = start;
        let mut output = String::new();
        while offset < total {
            let response = self.rex_api_call(
                "com.superlogical.terminal.blob_read",
                json!({
                    "id": blob_id,
                    "offset": offset,
                    "max_bytes": BLOB_CHUNK_BYTES,
                }),
            )?;
            let data = response
                .get("data")
                .and_then(Value::as_str)
                .context("Rex terminal blob_read returned invalid data")?;
            if data.is_empty() {
                if response.get("eof").and_then(Value::as_bool) == Some(true) {
                    break;
                }
                bail!("Rex terminal blob_read made no progress");
            }
            output.push_str(data);
            offset = offset.saturating_add(data.len());
            if response.get("eof").and_then(Value::as_bool) == Some(true) {
                break;
            }
        }
        Ok(output)
    }

    fn capture_recent_html_fallback(&self) -> Result<String> {
        let output = self.run_rex([
            "capture",
            "-s",
            self.session.as_str(),
            "-b",
            self.block.as_str(),
            "--format=html",
            "--trim",
            "--unwrap",
        ])?;
        Ok(last_lines(&output, self.line_limit))
    }

    fn rex_api_call(&self, method: &str, args: Value) -> Result<Value> {
        let payload = serde_json::to_string(&json!({
            "block_id": self.block,
            "args": args,
        }))?;
        let output = self.run_rex([
            "api",
            "call",
            "-s",
            self.session.as_str(),
            method,
            payload.as_str(),
        ])?;
        serde_json::from_str(&output)
            .with_context(|| format!("decode Rex API response from {method}"))
    }

    fn run_rex<const N: usize>(&self, args: [&str; N]) -> Result<String> {
        let output = Command::new(&self.rex_bin)
            .args(["--autostart=false", "--timeout", "15s"])
            .args(args)
            .output()
            .with_context(|| format!("run {}", self.rex_bin.display()))?;
        if !output.status.success() {
            let error = String::from_utf8_lossy(&output.stderr);
            bail!("Rex command failed: {}", error.trim());
        }
        String::from_utf8(output.stdout).context("Rex command returned non-UTF-8 output")
    }
}

/// Executes URL actions selected by the picker.
#[derive(Clone, Debug)]
pub struct UrlActionHandler {
    open_bin: PathBuf,
    clipboard_bin: PathBuf,
}

impl UrlActionHandler {
    pub fn new(open_bin: impl Into<PathBuf>, clipboard_bin: impl Into<PathBuf>) -> Self {
        Self {
            open_bin: open_bin.into(),
            clipboard_bin: clipboard_bin.into(),
        }
    }

    pub fn handle_result(&self, result: &PickerResult) -> Result<Option<PickerDocument>> {
        // Row actions on an empty list are no-ops; the picker stays open.
        let Some(selected) = result.selected.first() else {
            return Ok(None);
        };
        let url = selected
            .metadata
            .get("url")
            .and_then(Value::as_str)
            .context("URL picker selection is missing its URL")?;
        match result.action.as_str() {
            "accept" => run_status(&self.open_bin, [url]),
            BACKGROUND_ACTION => run_status(&self.open_bin, ["-g", url]),
            COPY_ACTION => copy_to_clipboard(&self.clipboard_bin, url),
            action => bail!("unsupported URL picker action {action:?}"),
        }?;
        Ok(None)
    }
}

fn run_status<const N: usize>(program: &Path, args: [&str; N]) -> Result<()> {
    let status = Command::new(program)
        .args(args)
        .status()
        .with_context(|| format!("run {}", program.display()))?;
    if !status.success() {
        bail!("{} exited with status {status}", program.display());
    }
    Ok(())
}

fn copy_to_clipboard(program: &Path, value: &str) -> Result<()> {
    let mut child = Command::new(program)
        .stdin(Stdio::piped())
        .spawn()
        .with_context(|| format!("run {}", program.display()))?;
    child
        .stdin
        .take()
        .context("clipboard process did not expose stdin")?
        .write_all(value.as_bytes())
        .context("write URL to clipboard")?;
    let status = child.wait().context("wait for clipboard process")?;
    if !status.success() {
        bail!("{} exited with status {status}", program.display());
    }
    Ok(())
}

/// Returns a document that can render immediately while scrollback capture runs.
pub fn loading_url_document() -> PickerDocument {
    PickerDocument {
        version: 1,
        options: PickerOptions {
            title: Some("Open URL".to_owned()),
            prompt: "› ".to_owned(),
            placeholder: "Filter URLs".to_owned(),
            header: Some("Loading URLs…".to_owned()),
            footer: Some(format!("Capturing the newest {LINE_LIMIT} lines")),
            initial_query: String::new(),
            multi: false,
            preview_visible: true,
            max_visible: 12,
            actions: url_actions(),
        },
        items: Vec::new(),
    }
}

fn url_document(candidates: Vec<UrlCandidate>, line_limit: usize) -> PickerDocument {
    let is_empty = candidates.is_empty();
    let items = candidates
        .into_iter()
        .enumerate()
        .map(|(index, candidate)| PickerItem {
            id: index.to_string(),
            label: display_url(&candidate),
            description: None,
            search_text: Some(format!(
                "{} {}",
                candidate.url,
                candidate.label.as_deref().unwrap_or_default()
            )),
            preview: None,
            metadata: json!({ "url": candidate.url }),
        })
        .collect();
    PickerDocument {
        version: 1,
        options: PickerOptions {
            title: Some("Open URL".to_owned()),
            prompt: "› ".to_owned(),
            placeholder: "Filter URLs".to_owned(),
            header: is_empty
                .then(|| format!("No HTTP or HTTPS URLs found in the last {line_limit} lines")),
            footer: None,
            initial_query: String::new(),
            multi: false,
            preview_visible: true,
            max_visible: 12,
            actions: url_actions(),
        },
        items,
    }
}

fn url_actions() -> Vec<PickerAction> {
    vec![
        PickerAction {
            id: COPY_ACTION.to_owned(),
            key: "ctrl-y".to_owned(),
            label: "copy".to_owned(),
        },
        PickerAction {
            id: BACKGROUND_ACTION.to_owned(),
            key: "ctrl-b".to_owned(),
            label: "background".to_owned(),
        },
    ]
}

/// Extracts unique URLs in reverse terminal order (newest first).
pub fn extract_candidates(capture_html: &str) -> Result<Vec<UrlCandidate>> {
    let parsed = parse_terminal_html(capture_html);
    let url_pattern = Regex::new(r#"(?i)https?://[^\s<>"']+"#)?;
    let mut events = parsed.links;
    for matched in url_pattern.find_iter(&parsed.text) {
        if let Some(url) = clean_visible_url(matched.as_str()) {
            events.push((matched.start(), url, None));
        }
    }
    events.sort_by_key(|event| std::cmp::Reverse(event.0));

    let mut seen = HashSet::with_capacity(events.len());
    Ok(events
        .into_iter()
        .filter_map(|(_, url, label)| {
            seen.insert(url.clone())
                .then_some(UrlCandidate { url, label })
        })
        .collect())
}

#[derive(Default)]
struct ParsedHtml {
    text: String,
    links: Vec<(usize, String, Option<String>)>,
    active_link: Option<ActiveLink>,
}

struct ActiveLink {
    position: usize,
    href: String,
    label: String,
}

#[derive(Clone, Default)]
struct HtmlSink(Rc<RefCell<ParsedHtml>>);

impl TokenSink for HtmlSink {
    type Handle = ();

    fn process_token(&self, token: Token, _line_number: u64) -> TokenSinkResult<Self::Handle> {
        let mut parsed = self.0.borrow_mut();
        match token {
            CharacterTokens(characters) => {
                parsed.text.push_str(&characters);
                if let Some(link) = &mut parsed.active_link {
                    link.label.push_str(&characters);
                }
            }
            TagToken(tag) if &*tag.name == "a" && tag.kind == StartTag => {
                if let Some(active) = parsed.active_link.take() {
                    push_active_link(&mut parsed, active);
                }
                let href = tag
                    .attrs
                    .iter()
                    .find(|attribute| &*attribute.name.local == "href")
                    .map(|attribute| attribute.value.to_string());
                if let Some(href) = href {
                    parsed.active_link = Some(ActiveLink {
                        position: parsed.text.len(),
                        href,
                        label: String::new(),
                    });
                }
            }
            TagToken(tag) if &*tag.name == "a" && tag.kind == EndTag => {
                if let Some(active) = parsed.active_link.take() {
                    push_active_link(&mut parsed, active);
                }
            }
            _ => {}
        }
        TokenSinkResult::Continue
    }
}

fn push_active_link(parsed: &mut ParsedHtml, active: ActiveLink) {
    if let Some(url) = clean_hyperlink(&active.href) {
        parsed
            .links
            .push((active.position, url, clean_label(&active.label)));
    }
}

fn parse_terminal_html(value: &str) -> ParsedHtml {
    let sink = HtmlSink::default();
    let input = BufferQueue::default();
    input.push_back(StrTendril::from(value));
    let tokenizer = Tokenizer::new(sink.clone(), Default::default());
    let _ = tokenizer.feed(&input);
    tokenizer.end();
    let mut parsed = sink.0.borrow_mut();
    if let Some(active) = parsed.active_link.take() {
        push_active_link(&mut parsed, active);
    }
    std::mem::take(&mut *parsed)
}

fn clean_label(value: &str) -> Option<String> {
    let mut label = value.split_whitespace().collect::<Vec<_>>().join(" ");
    if label.is_empty() || clean_visible_url(&label).as_deref() == Some(label.as_str()) {
        return None;
    }
    if label.chars().count() > 56 {
        label = label.chars().take(55).collect::<String>() + "…";
    }
    Some(label)
}

fn clean_visible_url(value: &str) -> Option<String> {
    let mut candidate = value.trim();
    loop {
        let final_character = candidate.chars().next_back()?;
        let trim = matches!(final_character, '.' | ',' | ';' | ':' | '!' | '?')
            || matching_opener(final_character).is_some_and(|opener| {
                candidate.matches(final_character).count() > candidate.matches(opener).count()
            });
        if !trim {
            break;
        }
        candidate = &candidate[..candidate.len() - final_character.len_utf8()];
    }
    valid_http_url(candidate).then(|| candidate.to_owned())
}

fn clean_hyperlink(value: &str) -> Option<String> {
    let candidate = value.trim();
    valid_http_url(candidate).then(|| candidate.to_owned())
}

fn valid_http_url(value: &str) -> bool {
    if value.is_empty() || value.chars().any(char::is_control) {
        return false;
    }
    Url::parse(value).is_ok_and(|url| matches!(url.scheme(), "http" | "https") && url.has_host())
}

fn matching_opener(character: char) -> Option<char> {
    match character {
        ')' => Some('('),
        ']' => Some('['),
        '}' => Some('{'),
        _ => None,
    }
}

fn display_url(candidate: &UrlCandidate) -> String {
    let Ok(parsed) = Url::parse(&candidate.url) else {
        return candidate.url.clone();
    };
    let mut authority = parsed.host_str().unwrap_or_default().to_owned();
    if let Some(port) = parsed.port() {
        authority.push(':');
        authority.push_str(&port.to_string());
    }
    let mut suffix = if parsed.path().is_empty() {
        "/".to_owned()
    } else {
        parsed.path().to_owned()
    };
    if let Some(query) = parsed.query() {
        suffix.push('?');
        suffix.push_str(query);
    }
    if let Some(fragment) = parsed.fragment() {
        suffix.push('#');
        suffix.push_str(fragment);
    }
    match &candidate.label {
        Some(label) => format!("{label}  ·  {authority}{suffix}"),
        None => format!("{authority}  {suffix}"),
    }
}

fn last_lines(value: &str, limit: usize) -> String {
    let mut lines: Vec<&str> = value.lines().rev().take(limit).collect();
    lines.reverse();
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_hyperlinks_visible_urls_and_balanced_punctuation() {
        let html = concat!(
            "<div><a href=\"https://docs.example.com/guide_(one)\">Rex documentation</a></div>\n",
            "<div>Visible https://example.com/wiki/Function_(mathematics)).</div>\n",
            "<div>Duplicate https://example.com/wiki/Function_(mathematics)</div>\n",
            "<div>Newest https://second.example/path?q=1;</div>\n",
        );
        assert_eq!(
            extract_candidates(html).expect("candidates"),
            vec![
                UrlCandidate {
                    url: "https://second.example/path?q=1".to_owned(),
                    label: None,
                },
                UrlCandidate {
                    url: "https://example.com/wiki/Function_(mathematics)".to_owned(),
                    label: None,
                },
                UrlCandidate {
                    url: "https://docs.example.com/guide_(one)".to_owned(),
                    label: Some("Rex documentation".to_owned()),
                },
            ]
        );
    }

    #[test]
    fn formats_url_rows() {
        assert_eq!(
            display_url(&UrlCandidate {
                url: "https://example.com/path?q=1#section".to_owned(),
                label: None,
            }),
            "example.com  /path?q=1#section"
        );
        assert_eq!(
            display_url(&UrlCandidate {
                url: "https://docs.example.com/guide".to_owned(),
                label: Some("Documentation".to_owned()),
            }),
            "Documentation  ·  docs.example.com/guide"
        );
    }

    #[test]
    fn keeps_only_the_requested_line_suffix() {
        assert_eq!(last_lines("one\ntwo\nthree\nfour\n", 2), "three\nfour");
    }
}
