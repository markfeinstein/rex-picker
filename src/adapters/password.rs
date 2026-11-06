use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use crate::{PickerDocument, PickerItem, PickerOptions, PickerResult};

/// Discovers password-store entry names without reading encrypted contents.
#[derive(Clone, Debug)]
pub struct PasswordStoreSource {
    root: PathBuf,
}

impl PasswordStoreSource {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn load_document(&self) -> Result<PickerDocument> {
        let root = self
            .root
            .canonicalize()
            .with_context(|| format!("resolve password store {}", self.root.display()))?;
        if !root.is_dir() {
            bail!("password store is not a directory: {}", root.display());
        }

        let mut entries = Vec::new();
        collect_entries(&root, &root, &mut entries)?;
        entries.sort_unstable();
        entries.dedup();

        let items = entries
            .into_iter()
            .enumerate()
            .map(|(index, entry)| PickerItem {
                id: index.to_string(),
                label: entry.clone(),
                description: None,
                search_text: None,
                preview: None,
                metadata: json!({ "entry": entry }),
            })
            .collect();
        Ok(PickerDocument {
            version: 1,
            options: PickerOptions {
                title: Some("Password Store".to_owned()),
                prompt: "› ".to_owned(),
                placeholder: "Filter passwords".to_owned(),
                header: None,
                footer: Some("Secret values are never rendered".to_owned()),
                initial_query: String::new(),
                multi: false,
                preview_visible: true,
                max_visible: 14,
                actions: Vec::new(),
            },
            items,
        })
    }
}

fn collect_entries(root: &Path, directory: &Path, entries: &mut Vec<String>) -> Result<()> {
    for child in fs::read_dir(directory)
        .with_context(|| format!("read password-store directory {}", directory.display()))?
    {
        let child = child.with_context(|| format!("read entry in {}", directory.display()))?;
        let path = child.path();
        let file_type = child
            .file_type()
            .with_context(|| format!("inspect {}", path.display()))?;
        if file_type.is_symlink() {
            continue;
        }
        if file_type.is_dir() {
            // Match `pass ls`: hidden directories such as `.git` and
            // `.extensions` hold no entries and can contain many objects.
            if child.file_name().as_encoded_bytes().starts_with(b".") {
                continue;
            }
            collect_entries(root, &path, entries)?;
            continue;
        }
        if !file_type.is_file() || path.extension().and_then(|value| value.to_str()) != Some("gpg")
        {
            continue;
        }

        let relative = path
            .strip_prefix(root)
            .with_context(|| format!("resolve password entry {}", path.display()))?;
        let Some(relative) = relative.to_str() else {
            continue;
        };
        let entry = relative.strip_suffix(".gpg").unwrap_or(relative);
        if !entry.contains(['\n', '\r']) {
            entries.push(entry.to_owned());
        }
    }
    Ok(())
}

/// Copies a selected password through `pass show --clip` without reading it.
#[derive(Clone, Debug)]
pub struct PasswordActionHandler {
    pass_bin: PathBuf,
}

impl PasswordActionHandler {
    pub fn new(pass_bin: impl Into<PathBuf>) -> Self {
        Self {
            pass_bin: pass_bin.into(),
        }
    }

    pub fn handle_result(&self, result: &PickerResult) -> Result<Option<PickerDocument>> {
        if result.action != "accept" {
            bail!("unsupported password-picker action {:?}", result.action);
        }
        let entry = result
            .selected
            .first()
            .and_then(|selected| selected.metadata.get("entry"))
            .and_then(Value::as_str)
            .context("password selection is missing its entry name")?;
        let status = Command::new(&self.pass_bin)
            .args(["show", "--clip", entry])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .with_context(|| format!("run {}", self.pass_bin.display()))?;
        if !status.success() {
            bail!(
                "{} failed to copy the selected entry",
                self.pass_bin.display()
            );
        }
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::symlink;

    use super::*;

    #[test]
    fn discovers_sorted_names_without_following_symlinks() {
        let directory = tempfile::tempdir().expect("temporary directory");
        fs::create_dir_all(directory.path().join("nested")).expect("nested directory");
        fs::write(directory.path().join("Zeta.gpg"), []).expect("entry");
        fs::write(directory.path().join("nested/Alpha.gpg"), []).expect("nested entry");
        fs::write(directory.path().join("ignored.txt"), []).expect("ignored file");
        fs::create_dir_all(directory.path().join(".git/objects")).expect("git directory");
        fs::write(directory.path().join(".git/objects/blob.gpg"), []).expect("git object");
        symlink(
            directory.path().join("nested"),
            directory.path().join("linked"),
        )
        .expect("symlink");

        let document = PasswordStoreSource::new(directory.path())
            .load_document()
            .expect("password document");
        assert_eq!(
            document
                .items
                .iter()
                .map(|item| item.label.as_str())
                .collect::<Vec<_>>(),
            ["Zeta", "nested/Alpha"]
        );
    }
}
