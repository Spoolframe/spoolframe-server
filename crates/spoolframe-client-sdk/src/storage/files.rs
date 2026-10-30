//! Files written whole and kept: each written beside its place, synced,
//! renamed into it, and the rename made durable by syncing the directory.
//! A crash leaves the old file or the new one, never half of one, and
//! never an earlier write lost behind a later one. The downloads' own
//! files are written so, and an app's storage in files may be (the FFI's
//! `FileStorage`).

use std::fs;
use std::io::{ErrorKind, Write};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

/// Each write's temporary file has its own name, so two writes of one
/// path never share one; with the process's id in it, so a sweep
/// ([`remove_temporaries`]) knows this process's own writes under way.
static WRITES: AtomicU64 = AtomicU64::new(0);

/// Write `bytes` to `path`, whole, made durable before it returns. Its
/// directory is made if it is not there.
pub fn write(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let dir = parent(path);
    fs::create_dir_all(dir)?;
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    let n = WRITES.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    let temporary = dir.join(format!(".{name}.{pid}.{n}.tmp"));
    let written = fs::File::create(&temporary)
        .and_then(|mut file| {
            file.write_all(bytes)?;
            file.sync_all()
        })
        .and_then(|()| fs::rename(&temporary, path));
    if let Err(error) = written {
        let _ = fs::remove_file(&temporary);
        return Err(error);
    }
    sync_dir(dir)
}

/// Remove `path`, made durable before it returns; one not there is fine.
pub fn remove(path: &Path) -> std::io::Result<()> {
    match fs::remove_file(path) {
        Ok(()) => sync_dir(parent(path)),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

/// Remove the temporary files [`write`] left in `dir` when a crash cut it
/// short: `.<name>.<pid>.<n>.tmp` (`.<name>.<n>.tmp` before 0.17) of
/// another process. This process's are writes under way (another
/// storage on the same directory, as an Android Activity made again
/// while the old client still writes), and are left. A name from
/// [`file_name`] never starts with `.`.
pub fn remove_temporaries(dir: &Path) -> std::io::Result<()> {
    let ours = format!(".{}.", std::process::id());
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        let name = path.file_name().unwrap_or_default().to_string_lossy();
        let Some(stem) = name.strip_suffix(".tmp").filter(|_| name.starts_with('.')) else {
            continue;
        };
        // `<stem>` ends `.<pid>.<n>`: ours where `<pid>` is.
        let mine = stem.rfind('.').is_some_and(|n| stem[..=n].ends_with(&ours));
        if !mine {
            fs::remove_file(&path)?;
        }
    }
    Ok(())
}

/// A key as one file name: letters, digits, `-` and `_` as they are,
/// every other byte as `%XX`. Keys have slashes (`sessions/<id>`), and a
/// key of `..` must not leave the directory.
pub fn file_name(key: &str) -> String {
    let mut name = String::with_capacity(key.len());
    for byte in key.bytes() {
        if byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_' {
            name.push(byte as char);
        } else {
            name.push_str(&format!("%{byte:02X}"));
        }
    }
    name
}

fn parent(path: &Path) -> &Path {
    path.parent().expect("a file in a directory")
}

/// Make the renames, removals and new directories in `dir` durable.
pub fn sync_dir(dir: &Path) -> std::io::Result<()> {
    fs::File::open(dir)?.sync_all()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn scratch(test: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "spoolframe-sdk-files-{}-{test}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    fn names(dir: &Path) -> Vec<String> {
        let mut names: Vec<_> = fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn a_write_replaces_the_file_and_leaves_nothing_beside_it() {
        let dir = scratch("write");
        let path = dir.join("manifest.json");
        write(&path, b"1").unwrap();
        write(&path, b"22").unwrap();
        assert_eq!(
            (fs::read(&path).unwrap(), names(&dir)),
            (b"22".to_vec(), vec!["manifest.json".to_owned()])
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_crash_s_temporary_files_are_removed_and_the_files_kept() {
        let dir = scratch("temporaries");
        write(&dir.join("servers"), b"[]").unwrap();
        fs::write(dir.join(".servers.9.tmp"), b"half").unwrap();
        remove_temporaries(&dir).unwrap();
        assert_eq!(names(&dir), ["servers"]);
        fs::remove_dir_all(&dir).unwrap();
    }

    /// A temporary file of this process is a write under way: a sweep
    /// leaves it, and removes another process's.
    #[test]
    fn a_sweep_leaves_this_process_s_writes_under_way() {
        let dir = scratch("ours");
        fs::create_dir_all(&dir).unwrap();
        let ours = format!(".session.{}.3.tmp", std::process::id());
        fs::write(dir.join(&ours), b"half").unwrap();
        fs::write(dir.join(".session.1.4.tmp"), b"half").unwrap();
        remove_temporaries(&dir).unwrap();
        assert_eq!(names(&dir), [ours]);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_removed_file_is_gone_and_one_not_there_is_fine() {
        let dir = scratch("remove");
        let path = dir.join("servers");
        write(&path, b"[]").unwrap();
        remove(&path).unwrap();
        assert_eq!((path.exists(), remove(&path).is_ok()), (false, true));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_key_with_slashes_and_dots_is_one_name() {
        assert_eq!(file_name("sessions/../x"), "sessions%2F%2E%2E%2Fx");
    }
}
