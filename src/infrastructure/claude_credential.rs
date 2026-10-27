//! The Claude credential the queue broker puts in place of a run token
//! (ADR-t2177-1, which amends ADR-t2114-2): the `[claude_credential]` table
//! of `host.toml`, never of `dagq.toml` (the kind is the choice of the
//! person who bears the contract, the cost and the terms on this host). The
//! queue's `<queue dir>/host.toml` replaces the host-wide
//! `$XDG_CONFIG_HOME/dagq/host.toml`'s table as a whole; neither is in the
//! repository.
//!
//! ```toml
//! [claude_credential]
//! kind = "api_key"                             # or "setup_token"; no default
//! file = "/Users/me/.config/dagq/claude-key"   # absolute, mode 0600
//! # or, on macOS, instead of file:
//! # keychain_service = "dagq-claude"
//! # keychain_account = "me"
//! ```
//!
//! - `kind`: which credential ([`ClaudeCredentialKind`]): `setup_token`, the
//!   long-lived token `claude setup-token` issued (sent as the
//!   `Authorization` bearer), or `api_key`, an API key issued in the
//!   Anthropic Console (sent in `x-api-key`). No default: without one, a
//!   container's worker cannot be used.
//! - `file`: the absolute path of the file that holds the chosen credential
//!   and nothing else (surrounding whitespace is dropped), a regular file of
//!   mode 0600 a person issued and put there. No default.
//! - `keychain_service` and `keychain_account`, together and in place of
//!   `file`: the generic password item of macOS's keychain that holds the
//!   chosen credential (surrounding whitespace is dropped), put there by a
//!   person (`security add-generic-password -s <service> -a <account> -w`,
//!   which asks for the value), read through [`Keychain`]. Not the item of
//!   Claude Code's own login (ADR-t2114-2 decision 3). No default.
//!
//! Exactly one place is chosen: both `file` and a keychain item, or
//! neither, does not read, and a keychain item on a host without macOS's
//! keychain cannot be used. Only the broker reads the value ([`read`]), on
//! each request; the gate of the container runtime ([`ClaudeCredentialGate`])
//! and `doctor` ([`report`]) look at its kind, place and presence (and a
//! file's mode) only, and nothing writes its value or passes it as an
//! argument.

use std::fs;
use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};

use super::kpi_config::HOST_FILE_NAME;
use super::run_env::{parse_string, strip_comment};
use crate::application::ContainerRuntime;
use crate::domain::container_runtime::{Availability, RuntimeTarget};
use crate::domain::queue_broker::{
    ClaudeCredential, ClaudeCredentialKind, ClaudeCredentialProblem, credential_mode_private,
};

/// The table of `host.toml` that chooses the Claude credential.
pub const TABLE: &str = "claude_credential";

/// What one `host.toml`'s `[claude_credential]` says.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ClaudeCredentialConfig {
    pub kind: Option<ClaudeCredentialKind>,
    pub file: Option<PathBuf>,
    pub keychain_service: Option<String>,
    pub keychain_account: Option<String>,
}

/// A generic password item of macOS's keychain: its service and account.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeychainItem {
    pub service: String,
    pub account: String,
}

impl std::fmt::Display for KeychainItem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "keychain item (service {:?}, account {:?})",
            self.service, self.account
        )
    }
}

/// Where the chosen credential is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CredentialPlace {
    File(PathBuf),
    Keychain(KeychainItem),
}

impl CredentialPlace {
    /// `doctor`'s `place`.
    fn name(&self) -> &'static str {
        match self {
            Self::File(_) => "file",
            Self::Keychain(_) => "keychain",
        }
    }
}

impl std::fmt::Display for CredentialPlace {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::File(file) => write!(f, "file {}", file.display()),
            Self::Keychain(item) => item.fmt(f),
        }
    }
}

impl ClaudeCredentialConfig {
    /// The one place the table chooses; why it chooses none or two.
    fn place(
        &self,
        kind: ClaudeCredentialKind,
    ) -> Result<CredentialPlace, ClaudeCredentialProblem> {
        let invalid = |why: &str| {
            ClaudeCredentialProblem::Invalid(format!(
                "[{TABLE}] chooses kind = {:?} but {why}",
                kind.as_str()
            ))
        };
        let keychain = match (&self.keychain_service, &self.keychain_account) {
            (Some(service), Some(account)) => Some(KeychainItem {
                service: service.clone(),
                account: account.clone(),
            }),
            (None, None) => None,
            _ => {
                return Err(invalid(
                    "names keychain_service without keychain_account or the other way round",
                ));
            }
        };
        match (&self.file, keychain) {
            (Some(file), None) => Ok(CredentialPlace::File(file.clone())),
            (None, Some(item)) => Ok(CredentialPlace::Keychain(item)),
            (Some(_), Some(_)) => Err(invalid("names both a file and a keychain item; choose one")),
            (None, None) => Err(invalid("names no file or keychain item")),
        }
    }
}

/// How long the keychain's command may take before the item counts as
/// unreadable (a locked keychain may wait for a person).
const KEYCHAIN_TIMEOUT: Duration = Duration::from_secs(10);

/// The `security` command's exit status for an item that is not there
/// (`errSecItemNotFound`).
const KEYCHAIN_ITEM_NOT_FOUND: i32 = 44;

/// A keychain the chosen item is looked up in.
pub trait KeychainLookup {
    /// Whether `item` is there, without its value (empty); with `value`,
    /// the value as it is stored.
    fn find(&self, item: &KeychainItem, value: bool) -> Result<Vec<u8>, ClaudeCredentialProblem>;
}

/// macOS's keychain, read through its `security` command: the service and
/// the account are its arguments, the value only its output, which nothing
/// but [`read`] keeps.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Keychain {
    pub program: PathBuf,
}

impl Keychain {
    /// This host's keychain: `/usr/bin/security` on macOS, none elsewhere.
    pub fn of_host() -> Option<Self> {
        cfg!(target_os = "macos").then(|| Self {
            program: PathBuf::from("/usr/bin/security"),
        })
    }
}

impl KeychainLookup for Keychain {
    fn find(&self, item: &KeychainItem, value: bool) -> Result<Vec<u8>, ClaudeCredentialProblem> {
        let unreadable =
            |why: String| ClaudeCredentialProblem::Unreadable(format!("{item}: {why}"));
        let mut command = Command::new(&self.program);
        command
            .args([
                "find-generic-password",
                "-s",
                &item.service,
                "-a",
                &item.account,
            ])
            .stdin(Stdio::null())
            .stdout(if value { Stdio::piped() } else { Stdio::null() })
            .stderr(Stdio::null());
        if value {
            command.arg("-w");
        }
        let mut child = command
            .spawn()
            .map_err(|error| unreadable(format!("start {}: {error}", self.program.display())))?;
        let deadline = Instant::now() + KEYCHAIN_TIMEOUT;
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Ok(None) => {
                    child.kill().ok();
                    child.wait().ok();
                    return Err(unreadable(format!(
                        "{} did not answer in {}s",
                        self.program.display(),
                        KEYCHAIN_TIMEOUT.as_secs()
                    )));
                }
                Err(error) => {
                    child.kill().ok();
                    child.wait().ok();
                    return Err(unreadable(error.to_string()));
                }
            }
        };
        if status.code() == Some(KEYCHAIN_ITEM_NOT_FOUND) {
            return Err(ClaudeCredentialProblem::Missing(item.to_string()));
        }
        if !status.success() {
            return Err(unreadable(format!(
                "{} failed ({status})",
                self.program.display()
            )));
        }
        let mut output = Vec::new();
        if let Some(mut stdout) = child.stdout.take() {
            stdout
                .read_to_end(&mut output)
                .map_err(|error| unreadable(error.to_string()))?;
        }
        Ok(output)
    }
}

/// The `[claude_credential]` table of a `host.toml`'s text, `label` naming
/// the file in errors; `None` when it has none.
pub fn parse_host_claude_credential(
    text: &str,
    label: &str,
) -> Result<Option<ClaudeCredentialConfig>> {
    let mut config: Option<ClaudeCredentialConfig> = None;
    let mut seen: Vec<String> = Vec::new();
    let mut inside = false;
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    for (index, raw) in text.lines().enumerate() {
        let number = index + 1;
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(header) = line.strip_prefix('[') {
            let name = strip_comment(header)
                .strip_suffix(']')
                .with_context(|| format!("{label}:{number}: unclosed table header"))?
                .trim();
            inside = name == TABLE;
            if inside {
                ensure!(
                    config.is_none(),
                    "{label}:{number}: [{TABLE}] is defined twice"
                );
                config = Some(ClaudeCredentialConfig::default());
            }
            continue;
        }
        let Some(config) = config.as_mut().filter(|_| inside) else {
            continue;
        };
        let (key, rest) = line
            .split_once('=')
            .with_context(|| format!("{label}:{number}: expected KEY = value"))?;
        let key = key.trim();
        ensure!(
            !seen.iter().any(|seen| seen == key),
            "{label}:{number}: {key} is defined twice"
        );
        seen.push(key.to_owned());
        let value = parse_string(rest.trim())
            .with_context(|| format!("{label}:{number}: value of {key}"))?;
        match key {
            "kind" => {
                config.kind = Some(value.parse().map_err(|_| {
                    anyhow::anyhow!(
                        "{label}:{number}: kind is \"setup_token\" or \"api_key\", not {value:?}"
                    )
                })?);
            }
            "file" => {
                let file = PathBuf::from(&value);
                ensure!(
                    file.is_absolute(),
                    "{label}:{number}: file is an absolute path, not {value:?}"
                );
                config.file = Some(file);
            }
            "keychain_service" | "keychain_account" => {
                ensure!(!value.is_empty(), "{label}:{number}: {key} is not empty");
                if key == "keychain_service" {
                    config.keychain_service = Some(value);
                } else {
                    config.keychain_account = Some(value);
                }
            }
            _ => bail!(
                "{label}:{number}: unknown key {key} in [{TABLE}]; the keys are kind, file, \
                 keychain_service, keychain_account"
            ),
        }
    }
    Ok(config)
}

/// The host's `[claude_credential]`: the queue's (`<queue_dir>/host.toml`),
/// else the host-wide file's; `None` when neither has one.
pub fn load(queue_dir: &Path, host_wide: Option<&Path>) -> Result<Option<ClaudeCredentialConfig>> {
    for path in [Some(queue_dir.join(HOST_FILE_NAME).as_path()), host_wide]
        .into_iter()
        .flatten()
    {
        let text = match fs::read_to_string(path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error).with_context(|| format!("read {}", path.display())),
        };
        if let Some(config) = parse_host_claude_credential(&text, &path.display().to_string())? {
            return Ok(Some(config));
        }
    }
    Ok(None)
}

/// The chosen kind and its place, looked at without reading the value: a
/// file that is a regular file of mode 0600 and not empty, or a keychain
/// item that is there on a host with a `keychain`; why not.
pub fn inspect(
    queue_dir: &Path,
    host_wide: Option<&Path>,
    keychain: Option<&dyn KeychainLookup>,
) -> Result<(ClaudeCredentialKind, CredentialPlace), ClaudeCredentialProblem> {
    let (kind, place) = chosen(queue_dir, host_wide)?;
    match &place {
        CredentialPlace::File(file) => inspect_file(file)?,
        CredentialPlace::Keychain(item) => {
            keychain
                .ok_or_else(|| ClaudeCredentialProblem::KeychainUnavailable(item.to_string()))?
                .find(item, false)?;
        }
    }
    Ok((kind, place))
}

/// The chosen kind and its one place, as the tables say.
fn chosen(
    queue_dir: &Path,
    host_wide: Option<&Path>,
) -> Result<(ClaudeCredentialKind, CredentialPlace), ClaudeCredentialProblem> {
    let config = load(queue_dir, host_wide)
        .map_err(|error| ClaudeCredentialProblem::Invalid(format!("{error:#}")))?
        .unwrap_or_default();
    let Some(kind) = config.kind else {
        return Err(ClaudeCredentialProblem::NotSelected);
    };
    Ok((kind, config.place(kind)?))
}

fn inspect_file(file: &Path) -> Result<(), ClaudeCredentialProblem> {
    let shown = file.display().to_string();
    let metadata = match fs::metadata(file) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(ClaudeCredentialProblem::Missing(format!("file {shown}")));
        }
        Err(error) => {
            return Err(ClaudeCredentialProblem::Unreadable(format!(
                "{shown}: {error}"
            )));
        }
    };
    let mode = metadata.permissions().mode() & 0o7777;
    if !metadata.is_file() || !credential_mode_private(mode) {
        return Err(ClaudeCredentialProblem::NotPrivate { file: shown, mode });
    }
    if metadata.len() == 0 {
        return Err(ClaudeCredentialProblem::Empty(format!("file {shown}")));
    }
    Ok(())
}

/// The chosen credential with its value: the queue broker's only.
pub fn read(
    queue_dir: &Path,
    host_wide: Option<&Path>,
    keychain: Option<&dyn KeychainLookup>,
) -> Result<ClaudeCredential, ClaudeCredentialProblem> {
    let (kind, place) = chosen(queue_dir, host_wide)?;
    let unreadable =
        |error: String| ClaudeCredentialProblem::Unreadable(format!("{place}: {error}"));
    let text = match &place {
        CredentialPlace::File(file) => {
            inspect_file(file)?;
            fs::read_to_string(file).map_err(|error| unreadable(error.to_string()))?
        }
        // One lookup, with the value: a missing item fails it the same.
        CredentialPlace::Keychain(item) => {
            let keychain = keychain
                .ok_or_else(|| ClaudeCredentialProblem::KeychainUnavailable(item.to_string()))?;
            String::from_utf8(keychain.find(item, true)?)
                .map_err(|_| unreadable("the value is not UTF-8".to_owned()))?
        }
    };
    let secret = text.trim();
    if secret.is_empty() {
        return Err(ClaudeCredentialProblem::Empty(place.to_string()));
    }
    Ok(ClaudeCredential {
        kind,
        secret: secret.to_owned(),
    })
}

/// `doctor`'s `claude_credential`: the chosen `kind` (null when none is),
/// its `place` (`file` or `keychain`, null when the table chooses none or
/// two), the `file` or the keychain item's `keychain_service` and
/// `keychain_account`, whether it is `present`, a file's `mode`, whether
/// the broker can send with it (`usable`), and else the `problem` and the
/// `message` (with the guidance on which to choose when none is). Never the
/// value.
pub fn report(
    queue_dir: &Path,
    host_wide: Option<&Path>,
    keychain: Option<&dyn KeychainLookup>,
) -> Value {
    let config = load(queue_dir, host_wide)
        .ok()
        .flatten()
        .unwrap_or_default();
    let place = config.kind.and_then(|kind| config.place(kind).ok());
    let mode = config
        .file
        .as_deref()
        .and_then(|file| fs::metadata(file).ok())
        .map(|metadata| format!("{:04o}", metadata.permissions().mode() & 0o7777));
    let checked = inspect(queue_dir, host_wide, keychain);
    let present = match (&place, &checked) {
        (Some(CredentialPlace::Keychain(_)), Ok(_)) => true,
        (Some(CredentialPlace::Keychain(_)), Err(problem)) => !matches!(
            problem,
            ClaudeCredentialProblem::Missing(_)
                | ClaudeCredentialProblem::KeychainUnavailable(_)
                | ClaudeCredentialProblem::Unreadable(_)
        ),
        // A file, chosen or not, as it is on disk.
        (Some(CredentialPlace::File(_)) | None, _) => mode.is_some(),
    };
    json!({
        "kind": config.kind.map(ClaudeCredentialKind::as_str),
        "place": place.as_ref().map(CredentialPlace::name),
        "file": config.file.as_deref().map(Path::display).map(|file| file.to_string()),
        "keychain_service": config.keychain_service,
        "keychain_account": config.keychain_account,
        "present": present,
        "mode": mode,
        "usable": checked.is_ok(),
        "problem": checked.as_ref().err().map(ClaudeCredentialProblem::reason),
        "message": checked.as_ref().err().map(ClaudeCredentialProblem::message),
    })
}

/// The container runtime, which a worker (and its resume) cannot use
/// while the queue broker has no Claude credential to send with
/// (ADR-t2177-1 decision 3): the worker waits, never on the host or with a
/// kind left to a default, and the answer says why. The verification of
/// `integrate` calls no Claude API, so it is `container`'s to answer.
pub struct ClaudeCredentialGate<C> {
    pub queue_dir: PathBuf,
    pub host_wide: Option<PathBuf>,
    pub keychain: Option<Arc<dyn KeychainLookup + Send + Sync>>,
    pub container: C,
}

impl<C> ClaudeCredentialGate<C> {
    /// The gate of the queue in `queue_dir`, with this environment's
    /// host-wide `host.toml`.
    pub fn of_queue(queue_dir: &Path, container: C) -> Self {
        Self {
            queue_dir: queue_dir.to_path_buf(),
            host_wide: super::kpi_config::host_wide_file(),
            keychain: Keychain::of_host()
                .map(|keychain| Arc::new(keychain) as Arc<dyn KeychainLookup + Send + Sync>),
            container,
        }
    }
}

impl<C: ContainerRuntime> ContainerRuntime for ClaudeCredentialGate<C> {
    fn availability(&self, target: RuntimeTarget) -> Availability {
        if target == RuntimeTarget::Worker
            && let Err(problem) = inspect(
                &self.queue_dir,
                self.host_wide.as_deref(),
                self.keychain
                    .as_deref()
                    .map(|keychain| keychain as &dyn KeychainLookup),
            )
        {
            return Availability::Unavailable(problem.message());
        }
        self.container.availability(target)
    }

    fn memory_wait(&self) -> Option<String> {
        self.container.memory_wait()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn credential_file(dir: &Path, mode: u32, content: &str) -> PathBuf {
        let file = dir.join("claude-credential");
        fs::write(&file, content).unwrap();
        fs::set_permissions(&file, fs::Permissions::from_mode(mode)).unwrap();
        file
    }

    fn host_toml(path: &Path, kind: &str, file: &Path) {
        fs::write(
            path,
            format!(
                "[push]\ncommand = []\n\n[{TABLE}]\nkind = \"{kind}\"  # chosen\nfile = '{}'\n",
                file.display()
            ),
        )
        .unwrap();
    }

    #[test]
    fn the_table_reads_its_kind_and_file_and_refuses_the_rest() {
        let config = parse_host_claude_credential(
            "[claude_credential]\nkind = \"api_key\"\nfile = \"/k\"\n",
            "h",
        )
        .unwrap()
        .unwrap();
        assert_eq!(config.kind, Some(ClaudeCredentialKind::ApiKey));
        assert_eq!(config.file, Some(PathBuf::from("/k")));
        assert_eq!(parse_host_claude_credential("[push]\n", "h").unwrap(), None);
        for (text, says) in [
            ("[claude_credential]\nkind = \"oauth\"\n", "setup_token"),
            ("[claude_credential]\nfile = \"key\"\n", "absolute"),
            ("[claude_credential]\ntoken = \"x\"\n", "unknown key token"),
            (
                "[claude_credential]\nkeychain_service = \"\"\n",
                "keychain_service is not empty",
            ),
            (
                "[claude_credential]\nkind = \"api_key\"\nkind = \"api_key\"\n",
                "twice",
            ),
            ("[claude_credential]\n[claude_credential]\n", "twice"),
        ] {
            let error = parse_host_claude_credential(text, "h").unwrap_err();
            assert!(format!("{error:#}").contains(says), "{error:#}");
        }
    }

    #[test]
    fn the_queue_s_table_replaces_the_host_wide_one_and_only_a_private_file_is_usable() {
        let dir = tempfile::tempdir().unwrap();
        let queue_dir = dir.path().join("queue");
        fs::create_dir(&queue_dir).unwrap();
        let wide = dir.path().join("wide.toml");
        let key = credential_file(dir.path(), 0o600, " sk-secret\n");

        // None chosen anywhere: not selected, and the guidance says which.
        let none = inspect(&queue_dir, Some(&wide), None).unwrap_err();
        assert_eq!(none, ClaudeCredentialProblem::NotSelected);
        let shown = report(&queue_dir, Some(&wide), None);
        assert_eq!(shown["kind"], Value::Null);
        assert_eq!(shown["usable"], false);
        assert_eq!(shown["problem"], "not_selected");
        assert!(shown["message"].as_str().unwrap().contains("api_key"));

        // The host-wide file's table holds without the queue's.
        host_toml(&wide, "setup_token", &key);
        let chosen = read(&queue_dir, Some(&wide), None).unwrap();
        assert_eq!(chosen.kind, ClaudeCredentialKind::SetupToken);
        assert_eq!(chosen.secret, "sk-secret");
        // The queue's replaces it.
        host_toml(&queue_dir.join(HOST_FILE_NAME), "api_key", &key);
        assert_eq!(
            inspect(&queue_dir, Some(&wide), None).unwrap(),
            (
                ClaudeCredentialKind::ApiKey,
                CredentialPlace::File(key.clone())
            )
        );
        let shown = report(&queue_dir, Some(&wide), None);
        assert_eq!(
            shown,
            json!({
                "kind": "api_key",
                "place": "file",
                "file": key.display().to_string(),
                "keychain_service": null,
                "keychain_account": null,
                "present": true,
                "mode": "0600",
                "usable": true,
                "problem": null,
                "message": null,
            })
        );
        assert!(!shown.to_string().contains("sk-secret"));

        // Open to others, empty, missing: not usable, the value never shown.
        fs::set_permissions(&key, fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(
            inspect(&queue_dir, None, None).unwrap_err(),
            ClaudeCredentialProblem::NotPrivate {
                file: key.display().to_string(),
                mode: 0o644
            }
        );
        let shown = report(&queue_dir, None, None);
        assert_eq!(shown["mode"], "0644");
        assert_eq!(shown["problem"], "not_private");
        assert!(!shown.to_string().contains("sk-secret"));
        credential_file(dir.path(), 0o600, "");
        assert_eq!(
            inspect(&queue_dir, None, None).unwrap_err().reason(),
            "empty"
        );
        credential_file(dir.path(), 0o600, " \n");
        assert_eq!(read(&queue_dir, None, None).unwrap_err().reason(), "empty");
        fs::remove_file(&key).unwrap();
        assert_eq!(
            inspect(&queue_dir, None, None).unwrap_err(),
            ClaudeCredentialProblem::Missing(format!("file {}", key.display()))
        );
        assert_eq!(report(&queue_dir, None, None)["present"], false);
        // A directory is no credential file.
        fs::create_dir(&key).unwrap();
        fs::set_permissions(&key, fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(
            inspect(&queue_dir, None, None).unwrap_err().reason(),
            "not_private"
        );
        // A file without a kind is shown as it is on disk.
        fs::write(
            queue_dir.join(HOST_FILE_NAME),
            format!("[claude_credential]\nfile = '{}'\n", key.display()),
        )
        .unwrap();
        let shown = report(&queue_dir, None, None);
        assert_eq!(shown["problem"], "not_selected");
        assert_eq!(shown["present"], true);
        // A table that chooses no file, or does not read.
        fs::write(
            queue_dir.join(HOST_FILE_NAME),
            "[claude_credential]\nkind = \"api_key\"\n",
        )
        .unwrap();
        assert_eq!(
            inspect(&queue_dir, None, None).unwrap_err().reason(),
            "invalid"
        );
        fs::write(
            queue_dir.join(HOST_FILE_NAME),
            "[claude_credential]\nkind = 1\n",
        )
        .unwrap();
        assert_eq!(
            inspect(&queue_dir, None, None).unwrap_err().reason(),
            "invalid"
        );
    }

    struct Ready;

    impl ContainerRuntime for Ready {
        fn availability(&self, _: RuntimeTarget) -> Availability {
            Availability::Available
        }
    }

    #[test]
    fn a_worker_s_container_cannot_be_used_without_a_usable_credential_and_no_kind_is_assumed() {
        let dir = tempfile::tempdir().unwrap();
        let gate = ClaudeCredentialGate {
            queue_dir: dir.path().to_path_buf(),
            host_wide: None,
            keychain: None,
            container: Ready,
        };
        let Availability::Unavailable(why) = gate.availability(RuntimeTarget::Worker) else {
            panic!("a worker's container was usable with no credential chosen");
        };
        assert!(
            why.contains("setup_token") && why.contains("api_key"),
            "{why}"
        );
        // The verification calls no Claude API.
        assert_eq!(
            gate.availability(RuntimeTarget::Verification),
            Availability::Available
        );
        let key = credential_file(dir.path(), 0o640, "sk");
        host_toml(&dir.path().join(HOST_FILE_NAME), "setup_token", &key);
        assert!(matches!(
            gate.availability(RuntimeTarget::Worker),
            Availability::Unavailable(why) if why.contains("0640")
        ));
        fs::set_permissions(&key, fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(
            gate.availability(RuntimeTarget::Worker),
            Availability::Available
        );
    }

    /// A keychain that holds the item (service `dagq-claude`, account
    /// `me`) with `secret`, or none for `None`, and counts the lookups that
    /// asked for the value.
    struct FakeKeychain {
        secret: Option<&'static str>,
        problem: Option<ClaudeCredentialProblem>,
        values_asked: std::sync::atomic::AtomicUsize,
    }

    impl FakeKeychain {
        fn holding(secret: Option<&'static str>) -> Arc<Self> {
            Arc::new(Self {
                secret,
                problem: None,
                values_asked: Default::default(),
            })
        }
    }

    impl KeychainLookup for FakeKeychain {
        fn find(
            &self,
            item: &KeychainItem,
            value: bool,
        ) -> Result<Vec<u8>, ClaudeCredentialProblem> {
            if let Some(problem) = &self.problem {
                return Err(problem.clone());
            }
            let secret = self
                .secret
                .filter(|_| item.service == "dagq-claude" && item.account == "me")
                .ok_or_else(|| ClaudeCredentialProblem::Missing(item.to_string()))?;
            if !value {
                return Ok(Vec::new());
            }
            self.values_asked
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(format!("{secret}\n").into_bytes())
        }
    }

    fn keychain_toml(path: &Path, kind: &str, account: &str) {
        fs::write(
            path,
            format!(
                "[{TABLE}]\nkind = \"{kind}\"\nkeychain_service = \"dagq-claude\"\nkeychain_account = \"{account}\"\n"
            ),
        )
        .unwrap();
    }

    #[test]
    fn a_keychain_item_is_read_on_each_request_and_sent_as_its_kind_never_shown() {
        use crate::domain::queue_broker::claude_headers;
        let dir = tempfile::tempdir().unwrap();
        let keychain = FakeKeychain::holding(Some("sk-from-keychain"));
        let host = dir.path().join(HOST_FILE_NAME);
        for (kind, header, value) in [
            ("setup_token", "Authorization", "Bearer sk-from-keychain"),
            ("api_key", "x-api-key", "sk-from-keychain"),
        ] {
            keychain_toml(&host, kind, "me");
            let credential = read(dir.path(), None, Some(&*keychain)).unwrap();
            assert_eq!(credential.kind.as_str(), kind);
            assert_eq!(credential.secret, "sk-from-keychain");
            let sent = claude_headers(&[], &credential);
            assert_eq!(sent, vec![(header.to_owned(), value.to_owned())]);
        }
        let item = KeychainItem {
            service: "dagq-claude".into(),
            account: "me".into(),
        };
        assert_eq!(
            inspect(dir.path(), None, Some(&*keychain)).unwrap(),
            (
                ClaudeCredentialKind::ApiKey,
                CredentialPlace::Keychain(item)
            )
        );
        let shown = report(dir.path(), None, Some(&*keychain));
        assert_eq!(
            shown,
            json!({
                "kind": "api_key",
                "place": "keychain",
                "file": null,
                "keychain_service": "dagq-claude",
                "keychain_account": "me",
                "present": true,
                "mode": null,
                "usable": true,
                "problem": null,
                "message": null,
            })
        );
        // Only the broker's read asks for the value.
        assert_eq!(
            keychain
                .values_asked
                .load(std::sync::atomic::Ordering::SeqCst),
            2
        );
        assert!(!shown.to_string().contains("sk-from-keychain"));
        let gate = ClaudeCredentialGate {
            queue_dir: dir.path().to_path_buf(),
            host_wide: None,
            keychain: Some(keychain),
            container: Ready,
        };
        assert_eq!(
            gate.availability(RuntimeTarget::Worker),
            Availability::Available
        );
    }

    #[test]
    fn a_keychain_item_that_is_missing_doubled_or_without_a_keychain_cannot_be_used() {
        let dir = tempfile::tempdir().unwrap();
        let keychain = FakeKeychain::holding(Some("sk-from-keychain"));
        let host = dir.path().join(HOST_FILE_NAME);
        let gate = |keychain: Option<Arc<FakeKeychain>>| ClaudeCredentialGate {
            queue_dir: dir.path().to_path_buf(),
            host_wide: None,
            keychain: keychain.map(|keychain| keychain as Arc<dyn KeychainLookup + Send + Sync>),
            container: Ready,
        };
        let unusable = |keychain: Option<&Arc<FakeKeychain>>, reason: &str| {
            let lookup = keychain.map(|keychain| &**keychain as &dyn KeychainLookup);
            let problem = read(dir.path(), None, lookup).unwrap_err();
            assert_eq!(problem.reason(), reason, "{problem:?}");
            assert!(!problem.message().contains("sk-from-keychain"));
            let shown = report(dir.path(), None, lookup);
            assert_eq!(shown["usable"], false);
            assert_eq!(shown["problem"], reason);
            assert!(!shown.to_string().contains("sk-from-keychain"));
            let Availability::Unavailable(why) =
                gate(keychain.cloned()).availability(RuntimeTarget::Worker)
            else {
                panic!("a worker's container was usable: {reason}");
            };
            assert_eq!(why, problem.message());
            shown
        };
        // The item is not there.
        keychain_toml(&host, "api_key", "someone-else");
        let shown = unusable(Some(&keychain), "missing");
        assert_eq!(shown["place"], "keychain");
        assert_eq!(shown["present"], false);
        // No keychain on this host (any OS but macOS).
        keychain_toml(&host, "api_key", "me");
        let shown = unusable(None, "keychain_unavailable");
        assert_eq!(shown["present"], false);
        assert!(shown["message"].as_str().unwrap().contains("macOS"));
        // An empty item: only the broker's read sees it, as the gate and
        // `doctor` never ask for the value.
        let empty = FakeKeychain::holding(Some(" "));
        assert_eq!(
            read(dir.path(), None, Some(&*empty)).unwrap_err().reason(),
            "empty"
        );
        // A keychain that cannot be read is no credential.
        let broken = Arc::new(FakeKeychain {
            problem: Some(ClaudeCredentialProblem::Unreadable("locked".into())),
            ..Arc::into_inner(FakeKeychain::holding(None)).unwrap()
        });
        unusable(Some(&broken), "unreadable");
        // Both a file and an item, half an item, or neither: no place.
        let key = credential_file(dir.path(), 0o600, "sk-file");
        for table in [
            format!(
                "kind = \"api_key\"\nfile = '{}'\nkeychain_service = \"dagq-claude\"\nkeychain_account = \"me\"\n",
                key.display()
            ),
            "kind = \"api_key\"\nkeychain_service = \"dagq-claude\"\n".to_owned(),
            "kind = \"api_key\"\nkeychain_account = \"me\"\n".to_owned(),
            "kind = \"setup_token\"\n".to_owned(),
        ] {
            fs::write(&host, format!("[{TABLE}]\n{table}")).unwrap();
            let shown = unusable(Some(&keychain), "invalid");
            assert_eq!(shown["place"], Value::Null);
            assert!(!shown.to_string().contains("sk-file"));
        }
        // No kind: not selected, whatever the place.
        fs::write(
            &host,
            format!("[{TABLE}]\nkeychain_service = \"dagq-claude\"\nkeychain_account = \"me\"\n"),
        )
        .unwrap();
        unusable(Some(&keychain), "not_selected");
    }
}
