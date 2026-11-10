//! Terminal credentials, outside worktrees and run directories. The default
//! tries the OS then a private file. Explicit file never opens the Keychain.
use crate::application::login::{CredentialStore, Credentials};
use anyhow::{Result, ensure};
use std::{
    fs,
    io::{Read, Write},
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::PathBuf,
};

/// One set at $XDG_CONFIG_HOME/dagq/credentials/login.json, or under
/// ~/.config when unset. Directory 0700, file 0600; atomic same-dir rename.
/// Files and the credential directory may not be symlinks.
pub struct FileCredentials {
    pub directory: PathBuf,
}
impl FileCredentials {
    pub fn from_env(env: impl Fn(&str) -> Option<String>) -> Result<Self> {
        let root = if let Some(root) = env("XDG_CONFIG_HOME").filter(|s| !s.is_empty()) {
            PathBuf::from(root)
        } else {
            PathBuf::from(
                env("HOME")
                    .ok_or_else(|| anyhow::anyhow!("HOME or XDG_CONFIG_HOME is required"))?,
            )
            .join(".config")
        };
        ensure!(
            root.is_absolute(),
            "credential config directory must be absolute"
        );
        Ok(Self {
            directory: root.join("dagq/credentials"),
        })
    }
    fn check_dir(&self) -> Result<()> {
        let meta = fs::symlink_metadata(&self.directory)?;
        ensure!(
            meta.is_dir() && meta.permissions().mode() & 0o077 == 0,
            "credential directory must be private (0700)"
        );
        Ok(())
    }
}
impl CredentialStore for FileCredentials {
    fn load(&self) -> Result<Option<Credentials>> {
        if !self.directory.try_exists()? {
            return Ok(None);
        }
        self.check_dir()?;
        let mut file = match fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(self.directory.join("login.json"))
        {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(_) => anyhow::bail!("cannot open credential file"),
        };
        let meta = file.metadata()?;
        ensure!(
            meta.is_file() && private_file_mode(meta.permissions().mode()),
            "credential file must have mode 0600"
        );
        let mut bytes = Vec::new();
        (&mut file).take(256 * 1024 + 1).read_to_end(&mut bytes)?;
        ensure!(bytes.len() <= 256 * 1024, "credential file is too large");
        serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|_| anyhow::anyhow!("invalid credential file; run dagq login"))
    }
    fn save(&self, credentials: &Credentials) -> Result<()> {
        use std::os::unix::fs::DirBuilderExt;
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&self.directory)?;
        self.check_dir()?;
        let temp = self
            .directory
            .join(format!(".{}.tmp", uuid::Uuid::new_v4()));
        let result = (|| -> Result<()> {
            let mut file = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&temp)?;
            file.write_all(&serde_json::to_vec(credentials)?)?;
            file.sync_all()?;
            fs::rename(&temp, self.directory.join("login.json"))?;
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(temp);
        }
        result
    }
    fn delete(&self) -> Result<()> {
        if !self.directory.try_exists()? {
            return Ok(());
        }
        self.check_dir()?;
        match fs::remove_file(self.directory.join("login.json")) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(_) => anyhow::bail!("cannot remove credential file"),
        }
    }
}
/// macOS uses Security.framework directly through security-framework (no
/// command-line secret). Linux secret-service clients require an external
/// D-Bus library or an async runtime, so Linux and other OSes use file storage.
/// No OS call is made by any unit test; the process test explicitly uses file.
pub struct OsCredentials;
#[cfg(target_os = "macos")]
impl CredentialStore for OsCredentials {
    fn load(&self) -> Result<Option<Credentials>> {
        match security_framework::passwords::get_generic_password("dagq.login", "terminal") {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map(Some)
                .map_err(|_| anyhow::anyhow!("invalid OS credentials")),
            Err(e) if e.code() == -25300 => Ok(None),
            Err(_) => anyhow::bail!("OS credential store unavailable"),
        }
    }
    fn save(&self, credentials: &Credentials) -> Result<()> {
        security_framework::passwords::set_generic_password(
            "dagq.login",
            "terminal",
            &serde_json::to_vec(credentials)?,
        )
        .map_err(|_| anyhow::anyhow!("OS credential store unavailable"))
    }
    fn delete(&self) -> Result<()> {
        match security_framework::passwords::delete_generic_password("dagq.login", "terminal") {
            Ok(()) => Ok(()),
            Err(e) if e.code() == -25300 => Ok(()),
            Err(_) => anyhow::bail!("OS credential store unavailable"),
        }
    }
}
#[cfg(not(target_os = "macos"))]
impl CredentialStore for OsCredentials {
    fn load(&self) -> Result<Option<Credentials>> {
        Err(crate::application::login::OsStoreUnavailable.into())
    }
    fn save(&self, _: &Credentials) -> Result<()> {
        Err(crate::application::login::OsStoreUnavailable.into())
    }
    fn delete(&self) -> Result<()> {
        Err(crate::application::login::OsStoreUnavailable.into())
    }
}

/// The person's token for their client-mode `dagq` to a control side
/// (`DAGQ_CONTROL_URL`): the credential set of the store
/// `DAGQ_CREDENTIAL_STORE` chooses, read by
/// [`crate::application::login::read_token`] for the URL the call goes to,
/// which refuses a set of another control side (`dagq login`'s guide) and
/// renews once on a refusal. The store is opened only when a call is sent,
/// never by a command refused before it. The command line alone uses it: a
/// runner or a CI gives its own token.
pub struct TerminalBearer;
impl crate::infrastructure::queue_service::BearerSource for TerminalBearer {
    fn token(&self, url: &str, renew: bool) -> Result<String> {
        use crate::application::login::{self, Rejected, StoreChoice, Stores};
        let env = |name: &str| std::env::var(name).ok();
        let choice = StoreChoice::parse(env("DAGQ_CREDENTIAL_STORE").as_deref())?;
        let file = FileCredentials::from_env(env)?;
        let stores = Stores {
            choice,
            os: &OsCredentials,
            file: &file,
        };
        let rejected = if renew { Rejected::First } else { Rejected::No };
        let now = crate::application::unix_seconds(std::time::SystemTime::now());
        let http = crate::infrastructure::oidc_keys::HttpsDocumentSource::for_login();
        Ok(login::read_token(url, rejected, now, &http, &stores)?.token)
    }
}

/// Reject any group/other permission, executable bit or special mode.
pub fn private_file_mode(mode: u32) -> bool {
    mode & 0o7777 == 0o600
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn file_location_and_permissions_are_private_and_outside_the_checkout() {
        let home =
            FileCredentials::from_env(|name| (name == "HOME").then(|| "/home/person".into()))
                .unwrap();
        assert_eq!(
            home.directory,
            PathBuf::from("/home/person/.config/dagq/credentials")
        );
        let xdg = FileCredentials::from_env(|name| {
            Some(
                if name == "XDG_CONFIG_HOME" {
                    "/personal/config"
                } else {
                    "/ignored"
                }
                .into(),
            )
        })
        .unwrap();
        assert_eq!(
            xdg.directory,
            PathBuf::from("/personal/config/dagq/credentials")
        );
        assert!(FileCredentials::from_env(|_| None).is_err());
        assert!(FileCredentials::from_env(|_| Some("relative".into())).is_err());
        assert!(private_file_mode(0o100600));
        for mode in [0o644, 0o660, 0o400, 0o700, 0o4600] {
            assert!(!private_file_mode(mode));
        }
    }
}
