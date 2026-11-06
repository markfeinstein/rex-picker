#![cfg(unix)]

use std::{fs, os::unix::fs::PermissionsExt, path::Path};

use rex_picker::{PickerResult, PickerSelection, adapters::password::PasswordActionHandler};
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

#[test]
fn copies_by_entry_name_without_reading_or_rendering_the_secret() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let pass = directory.path().join("pass");
    let log = directory.path().join("pass.log");
    executable(
        &pass,
        &format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > {}\nprintf '%s\\n' 'SECRET-MUST-NOT-BE-CAPTURED'\n",
            shell_quote(&log)
        ),
    );
    let handler = PasswordActionHandler::new(pass);
    let result = PickerResult {
        action: "accept".to_owned(),
        selected: vec![PickerSelection {
            id: "entry".to_owned(),
            label: "nested/account".to_owned(),
            metadata: json!({ "entry": "nested/account" }),
        }],
    };

    handler.handle_result(&result).expect("copy password");
    assert_eq!(
        fs::read_to_string(log).expect("pass log"),
        "show\n--clip\nnested/account\n"
    );
}

#[test]
fn rejects_non_accept_actions_before_invoking_pass() {
    let handler = PasswordActionHandler::new("/does/not/exist");
    let error = handler
        .handle_result(&PickerResult {
            action: "preview".to_owned(),
            selected: Vec::new(),
        })
        .expect_err("unsupported action");
    assert!(
        error
            .to_string()
            .contains("unsupported password-picker action")
    );
}
