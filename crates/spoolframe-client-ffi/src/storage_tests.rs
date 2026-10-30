//! `FileStorage`: what it keeps outlives it, its secrets are sealed on
//! disk and bound to their keys, and every key is one file in its
//! directory.

use super::*;

/// Seals by prefixing the name and flipping every byte: enough to tell
/// sealed from plain, and a name from another.
struct FlipCipher;

impl SecretCipher for FlipCipher {
    fn seal(&self, name: String, plaintext: Vec<u8>) -> Result<Vec<u8>, StorageFailure> {
        let mut sealed = format!("{name}:").into_bytes();
        sealed.extend(plaintext.iter().map(|b| !b));
        Ok(sealed)
    }

    fn unseal(&self, name: String, sealed: Vec<u8>) -> Result<Vec<u8>, StorageFailure> {
        let prefix = format!("{name}:").into_bytes();
        match sealed.strip_prefix(prefix.as_slice()) {
            Some(rest) => Ok(rest.iter().map(|b| !b).collect()),
            None => Err(StorageFailure::Failed {
                message: "sealed under another name".into(),
            }),
        }
    }
}

/// An empty directory of this test's own.
fn scratch(test: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "spoolframe-ffi-storage-{}-{test}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&dir);
    dir
}

fn storage(dir: &Path) -> Arc<FileStorage> {
    FileStorage::new(dir.display().to_string(), Arc::new(FlipCipher)).unwrap()
}

fn names(dir: &Path) -> Vec<String> {
    let mut names: Vec<_> = fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

#[test]
fn data_outlives_the_storage_that_wrote_it() {
    let dir = scratch("outlives");
    storage(&dir)
        .set_data("servers".into(), Some(b"[1]".to_vec()))
        .unwrap();
    let read = storage(&dir).data("servers".into()).unwrap();
    assert_eq!(read, Some(b"[1]".to_vec()));
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn a_secret_is_sealed_on_disk_and_reads_back_plain() {
    let dir = scratch("sealed");
    let store = storage(&dir);
    store
        .set_secret("endpoint-key".into(), Some(vec![1, 2, 3]))
        .unwrap();
    let on_disk = fs::read(dir.join("secrets/endpoint-key")).unwrap();
    assert_eq!(
        (
            on_disk.ends_with(&[1, 2, 3]),
            store.secret("endpoint-key".into()).ok()
        ),
        (false, Some(Some(vec![1, 2, 3])))
    );
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn a_secret_moved_under_another_key_does_not_open() {
    let dir = scratch("moved");
    let store = storage(&dir);
    store.set_secret("a".into(), Some(vec![7])).unwrap();
    fs::rename(dir.join("secrets/a"), dir.join("secrets/b")).unwrap();
    assert!(matches!(
        store.secret("b".into()),
        Err(StorageFailure::Failed { .. })
    ));
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn a_key_with_slashes_and_dots_is_one_file_in_its_directory() {
    let dir = scratch("escaped");
    let store = storage(&dir);
    store
        .set_data("sessions/../x".into(), Some(vec![1]))
        .unwrap();
    assert_eq!(
        (names(&dir.join("data")), names(&dir)),
        (
            vec!["sessions%2F%2E%2E%2Fx".to_owned()],
            vec!["data".to_owned(), "secrets".to_owned()]
        )
    );
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn a_key_set_to_none_is_gone_and_removing_a_missing_one_is_fine() {
    let dir = scratch("removed");
    let store = storage(&dir);
    store.set_data("servers".into(), Some(vec![1])).unwrap();
    store.set_data("servers".into(), None).unwrap();
    assert_eq!(
        (
            store.data("servers".into()).ok(),
            store.set_data("servers".into(), None).ok()
        ),
        (Some(None), Some(()))
    );
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn a_write_replaces_the_value_and_leaves_no_temporary_file() {
    let dir = scratch("replaced");
    let store = storage(&dir);
    store.set_data("servers".into(), Some(vec![1])).unwrap();
    store.set_data("servers".into(), Some(vec![2, 2])).unwrap();
    assert_eq!(
        (store.data("servers".into()).ok(), names(&dir.join("data"))),
        (Some(Some(vec![2, 2])), vec!["servers".to_owned()])
    );
    fs::remove_dir_all(&dir).unwrap();
}

#[tokio::test]
async fn the_sdk_reads_what_it_wrote_through_the_app_s_storage() {
    let dir = scratch("sdk");
    let sdk = Bridge(storage(&dir));
    sdk.set_secret("endpoint-key", Some(vec![9; 32]))
        .await
        .unwrap();
    let read = sdk.secret("endpoint-key").await.unwrap();
    assert_eq!(read, Some(vec![9; 32]));
    fs::remove_dir_all(&dir).unwrap();
}

/// A write a crash cut short leaves its temporary file: the first open
/// in the next process removes it, and keeps the values.
#[test]
fn opening_removes_the_temporary_files_a_crash_left() {
    let dir = scratch("stale");
    fs::create_dir_all(dir.join("data")).unwrap();
    fs::create_dir_all(dir.join("secrets")).unwrap();
    fs::write(dir.join("data/servers"), [1]).unwrap();
    fs::write(dir.join("data/.servers.7.tmp"), [9]).unwrap();
    fs::write(dir.join("secrets/.endpoint-key.0.tmp"), [9]).unwrap();
    storage(&dir);
    assert_eq!(
        (names(&dir.join("data")), names(&dir.join("secrets"))),
        (vec!["servers".to_owned()], Vec::<String>::new())
    );
    fs::remove_dir_all(&dir).unwrap();
}

/// A second storage opened on the same root in this process (an Android
/// Activity made again while the old client still writes) leaves the
/// first one's write under way alone.
#[test]
fn a_second_open_leaves_a_write_under_way_alone() {
    let dir = scratch("second-open");
    let _first = storage(&dir);
    let under_way = format!(".session.{}.3.tmp", std::process::id());
    fs::write(dir.join("data").join(&under_way), [9]).unwrap();
    storage(&dir);
    assert_eq!(names(&dir.join("data")), [under_way]);
    fs::remove_dir_all(&dir).unwrap();
}
