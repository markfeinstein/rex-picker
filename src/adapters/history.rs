use std::{
    collections::HashSet,
    fs,
    io::Write,
    path::PathBuf,
    process::{Command, Stdio},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;
use serde_json::{Value, json};

use crate::{
    PickerAction, PickerDocument, PickerItem, PickerOptions, PickerResult, fs_util::write_atomic,
};

const CYCLE_SCOPE_ACTION: &str = "cycle-scope";
const INSERT_TAB_ACTION: &str = "insert-tab";
const INSERT_YANK_ACTION: &str = "insert-yank";
const EDIT_ACTION: &str = "edit";
/// Nushell may hold a write lock while recording a command; wait briefly
/// rather than failing the picker with `SQLITE_BUSY`.
const BUSY_TIMEOUT: Duration = Duration::from_millis(750);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HistoryScope {
    Session,
    Workspace,
    Global,
}

impl HistoryScope {
    fn name(self) -> &'static str {
        match self {
            Self::Session => "session",
            Self::Workspace => "workspace",
            Self::Global => "global",
        }
    }

    fn next(self) -> Self {
        match self {
            Self::Session => Self::Workspace,
            Self::Workspace => Self::Global,
            Self::Global => Self::Session,
        }
    }

    fn limits(self) -> (usize, usize) {
        match self {
            Self::Session => (200, 1_000),
            Self::Workspace => (500, 2_500),
            Self::Global => (1_000, 5_000),
        }
    }
}

#[derive(Clone, Debug)]
struct HistoryRecord {
    id: i64,
    /// Whitespace-normalized command used for display and deduplication.
    command: String,
    exit_status: i64,
    duration_ms: Option<i64>,
    start_timestamp: Option<i64>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct HistorySelectionResult {
    pub command: String,
    pub action: String,
}

pub struct HistoryAdapter {
    connection: Connection,
    database: PathBuf,
    history_session: i64,
    working_directory: PathBuf,
    workspace: PathBuf,
    result_file: Option<PathBuf>,
    rex_bin: PathBuf,
    rex_session: String,
    rex_block: String,
    editor: PathBuf,
    scope: HistoryScope,
    initial_query: String,
    last_result: Option<HistorySelectionResult>,
}

impl HistoryAdapter {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        database: impl Into<PathBuf>,
        history_session: i64,
        working_directory: impl Into<PathBuf>,
        workspace: impl Into<PathBuf>,
        result_file: Option<PathBuf>,
        rex_bin: impl Into<PathBuf>,
        rex_session: impl Into<String>,
        rex_block: impl Into<String>,
        editor: impl Into<PathBuf>,
        initial_query: impl Into<String>,
    ) -> Result<Self> {
        let database = database.into();
        let connection = Connection::open(&database)
            .with_context(|| format!("open Nushell history database {}", database.display()))?;
        connection
            .busy_timeout(BUSY_TIMEOUT)
            .context("configure Nushell history busy timeout")?;
        connection
            .execute_batch(
                "CREATE INDEX IF NOT EXISTS idx_history_session_id ON history(session_id, id DESC);\n\
                 CREATE INDEX IF NOT EXISTS idx_history_cwd_id ON history(cwd, id DESC);",
            )
            .context("create Nushell history indexes")?;
        Ok(Self {
            connection,
            database,
            history_session,
            working_directory: working_directory.into(),
            workspace: workspace.into(),
            result_file,
            rex_bin: rex_bin.into(),
            rex_session: rex_session.into(),
            rex_block: rex_block.into(),
            editor: editor.into(),
            scope: HistoryScope::Session,
            initial_query: initial_query.into(),
            last_result: None,
        })
    }

    pub fn load_initial(&mut self) -> Result<PickerDocument> {
        for scope in [
            HistoryScope::Session,
            HistoryScope::Workspace,
            HistoryScope::Global,
        ] {
            let records = self.records(scope)?;
            if !records.is_empty() {
                self.scope = scope;
                return Ok(self.document(records));
            }
        }
        Ok(self.document(Vec::new()))
    }

    pub fn handle_result(&mut self, result: &PickerResult) -> Result<Option<PickerDocument>> {
        match result.action.as_str() {
            CYCLE_SCOPE_ACTION => self.cycle_scope().map(Some),
            // Row actions on an empty list are no-ops; the picker stays open.
            _ if result.selected.is_empty() => Ok(None),
            "accept" => {
                self.finish(result, "run", false)?;
                Ok(None)
            }
            INSERT_TAB_ACTION | INSERT_YANK_ACTION => {
                self.finish(result, "insert", false)?;
                Ok(None)
            }
            EDIT_ACTION => {
                self.finish(result, "insert", true)?;
                Ok(None)
            }
            action => bail!("unsupported history-picker action {action:?}"),
        }
    }

    pub fn take_result(&mut self) -> Option<HistorySelectionResult> {
        self.last_result.take()
    }

    fn cycle_scope(&mut self) -> Result<PickerDocument> {
        let mut scope = self.scope.next();
        for _ in 0..3 {
            let records = self.records(scope)?;
            if !records.is_empty() {
                self.scope = scope;
                return Ok(self.document(records));
            }
            scope = scope.next();
        }
        Ok(self.document(Vec::new()))
    }

    fn records(&self, scope: HistoryScope) -> Result<Vec<HistoryRecord>> {
        let (_, scan_limit) = scope.limits();
        let sql_base = "SELECT id, command_line, COALESCE(exit_status, 0), duration_ms, start_timestamp FROM history";
        let mut records = match scope {
            HistoryScope::Session => {
                let mut statement = self.connection.prepare(&format!(
                    "{sql_base} WHERE session_id = ?1 ORDER BY id DESC LIMIT ?2"
                ))?;
                collect_records(
                    statement
                        .query_map(params![self.history_session, scan_limit as i64], map_record)?,
                )?
            }
            HistoryScope::Workspace => {
                let workspace = self.workspace.to_string_lossy();
                let prefix = format!("{}/%", escape_like(&workspace));
                let mut statement = self.connection.prepare(&format!(
                    "{sql_base} WHERE cwd = ?1 OR cwd LIKE ?2 ESCAPE '\\' ORDER BY id DESC LIMIT ?3"
                ))?;
                collect_records(statement.query_map(
                    params![workspace.as_ref(), prefix, scan_limit as i64],
                    map_record,
                )?)?
            }
            HistoryScope::Global => {
                let mut statement = self
                    .connection
                    .prepare(&format!("{sql_base} ORDER BY id DESC LIMIT ?1"))?;
                collect_records(statement.query_map(params![scan_limit as i64], map_record)?)?
            }
        };

        let (limit, _) = scope.limits();
        for record in &mut records {
            record.command = display_command(&record.command);
        }
        let mut seen = HashSet::with_capacity(records.len());
        records.retain(|record| seen.insert(record.command.clone()));
        records.truncate(limit);
        Ok(records)
    }

    fn document(&self, records: Vec<HistoryRecord>) -> PickerDocument {
        let now_ms = now_ms();
        let items = records
            .into_iter()
            .map(|record| {
                PickerItem {
                    id: record.id.to_string(),
                    label: record.command,
                    description: Some(format!(
                        "{}  {}  {}",
                        if record.exit_status == 0 {
                            "✓"
                        } else {
                            "✗"
                        },
                        format_duration(record.duration_ms),
                        format_age(now_ms, record.start_timestamp),
                    )),
                    // The label is already indexed; repeating it as search
                    // text doubled matcher work for every history row.
                    search_text: None,
                    preview: None,
                    metadata: json!({ "historyId": record.id }),
                }
            })
            .collect::<Vec<_>>();
        PickerDocument {
            version: 1,
            options: PickerOptions {
                title: Some("Nushell History".to_owned()),
                prompt: format!("{} history › ", self.scope.name()),
                placeholder: "Filter commands".to_owned(),
                header: items
                    .is_empty()
                    .then(|| "No history entries in this scope".to_owned()),
                footer: Some(format!(
                    "database {}",
                    self.database
                        .file_name()
                        .and_then(|value| value.to_str())
                        .unwrap_or("history.sqlite3")
                )),
                initial_query: self.initial_query.clone(),
                multi: false,
                preview_visible: false,
                max_visible: 18,
                actions: vec![
                    PickerAction {
                        id: CYCLE_SCOPE_ACTION.to_owned(),
                        key: "ctrl-r".to_owned(),
                        label: "scope".to_owned(),
                    },
                    PickerAction {
                        id: INSERT_TAB_ACTION.to_owned(),
                        key: "tab".to_owned(),
                        label: "insert".to_owned(),
                    },
                    PickerAction {
                        id: EDIT_ACTION.to_owned(),
                        key: "ctrl-o".to_owned(),
                        label: "edit".to_owned(),
                    },
                    PickerAction {
                        id: INSERT_YANK_ACTION.to_owned(),
                        key: "ctrl-y".to_owned(),
                        label: "insert".to_owned(),
                    },
                ],
            },
            items,
        }
    }

    fn finish(&mut self, result: &PickerResult, action: &str, edit: bool) -> Result<()> {
        let history_id = result
            .selected
            .first()
            .and_then(|selected| selected.metadata.get("historyId"))
            .and_then(Value::as_i64)
            .context("history selection is missing its ID")?;
        let mut command: String = self
            .connection
            .query_row(
                "SELECT command_line FROM history WHERE id = ?1 LIMIT 1",
                params![history_id],
                |row| row.get(0),
            )
            .optional()?
            .context("selected history entry no longer exists")?;
        if edit {
            command = self.edit_command(&command)?;
            if command.is_empty() {
                bail!("edited history command is empty");
            }
        }
        let result = HistorySelectionResult {
            command,
            action: action.to_owned(),
        };
        if let Some(path) = &self.result_file {
            let bytes = serde_json::to_vec(&result).context("serialize history result")?;
            write_atomic(path, &bytes)?;
            self.signal_result_ready()?;
        } else {
            self.last_result = Some(result);
        }
        Ok(())
    }

    fn edit_command(&self, command: &str) -> Result<String> {
        let mut file = tempfile::Builder::new()
            .suffix(".sh")
            .tempfile()
            .context("create history edit file")?;
        file.write_all(command.as_bytes())
            .context("write history edit file")?;
        file.flush().context("flush history edit file")?;
        let status = Command::new(&self.editor)
            .arg(file.path())
            .current_dir(&self.working_directory)
            .status()
            .with_context(|| format!("run editor {}", self.editor.display()))?;
        if !status.success() {
            bail!("editor exited with status {status}");
        }
        Ok(fs::read_to_string(file.path())?.trim().to_owned())
    }

    fn signal_result_ready(&self) -> Result<()> {
        let status = Command::new(&self.rex_bin)
            .args([
                "--autostart=false",
                "send-key",
                "-s",
                &self.rex_session,
                "-b",
                &self.rex_block,
                "f13",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .with_context(|| format!("run {}", self.rex_bin.display()))?;
        if !status.success() {
            bail!("{} failed to signal history result", self.rex_bin.display());
        }
        Ok(())
    }
}

fn map_record(row: &rusqlite::Row<'_>) -> rusqlite::Result<HistoryRecord> {
    Ok(HistoryRecord {
        id: row.get(0)?,
        command: row.get(1)?,
        exit_status: row.get(2)?,
        duration_ms: row.get(3)?,
        start_timestamp: row.get(4)?,
    })
}

fn collect_records<T>(rows: rusqlite::MappedRows<'_, T>) -> Result<Vec<HistoryRecord>>
where
    T: FnMut(&rusqlite::Row<'_>) -> rusqlite::Result<HistoryRecord>,
{
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .context("query Nushell history")
}

fn escape_like(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

fn display_command(command: &str) -> String {
    command.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn format_duration(duration_ms: Option<i64>) -> String {
    match duration_ms.unwrap_or(0) {
        value if value < 1_000 => format!("{}ms", value.max(0)),
        value if value < 60_000 => format!("{}s", value / 1_000),
        value => format!("{}m", value / 60_000),
    }
}

fn format_age(now_ms: i64, timestamp: Option<i64>) -> String {
    let Some(timestamp) = timestamp else {
        return "?".to_owned();
    };
    let elapsed = now_ms.saturating_sub(timestamp).max(0);
    if elapsed < 60_000 {
        format!("{}s ago", elapsed / 1_000)
    } else if elapsed < 3_600_000 {
        format!("{}m ago", elapsed / 60_000)
    } else if elapsed < 86_400_000 {
        format!("{}h ago", elapsed / 3_600_000)
    } else {
        format!("{}d ago", elapsed / 86_400_000)
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_millis() as i64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_commands_durations_and_ages() {
        assert_eq!(
            display_command("git\tstatus\n--short"),
            "git status --short"
        );
        assert_eq!(format_duration(Some(999)), "999ms");
        assert_eq!(format_duration(Some(12_000)), "12s");
        assert_eq!(format_duration(Some(120_000)), "2m");
        assert_eq!(format_age(120_000, Some(60_000)), "1m ago");
    }

    #[test]
    fn escapes_workspace_like_patterns() {
        assert_eq!(escape_like("/tmp/a_b%"), "/tmp/a\\_b\\%");
    }
}
