use std::{fs, process::Command};

#[test]
fn validates_a_well_formed_picker_document() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let input = directory.path().join("picker.json");
    fs::write(
        &input,
        r#"{
          "version": 1,
          "options": { "title": "Models" },
          "items": [
            { "id": "one", "label": "First" },
            { "id": "two", "label": "Second", "search_text": "alias" }
          ]
        }"#,
    )
    .expect("fixture");

    let status = Command::new(env!("CARGO_BIN_EXE_rex-picker"))
        .args(["validate", "--input"])
        .arg(&input)
        .status()
        .expect("run rex-picker");

    assert!(status.success());
}

#[test]
fn rejects_duplicate_item_ids() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let input = directory.path().join("picker.json");
    fs::write(
        &input,
        r#"{
          "items": [
            { "id": "same", "label": "First" },
            { "id": "same", "label": "Second" }
          ]
        }"#,
    )
    .expect("fixture");

    let output = Command::new(env!("CARGO_BIN_EXE_rex-picker"))
        .args(["validate", "--input"])
        .arg(&input)
        .output()
        .expect("run rex-picker");

    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("duplicate item ID"));
}
