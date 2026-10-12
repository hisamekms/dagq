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
//!
//! Only the broker reads the file's content ([`read`]); the gate of the
//! container runtime ([`ClaudeCredentialGate`]) and `doctor` ([`report`])
//! look at its kind, presence and mode only, and nothing writes its value.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

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
            _ => bail!("{label}:{number}: unknown key {key} in [{TABLE}]; the keys are kind, file"),
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

/// The chosen kind and its file, looked at without reading the content: a
/// regular file of mode 0600 that is not empty; why not.
pub fn inspect(
    queue_dir: &Path,
    host_wide: Option<&Path>,
) -> Result<(ClaudeCredentialKind, PathBuf), ClaudeCredentialProblem> {
    let config = load(queue_dir, host_wide)
        .map_err(|error| ClaudeCredentialProblem::Invalid(format!("{error:#}")))?
        .unwrap_or_default();
    let Some(kind) = config.kind else {
        return Err(ClaudeCredentialProblem::NotSelected);
    };
    let file = config.file.ok_or_else(|| {
        ClaudeCredentialProblem::Invalid(format!(
            "[{TABLE}] chooses kind = {:?} but names no file",
            kind.as_str()
        ))
    })?;
    let shown = file.display().to_string();
    let metadata = match fs::metadata(&file) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(ClaudeCredentialProblem::Missing(shown));
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
        return Err(ClaudeCredentialProblem::Empty(shown));
    }
    Ok((kind, file))
}

/// The chosen credential with its value: the queue broker's only.
pub fn read(
    queue_dir: &Path,
    host_wide: Option<&Path>,
) -> Result<ClaudeCredential, ClaudeCredentialProblem> {
    let (kind, file) = inspect(queue_dir, host_wide)?;
    let shown = file.display().to_string();
    let text = fs::read_to_string(&file)
        .map_err(|error| ClaudeCredentialProblem::Unreadable(format!("{shown}: {error}")))?;
    let secret = text.trim();
    if secret.is_empty() {
        return Err(ClaudeCredentialProblem::Empty(shown));
    }
    Ok(ClaudeCredential {
        kind,
        secret: secret.to_owned(),
    })
}

/// `doctor`'s `claude_credential`: the chosen `kind` (null when none is),
/// the `file` and whether it is `present`, its `mode`, whether the broker
/// can send with it (`usable`), and else the `problem` and the `message`
/// (with the guidance on which to choose when none is). Never the value.
pub fn report(queue_dir: &Path, host_wide: Option<&Path>) -> Value {
    let config = load(queue_dir, host_wide)
        .ok()
        .flatten()
        .unwrap_or_default();
    let mode = config
        .file
        .as_deref()
        .and_then(|file| fs::metadata(file).ok())
        .map(|metadata| format!("{:04o}", metadata.permissions().mode() & 0o7777));
    let checked = inspect(queue_dir, host_wide);
    json!({
        "kind": config.kind.map(ClaudeCredentialKind::as_str),
        "file": config.file.as_deref().map(Path::display).map(|file| file.to_string()),
        "present": mode.is_some(),
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
    pub container: C,
}

impl<C> ClaudeCredentialGate<C> {
    /// The gate of the queue in `queue_dir`, with this environment's
    /// host-wide `host.toml`.
    pub fn of_queue(queue_dir: &Path, container: C) -> Self {
        Self {
            queue_dir: queue_dir.to_path_buf(),
            host_wide: super::kpi_config::host_wide_file(),
            container,
        }
    }
}

impl<C: ContainerRuntime> ContainerRuntime for ClaudeCredentialGate<C> {
    fn availability(&self, target: RuntimeTarget) -> Availability {
        if target == RuntimeTarget::Worker
            && let Err(problem) = inspect(&self.queue_dir, self.host_wide.as_deref())
        {
            return Availability::Unavailable(problem.message());
        }
        self.container.availability(target)
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
        let none = inspect(&queue_dir, Some(&wide)).unwrap_err();
        assert_eq!(none, ClaudeCredentialProblem::NotSelected);
        let shown = report(&queue_dir, Some(&wide));
        assert_eq!(shown["kind"], Value::Null);
        assert_eq!(shown["usable"], false);
        assert_eq!(shown["problem"], "not_selected");
        assert!(shown["message"].as_str().unwrap().contains("api_key"));

        // The host-wide file's table holds without the queue's.
        host_toml(&wide, "setup_token", &key);
        let chosen = read(&queue_dir, Some(&wide)).unwrap();
        assert_eq!(chosen.kind, ClaudeCredentialKind::SetupToken);
        assert_eq!(chosen.secret, "sk-secret");
        // The queue's replaces it.
        host_toml(&queue_dir.join(HOST_FILE_NAME), "api_key", &key);
        assert_eq!(
            inspect(&queue_dir, Some(&wide)).unwrap(),
            (ClaudeCredentialKind::ApiKey, key.clone())
        );
        let shown = report(&queue_dir, Some(&wide));
        assert_eq!(
            shown,
            json!({
                "kind": "api_key",
                "file": key.display().to_string(),
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
            inspect(&queue_dir, None).unwrap_err(),
            ClaudeCredentialProblem::NotPrivate {
                file: key.display().to_string(),
                mode: 0o644
            }
        );
        let shown = report(&queue_dir, None);
        assert_eq!(shown["mode"], "0644");
        assert_eq!(shown["problem"], "not_private");
        assert!(!shown.to_string().contains("sk-secret"));
        credential_file(dir.path(), 0o600, "");
        assert_eq!(inspect(&queue_dir, None).unwrap_err().reason(), "empty");
        credential_file(dir.path(), 0o600, " \n");
        assert_eq!(read(&queue_dir, None).unwrap_err().reason(), "empty");
        fs::remove_file(&key).unwrap();
        assert_eq!(
            inspect(&queue_dir, None).unwrap_err(),
            ClaudeCredentialProblem::Missing(key.display().to_string())
        );
        assert_eq!(report(&queue_dir, None)["present"], false);
        // A directory is no credential file.
        fs::create_dir(&key).unwrap();
        fs::set_permissions(&key, fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(
            inspect(&queue_dir, None).unwrap_err().reason(),
            "not_private"
        );
        // A table that chooses no file, or does not read.
        fs::write(
            queue_dir.join(HOST_FILE_NAME),
            "[claude_credential]\nkind = \"api_key\"\n",
        )
        .unwrap();
        assert_eq!(inspect(&queue_dir, None).unwrap_err().reason(), "invalid");
        fs::write(
            queue_dir.join(HOST_FILE_NAME),
            "[claude_credential]\nkind = 1\n",
        )
        .unwrap();
        assert_eq!(inspect(&queue_dir, None).unwrap_err().reason(), "invalid");
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
}
