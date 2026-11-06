//! Small filesystem helpers shared by the CLI and adapters.

use std::{fs, io::Write, path::Path};

use anyhow::{Context, Result};

/// Atomically replaces `path` with `contents`.
///
/// The bytes are written to a uniquely named sibling file, flushed to stable
/// storage, and renamed over the destination, so readers observe either the
/// previous file or the complete new one. The temporary file is removed on
/// failure.
pub fn write_atomic(path: &Path, contents: &[u8]) -> Result<()> {
    let parent = match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    };
    fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    let file_name = path
        .file_name()
        .with_context(|| format!("{} has no file name", path.display()))?;
    let mut temporary = tempfile::Builder::new()
        .prefix(&{
            let mut prefix = std::ffi::OsString::from(".");
            prefix.push(file_name);
            prefix.push(".");
            prefix
        })
        .suffix(".tmp")
        .tempfile_in(parent)
        .with_context(|| format!("create temporary file in {}", parent.display()))?;
    temporary
        .write_all(contents)
        .with_context(|| format!("write {}", temporary.path().display()))?;
    temporary
        .as_file()
        .sync_all()
        .with_context(|| format!("sync {}", temporary.path().display()))?;
    temporary
        .persist(path)
        .map_err(|error| error.error)
        .with_context(|| format!("replace {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replaces_existing_file_and_leaves_no_temporaries() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let path = directory.path().join("nested/result.json");
        write_atomic(&path, b"old").expect("first write");
        write_atomic(&path, b"new").expect("second write");

        assert_eq!(fs::read(&path).expect("read"), b"new");
        let entries = fs::read_dir(path.parent().expect("parent"))
            .expect("list")
            .count();
        assert_eq!(entries, 1, "temporary files must not be left behind");
    }
}
