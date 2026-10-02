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

/// The key file's name inside the state directory.
pub const KEY_FILE: &str = "endpoint.key";

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
    #[error("cannot create the server key {path}: {source}")]
    Create {
        path: PathBuf,
        source: std::io::Error,
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

    fn state_dir(test: &str) -> PathBuf {
        let dir = std::env::temp_dir()
            .join(format!("spoolframe-identity-{}", std::process::id()))
            .join(test);
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn a_new_key_is_created_private() {
        let dir = state_dir("create");
        load_or_create(&dir).unwrap();
        let mode = fs::metadata(dir.join(KEY_FILE))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn the_same_key_is_loaded_again() {
        let dir = state_dir("reload");
        let first = load_or_create(&dir).unwrap();
        let second = load_or_create(&dir).unwrap();
        assert_eq!(first.public(), second.public());
    }

    #[test]
    fn an_unreadable_key_is_fatal_and_left_alone() {
        let dir = state_dir("unreadable");
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

    #[test]
    fn a_key_of_the_wrong_size_is_fatal() {
        let dir = state_dir("malformed");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join(KEY_FILE);
        fs::write(&path, b"short").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        assert!(matches!(
            load_or_create(&dir),
            Err(IdentityError::Malformed { len: 5, .. })
        ));
    }

    #[test]
    fn a_key_others_can_read_is_refused() {
        let dir = state_dir("exposed");
        load_or_create(&dir).unwrap();
        let path = dir.join(KEY_FILE);
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        let err = load_or_create(&dir).unwrap_err();
        assert!(err.to_string().contains("chmod 600"), "{err}");
    }
}
