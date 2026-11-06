#![cfg(unix)]

use std::{fs, os::unix::fs::PermissionsExt, path::Path};

use rex_picker::{PickerResult, PickerSelection, adapters::history::HistoryAdapter};
use rusqlite::{Connection, params};

fn executable(path: &Path, contents: &str) {
    fs::write(path, contents).expect("write executable");
    let mut permissions = fs::metadata(path).expect("metadata").permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(path, permissions).expect("chmod executable");
}

fn fixture(path: &Path) {
    let connection = Connection::open(path).expect("history database");
    connection
        .execute_batch(
            "CREATE TABLE history (
                id INTEGER PRIMARY KEY,
                command_line TEXT NOT NULL,
                session_id INTEGER,
                cwd TEXT,
                exit_status INTEGER,
                duration_ms INTEGER,
                start_timestamp INTEGER
             );",
        )
        .expect("history schema");
    let rows = [
        (1, "global only", 1, "/other", 0, 10),
        (2, "workspace command", 2, "/workspace/project", 0, 20),
        (3, "git status", 42, "/workspace/project", 0, 30),
        (4, "git\tstatus", 42, "/workspace/project", 1, 40),
    ];
    for (id, command, session, cwd, status, duration) in rows {
        connection
            .execute(
                "INSERT INTO history
                 (id, command_line, session_id, cwd, exit_status, duration_ms, start_timestamp)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, 1000)",
                params![id, command, session, cwd, status, duration],
            )
            .expect("history row");
    }
}

fn selected(document: &rex_picker::PickerDocument, index: usize, action: &str) -> PickerResult {
    let item = &document.items[index];
    PickerResult {
        action: action.to_owned(),
        selected: vec![PickerSelection {
            id: item.id.clone(),
            label: item.label.clone(),
            metadata: item.metadata.clone(),
        }],
    }
}

fn adapter(database: &Path, result_file: Option<std::path::PathBuf>, rex: &Path) -> HistoryAdapter {
    HistoryAdapter::new(
        database,
        42,
        "/workspace/project",
        "/workspace",
        result_file,
        rex,
        "session:test",
        "block:test",
        "/usr/bin/true",
        "git",
    )
    .expect("history adapter")
}

#[test]
fn chooses_session_scope_deduplicates_and_cycles_scopes() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let database = directory.path().join("history.sqlite3");
    fixture(&database);
    let mut adapter = adapter(&database, None, Path::new("/usr/bin/true"));

    let session = adapter.load_initial().expect("session history");
    assert_eq!(session.options.prompt, "session history › ");
    assert_eq!(session.options.initial_query, "git");
    assert_eq!(session.items.len(), 1);
    assert_eq!(session.items[0].label, "git status");

    let workspace = adapter
        .handle_result(&PickerResult {
            action: "cycle-scope".to_owned(),
            selected: Vec::new(),
        })
        .expect("cycle scope")
        .expect("workspace document");
    assert_eq!(workspace.options.prompt, "workspace history › ");
    assert!(
        workspace
            .items
            .iter()
            .any(|item| item.label == "workspace command")
    );

    adapter
        .handle_result(&selected(&workspace, 0, "insert-tab"))
        .expect("insert history");
    let result = adapter.take_result().expect("history result");
    assert_eq!(result.action, "insert");
    assert!(!result.command.is_empty());
}

#[test]
fn writes_result_atomically_and_signals_rex() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let database = directory.path().join("history.sqlite3");
    let result_file = directory.path().join("result.json");
    let rex = directory.path().join("rex");
    let rex_log = directory.path().join("rex.log");
    fixture(&database);
    executable(
        &rex,
        &format!(
            "#!/bin/sh\nprintf '%s\\n' \"$*\" > \"{}\"\n",
            rex_log.display()
        ),
    );
    let mut adapter = adapter(&database, Some(result_file.clone()), &rex);
    let document = adapter.load_initial().expect("history document");

    adapter
        .handle_result(&selected(&document, 0, "accept"))
        .expect("run history");
    let value: serde_json::Value =
        serde_json::from_slice(&fs::read(result_file).expect("result file")).expect("result JSON");
    assert_eq!(value["action"], "run");
    assert!(
        value["command"]
            .as_str()
            .is_some_and(|command| !command.is_empty())
    );
    assert!(
        fs::read_to_string(rex_log)
            .expect("Rex log")
            .contains("send-key -s session:test -b block:test f13")
    );
}
