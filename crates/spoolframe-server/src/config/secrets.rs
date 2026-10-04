//! Secrets set at run time (P3-10): one file each under the state
//! directory's `secrets/`, readable by the server's user only. Never in a
//! config layer, never shown, never logged: `config show` and the admin
//! API say only whether one is set.

use std::io::Write as _;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

/// The secrets of the server whose state is in `state_dir`.
#[derive(Debug, Clone)]
pub struct Secrets {
    dir: PathBuf,
}

impl Secrets {
    pub fn new(state_dir: &Path) -> Self {
        Self {
            dir: state_dir.join("secrets"),
        }
    }

    /// The file secret `name` is kept in.
    pub fn path(&self, name: &str) -> PathBuf {
        self.dir.join(name)
    }

    /// Secret `name`, without surrounding whitespace; `None` when it is not
    /// set.
    pub fn get(&self, name: &str) -> std::io::Result<Option<String>> {
        match std::fs::read_to_string(self.path(name)) {
            Ok(text) => Ok(Some(text.trim().to_owned()).filter(|t| !t.is_empty())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    pub fn is_set(&self, name: &str) -> bool {
        self.get(name).ok().flatten().is_some()
    }

    /// Keep `value` as secret `name`: written to a new file (0600) that
    /// then replaces the old one, so a crash leaves one or the other.
    pub fn set(&self, name: &str, value: &str) -> std::io::Result<()> {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&self.dir)?;
        let path = self.path(name);
        let partial = self.dir.join(format!(".{name}.new"));
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&partial)?;
        file.write_all(value.as_bytes())?;
        file.sync_all()?;
        std::fs::rename(&partial, &path)
    }

    /// This installation's id at plex.tv, which a PIN is bound to: made
    /// once, and kept beside the secrets. Shared by the wizard's Sign in
    /// with Plex (P3-10) and each user's link (P3-12).
    pub fn plex_client_id(&self) -> std::io::Result<String> {
        const NAME: &str = "plex-client-id";
        if let Some(id) = self.get(NAME)? {
            return Ok(id);
        }
        use password_hash::rand_core::{OsRng, RngCore};
        let mut bytes = [0u8; 16];
        OsRng.fill_bytes(&mut bytes);
        let id: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
        self.set(NAME, &id)?;
        Ok(id)
    }

    /// Forget secret `name`; whether it was set.
    pub fn remove(&self, name: &str) -> std::io::Result<bool> {
        match std::fs::remove_file(self.path(name)) {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    #[tokio::test]
    async fn a_secret_is_kept_for_the_server_s_user_only() {
        let dir = spoolframe_test_dir::TestDir::new("spoolframe-secrets").await;
        let secrets = Secrets::new(dir.as_ref());
        assert_eq!(secrets.get("plex-token").unwrap(), None);
        secrets.set("plex-token", "abc123").unwrap();
        assert_eq!(
            secrets.get("plex-token").unwrap().as_deref(),
            Some("abc123")
        );
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&secrets.path("plex-token")), 0o600);
        assert_eq!(mode(&dir.as_ref().join("secrets")), 0o700);
        assert!(secrets.remove("plex-token").unwrap());
        assert!(!secrets.is_set("plex-token"));
    }
}
