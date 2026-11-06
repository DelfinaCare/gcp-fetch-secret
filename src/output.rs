//! Rendering fetched secrets into file contents and writing them to disk.

use std::collections::HashMap;
use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
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

/// How existing output files are updated.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, clap::ValueEnum)]
pub enum WriteMode {
    /// Rewrite the existing file (same inode), so containers that bind-mount
    /// the single file see the update. A reader racing the write can see a
    /// truncated file for an instant.
    #[default]
    InPlace,
    /// Write a new file and rename it over the old one. Readers never see a
    /// partial file, but a bind-mount of the single file keeps showing the
    /// old content; mount the directory instead.
    Replace,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WriteOptions {
    /// Permission bits for written files.
    pub file_mode: u32,
    pub write_mode: WriteMode,
}

impl Default for WriteOptions {
    fn default() -> Self {
        Self {
            file_mode: 0o444,
            write_mode: WriteMode::default(),
        }
    }
}

/// Write `contents` to `path` and set its permission bits.
///
/// New files are always created atomically: the data goes into a private
/// (`0600`) temporary file that is renamed into place, so a secret is never
/// readable by others with the wrong permissions and never seen half
/// written. Existing files are updated according to `opts.write_mode`.
///
/// Returns `false` (and touches nothing) when the file already has exactly
/// this content and mode.
pub fn write_file(path: &Path, contents: &[u8], opts: WriteOptions) -> Result<bool, Error> {
    let write_err = |source: io::Error| Error::Write {
        path: path.to_owned(),
        source,
    };
    if is_up_to_date(path, contents, opts.file_mode) {
        return Ok(false);
    }

    let dir = match path.parent() {
        Some(d) if !d.as_os_str().is_empty() => d.to_owned(),
        _ => PathBuf::from("."),
    };
    fs::create_dir_all(&dir).map_err(write_err)?;

    let exists = match fs::symlink_metadata(path) {
        Ok(_) => true,
        Err(e) if e.kind() == io::ErrorKind::NotFound => false,
        Err(e) => return Err(write_err(e)),
    };
    match (opts.write_mode, exists) {
        (WriteMode::InPlace, true) => overwrite_in_place(path, contents, opts.file_mode),
        _ => replace(path, &dir, contents, opts.file_mode),
    }
    .map_err(write_err)?;
    Ok(true)
}

fn replace(path: &Path, dir: &Path, contents: &[u8], mode: u32) -> io::Result<()> {
    let mut tmp = tempfile::Builder::new()
        .prefix(".gcp-fetch-secret-")
        .tempfile_in(dir)?;
    tmp.write_all(contents)?;
    tmp.as_file()
        .set_permissions(fs::Permissions::from_mode(mode))?;
    tmp.as_file().sync_all()?;
    tmp.persist(path).map_err(|e| e.error)?;
    // Make the rename durable.
    fs::File::open(dir)?.sync_all()
}

/// Rewrite an existing regular file, keeping its inode.
///
/// The file is usually read-only (`0444`), so the owner-write bit is added
/// for the duration of the write; the read bits are left alone so readers
/// are never locked out. Symlinks are refused rather than followed, so a
/// planted link can't redirect a secret elsewhere.
fn overwrite_in_place(path: &Path, contents: &[u8], mode: u32) -> io::Result<()> {
    let nofollow = |o: &mut fs::OpenOptions| {
        o.custom_flags(libc::O_NOFOLLOW);
    };
    let mut opts = fs::OpenOptions::new();
    opts.read(true);
    nofollow(&mut opts);
    let handle = opts.open(path).map_err(|e| {
        if e.raw_os_error() == Some(libc::ELOOP) {
            io::Error::other("refusing to write through a symlink")
        } else {
            e
        }
    })?;
    let meta = handle.metadata()?;
    if !meta.is_file() {
        return Err(io::Error::other("not a regular file"));
    }

    let current = meta.permissions().mode() & 0o7777;
    if current & 0o200 == 0 {
        handle.set_permissions(fs::Permissions::from_mode(current | 0o200))?;
    }
    let written = (|| {
        let mut opts = fs::OpenOptions::new();
        opts.write(true);
        nofollow(&mut opts);
        let mut file = opts.open(path)?;
        let now = file.metadata()?;
        if (now.dev(), now.ino()) != (meta.dev(), meta.ino()) {
            return Err(io::Error::other("file was replaced while being written"));
        }
        file.set_len(0)?;
        file.write_all(contents)?;
        file.sync_all()
    })();
    // Apply the final mode even if the write failed, so the file is never
    // left owner-writable.
    let chmod = handle.set_permissions(fs::Permissions::from_mode(mode));
    written.and(chmod)
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

    fn inode(p: &Path) -> u64 {
        fs::metadata(p).unwrap().ino()
    }

    fn opts(file_mode: u32, write_mode: WriteMode) -> WriteOptions {
        WriteOptions {
            file_mode,
            write_mode,
        }
    }

    const BOTH: [WriteMode; 2] = [WriteMode::InPlace, WriteMode::Replace];

    #[test]
    fn default_is_in_place_read_only() {
        assert_eq!(WriteOptions::default(), opts(0o444, WriteMode::InPlace));
    }

    #[test]
    fn write_creates_dirs_and_sets_mode() {
        for wm in BOTH {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("a/b/secret.env");
            assert!(write_file(&path, b"X=1\n", opts(0o444, wm)).unwrap());
            assert_eq!(fs::read(&path).unwrap(), b"X=1\n");
            assert_eq!(mode_of(&path), 0o444);
            // No temp files left behind.
            assert_eq!(fs::read_dir(path.parent().unwrap()).unwrap().count(), 1);
        }
    }

    #[test]
    fn write_updates_read_only_file() {
        for wm in BOTH {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("secret");
            write_file(&path, b"old content", opts(0o444, wm)).unwrap();
            assert!(write_file(&path, b"new", opts(0o444, wm)).unwrap());
            assert_eq!(fs::read(&path).unwrap(), b"new");
            assert_eq!(mode_of(&path), 0o444);
            assert!(write_file(&path, b"longer content", opts(0o640, wm)).unwrap());
            assert_eq!(fs::read(&path).unwrap(), b"longer content");
            assert_eq!(mode_of(&path), 0o640);
            assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
        }
    }

    #[test]
    fn in_place_keeps_inode_and_replace_does_not() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("secret");
        write_file(&path, b"one", opts(0o444, WriteMode::InPlace)).unwrap();
        let first = inode(&path);
        write_file(&path, b"two", opts(0o444, WriteMode::InPlace)).unwrap();
        assert_eq!(inode(&path), first);

        // An open handle (like a bind mount) sees the new content.
        let held = fs::File::open(&path).unwrap();
        write_file(&path, b"three", opts(0o444, WriteMode::InPlace)).unwrap();
        assert_eq!(io::read_to_string(&held).unwrap(), "three");

        write_file(&path, b"four", opts(0o444, WriteMode::Replace)).unwrap();
        assert_ne!(inode(&path), first);
    }

    #[test]
    fn write_skips_identical_file() {
        for wm in BOTH {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("secret");
            write_file(&path, b"same", opts(0o444, wm)).unwrap();
            let before = fs::metadata(&path).unwrap().modified().unwrap();
            let ino = inode(&path);
            assert!(!write_file(&path, b"same", opts(0o444, wm)).unwrap());
            assert_eq!(fs::metadata(&path).unwrap().modified().unwrap(), before);
            assert_eq!(inode(&path), ino);
            // A mode change alone is still applied.
            assert!(write_file(&path, b"same", opts(0o440, wm)).unwrap());
            assert_eq!(mode_of(&path), 0o440);
        }
    }

    #[test]
    fn in_place_refuses_symlinks() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target");
        fs::write(&target, b"untouched").unwrap();
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let err = write_file(&link, b"secret", opts(0o444, WriteMode::InPlace)).unwrap_err();
        assert!(err.to_string().contains("symlink"), "{err}");
        assert_eq!(fs::read(&target).unwrap(), b"untouched");
    }

    #[test]
    fn in_place_refuses_non_regular_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("subdir");
        fs::create_dir(&path).unwrap();
        assert!(write_file(&path, b"x", opts(0o444, WriteMode::InPlace)).is_err());
    }

    #[test]
    fn replace_swaps_out_symlinks_without_touching_target() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target");
        fs::write(&target, b"data").unwrap();
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert!(write_file(&link, b"secret", opts(0o444, WriteMode::Replace)).unwrap());
        assert!(fs::symlink_metadata(&link).unwrap().is_file());
        assert_eq!(fs::read(&target).unwrap(), b"data");
    }
}
