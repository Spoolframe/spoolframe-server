//! The server's Iroh secret key: its permanent identity (ARCHITECTURE §11).
//!
//! Clients pin the server's `EndpointId` (P1-13) and federation will use it
//! as the server's name (§27), so the key is made once and then only ever
//! loaded. A key file that exists but cannot be used is a fatal error,
//! never a reason to make a new one: that would silently break every pin.

use std::fs::{self, OpenOptions};
use std::io::{ErrorKind, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use iroh::SecretKey;
use password_hash::rand_core::{OsRng, RngCore};
use spoolframe_protocol::server_id::SERVER_ID_BYTES;
use spoolframe_protocol::{InvalidServerId, ServerId};

/// The key file's name inside the state directory.
pub const KEY_FILE: &str = "endpoint.key";

/// The server id's file inside the state directory, beside the key.
pub const SERVER_ID_FILE: &str = "server.id";

#[derive(Debug, thiserror::Error)]
pub enum IdentityError {
    #[error("cannot read the server key {path}: {source}")]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("the server key {path} is not a key ({len} bytes, expected 32)")]
    Malformed { path: PathBuf, len: usize },
    #[error(
        "the server key {path} is readable by others (mode {mode:o}); \
         run `chmod 600 {path}`"
    )]
    Exposed { path: PathBuf, mode: u32 },
    #[error("cannot create {path}: {source}")]
    Create {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error(
        "the server id {path} is not one ({source}); restore it from a backup: \
         a new one would leave every app passkey behind"
    )]
    MalformedServerId {
        path: PathBuf,
        source: InvalidServerId,
    },
}

/// The key in `state_dir`, made (with mode `0600`) if there is none yet.
pub fn load_or_create(state_dir: &Path) -> Result<SecretKey, IdentityError> {
    let path = state_dir.join(KEY_FILE);
    match fs::metadata(&path) {
        Ok(meta) => {
            // Like ssh: a key others can read may already be someone
            // else's, and serving with it would hide that.
            let mode = meta.permissions().mode() & 0o777;
            if mode & 0o077 != 0 {
                return Err(IdentityError::Exposed { path, mode });
            }
            load(&path)
        }
        Err(e) if e.kind() == ErrorKind::NotFound => create(state_dir, &path),
        Err(source) => Err(IdentityError::Read { path, source }),
    }
}

/// The server id in `state_dir` ([`ServerId`]), made (with mode `0600`)
/// if there is none yet. It is apart from the key, so a new key keeps it.
/// A file that exists but cannot be used is fatal, as the key's is: a new
/// id would leave every app passkey behind. It is no secret (it goes
/// wherever the passkeys sync), but it is written as privately as the key.
pub fn load_or_create_server_id(state_dir: &Path) -> Result<ServerId, IdentityError> {
    let path = state_dir.join(SERVER_ID_FILE);
    match fs::read_to_string(&path) {
        // A newline is what an editor or `echo` leaves.
        Ok(text) => ServerId::parse(text.trim_end_matches('\n'))
            .map_err(|source| IdentityError::MalformedServerId { path, source }),
        Err(e) if e.kind() == ErrorKind::NotFound => create_server_id(state_dir, &path),
        Err(source) => Err(IdentityError::Read { path, source }),
    }
}

fn create_server_id(state_dir: &Path, path: &Path) -> Result<ServerId, IdentityError> {
    let failed = |source| IdentityError::Create {
        path: path.to_owned(),
        source,
    };
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(state_dir)
        .map_err(failed)?;
    let mut bytes = [0; SERVER_ID_BYTES];
    OsRng.fill_bytes(&mut bytes);
    let id = ServerId::from_bytes(bytes);
    // create_new: if another process made one in the meantime, fail
    // rather than overwrite it.
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .map_err(failed)?;
    file.write_all(id.as_str().as_bytes()).map_err(failed)?;
    file.sync_all().map_err(failed)?;
    tracing::info!(path = %path.display(), server_id = %id, "created the server id");
    Ok(id)
}

/// The fingerprint a person compares with a client's before it pins this
/// key (P1-21).
pub fn fingerprint(key: &SecretKey) -> String {
    spoolframe_protocol::fingerprint::fingerprint(key.public().as_bytes())
}

fn load(path: &Path) -> Result<SecretKey, IdentityError> {
    let bytes = fs::read(path).map_err(|source| IdentityError::Read {
        path: path.to_owned(),
        source,
    })?;
    let bytes: [u8; 32] = bytes
        .as_slice()
        .try_into()
        .map_err(|_| IdentityError::Malformed {
            path: path.to_owned(),
            len: bytes.len(),
        })?;
    Ok(SecretKey::from_bytes(&bytes))
}

fn create(state_dir: &Path, path: &Path) -> Result<SecretKey, IdentityError> {
    let failed = |source| IdentityError::Create {
        path: path.to_owned(),
        source,
    };
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(state_dir)
        .map_err(failed)?;
    let key = SecretKey::generate();
    // create_new: if another process made the key in the meantime, fail
    // rather than overwrite it.
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .map_err(failed)?;
    file.write_all(&key.to_bytes()).map_err(failed)?;
    file.sync_all().map_err(failed)?;
    tracing::info!(path = %path.display(), endpoint_id = %key.public(), "created the server key");
    Ok(key)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A directory of the test's own, removed when dropped, and a state
    /// directory in it, not made yet.
    async fn state_dir(test: &str) -> (spoolframe_test_dir::TestDir, PathBuf) {
        let tmp = spoolframe_test_dir::TestDir::new(&format!("spoolframe-identity-{test}")).await;
        let dir = tmp.as_ref().join("state");
        (tmp, dir)
    }

    #[tokio::test]
    async fn a_new_key_is_created_private() {
        let (_tmp, dir) = state_dir("create").await;
        load_or_create(&dir).unwrap();
        let mode = fs::metadata(dir.join(KEY_FILE))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[tokio::test]
    async fn the_same_key_is_loaded_again() {
        let (_tmp, dir) = state_dir("reload").await;
        let first = load_or_create(&dir).unwrap();
        let second = load_or_create(&dir).unwrap();
        assert_eq!(first.public(), second.public());
    }

    #[tokio::test]
    async fn an_unreadable_key_is_fatal_and_left_alone() {
        let (_tmp, dir) = state_dir("unreadable").await;
        load_or_create(&dir).unwrap();
        let path = dir.join(KEY_FILE);
        let before = fs::read(&path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o000)).unwrap();
        let result = load_or_create(&dir);
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        assert!(
            matches!(result, Err(IdentityError::Read { .. })),
            "{result:?}"
        );
        assert_eq!(fs::read(&path).unwrap(), before);
    }

    #[tokio::test]
    async fn a_key_of_the_wrong_size_is_fatal() {
        let (_tmp, dir) = state_dir("malformed").await;
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join(KEY_FILE);
        fs::write(&path, b"short").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        assert!(matches!(
            load_or_create(&dir),
            Err(IdentityError::Malformed { len: 5, .. })
        ));
    }

    #[tokio::test]
    async fn a_key_others_can_read_is_refused() {
        let (_tmp, dir) = state_dir("exposed").await;
        load_or_create(&dir).unwrap();
        let path = dir.join(KEY_FILE);
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        let err = load_or_create(&dir).unwrap_err();
        assert!(err.to_string().contains("chmod 600"), "{err}");
    }

    #[tokio::test]
    async fn a_new_server_id_is_created_private_and_kept() {
        let (_tmp, dir) = state_dir("server-id-create").await;
        let id = load_or_create_server_id(&dir).unwrap();
        let path = dir.join(SERVER_ID_FILE);
        let mode = fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        assert_eq!(fs::read_to_string(&path).unwrap(), id.as_str());
    }

    #[tokio::test]
    async fn the_same_server_id_is_loaded_again() {
        let (_tmp, dir) = state_dir("server-id-reload").await;
        let first = load_or_create_server_id(&dir).unwrap();
        assert_eq!(load_or_create_server_id(&dir).unwrap(), first);
    }

    /// Made at random: two servers do not share one.
    #[tokio::test]
    async fn another_server_has_another_id() {
        let (_one, one) = state_dir("server-id-one").await;
        let (_two, two) = state_dir("server-id-two").await;
        assert_ne!(
            load_or_create_server_id(&one).unwrap(),
            load_or_create_server_id(&two).unwrap()
        );
    }

    /// The server id is apart from the key: a new key keeps it.
    #[tokio::test]
    async fn a_new_key_keeps_the_server_id() {
        let (_tmp, dir) = state_dir("server-id-new-key").await;
        load_or_create(&dir).unwrap();
        let id = load_or_create_server_id(&dir).unwrap();
        fs::remove_file(dir.join(KEY_FILE)).unwrap();
        load_or_create(&dir).unwrap();
        assert_eq!(load_or_create_server_id(&dir).unwrap(), id);
    }

    /// One written by hand with a newline is read as itself.
    #[tokio::test]
    async fn a_server_id_with_a_newline_is_read() {
        let (_tmp, dir) = state_dir("server-id-newline").await;
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(SERVER_ID_FILE), "mzxw6ytbojtg633c\n").unwrap();
        assert_eq!(
            load_or_create_server_id(&dir).unwrap().as_str(),
            "mzxw6ytbojtg633c"
        );
    }

    /// A new id would leave every app passkey behind: one that cannot be
    /// read is fatal, and stays as it is.
    #[tokio::test]
    async fn a_server_id_that_is_not_one_is_fatal_and_left_alone() {
        let (_tmp, dir) = state_dir("server-id-malformed").await;
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join(SERVER_ID_FILE);
        fs::write(&path, "Not-An-Id").unwrap();
        let err = load_or_create_server_id(&dir).unwrap_err();
        assert!(
            matches!(err, IdentityError::MalformedServerId { .. }),
            "{err:?}"
        );
        assert_eq!(fs::read_to_string(&path).unwrap(), "Not-An-Id");
    }
}
