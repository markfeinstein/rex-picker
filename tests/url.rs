#![cfg(unix)]

use std::{fs, os::unix::fs::PermissionsExt, path::Path};

use rex_picker::{
    PickerResult, PickerSelection,
    adapters::url::{RexUrlSource, UrlActionHandler},
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

fn selection(document: &rex_picker::PickerDocument, index: usize, action: &str) -> PickerResult {
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

#[test]
fn captures_rex_html_and_builds_newest_first_candidates() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let rex = directory.path().join("rex");
    let response = directory.path().join("response.json");
    let calls = directory.path().join("rex.log");
    let html = concat!(
        "<div><a href=\"https://docs.example/guide\">Documentation</a></div>\n",
        "<div>https://older.example/path</div>\n",
        "<div>https://newest.example/final.</div>\n",
    );
    fs::write(
        &response,
        serde_json::to_vec(&json!({ "content": html })).unwrap(),
    )
    .expect("response fixture");
    executable(
        &rex,
        &format!(
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> {}\ncat {}\n",
            shell_quote(&calls),
            shell_quote(&response),
        ),
    );

    let document = RexUrlSource::new(&rex, "session:test", "block:test")
        .load_document()
        .expect("URL document");

    assert_eq!(document.items.len(), 3);
    assert_eq!(document.items[0].label, "newest.example  /final");
    assert_eq!(
        document.items[2].label,
        "Documentation  ·  docs.example/guide"
    );
    let call = fs::read_to_string(calls).expect("Rex call log");
    assert!(call.contains("com.superlogical.terminal.format"));
    assert!(call.contains("session:test"));
    assert!(call.contains("block:test"));
}

#[test]
fn executes_open_background_and_copy_actions_without_a_shell() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let open = directory.path().join("open");
    let clipboard = directory.path().join("pbcopy");
    let open_log = directory.path().join("open.log");
    let clipboard_log = directory.path().join("clipboard.log");
    executable(
        &open,
        &format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > {}\n",
            shell_quote(&open_log)
        ),
    );
    executable(
        &clipboard,
        &format!("#!/bin/sh\ncat > {}\n", shell_quote(&clipboard_log)),
    );
    let handler = UrlActionHandler::new(&open, &clipboard);
    let item = rex_picker::PickerItem {
        id: "url".to_owned(),
        label: "example.com  /path".to_owned(),
        description: None,
        search_text: None,
        preview: None,
        metadata: json!({ "url": "https://example.com/path" }),
    };
    let document = rex_picker::PickerDocument {
        version: 1,
        options: Default::default(),
        items: vec![item],
    };

    handler
        .handle_result(&selection(&document, 0, "accept"))
        .expect("open URL");
    assert_eq!(
        fs::read_to_string(&open_log).expect("open log"),
        "https://example.com/path\n"
    );

    handler
        .handle_result(&selection(&document, 0, "background"))
        .expect("open URL in background");
    assert_eq!(
        fs::read_to_string(&open_log).expect("open log"),
        "-g\nhttps://example.com/path\n"
    );

    handler
        .handle_result(&selection(&document, 0, "copy"))
        .expect("copy URL");
    assert_eq!(
        fs::read_to_string(clipboard_log).expect("clipboard log"),
        "https://example.com/path"
    );
}
