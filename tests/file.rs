#![cfg(unix)]

use std::{fs, os::unix::fs::PermissionsExt, path::Path};

use rex_picker::{
    PickerResult, PickerSelection,
    adapters::file::{FilePickerAdapter, FileSource, ShellKind},
};
use serde_json::json;

fn shell_quote(path: &Path) -> String {
    let value = path
        .display()
        .to_string()
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('$', "\\$")
        .replace('`', "\\`");
    format!("\"{value}\"")
}

fn executable(path: &Path, contents: &str) {
    fs::write(path, contents).expect("write executable");
    let mut permissions = fs::metadata(path).expect("metadata").permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(path, permissions).expect("chmod executable");
}

fn adapter(root: &Path, rex: &Path, clipboard: &Path, opener: &Path) -> FilePickerAdapter {
    FilePickerAdapter::new(
        FileSource::new(root).expect("file source"),
        rex,
        "session:test",
        "block:test",
        ShellKind::Nushell,
        clipboard,
        opener,
    )
}

#[test]
fn toggles_hidden_and_ignored_discovery() {
    let directory = tempfile::tempdir().expect("temporary directory");
    fs::create_dir(directory.path().join(".git")).expect("git directory");
    fs::write(directory.path().join(".gitignore"), "ignored.txt\n").expect("gitignore");
    fs::write(directory.path().join("visible.txt"), "visible").expect("visible file");
    fs::write(directory.path().join("ignored.txt"), "ignored").expect("ignored file");
    fs::write(directory.path().join(".hidden.txt"), "hidden").expect("hidden file");
    fs::write(directory.path().join(".git/config"), "secret").expect("git metadata");

    let mut picker = adapter(
        directory.path(),
        Path::new("/does/not/matter"),
        Path::new("/does/not/matter"),
        Path::new("/does/not/matter"),
    );
    let initial = picker.load().expect("initial files");
    let labels = initial
        .items
        .iter()
        .map(|item| item.label.as_str())
        .collect::<Vec<_>>();
    assert!(labels.contains(&"visible.txt"));
    assert!(labels.contains(&"ignored.txt"));
    assert!(!labels.contains(&".hidden.txt"));
    assert!(!labels.iter().any(|label| label.starts_with(".git/")));

    let hidden = picker
        .handle_result(&PickerResult {
            action: "toggle-hidden".to_owned(),
            selected: Vec::new(),
        })
        .expect("toggle hidden")
        .expect("replacement");
    assert!(hidden.items.iter().any(|item| item.label == ".hidden.txt"));

    let filtered = picker
        .handle_result(&PickerResult {
            action: "toggle-ignored".to_owned(),
            selected: Vec::new(),
        })
        .expect("toggle ignored")
        .expect("replacement");
    assert!(
        !filtered
            .items
            .iter()
            .any(|item| item.label == "ignored.txt")
    );
}

#[test]
fn injects_a_shell_safe_multi_file_editor_command_through_rex() {
    let directory = tempfile::tempdir().expect("temporary directory");
    fs::write(directory.path().join("one file.txt"), "one").expect("file one");
    fs::write(directory.path().join("two.txt"), "two").expect("file two");
    let rex = directory.path().join("rex");
    let log = directory.path().join("rex.log");
    executable(
        &rex,
        &format!(
            "#!/bin/sh\nprintf '%s\\t' \"$@\" >> {}\nprintf '\\n' >> {}\n",
            shell_quote(&log),
            shell_quote(&log),
        ),
    );
    let mut picker = adapter(
        directory.path(),
        &rex,
        Path::new("/does/not/matter"),
        Path::new("/does/not/matter"),
    );
    picker.load().expect("files");
    let result = PickerResult {
        action: "accept".to_owned(),
        selected: vec![
            PickerSelection {
                id: "one".to_owned(),
                label: "one file.txt".to_owned(),
                metadata: json!({ "path": "one file.txt" }),
            },
            PickerSelection {
                id: "two".to_owned(),
                label: "two.txt".to_owned(),
                metadata: json!({ "path": "two.txt" }),
            },
        ],
    };

    picker.handle_result(&result).expect("open editor");
    let calls = fs::read_to_string(log).expect("Rex calls");
    assert!(calls.contains("send-key\t-s\tsession:test\t-b\tblock:test\tctrl+c"));
    assert!(calls.contains(
        "send\t-s\tsession:test\t-b\tblock:test\tnvim -O2 -- \"one file.txt\" \"two.txt\""
    ));
    assert!(calls.contains("send-key\t-s\tsession:test\t-b\tblock:test\tenter"));
}
