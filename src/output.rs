//! Rendering fetched secrets into file contents and writing them to disk.

use std::collections::HashMap;
use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use crate::Error;
use crate::config::{FileContent, OutputFile};
use crate::source::FetchedSecret;

/// Produce the bytes for one output file.
///
/// * Env files get one `NAME=value` line per entry. Trailing line breaks on a
///   value (as left by `echo value | gcloud secrets versions add`) are
///   dropped; any other line break, or a NUL, is an error because it would
///   corrupt the env file.
/// * Raw files contain exactly the secret's bytes.
pub fn render(
    file: &OutputFile,
    secrets: &HashMap<String, FetchedSecret>,
) -> Result<Vec<u8>, Error> {
    let get = |id: &str| {
        secrets
            .get(id)
            .unwrap_or_else(|| panic!("secret {id} was not fetched"))
    };
    match &file.content {
        FileContent::Raw { secret_id } => Ok(get(secret_id).data.clone()),
        FileContent::Env(entries) => {
            let mut out = Vec::new();
            for entry in entries {
                let invalid = |reason: &str| Error::InvalidEnvValue {
                    secret_id: entry.secret_id.clone(),
                    env_name: entry.name.clone(),
                    reason: reason.into(),
                };
                let value = std::str::from_utf8(&get(&entry.secret_id).data)
                    .map_err(|_| invalid("value is not valid UTF-8"))?
                    .trim_end_matches(['\r', '\n']);
                if value.contains(['\r', '\n']) {
                    return Err(invalid("value contains a line break"));
                }
                if value.contains('\0') {
                    return Err(invalid("value contains a NUL byte"));
                }
                out.extend_from_slice(entry.name.as_bytes());
                out.push(b'=');
                out.extend_from_slice(value.as_bytes());
                out.push(b'\n');
            }
            Ok(out)
        }
    }
}

/// Atomically replace `path` with `contents` and the given permission bits.
///
/// The data is written to a private temporary file in the same directory and
/// renamed into place, so readers only ever see the old or the new file,
/// never a partially written one. Returns `false` (and touches nothing) when
/// the file already has exactly this content and mode.
pub fn write_atomic(path: &Path, contents: &[u8], mode: u32) -> Result<bool, Error> {
    let write_err = |source: io::Error| Error::Write {
        path: path.to_owned(),
        source,
    };
    if is_up_to_date(path, contents, mode) {
        return Ok(false);
    }

    let dir = match path.parent() {
        Some(d) if !d.as_os_str().is_empty() => d.to_owned(),
        _ => PathBuf::from("."),
    };
    fs::create_dir_all(&dir).map_err(write_err)?;

    // NamedTempFile is created 0600, so the secret is never readable by
    // others before the final mode is applied.
    let mut tmp = tempfile::Builder::new()
        .prefix(".secretsfetch-")
        .tempfile_in(&dir)
        .map_err(write_err)?;
    tmp.write_all(contents).map_err(write_err)?;
    tmp.as_file()
        .set_permissions(fs::Permissions::from_mode(mode))
        .map_err(write_err)?;
    tmp.as_file().sync_all().map_err(write_err)?;
    tmp.persist(path).map_err(|e| write_err(e.error))?;
    // Make the rename durable.
    fs::File::open(&dir)
        .and_then(|d| d.sync_all())
        .map_err(write_err)?;
    Ok(true)
}

fn is_up_to_date(path: &Path, contents: &[u8], mode: u32) -> bool {
    let Ok(meta) = fs::symlink_metadata(path) else {
        return false;
    };
    meta.is_file()
        && meta.permissions().mode() & 0o7777 == mode
        && meta.len() == contents.len() as u64
        && fs::read(path).is_ok_and(|existing| existing == contents)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::EnvEntry;

    fn secrets(items: &[(&str, &[u8])]) -> HashMap<String, FetchedSecret> {
        items
            .iter()
            .map(|(id, data)| {
                (
                    id.to_string(),
                    FetchedSecret {
                        version: "1".into(),
                        data: data.to_vec(),
                    },
                )
            })
            .collect()
    }

    fn env_file(entries: &[(&str, &str)]) -> OutputFile {
        OutputFile {
            path: "x".into(),
            content: FileContent::Env(
                entries
                    .iter()
                    .map(|(name, id)| EnvEntry {
                        name: name.to_string(),
                        secret_id: id.to_string(),
                    })
                    .collect(),
            ),
        }
    }

    fn raw_file(id: &str) -> OutputFile {
        OutputFile {
            path: "x".into(),
            content: FileContent::Raw {
                secret_id: id.into(),
            },
        }
    }

    #[test]
    fn renders_env_file() {
        let s = secrets(&[("a", b"alpha"), ("b", b"b=e:t a")]);
        let out = render(&env_file(&[("A", "a"), ("B", "b"), ("A2", "a")]), &s).unwrap();
        assert_eq!(out, b"A=alpha\nB=b=e:t a\nA2=alpha\n");
    }

    #[test]
    fn env_values_drop_trailing_newlines() {
        let s = secrets(&[("a", b"alpha\n"), ("b", b"beta\r\n")]);
        let out = render(&env_file(&[("A", "a"), ("B", "b")]), &s).unwrap();
        assert_eq!(out, b"A=alpha\nB=beta\n");
    }

    #[test]
    fn env_values_reject_embedded_newlines_nul_and_binary() {
        for bad in [&b"one\ntwo"[..], b"one\rtwo", b"a\0b", b"\xff\xfe"] {
            let s = secrets(&[("a", bad)]);
            let err = render(&env_file(&[("A", "a")]), &s).unwrap_err();
            assert!(matches!(err, Error::InvalidEnvValue { .. }), "{err}");
        }
    }

    #[test]
    fn raw_files_are_verbatim() {
        let data: &[u8] = b"-----BEGIN KEY-----\nabc\n-----END KEY-----\n\xff";
        let s = secrets(&[("k", data), ("other", b"nope")]);
        assert_eq!(render(&raw_file("k"), &s).unwrap(), data);
    }

    #[test]
    fn raw_files_each_get_their_own_secret() {
        // Regression test: the Python version wrote the *last fetched*
        // secret into every raw file.
        let s = secrets(&[("one", b"1"), ("two", b"2")]);
        assert_eq!(render(&raw_file("one"), &s).unwrap(), b"1");
        assert_eq!(render(&raw_file("two"), &s).unwrap(), b"2");
    }

    fn mode_of(p: &Path) -> u32 {
        fs::metadata(p).unwrap().permissions().mode() & 0o7777
    }

    #[test]
    fn write_creates_dirs_and_sets_mode() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a/b/secret.env");
        assert!(write_atomic(&path, b"X=1\n", 0o444).unwrap());
        assert_eq!(fs::read(&path).unwrap(), b"X=1\n");
        assert_eq!(mode_of(&path), 0o444);
        // No temp files left behind.
        assert_eq!(fs::read_dir(path.parent().unwrap()).unwrap().count(), 1);
    }

    #[test]
    fn write_replaces_read_only_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("secret");
        write_atomic(&path, b"old", 0o444).unwrap();
        assert!(write_atomic(&path, b"new", 0o444).unwrap());
        assert_eq!(fs::read(&path).unwrap(), b"new");
    }

    #[test]
    fn write_skips_identical_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("secret");
        write_atomic(&path, b"same", 0o444).unwrap();
        let inode = std::os::unix::fs::MetadataExt::ino(&fs::metadata(&path).unwrap());
        assert!(!write_atomic(&path, b"same", 0o444).unwrap());
        let after = std::os::unix::fs::MetadataExt::ino(&fs::metadata(&path).unwrap());
        assert_eq!(inode, after);
        // A mode change alone is still applied.
        assert!(write_atomic(&path, b"same", 0o440).unwrap());
        assert_eq!(mode_of(&path), 0o440);
    }

    #[test]
    fn write_never_follows_symlink_target_for_up_to_date_check() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target");
        fs::write(&target, b"data").unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o444)).unwrap();
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        // The symlink is replaced by a real file; the target is untouched.
        assert!(write_atomic(&link, b"data", 0o444).unwrap());
        assert!(fs::symlink_metadata(&link).unwrap().is_file());
    }
}
