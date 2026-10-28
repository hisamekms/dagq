//! The limits of the queue broker on this host ([`BrokerLimits`]): the
//! `[queue_broker]` table of `host.toml`, never of `dagq.toml` (they size
//! what one host process may hold for every run of the host, as the
//! container's memory, cpu and pids are, ADR-t2257-1 decision 6). The
//! queue's `<queue dir>/host.toml` replaces the host-wide
//! `$XDG_CONFIG_HOME/dagq/host.toml`'s table as a whole, and a key the
//! table leaves out keeps the runtime's default; neither file is in the
//! repository.
//!
//! ```toml
//! [queue_broker]
//! unchecked_bytes = 65536         # a positive whole number of bytes
//! max_waiting = 512               # a positive whole number
//! max_connections_per_run = 64    # a positive whole number
//! max_connections = 256           # a positive whole number, more
//!                                 # than max_connections_per_run
//! ```
//!
//! What each key bounds, its default and why are the doc comments of
//! [`BrokerLimits`]. A key it does not know, a value that is not a
//! positive whole number, a key or the table set twice is an error naming
//! the file and line, and the broker does not start on it.

use std::fs;
use std::path::Path;

use anyhow::{Context, Result, bail, ensure};

use super::kpi_config::HOST_FILE_NAME;
use super::run_env::{parse_positive, strip_comment};
use crate::domain::queue_broker::BrokerLimits;

/// The most `unchecked_bytes` may be: a head longer than the broker's
/// own limits of its lines and headers never reads anyway.
pub const MAX_UNCHECKED_BYTES: usize = 1024 * 1024;

/// The table of `host.toml` that sets the broker's limits.
pub const TABLE: &str = "queue_broker";

/// The `[queue_broker]` table of a `host.toml`'s text over the defaults,
/// `label` naming the file in errors; `None` when it has none.
pub fn parse_host_broker_limits(text: &str, label: &str) -> Result<Option<BrokerLimits>> {
    let mut limits: Option<BrokerLimits> = None;
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
                    limits.is_none(),
                    "{label}:{number}: [{TABLE}] is defined twice"
                );
                limits = Some(BrokerLimits::default());
            }
            continue;
        }
        let Some(limits) = limits.as_mut().filter(|_| inside) else {
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
        let slot = match key {
            "unchecked_bytes" => &mut limits.unchecked_bytes,
            "max_waiting" => &mut limits.max_waiting,
            "max_connections_per_run" => &mut limits.max_connections_per_run,
            "max_connections" => &mut limits.max_connections,
            _ => bail!(
                "{label}:{number}: unknown key {key} in [{TABLE}]; the keys are \
                 unchecked_bytes, max_waiting, max_connections_per_run, max_connections"
            ),
        };
        let value = parse_positive(rest.trim(), "number")
            .with_context(|| format!("{label}:{number}: [{TABLE}] {key}"))?;
        *slot = usize::try_from(value)
            .ok()
            .filter(|value| key != "unchecked_bytes" || *value <= MAX_UNCHECKED_BYTES)
            .with_context(|| format!("{label}:{number}: [{TABLE}] {key}: {value} is too large"))?;
    }
    // One run alone never fills the broker: another run has a slot left.
    if let Some(limits) = &limits {
        ensure!(
            limits.max_connections_per_run < limits.max_connections,
            "{label}: [{TABLE}] max_connections_per_run ({}) is not less than max_connections ({})",
            limits.max_connections_per_run,
            limits.max_connections
        );
    }
    Ok(limits)
}

/// The broker's limits on this host: the queue's `host.toml`'s table,
/// else the host-wide file's, else the defaults.
pub fn load(queue_dir: &Path, host_wide: Option<&Path>) -> Result<BrokerLimits> {
    for path in [Some(queue_dir.join(HOST_FILE_NAME).as_path()), host_wide]
        .into_iter()
        .flatten()
    {
        let text = match fs::read_to_string(path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error).with_context(|| format!("read {}", path.display())),
        };
        if let Some(limits) = parse_host_broker_limits(&text, &path.display().to_string())? {
            return Ok(limits);
        }
    }
    Ok(BrokerLimits::default())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_table_sets_its_keys_over_the_defaults() {
        let defaults = BrokerLimits::default();
        assert_eq!(
            parse_host_broker_limits("[push]\nx = 1\n", "h").unwrap(),
            None
        );
        assert_eq!(
            parse_host_broker_limits("[queue_broker]\n", "h").unwrap(),
            Some(defaults)
        );
        assert_eq!(
            parse_host_broker_limits(
                "[queue_broker] # mine\nmax_connections_per_run = 8 # few\n[other]\nmax_connections_per_run = 9\n",
                "h"
            )
            .unwrap(),
            Some(BrokerLimits {
                max_connections_per_run: 8,
                ..defaults
            })
        );
        assert_eq!(
            parse_host_broker_limits(
                "[queue_broker]\nunchecked_bytes = 1_024\nmax_waiting = 3\nmax_connections_per_run = 2\nmax_connections = 5\n",
                "h"
            )
            .unwrap(),
            Some(BrokerLimits {
                unchecked_bytes: 1024,
                max_waiting: 3,
                max_connections_per_run: 2,
                max_connections: 5,
            })
        );
    }

    #[test]
    fn a_wrong_value_or_key_is_refused_where_it_is() {
        for (text, said) in [
            ("[queue_broker]\nmax_connections_per_run = 0\n", "h:2:"),
            ("[queue_broker]\nmax_connections_per_run = -1\n", "positive"),
            (
                "[queue_broker]\nmax_connections_per_run = \"8\"\n",
                "whole number",
            ),
            ("[queue_broker]\nunchecked_bytes = 1.5\n", "h:2:"),
            ("[queue_broker]\nunchecked_bytes = 1048577\n", "too large"),
            ("[queue_broker]\nunchecked_bytes =\n", "missing value"),
            (
                "[queue_broker]\nmax_threads = 8\n",
                "unknown key max_threads",
            ),
            ("[queue_broker]\nmax_connections_per_run\n", "KEY = value"),
            (
                "[queue_broker]\nmax_connections_per_run = 1\nmax_connections_per_run = 2\n",
                "h:3: max_connections_per_run is defined twice",
            ),
            (
                "[queue_broker]\n[queue_broker]\n",
                "h:2: [queue_broker] is defined twice",
            ),
            ("[queue_broker\n", "unclosed"),
            ("[queue_broker]\nmax_connections = 0\n", "h:2:"),
            (
                "[queue_broker]\nmax_connections = 8\nmax_connections_per_run = 9\n",
                "h: [queue_broker] max_connections_per_run (9) is not less than max_connections (8)",
            ),
            // One run alone would fill the broker.
            (
                "[queue_broker]\nmax_connections = 1\nmax_connections_per_run = 1\n",
                "h: [queue_broker] max_connections_per_run (1) is not less than max_connections (1)",
            ),
            // The default of the left-out key counts too.
            (
                "[queue_broker]\nmax_connections = 10\n",
                "max_connections_per_run (64) is not less than max_connections (10)",
            ),
            (
                "[queue_broker]\nmax_connections = 64\n",
                "max_connections_per_run (64) is not less than max_connections (64)",
            ),
            (
                "[queue_broker]\nmax_connections_per_run = 256\n",
                "max_connections_per_run (256) is not less than max_connections (256)",
            ),
        ] {
            let error = parse_host_broker_limits(text, "h").unwrap_err();
            assert!(format!("{error:#}").contains(said), "{text:?}: {error:#}");
        }
    }

    #[test]
    fn the_queue_s_table_replaces_the_host_wide_one_and_none_is_the_default() {
        let queue = tempfile::tempdir().unwrap();
        let host = tempfile::tempdir().unwrap();
        let host_wide = host.path().join(HOST_FILE_NAME);
        assert_eq!(
            load(queue.path(), Some(&host_wide)).unwrap(),
            BrokerLimits::default()
        );
        fs::write(
            &host_wide,
            "[queue_broker]\nmax_connections_per_run = 3\nunchecked_bytes = 10\nmax_waiting = 4\nmax_connections = 6\n",
        )
        .unwrap();
        assert_eq!(
            load(queue.path(), Some(&host_wide)).unwrap(),
            BrokerLimits {
                unchecked_bytes: 10,
                max_waiting: 4,
                max_connections_per_run: 3,
                max_connections: 6,
            }
        );
        // A queue's file without the table leaves the host-wide one.
        fs::write(queue.path().join(HOST_FILE_NAME), "[push]\n").unwrap();
        assert_eq!(
            load(queue.path(), Some(&host_wide))
                .unwrap()
                .max_connections_per_run,
            3
        );
        // A queue's table replaces it whole: its left-out key is the default.
        fs::write(
            queue.path().join(HOST_FILE_NAME),
            "[queue_broker]\nmax_connections_per_run = 5\n",
        )
        .unwrap();
        assert_eq!(
            load(queue.path(), Some(&host_wide)).unwrap(),
            BrokerLimits {
                max_connections_per_run: 5,
                ..BrokerLimits::default()
            }
        );
        // A wrong value names its file.
        fs::write(&host_wide, "[queue_broker]\nmax_connections_per_run = 0\n").unwrap();
        let error = load(queue.path().join("none").as_path(), Some(&host_wide)).unwrap_err();
        assert!(
            format!("{error:#}").contains(&format!("{}:2:", host_wide.display())),
            "{error:#}"
        );
    }
}
