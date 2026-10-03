//! The host's side of the resource broker's settings (ADR-t827-4 decision
//! 4): the `[broker]` table of `host.toml` (the queue's `<queue dir>/host.toml`
//! replaces the host-wide `$XDG_CONFIG_HOME/dagq/host.toml`'s as a whole,
//! as `[update]` and `[push]` do; never `dagq.toml`).
//!
//! ```toml
//! [broker]
//! mode = "disabled"            # the only mode a host sets: it lowers, never raises
//! podman = "/opt/homebrew/bin/podman"
//! machine_cpus = 1
//! machine_memory_mib = 1024
//! machine_disk_gib = 10
//! container_memory = "512m"
//! container_cpus = "1"
//! container_pids = 256
//! port = 0                     # 0 picks a free port
//! ```
//!
//! A value of the wrong shape keeps its default and is reported as a
//! warning, so a mistake in it never turns the broker on.

use std::{
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Result, bail};
use serde::Serialize;

use super::kpi_config::HOST_FILE_NAME;
use super::run_env::{parse_positive, parse_string, strip_comment};
use crate::domain::broker::{BrokerMode, HostBroker};

/// The host's `[broker]` with where it came from and what was wrong in it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct LoadedHostBroker {
    #[serde(flatten)]
    pub config: HostBroker,
    /// The file whose `[broker]` holds; `None` when neither has one.
    pub source: Option<PathBuf>,
    /// The values that kept their default, and why.
    pub warnings: Vec<String>,
}

/// The `[broker]` table of a `host.toml`'s text, `label` naming the file
/// in the warnings; `None` when it has none.
pub fn parse_host_broker(text: &str, label: &str) -> Option<(HostBroker, Vec<String>)> {
    let mut found: Option<(HostBroker, Vec<String>)> = None;
    let mut inside = false;
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    for (index, raw) in text.lines().enumerate() {
        let number = index + 1;
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(header) = line.strip_prefix('[') {
            let name = strip_comment(header).strip_suffix(']').map(str::trim);
            inside = name == Some("broker");
            if inside {
                if let Some((_, warnings)) = found.as_mut() {
                    // The first table holds.
                    warnings.push(format!("{label}:{number}: [broker] is defined twice"));
                    inside = false;
                } else {
                    found = Some((HostBroker::default(), Vec::new()));
                }
            }
            continue;
        }
        if !inside {
            continue;
        }
        let Some((config, warnings)) = found.as_mut() else {
            continue;
        };
        let at = format!("{label}:{number}");
        let Some((key, value)) = line.split_once('=') else {
            warnings.push(format!("{at}: expected KEY = value"));
            continue;
        };
        let (key, value) = (key.trim(), value.trim());
        if let Err(error) = set(config, key, value) {
            warnings.push(format!("{at}: {key}: {error:#}; the default holds"));
        }
    }
    found
}

fn set(config: &mut HostBroker, key: &str, value: &str) -> Result<()> {
    let whole =
        |value: &str| -> Result<u32> { Ok(u32::try_from(parse_positive(value, "number")?)?) };
    match key {
        "mode" => {
            let text = parse_string(value)?;
            match BrokerMode::parse(&text) {
                Some(BrokerMode::Disabled) => config.disabled = true,
                Some(mode) => bail!(
                    "host.toml can only lower the mode to \"disabled\", not set it to {:?}: the mode is dagq.toml's",
                    mode.as_str()
                ),
                None => bail!("expected \"disabled\", not {text:?}"),
            }
        }
        "podman" => config.podman = Some(parse_string(value)?),
        "machine_cpus" => config.machine_cpus = Some(whole(value)?),
        "machine_memory_mib" => config.machine_memory_mib = Some(whole(value)?),
        "machine_disk_gib" => config.machine_disk_gib = Some(whole(value)?),
        "container_memory" => config.container_memory = Some(parse_string(value)?),
        "container_cpus" => config.container_cpus = Some(parse_string(value)?),
        "container_pids" => config.container_pids = Some(whole(value)?),
        "port" => {
            let digits = strip_comment(value);
            config.port =
                Some(digits.parse::<u16>().map_err(|_| {
                    anyhow::anyhow!("expected a port from 0 to 65535, not {digits}")
                })?);
        }
        _ => bail!(
            "unknown key in [broker]; the keys are {}",
            HostBroker::KEYS.join(", ")
        ),
    }
    Ok(())
}

/// The host's `[broker]`: the queue's (`<queue_dir>/host.toml`), else the
/// host-wide file's, else the defaults. A file that cannot be read is a
/// warning, and the next one is read.
pub fn load_host_broker(queue_dir: &Path, host_wide: Option<&Path>) -> LoadedHostBroker {
    load_host_broker_files(
        [Some(queue_dir.join(HOST_FILE_NAME).as_path()), host_wide]
            .into_iter()
            .flatten(),
    )
}

/// Read the first broker table in the supplied files, retaining the same
/// warning and fallback rules for callers without a queue (the e2e gate).
pub(crate) fn load_host_broker_files<'a>(
    paths: impl IntoIterator<Item = &'a Path>,
) -> LoadedHostBroker {
    let mut warnings = Vec::new();
    for path in paths {
        let text = match fs::read_to_string(path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                warnings.push(format!("read {}: {error}", path.display()));
                continue;
            }
        };
        if let Some((config, found)) = parse_host_broker(&text, &path.display().to_string()) {
            warnings.extend(found);
            return LoadedHostBroker {
                config,
                source: Some(path.to_path_buf()),
                warnings,
            };
        }
    }
    LoadedHostBroker {
        warnings,
        ..LoadedHostBroker::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_broker_table_is_read_and_a_wrong_value_keeps_its_default() {
        let text = "[update]\nrelease = \"off\"\n\n[broker]\nmode = \"disabled\" # not here\npodman = \"/opt/homebrew/bin/podman\"\nmachine_cpus = 2\nmachine_memory_mib = 2048\nmachine_disk_gib = 20\ncontainer_memory = \"1g\"\ncontainer_cpus = \"2\"\ncontainer_pids = 512\nport = 18750\n";
        let (config, warnings) = parse_host_broker(text, "h").unwrap();
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(
            config,
            HostBroker {
                disabled: true,
                podman: Some("/opt/homebrew/bin/podman".into()),
                machine_cpus: Some(2),
                machine_memory_mib: Some(2048),
                machine_disk_gib: Some(20),
                container_memory: Some("1g".into()),
                container_cpus: Some("2".into()),
                container_pids: Some(512),
                port: Some(18750),
            }
        );
        assert!(parse_host_broker("[update]\nrelease = \"off\"\n", "h").is_none());
    }

    #[test]
    fn the_host_cannot_raise_the_mode() {
        for mode in ["preferred", "required"] {
            let (config, warnings) =
                parse_host_broker(&format!("[broker]\nmode = \"{mode}\"\n"), "h").unwrap();
            assert!(!config.disabled);
            assert_eq!(config, HostBroker::default());
            assert!(
                warnings[0].starts_with("h:2: mode: host.toml can only lower the mode"),
                "{warnings:?}"
            );
        }
        let (config, warnings) = parse_host_broker(
            "[broker]\nmode = \"off\"\nmachine_cpus = 0\nport = 70000\ncolour = 1\nbroken\n[broker]\n",
            "h",
        )
        .unwrap();
        assert_eq!(config, HostBroker::default());
        assert_eq!(warnings.len(), 6, "{warnings:?}");
        assert!(warnings[3].contains("unknown key"), "{warnings:?}");
        assert!(warnings[5].contains("defined twice"), "{warnings:?}");
    }

    #[test]
    fn the_queue_table_wins_as_a_whole() {
        let dir = tempfile::tempdir().unwrap();
        let wide = dir.path().join("wide.toml");
        fs::write(&wide, "[broker]\nmode = \"disabled\"\nmachine_cpus = 4\n").unwrap();
        let loaded = load_host_broker(dir.path(), Some(&wide));
        assert!(loaded.config.disabled);
        assert_eq!(loaded.source.as_deref(), Some(wide.as_path()));
        fs::write(dir.path().join(HOST_FILE_NAME), "[broker]\nport = 0\n").unwrap();
        let loaded = load_host_broker(dir.path(), Some(&wide));
        // Not merged with the host-wide table.
        assert!(!loaded.config.disabled);
        assert_eq!(loaded.config.machine_cpus, None);
        assert_eq!(loaded.config.port, Some(0));
        assert_eq!(
            load_host_broker(&dir.path().join("none"), None),
            LoadedHostBroker::default()
        );
    }
}
