#![forbid(unsafe_code)]

use std::{
    fs,
    io::{self, Read, Write},
    path::{Path, PathBuf},
    process::{Command as ProcessCommand, ExitCode, Stdio},
    sync::mpsc::{self, TryRecvError},
    thread,
    time::Duration,
};

use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand, ValueEnum};
#[cfg(unix)]
use rex_picker::adapters::{
    password::{PasswordActionHandler, PasswordStoreSource},
    pi_model::{PiModelAdapter, PiModelClient, PiModelScope},
    url::{RexUrlSource, UrlActionHandler, loading_url_document},
};
use rex_picker::{
    PickerAction, PickerDocument, PickerItem, PickerOptions, PickerResult, run_picker,
    run_picker_runtime, run_picker_with_handler,
};
use serde_json::json;

#[derive(Debug, Parser)]
#[command(version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Pick from a versioned JSON document.
    Json(JsonArgs),
    /// Pick from newline- or NUL-delimited input.
    Lines(LinesArgs),
    /// Copy an entry from a pass-compatible password store.
    #[cfg(unix)]
    Password(PasswordArgs),
    /// Select a model through a Pi session's Rex model-picker bridge.
    #[cfg(unix)]
    PiModel(PiModelArgs),
    /// Pick an HTTP(S) URL from recent Rex terminal scrollback.
    #[cfg(unix)]
    Url(UrlArgs),
    /// Validate a JSON picker document without opening the terminal UI.
    Validate(InputArgs),
}

#[derive(Debug, Args)]
struct InputArgs {
    /// Input path, or `-` for standard input.
    #[arg(short, long, default_value = "-")]
    input: PathBuf,
}

#[derive(Debug, Args)]
struct JsonArgs {
    #[command(flatten)]
    input: InputArgs,
    /// Result path, or `-` for standard output.
    #[arg(short, long, default_value = "-")]
    output: PathBuf,
    /// Pretty-print the result JSON.
    #[arg(long)]
    pretty: bool,
}

#[derive(Debug, Args)]
struct LinesArgs {
    #[command(flatten)]
    input: InputArgs,
    /// Result path, or `-` for standard output.
    #[arg(short, long, default_value = "-")]
    output: PathBuf,
    /// Read NUL-delimited records instead of lines.
    #[arg(long)]
    read0: bool,
    /// Permit selecting multiple records with Tab.
    #[arg(short, long)]
    multi: bool,
    /// Optional picker title.
    #[arg(long)]
    title: Option<String>,
    /// Query prompt.
    #[arg(long, default_value = "› ")]
    prompt: String,
    /// Hint shown while the query is empty.
    #[arg(long, default_value = "Filter items")]
    placeholder: String,
    /// Optional footer text.
    #[arg(long)]
    footer: Option<String>,
    /// Initial fuzzy query.
    #[arg(long, default_value = "")]
    query: String,
    /// Maximum number of visible rows.
    #[arg(long, default_value_t = 12)]
    max_visible: usize,
    /// Alternate action in KEY:ID:LABEL form. May be repeated.
    #[arg(long, value_parser = parse_action)]
    action: Vec<PickerAction>,
    /// Pretty-print the result JSON.
    #[arg(long)]
    pretty: bool,
}

#[cfg(unix)]
#[derive(Debug, Args)]
struct PasswordArgs {
    /// Password-store root. Defaults to PASSWORD_STORE_DIR or ~/.password-store.
    #[arg(long)]
    store: Option<PathBuf>,
    /// pass-compatible executable used to copy the selected entry.
    #[arg(long, default_value = "pass")]
    pass_bin: PathBuf,
    /// Rex block that should regain focus after the picker exits.
    #[arg(long)]
    origin_block: Option<String>,
    /// Do not ask Rex to restore focus on exit.
    #[arg(long)]
    no_restore_focus: bool,
    /// Rex CLI used for focus restoration.
    #[arg(long, default_value = "rex")]
    rex_bin: PathBuf,
}

#[cfg(unix)]
#[derive(Clone, Copy, Debug, ValueEnum)]
enum PiModelScopeArg {
    All,
    Scoped,
}

#[cfg(unix)]
impl From<PiModelScopeArg> for PiModelScope {
    fn from(value: PiModelScopeArg) -> Self {
        match value {
            PiModelScopeArg::All => Self::All,
            PiModelScopeArg::Scoped => Self::Scoped,
        }
    }
}

#[cfg(unix)]
#[derive(Debug, Args)]
struct PiModelArgs {
    /// Rex block containing the Pi session.
    block: String,
    /// Override the Pi extension's Unix socket path.
    #[arg(long)]
    socket: Option<PathBuf>,
    /// Initial model scope. Pi chooses the configured default when omitted.
    #[arg(long, value_enum)]
    scope: Option<PiModelScopeArg>,
    /// Refresh provider catalogs before displaying the initial list.
    #[arg(long)]
    refresh: bool,
    /// Timeout for ordinary list and selection requests.
    #[arg(long, default_value_t = 3_000)]
    timeout_ms: u64,
    /// Timeout for catalog refresh requests.
    #[arg(long, default_value_t = 20_000)]
    refresh_timeout_ms: u64,
    /// Do not ask Rex to restore focus to the originating Pi block on exit.
    #[arg(long)]
    no_restore_focus: bool,
    /// Rex CLI used for focus restoration.
    #[arg(long, default_value = "rex")]
    rex_bin: PathBuf,
}

#[cfg(unix)]
#[derive(Debug, Args)]
struct UrlArgs {
    /// Rex session containing the source terminal.
    session: String,
    /// Rex terminal block whose scrollback should be searched.
    block: String,
    /// Rex CLI used to capture scrollback and restore focus.
    #[arg(long, default_value = "rex")]
    rex_bin: PathBuf,
    /// macOS opener used for accepted URLs.
    #[arg(long, default_value = "/usr/bin/open")]
    open_bin: PathBuf,
    /// Clipboard program that accepts the URL on standard input.
    #[arg(long, default_value = "/usr/bin/pbcopy")]
    clipboard_bin: PathBuf,
    /// Do not ask Rex to restore focus to the source block on exit.
    #[arg(long)]
    no_restore_focus: bool,
}

fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("rex-picker: {error:#}");
            ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli) -> Result<()> {
    match cli.command {
        Command::Json(args) => {
            let document = read_document(&args.input.input)?;
            if let Some(result) = run_picker(document)? {
                write_result(&args.output, &result, args.pretty)?;
            }
        }
        Command::Lines(args) => {
            let document = lines_document(&args)?;
            if let Some(result) = run_picker(document)? {
                write_result(&args.output, &result, args.pretty)?;
            }
        }
        #[cfg(unix)]
        Command::Password(args) => run_password_picker(args)?,
        #[cfg(unix)]
        Command::PiModel(args) => run_pi_model(args)?,
        #[cfg(unix)]
        Command::Url(args) => run_url_picker(args)?,
        Command::Validate(args) => {
            let document = read_document(&args.input)?;
            document.validate().map_err(anyhow::Error::msg)?;
        }
    }
    Ok(())
}

#[cfg(unix)]
fn run_password_picker(args: PasswordArgs) -> Result<()> {
    let store = args
        .store
        .or_else(|| std::env::var_os("PASSWORD_STORE_DIR").map(PathBuf::from))
        .or_else(|| {
            std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".password-store"))
        })
        .context("password store is not configured and HOME is unavailable")?;
    let _focus_guard = RexFocusGuard::new(
        resolved_rex_bin(&args.rex_bin),
        std::env::var("REX_SESSION").ok(),
        args.origin_block,
        args.no_restore_focus,
    );
    let document = PasswordStoreSource::new(store).load_document()?;
    let handler = PasswordActionHandler::new(args.pass_bin);
    run_picker_with_handler(document, |result| handler.handle_result(result))?;
    Ok(())
}

#[cfg(unix)]
fn run_pi_model(args: PiModelArgs) -> Result<()> {
    if args.timeout_ms == 0 || args.refresh_timeout_ms == 0 {
        bail!("Pi model-picker timeouts must be greater than zero");
    }

    let rex_bin = resolved_rex_bin(&args.rex_bin);
    let _focus_guard = RexFocusGuard::new(
        rex_bin,
        std::env::var("REX_SESSION").ok(),
        Some(args.block.clone()),
        args.no_restore_focus,
    );
    let client = match args.socket {
        Some(path) => PiModelClient::new(path),
        None => PiModelClient::for_block(&args.block)?,
    }
    .with_timeouts(
        Duration::from_millis(args.timeout_ms),
        Duration::from_millis(args.refresh_timeout_ms),
    );
    let mut adapter = PiModelAdapter::new(client, args.scope.map(Into::into));
    let document = adapter.load(args.refresh)?;
    run_picker_with_handler(document, |result| adapter.handle_result(result))?;
    Ok(())
}

#[cfg(unix)]
fn run_url_picker(args: UrlArgs) -> Result<()> {
    let rex_bin = resolved_rex_bin(&args.rex_bin);
    let _focus_guard = RexFocusGuard::new(
        rex_bin.clone(),
        Some(args.session.clone()),
        Some(args.block.clone()),
        args.no_restore_focus,
    );
    let source = RexUrlSource::new(rex_bin, args.session, args.block);
    let handler = UrlActionHandler::new(args.open_bin, args.clipboard_bin);
    let (sender, receiver) = mpsc::sync_channel(1);
    thread::spawn(move || {
        let _ = sender.send(source.load_document());
    });
    let mut capture_complete = false;

    run_picker_runtime(
        loading_url_document(),
        |result| handler.handle_result(result),
        || {
            if capture_complete {
                return Ok(None);
            }
            match receiver.try_recv() {
                Ok(result) => {
                    capture_complete = true;
                    result.map(Some)
                }
                Err(TryRecvError::Empty) => Ok(None),
                Err(TryRecvError::Disconnected) => {
                    bail!("URL capture worker exited without returning a result")
                }
            }
        },
    )?;
    Ok(())
}

#[cfg(unix)]
fn resolved_rex_bin(default: &Path) -> PathBuf {
    std::env::var_os("REX_BIN_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|| default.to_owned())
}

#[cfg(unix)]
struct RexFocusGuard {
    rex_bin: PathBuf,
    session: String,
    block: String,
}

#[cfg(unix)]
impl RexFocusGuard {
    fn new(
        rex_bin: PathBuf,
        session: Option<String>,
        block: Option<String>,
        disabled: bool,
    ) -> Option<Self> {
        if disabled {
            return None;
        }
        Some(Self {
            rex_bin,
            session: session?,
            block: block?,
        })
    }
}

#[cfg(unix)]
impl Drop for RexFocusGuard {
    fn drop(&mut self) {
        let _ = ProcessCommand::new(&self.rex_bin)
            .arg("--autostart=false")
            .arg("focus")
            .args(["-s", &self.session, &self.block])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

fn read_document(path: &Path) -> Result<PickerDocument> {
    let input = read_input(path)?;
    let document: PickerDocument =
        serde_json::from_slice(&input).context("parse picker document as JSON")?;
    document.validate().map_err(anyhow::Error::msg)?;
    Ok(document)
}

fn lines_document(args: &LinesArgs) -> Result<PickerDocument> {
    if args.max_visible == 0 {
        bail!("--max-visible must be greater than zero");
    }

    let input = read_input(&args.input.input)?;
    let records: Vec<&[u8]> = if args.read0 {
        input.split(|byte| *byte == 0).collect()
    } else {
        input.split(|byte| *byte == b'\n').collect()
    };

    let items = records
        .into_iter()
        .filter(|record| !record.is_empty())
        .enumerate()
        .map(|(index, record)| {
            let value = std::str::from_utf8(record)
                .with_context(|| format!("input record {} is not valid UTF-8", index + 1))?;
            let value = value.strip_suffix('\r').unwrap_or(value);
            Ok(PickerItem {
                id: index.to_string(),
                label: value.to_owned(),
                description: None,
                search_text: None,
                preview: None,
                metadata: json!({ "value": value }),
            })
        })
        .collect::<Result<Vec<_>>>()?;

    let document = PickerDocument {
        version: 1,
        options: PickerOptions {
            title: args.title.clone(),
            prompt: args.prompt.clone(),
            placeholder: args.placeholder.clone(),
            header: None,
            footer: args.footer.clone(),
            initial_query: args.query.clone(),
            multi: args.multi,
            max_visible: args.max_visible,
            actions: args.action.clone(),
        },
        items,
    };
    document.validate().map_err(anyhow::Error::msg)?;
    Ok(document)
}

fn read_input(path: &Path) -> Result<Vec<u8>> {
    if path == Path::new("-") {
        let mut input = Vec::new();
        io::stdin()
            .read_to_end(&mut input)
            .context("read standard input")?;
        Ok(input)
    } else {
        fs::read(path).with_context(|| format!("read {}", path.display()))
    }
}

fn write_result(path: &Path, result: &PickerResult, pretty: bool) -> Result<()> {
    let mut encoded = if pretty {
        serde_json::to_vec_pretty(result)
    } else {
        serde_json::to_vec(result)
    }
    .context("serialize picker result")?;
    encoded.push(b'\n');

    if path == Path::new("-") {
        let mut stdout = io::stdout().lock();
        stdout.write_all(&encoded).context("write picker result")?;
        stdout.flush().context("flush picker result")?;
        return Ok(());
    }

    write_atomic(path, &encoded)
}

fn write_atomic(path: &Path, contents: &[u8]) -> Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .context("output path must have a UTF-8 file name")?;
    let temporary = parent.join(format!(".{file_name}.{}.tmp", std::process::id()));

    let write_result = (|| -> Result<()> {
        let mut file = fs::File::create(&temporary)
            .with_context(|| format!("create {}", temporary.display()))?;
        file.write_all(contents)
            .with_context(|| format!("write {}", temporary.display()))?;
        file.sync_all()
            .with_context(|| format!("sync {}", temporary.display()))?;
        fs::rename(&temporary, path).with_context(|| format!("replace {}", path.display()))?;
        Ok(())
    })();

    if write_result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    write_result
}

fn parse_action(value: &str) -> Result<PickerAction, String> {
    let mut parts = value.splitn(3, ':');
    let key = parts.next().unwrap_or_default();
    let id = parts.next().unwrap_or_default();
    let label = parts.next().unwrap_or_default();
    if key.is_empty() || id.is_empty() || label.is_empty() {
        return Err("expected KEY:ID:LABEL".to_owned());
    }
    Ok(PickerAction {
        id: id.to_owned(),
        key: key.to_owned(),
        label: label.to_owned(),
    })
}

#[cfg(test)]
mod tests {
    use serde_json::Value;

    use super::*;

    fn lines_args(input: PathBuf) -> LinesArgs {
        LinesArgs {
            input: InputArgs { input },
            output: PathBuf::from("-"),
            read0: false,
            multi: false,
            title: None,
            prompt: "› ".to_owned(),
            placeholder: "Filter items".to_owned(),
            footer: None,
            query: String::new(),
            max_visible: 12,
            action: Vec::new(),
            pretty: false,
        }
    }

    #[test]
    fn parses_action_specification() {
        assert_eq!(
            parse_action("ctrl-y:copy:copy URL").expect("action"),
            PickerAction {
                id: "copy".to_owned(),
                key: "ctrl-y".to_owned(),
                label: "copy URL".to_owned(),
            }
        );
    }

    #[test]
    fn builds_items_from_lines() {
        let file = tempfile::NamedTempFile::new().expect("temp file");
        fs::write(file.path(), "alpha\nbeta\n").expect("fixture");

        let document = lines_document(&lines_args(file.path().to_owned())).expect("document");
        assert_eq!(document.items.len(), 2);
        assert_eq!(document.items[1].label, "beta");
        assert_eq!(document.items[1].metadata, json!({ "value": "beta" }));
    }

    #[test]
    fn atomic_result_replaces_destination() {
        let directory = tempfile::tempdir().expect("temp directory");
        let output = directory.path().join("result.json");
        fs::write(&output, "old").expect("fixture");
        let result = PickerResult {
            action: "accept".to_owned(),
            selected: vec![rex_picker::PickerSelection {
                id: "1".to_owned(),
                label: "beta".to_owned(),
                metadata: Value::Null,
            }],
        };

        write_result(&output, &result, false).expect("write result");
        let value: Value =
            serde_json::from_slice(&fs::read(output).expect("read result")).expect("parse result");
        assert_eq!(value["selected"][0]["label"], "beta");
    }
}
