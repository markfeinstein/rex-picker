use std::{
    fs::File,
    io::{Read, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    thread,
};

use anyhow::{Context, Result, bail};
use ignore::{WalkBuilder, WalkState};
use serde_json::{Value, json};

use crate::{
    PickerAction, PickerDocument, PickerItem, PickerOptions, PickerResult, PreviewProvider,
};

const MAX_FILES: usize = 5_000;
const TOGGLE_HIDDEN_ACTION: &str = "toggle-hidden";
const TOGGLE_IGNORED_ACTION: &str = "toggle-ignored";
const TOGGLE_PREVIEW_ACTION: &str = "toggle-preview";
const COPY_ACTION: &str = "copy-paths";
const REVEAL_ACTION: &str = "reveal";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ShellKind {
    Nushell,
    Zsh,
}

#[derive(Clone, Debug)]
struct FileCandidate {
    relative: String,
}

#[derive(Clone, Debug)]
pub struct FileSource {
    root: PathBuf,
    include_hidden: bool,
    include_ignored: bool,
    max_files: usize,
}

impl FileSource {
    pub fn new(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root
            .into()
            .canonicalize()
            .context("resolve file-picker working directory")?;
        if !root.is_dir() {
            bail!(
                "file-picker working directory is not a directory: {}",
                root.display()
            );
        }
        Ok(Self {
            root,
            include_hidden: false,
            include_ignored: true,
            max_files: MAX_FILES,
        })
    }

    fn discover(&self) -> Result<Vec<FileCandidate>> {
        let files = Arc::new(Mutex::new(Vec::new()));
        let errors = Arc::new(Mutex::new(Vec::new()));
        let count = Arc::new(AtomicUsize::new(0));
        let mut builder = WalkBuilder::new(&self.root);
        builder
            .standard_filters(!self.include_ignored)
            .hidden(!self.include_hidden)
            .follow_links(false)
            .threads(
                thread::available_parallelism()
                    .map(usize::from)
                    .unwrap_or(1)
                    .clamp(1, 8),
            )
            .filter_entry(|entry| entry.depth() == 0 || entry.file_name().to_str() != Some(".git"));
        let walker = builder.build_parallel();
        walker.run(|| {
            let files = Arc::clone(&files);
            let errors = Arc::clone(&errors);
            let count = Arc::clone(&count);
            let root = self.root.clone();
            let max_files = self.max_files;
            Box::new(move |entry| match entry {
                Ok(entry) => {
                    if !entry.file_type().is_some_and(|kind| kind.is_file()) {
                        return WalkState::Continue;
                    }
                    if count.fetch_add(1, Ordering::Relaxed) >= max_files {
                        return WalkState::Quit;
                    }
                    let Some(relative) =
                        entry.path().strip_prefix(&root).ok().and_then(Path::to_str)
                    else {
                        return WalkState::Continue;
                    };
                    if relative.contains(['\n', '\r']) {
                        return WalkState::Continue;
                    }
                    files
                        .lock()
                        .expect("file discovery lock")
                        .push(FileCandidate {
                            relative: relative.to_owned(),
                        });
                    WalkState::Continue
                }
                Err(error) => {
                    errors
                        .lock()
                        .expect("file discovery error lock")
                        .push(error.to_string());
                    WalkState::Continue
                }
            })
        });

        let mut files = Arc::into_inner(files)
            .expect("file discovery workers finished")
            .into_inner()
            .expect("file discovery lock");
        files.sort_unstable_by(|left, right| left.relative.cmp(&right.relative));
        files.truncate(self.max_files);
        if files.is_empty()
            && let Some(error) = errors.lock().expect("file discovery error lock").first()
        {
            bail!("file discovery failed: {error}");
        }
        Ok(files)
    }
}

pub struct FilePickerAdapter {
    source: FileSource,
    candidates: Vec<FileCandidate>,
    preview_visible: bool,
    rex_bin: PathBuf,
    session: String,
    block: String,
    shell: ShellKind,
    clipboard_bin: PathBuf,
    open_bin: PathBuf,
}

impl FilePickerAdapter {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        source: FileSource,
        rex_bin: impl Into<PathBuf>,
        session: impl Into<String>,
        block: impl Into<String>,
        shell: ShellKind,
        clipboard_bin: impl Into<PathBuf>,
        open_bin: impl Into<PathBuf>,
    ) -> Self {
        Self {
            source,
            candidates: Vec::new(),
            preview_visible: true,
            rex_bin: rex_bin.into(),
            session: session.into(),
            block: block.into(),
            shell,
            clipboard_bin: clipboard_bin.into(),
            open_bin: open_bin.into(),
        }
    }

    pub fn load(&mut self) -> Result<PickerDocument> {
        self.candidates = self.source.discover()?;
        Ok(self.document())
    }

    pub fn handle_result(&mut self, result: &PickerResult) -> Result<Option<PickerDocument>> {
        match result.action.as_str() {
            TOGGLE_HIDDEN_ACTION => {
                self.source.include_hidden = !self.source.include_hidden;
                self.load().map(Some)
            }
            TOGGLE_IGNORED_ACTION => {
                self.source.include_ignored = !self.source.include_ignored;
                self.load().map(Some)
            }
            TOGGLE_PREVIEW_ACTION => {
                self.preview_visible = !self.preview_visible;
                Ok(Some(self.document()))
            }
            COPY_ACTION => {
                let paths = self.selected_paths(result)?;
                copy_paths(&self.clipboard_bin, &paths)?;
                Ok(Some(self.document()))
            }
            REVEAL_ACTION => {
                let paths = self.selected_paths(result)?;
                reveal_paths(&self.open_bin, &self.source.root, &paths)?;
                Ok(Some(self.document()))
            }
            "accept" => {
                let paths = self.selected_paths(result)?;
                self.open_in_editor(&paths)?;
                Ok(None)
            }
            action => bail!("unsupported file-picker action {action:?}"),
        }
    }

    fn document(&self) -> PickerDocument {
        let items = self
            .candidates
            .iter()
            .enumerate()
            .map(|(index, candidate)| PickerItem {
                id: index.to_string(),
                label: candidate.relative.clone(),
                description: None,
                search_text: None,
                preview: None,
                metadata: json!({ "path": candidate.relative }),
            })
            .collect();
        PickerDocument {
            version: 1,
            options: PickerOptions {
                title: Some("File Picker".to_owned()),
                prompt: "files › ".to_owned(),
                placeholder: "Filter files".to_owned(),
                header: None,
                footer: Some(format!(
                    "hidden {}   ignored {}",
                    on_off(self.source.include_hidden),
                    on_off(self.source.include_ignored),
                )),
                initial_query: String::new(),
                multi: true,
                preview_visible: self.preview_visible,
                max_visible: 18,
                actions: vec![
                    PickerAction {
                        id: TOGGLE_HIDDEN_ACTION.to_owned(),
                        key: "alt-.".to_owned(),
                        label: "hidden".to_owned(),
                    },
                    PickerAction {
                        id: TOGGLE_IGNORED_ACTION.to_owned(),
                        key: "alt-i".to_owned(),
                        label: "ignored".to_owned(),
                    },
                    PickerAction {
                        id: TOGGLE_PREVIEW_ACTION.to_owned(),
                        key: "alt-v".to_owned(),
                        label: "preview".to_owned(),
                    },
                    PickerAction {
                        id: COPY_ACTION.to_owned(),
                        key: "alt-y".to_owned(),
                        label: "copy paths".to_owned(),
                    },
                    PickerAction {
                        id: REVEAL_ACTION.to_owned(),
                        key: "alt-o".to_owned(),
                        label: "reveal".to_owned(),
                    },
                ],
            },
            items,
        }
    }

    fn selected_paths(&self, result: &PickerResult) -> Result<Vec<String>> {
        if result.selected.is_empty() {
            bail!("file picker accepted without a selection");
        }
        result
            .selected
            .iter()
            .map(|selected| {
                let relative = selected
                    .metadata
                    .get("path")
                    .and_then(Value::as_str)
                    .context("file selection is missing its path")?;
                validate_relative_path(relative)?;
                Ok(relative.to_owned())
            })
            .collect()
    }

    fn open_in_editor(&self, paths: &[String]) -> Result<()> {
        let mut command = String::from("nvim");
        if paths.len() > 1 {
            command.push_str(" -O2");
        }
        command.push_str(" --");
        for path in paths {
            command.push(' ');
            command.push_str(&quote_for_shell(path, self.shell)?);
        }
        self.rex(["send-key", "-s", &self.session, "-b", &self.block, "ctrl+c"])?;
        self.rex(["send", "-s", &self.session, "-b", &self.block, &command])?;
        self.rex(["send-key", "-s", &self.session, "-b", &self.block, "enter"])?;
        Ok(())
    }

    fn rex<const N: usize>(&self, args: [&str; N]) -> Result<()> {
        let status = Command::new(&self.rex_bin)
            .args(["--autostart=false"])
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .with_context(|| format!("run {}", self.rex_bin.display()))?;
        if !status.success() {
            bail!("{} command failed", self.rex_bin.display());
        }
        Ok(())
    }
}

fn on_off(value: bool) -> &'static str {
    if value { "on" } else { "off" }
}

fn validate_relative_path(path: &str) -> Result<()> {
    let path = Path::new(path);
    if path.is_absolute()
        || path
            .components()
            .any(|component| matches!(component, std::path::Component::ParentDir))
    {
        bail!("file picker returned an unsafe path");
    }
    Ok(())
}

fn quote_for_shell(path: &str, shell: ShellKind) -> Result<String> {
    validate_relative_path(path)?;
    Ok(match shell {
        ShellKind::Nushell => serde_json::to_string(path)?,
        ShellKind::Zsh => format!("'{}'", path.replace(char::from(39), "'\\''")),
    })
}

fn copy_paths(program: &Path, paths: &[String]) -> Result<()> {
    let mut child = Command::new(program)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("run {}", program.display()))?;
    let mut input = paths.join("\n");
    input.push('\n');
    child
        .stdin
        .take()
        .context("clipboard process did not expose stdin")?
        .write_all(input.as_bytes())
        .context("copy file paths")?;
    let status = child.wait().context("wait for clipboard process")?;
    if !status.success() {
        bail!("{} failed to copy paths", program.display());
    }
    Ok(())
}

fn reveal_paths(program: &Path, root: &Path, paths: &[String]) -> Result<()> {
    let status = Command::new(program)
        .arg("-R")
        .args(paths.iter().map(|path| root.join(path)))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .with_context(|| format!("run {}", program.display()))?;
    if !status.success() {
        bail!("{} failed to reveal paths", program.display());
    }
    Ok(())
}

pub fn loading_file_document() -> PickerDocument {
    PickerDocument {
        version: 1,
        options: PickerOptions {
            title: Some("File Picker".to_owned()),
            prompt: "files › ".to_owned(),
            placeholder: "Filter files".to_owned(),
            header: Some("Discovering files…".to_owned()),
            footer: None,
            initial_query: String::new(),
            multi: true,
            preview_visible: true,
            max_visible: 18,
            actions: Vec::new(),
        },
        items: Vec::new(),
    }
}

pub struct FilePreviewProvider {
    root: PathBuf,
    max_bytes: u64,
    max_lines: usize,
}

impl FilePreviewProvider {
    pub fn new(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into().canonicalize().context("resolve preview root")?;
        Ok(Self {
            root,
            max_bytes: 128 * 1024,
            max_lines: 1_000,
        })
    }
}

impl PreviewProvider for FilePreviewProvider {
    fn load(&self, item: &PickerItem) -> Result<Option<String>> {
        let relative = item
            .metadata
            .get("path")
            .and_then(Value::as_str)
            .context("file preview item is missing its path")?;
        validate_relative_path(relative)?;
        let path = self.root.join(relative);
        let canonical = path
            .canonicalize()
            .with_context(|| format!("resolve preview path {}", path.display()))?;
        if !canonical.starts_with(&self.root) || !canonical.is_file() {
            bail!("preview path escaped the picker root");
        }

        let metadata = canonical.metadata().context("read preview metadata")?;
        let mut bytes = Vec::with_capacity(metadata.len().min(self.max_bytes) as usize);
        File::open(&canonical)
            .with_context(|| format!("open {}", canonical.display()))?
            .take(self.max_bytes + 1)
            .read_to_end(&mut bytes)
            .with_context(|| format!("read {}", canonical.display()))?;
        if bytes.iter().take(8 * 1024).any(|byte| *byte == 0) {
            return Ok(Some(format!(
                "Binary file\n{} bytes\n{}",
                metadata.len(),
                relative
            )));
        }
        let truncated = bytes.len() as u64 > self.max_bytes;
        bytes.truncate(self.max_bytes as usize);
        let text = String::from_utf8_lossy(&bytes);
        let line_count = text.lines().count().min(self.max_lines);
        let width = line_count.max(1).to_string().len();
        let mut preview = String::new();
        for (index, line) in text.lines().take(self.max_lines).enumerate() {
            use std::fmt::Write as _;
            let _ = writeln!(preview, "{:>width$} │ {line}", index + 1);
        }
        if truncated || text.lines().count() > self.max_lines {
            preview.push_str("… preview truncated\n");
        }
        Ok(Some(preview))
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    #[test]
    fn quotes_paths_for_nushell_and_zsh() {
        assert_eq!(
            quote_for_shell("a file's name.txt", ShellKind::Nushell).expect("Nushell quote"),
            "\"a file's name.txt\""
        );
        assert_eq!(
            quote_for_shell("a file's name.txt", ShellKind::Zsh).expect("Zsh quote"),
            "'a file'\\''s name.txt'"
        );
    }

    #[test]
    fn preview_reads_text_and_detects_binary_files() {
        let directory = tempfile::tempdir().expect("temporary directory");
        fs::write(directory.path().join("text.txt"), "first\nsecond\n").expect("text fixture");
        fs::write(directory.path().join("binary"), b"a\0b").expect("binary fixture");
        let provider = FilePreviewProvider::new(directory.path()).expect("provider");
        let item = |path: &str| PickerItem {
            id: path.to_owned(),
            label: path.to_owned(),
            description: None,
            search_text: None,
            preview: None,
            metadata: json!({ "path": path }),
        };

        let text = provider
            .load(&item("text.txt"))
            .expect("text preview")
            .expect("preview");
        assert!(text.contains("1 │ first"));
        assert!(text.contains("2 │ second"));
        let binary = provider
            .load(&item("binary"))
            .expect("binary preview")
            .expect("preview");
        assert!(binary.starts_with("Binary file"));
    }
}
