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
    depth: usize,
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

    /// Discovers at most `max_files` files, preferring the shallowest.
    ///
    /// The cap is deterministic: every file above the cutoff depth is kept,
    /// plus the alphabetically first files at the cutoff depth. Parallel
    /// walkers count files per depth and stop descending once shallower
    /// levels already fill the cap, so large trees are pruned early.
    fn discover(&self) -> Result<Vec<FileCandidate>> {
        let shared = Arc::new(DiscoveryShared {
            files: Mutex::new(Vec::new()),
            errors: Mutex::new(Vec::new()),
            depth_counts: (0..MAX_TRACKED_DEPTH)
                .map(|_| AtomicUsize::new(0))
                .collect(),
            cutoff: AtomicUsize::new(usize::MAX),
            deepest: AtomicUsize::new(0),
            max_files: self.max_files,
        });
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
            // Each walker thread collects into its own buffer and merges once
            // when it finishes, instead of contending on a lock per file.
            let mut visitor = DiscoveryVisitor {
                files: Vec::new(),
                shared: Arc::clone(&shared),
                root: self.root.clone(),
            };
            Box::new(move |entry| visitor.visit(entry))
        });

        let shared = Arc::into_inner(shared).expect("file discovery workers finished");
        let mut files = shared.files.into_inner().expect("file discovery lock");
        files.sort_unstable_by(|left, right| {
            (left.depth, &left.relative).cmp(&(right.depth, &right.relative))
        });
        files.truncate(self.max_files);
        files.sort_unstable_by(|left, right| left.relative.cmp(&right.relative));
        if files.is_empty()
            && let Some(error) = shared
                .errors
                .into_inner()
                .expect("file discovery error lock")
                .first()
        {
            bail!("file discovery failed: {error}");
        }
        Ok(files)
    }
}

/// Depths beyond this share one counter; they are pruned conservatively.
const MAX_TRACKED_DEPTH: usize = 128;

struct DiscoveryShared {
    files: Mutex<Vec<FileCandidate>>,
    errors: Mutex<Vec<String>>,
    /// Accepted files per walk depth.
    depth_counts: Vec<AtomicUsize>,
    /// Deepest level still needed; entries below it cannot make the cap.
    cutoff: AtomicUsize,
    /// Deepest tracked level with any file, bounding the cutoff scan.
    deepest: AtomicUsize,
    max_files: usize,
}

impl DiscoveryShared {
    fn record(&self, depth: usize) {
        let slot = depth.min(MAX_TRACKED_DEPTH - 1);
        self.depth_counts[slot].fetch_add(1, Ordering::Relaxed);
        let deepest = self.deepest.fetch_max(slot, Ordering::Relaxed).max(slot);
        // Counts only grow, so once levels 0..=level hold the cap they always
        // will, and nothing deeper can be selected.
        let cutoff = self.cutoff.load(Ordering::Relaxed);
        let mut total = 0;
        let scan = deepest
            .min(cutoff.saturating_sub(1))
            .min(MAX_TRACKED_DEPTH - 2);
        for (level, count) in self.depth_counts.iter().enumerate().take(scan + 1) {
            total += count.load(Ordering::Relaxed);
            if total >= self.max_files {
                self.cutoff.fetch_min(level, Ordering::Relaxed);
                break;
            }
        }
    }
}

struct DiscoveryVisitor {
    files: Vec<FileCandidate>,
    shared: Arc<DiscoveryShared>,
    root: PathBuf,
}

impl DiscoveryVisitor {
    fn visit(&mut self, entry: Result<ignore::DirEntry, ignore::Error>) -> WalkState {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                self.shared
                    .errors
                    .lock()
                    .expect("file discovery error lock")
                    .push(error.to_string());
                return WalkState::Continue;
            }
        };
        let depth = entry.depth();
        let cutoff = self.shared.cutoff.load(Ordering::Relaxed);
        let Some(file_type) = entry.file_type() else {
            return WalkState::Continue;
        };
        if file_type.is_dir() {
            // Children of a directory at the cutoff are deeper than it.
            return if depth >= cutoff {
                WalkState::Skip
            } else {
                WalkState::Continue
            };
        }
        if !file_type.is_file() || depth > cutoff {
            return WalkState::Continue;
        }
        let Some(relative) = entry
            .path()
            .strip_prefix(&self.root)
            .ok()
            .and_then(Path::to_str)
        else {
            return WalkState::Continue;
        };
        if relative.contains(['\n', '\r']) {
            return WalkState::Continue;
        }
        self.files.push(FileCandidate {
            relative: relative.to_owned(),
            depth,
        });
        self.shared.record(depth);
        WalkState::Continue
    }
}

impl Drop for DiscoveryVisitor {
    fn drop(&mut self) {
        if self.files.is_empty() {
            return;
        }
        if let Ok(mut shared) = self.shared.files.lock() {
            shared.append(&mut self.files);
        }
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
            COPY_ACTION | REVEAL_ACTION if result.selected.is_empty() => Ok(None),
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
        let mut truncated = bytes.len() as u64 > self.max_bytes;
        bytes.truncate(self.max_bytes as usize);
        let text = String::from_utf8_lossy(&bytes);
        let mut lines = text.lines();
        let shown: Vec<&str> = lines.by_ref().take(self.max_lines).collect();
        truncated |= lines.next().is_some();
        Ok(Some(number_lines(&shown, truncated)))
    }
}

/// Formats preview lines with a line-number gutter.
///
/// Tabs are expanded because the terminal renderer drops control characters,
/// which would otherwise erase indentation in tab-indented sources.
fn number_lines(lines: &[&str], truncated: bool) -> String {
    use std::fmt::Write as _;

    const TAB_WIDTH: usize = 4;
    let width = lines.len().max(1).to_string().len();
    let capacity = lines
        .iter()
        .map(|line| line.len() + width + 4)
        .sum::<usize>();
    let mut preview = String::with_capacity(capacity);
    for (index, line) in lines.iter().enumerate() {
        let _ = write!(preview, "{:>width$} │ ", index + 1);
        let mut column = 0;
        for character in line.chars() {
            if character == '\t' {
                let spaces = TAB_WIDTH - column % TAB_WIDTH;
                preview.extend(std::iter::repeat_n(' ', spaces));
                column += spaces;
            } else if !character.is_control() {
                preview.push(character);
                column += 1;
            }
        }
        preview.push('\n');
    }
    if truncated {
        preview.push_str("… preview truncated\n");
    }
    preview
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    #[test]
    fn caps_discovery_deterministically_by_depth_then_name() {
        let directory = tempfile::tempdir().expect("temporary directory");
        for name in ["b.txt", "a.txt"] {
            fs::write(directory.path().join(name), "").expect("top-level file");
        }
        for branch in ["x", "y", "z"] {
            let path = directory.path().join(branch).join("deep");
            fs::create_dir_all(&path).expect("nested directory");
            for index in 0..20 {
                fs::write(
                    directory.path().join(branch).join(format!("{index:02}")),
                    "",
                )
                .expect("level-two file");
                fs::write(path.join(format!("{index:02}")), "").expect("level-three file");
            }
        }
        let mut source = FileSource::new(directory.path()).expect("source");
        source.max_files = 25;

        let expected: Vec<String> = ["a.txt", "b.txt"]
            .into_iter()
            .map(str::to_owned)
            .chain((0..20).map(|index| format!("x/{index:02}")))
            .chain((0..3).map(|index| format!("y/{index:02}")))
            .collect();
        for _ in 0..10 {
            let files: Vec<String> = source
                .discover()
                .expect("discovery")
                .into_iter()
                .map(|file| file.relative)
                .collect();
            assert_eq!(files, expected);
        }
    }

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
    fn preview_expands_tabs_and_marks_truncation() {
        assert_eq!(
            number_lines(&["\tfn main() {}", "a\tb"], false),
            "1 │     fn main() {}\n2 │ a   b\n"
        );
        assert!(number_lines(&["x"], true).ends_with("… preview truncated\n"));
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
