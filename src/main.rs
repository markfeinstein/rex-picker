#![forbid(unsafe_code)]

use std::{
    fs,
    io::{self, Read, Write},
    path::{Path, PathBuf},
    process::{Command as ProcessCommand, ExitCode, Stdio},
    time::Duration,
};

use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand, ValueEnum};
#[cfg(unix)]
use rex_picker::adapters::pi_model::{PiModelAdapter, PiModelClient, PiModelScope};
use rex_picker::{
    PickerAction, PickerDocument, PickerItem, PickerOptions, PickerResult, run_picker,
    run_picker_with_handler,
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
    /// Select a model through a Pi session's Rex model-picker bridge.
    #[cfg(unix)]
    PiModel(PiModelArgs),
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
        Command::PiModel(args) => run_pi_model(args)?,
        Command::Validate(args) => {
            let document = read_document(&args.input)?;
            document.validate().map_err(anyhow::Error::msg)?;
        }
    }
    Ok(())
}

#[cfg(unix)]
fn run_pi_model(args: PiModelArgs) -> Result<()> {
    if args.timeout_ms == 0 || args.refresh_timeout_ms == 0 {
        bail!("Pi model-picker timeouts must be greater than zero");
    }

    let _focus_guard = RexFocusGuard::new(&args);
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
struct RexFocusGuard {
    rex_bin: PathBuf,
    session: String,
    block: String,
}

#[cfg(unix)]
impl RexFocusGuard {
    fn new(args: &PiModelArgs) -> Option<Self> {
        if args.no_restore_focus {
            return None;
        }
        let session = std::env::var("REX_SESSION").ok()?;
        let rex_bin = std::env::var_os("REX_BIN_PATH")
            .map(PathBuf::from)
            .unwrap_or_else(|| args.rex_bin.clone());
        Some(Self {
            rex_bin,
            session,
            block: args.block.clone(),
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
