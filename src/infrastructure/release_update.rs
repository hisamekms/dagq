//! The host's side of the release check (ADR-t618-1 decisions 2 and 3):
//! the `[update]` table of `host.toml` (the queue's `<queue dir>/host.toml`
//! replaces the host-wide `$XDG_CONFIG_HOME/dagq/host.toml`'s as a whole,
//! as `[push]` does; never `dagq.toml`), and `curl` reading crates.io's
//! sparse index.
//!
//! ```toml
//! [update]
//! release = "ask"          # "ask" | "auto" | "off"
//! check_interval_secs = 86400
//! ```
//!
//! A value of the wrong shape is taken as its default and reported as a
//! warning (`doctor`), so a mistake never turns the check off.

use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};

use anyhow::{Context, Result, bail};
use serde::Serialize;

use super::kpi_config::HOST_FILE_NAME;
use super::run_env::{parse_positive, parse_string, strip_comment};
use crate::application::release_update::{IndexFetch, ReleaseIndex};
use crate::domain::release_update::{INDEX_URL, ReleaseMode, ReleaseUpdateConfig};

/// The seconds `curl` may take in all (`--max-time`).
pub const CURL_MAX_TIME_SECS: u64 = 10;

/// The host's `[update]` with where it came from and what was wrong in it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct HostUpdate {
    #[serde(flatten)]
    pub config: ReleaseUpdateConfig,
    /// The file whose `[update]` holds; `None` when neither has one.
    pub source: Option<PathBuf>,
    /// The values taken as their default, and why.
    pub warnings: Vec<String>,
}

/// The `[update]` table of a `host.toml`'s text, `label` naming the file in
/// the warnings; `None` when it has none.
pub fn parse_host_update(text: &str, label: &str) -> Option<(ReleaseUpdateConfig, Vec<String>)> {
    let mut found: Option<(ReleaseUpdateConfig, Vec<String>)> = None;
    let mut in_update = false;
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    for (index, raw) in text.lines().enumerate() {
        let number = index + 1;
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(header) = line.strip_prefix('[') {
            let name = strip_comment(header).strip_suffix(']').map(str::trim);
            in_update = name == Some("update");
            if in_update {
                if found.is_some() {
                    // The first table holds.
                    if let Some((_, warnings)) = found.as_mut() {
                        warnings.push(format!("{label}:{number}: [update] is defined twice"));
                    }
                    in_update = false;
                } else {
                    found = Some((ReleaseUpdateConfig::default(), Vec::new()));
                }
            }
            continue;
        }
        if !in_update {
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
        match key {
            "release" => match parse_mode(value) {
                Ok(mode) => config.release = mode,
                Err(error) => warnings.push(format!(
                    "{at}: release: {error:#}; the default \"ask\" holds"
                )),
            },
            "check_interval_secs" => match parse_positive(value, "number of seconds") {
                Ok(secs) => config.check_interval_secs = secs.unsigned_abs(),
                Err(error) => warnings.push(format!(
                    "{at}: check_interval_secs: {error:#}; the default {} holds",
                    crate::domain::release_update::DEFAULT_CHECK_INTERVAL_SECS
                )),
            },
            _ => warnings.push(format!(
                "{at}: unknown key {key} in [update]; the keys are release, check_interval_secs"
            )),
        }
    }
    found
}

fn parse_mode(value: &str) -> Result<ReleaseMode> {
    let text = parse_string(value)?;
    match ReleaseMode::parse(&text) {
        Some(mode) => Ok(mode),
        None => bail!("expected \"ask\", \"auto\" or \"off\", not {text:?}"),
    }
}

/// The host's `[update]`: the queue's (`<queue_dir>/host.toml`), else the
/// host-wide file's, else the defaults. A file that cannot be read is a
/// warning, and the next one is read.
pub fn load_host_update(queue_dir: &Path, host_wide: Option<&Path>) -> HostUpdate {
    let mut warnings = Vec::new();
    for path in [Some(queue_dir.join(HOST_FILE_NAME).as_path()), host_wide]
        .into_iter()
        .flatten()
    {
        let text = match fs::read_to_string(path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                warnings.push(format!("read {}: {error}", path.display()));
                continue;
            }
        };
        if let Some((config, found)) = parse_host_update(&text, &path.display().to_string()) {
            warnings.extend(found);
            return HostUpdate {
                config,
                source: Some(path.to_path_buf()),
                warnings,
            };
        }
    }
    HostUpdate {
        warnings,
        ..HostUpdate::default()
    }
}

/// Reads the index with `curl -fsS --max-time 10`, the response's headers
/// on stdout before its body (`-D -`).
#[derive(Debug, Clone)]
pub struct CurlIndex {
    pub program: PathBuf,
    pub url: String,
}

impl Default for CurlIndex {
    fn default() -> Self {
        Self {
            program: PathBuf::from("curl"),
            url: INDEX_URL.to_owned(),
        }
    }
}

impl ReleaseIndex for CurlIndex {
    fn fetch(&self, etag: Option<&str>) -> Result<IndexFetch> {
        let mut command = Command::new(&self.program);
        command
            .args(["-fsS", "--max-time", &CURL_MAX_TIME_SECS.to_string()])
            .args(["-D", "-"]);
        if let Some(etag) = etag {
            command.arg("-H").arg(format!("If-None-Match: {etag}"));
        }
        command.arg(&self.url);
        let output = command
            .output()
            .with_context(|| format!("run {}", self.program.display()))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!(
                "{} exited with {}: {}",
                self.program.display(),
                output.status,
                stderr.trim()
            );
        }
        parse_response(&String::from_utf8_lossy(&output.stdout))
    }
}

/// The status, `ETag` and body of `curl -D -`'s output: header blocks (a
/// `1xx` one before the final) and then the body.
fn parse_response(output: &str) -> Result<IndexFetch> {
    let mut rest = output;
    loop {
        let (head, body) = rest
            .split_once("\r\n\r\n")
            .or_else(|| rest.split_once("\n\n"))
            .unwrap_or((rest, ""));
        let mut lines = head.lines();
        let status_line = lines.next().unwrap_or_default();
        let code: u16 = status_line
            .split_whitespace()
            .nth(1)
            .and_then(|code| code.parse().ok())
            .with_context(|| format!("no HTTP status in curl's output: {status_line:?}"))?;
        // A `1xx`, or a proxy's `200 Connection established`, is followed
        // by another block: the last one is the response.
        if (100..200).contains(&code) || body.starts_with("HTTP/") {
            rest = body;
            continue;
        }
        if code == 304 {
            return Ok(IndexFetch::NotModified);
        }
        if !(200..300).contains(&code) {
            bail!("the index answered HTTP {code}");
        }
        let etag = lines.find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.trim()
                .eq_ignore_ascii_case("etag")
                .then(|| value.trim().to_owned())
        });
        return Ok(IndexFetch::Fetched {
            body: body.to_owned(),
            etag,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn the_update_table_is_read_and_a_wrong_value_keeps_its_default() {
        let text = "[push]\ncommand = []\n\n[update]\nrelease = \"off\" # quiet\ncheck_interval_secs = 3600\n";
        let (config, warnings) = parse_host_update(text, "h").unwrap();
        assert_eq!(config.release, ReleaseMode::Off);
        assert_eq!(config.check_interval_secs, 3600);
        assert!(warnings.is_empty(), "{warnings:?}");

        let text = "[update]\nrelease = \"never\"\ncheck_interval_secs = -1\ncolour = 1\nbroken\n[update]\n";
        let (config, warnings) = parse_host_update(text, "h").unwrap();
        assert_eq!(config, ReleaseUpdateConfig::default());
        assert_eq!(warnings.len(), 5, "{warnings:?}");
        assert!(warnings[0].starts_with("h:2: release"), "{warnings:?}");
        assert!(warnings[1].starts_with("h:3: check_interval_secs"));
        assert!(warnings[2].contains("unknown key colour"));
        assert!(warnings[3].contains("KEY = value"));
        assert!(warnings[4].contains("defined twice"));

        assert!(parse_host_update("[push]\ncommand = []\n", "h").is_none());
    }

    #[test]
    fn the_queue_table_wins_as_a_whole() {
        let dir = tempfile::tempdir().unwrap();
        let wide = dir.path().join("wide.toml");
        fs::write(
            &wide,
            "[update]\nrelease = \"off\"\ncheck_interval_secs = 60\n",
        )
        .unwrap();
        let loaded = load_host_update(dir.path(), Some(&wide));
        assert_eq!(loaded.config.release, ReleaseMode::Off);
        assert_eq!(loaded.source.as_deref(), Some(wide.as_path()));

        fs::write(
            dir.path().join(HOST_FILE_NAME),
            "[update]\nrelease = \"auto\"\n",
        )
        .unwrap();
        let loaded = load_host_update(dir.path(), Some(&wide));
        assert_eq!(loaded.config.release, ReleaseMode::Auto);
        // Not merged with the host-wide table.
        assert_eq!(loaded.config.check_interval_secs, 86_400);

        let none = load_host_update(&dir.path().join("none"), None);
        assert_eq!(none, HostUpdate::default());

        // A directory in the file's place is a warning, then the next file.
        let odd = tempfile::tempdir().unwrap();
        fs::create_dir(odd.path().join(HOST_FILE_NAME)).unwrap();
        let loaded = load_host_update(odd.path(), Some(&wide));
        assert_eq!(loaded.config.release, ReleaseMode::Off);
        assert_eq!(loaded.warnings.len(), 1, "{:?}", loaded.warnings);
    }

    #[test]
    fn a_response_gives_its_body_and_etag_or_not_modified() {
        let ok = "HTTP/1.1 100 Continue\r\n\r\nHTTP/2 200\r\ncontent-type: text/plain\r\nETag: \"abc\"\r\n\r\n{\"vers\":\"0.3.0\"}\n";
        assert_eq!(
            parse_response(ok).unwrap(),
            IndexFetch::Fetched {
                body: "{\"vers\":\"0.3.0\"}\n".to_owned(),
                etag: Some("\"abc\"".to_owned()),
            }
        );
        assert_eq!(
            parse_response("HTTP/2 304\r\netag: \"abc\"\r\n\r\n").unwrap(),
            IndexFetch::NotModified
        );
        let proxied = "HTTP/1.1 200 Connection established\r\n\r\nHTTP/2 304\r\n\r\n";
        assert_eq!(parse_response(proxied).unwrap(), IndexFetch::NotModified);
        assert!(parse_response("HTTP/2 302\r\n\r\n").is_err());
        assert!(parse_response("garbage").is_err());
    }

    #[test]
    fn curl_is_run_with_the_etag_and_its_failure_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let args = dir.path().join("args");
        let curl = dir.path().join("curl");
        fs::write(
            &curl,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'\ncase \"$*\" in *If-None-Match*) printf 'HTTP/2 304\\r\\n\\r\\n';; *) printf 'HTTP/2 200\\r\\netag: \"e1\"\\r\\n\\r\\n{{\"vers\":\"0.3.0\"}}\\n';; esac\n",
                args.display()
            ),
        )
        .unwrap();
        fs::set_permissions(&curl, fs::Permissions::from_mode(0o755)).unwrap();
        let index = CurlIndex {
            program: curl.clone(),
            url: "https://example.invalid/dagq".to_owned(),
        };
        assert_eq!(
            index.fetch(None).unwrap(),
            IndexFetch::Fetched {
                body: "{\"vers\":\"0.3.0\"}\n".to_owned(),
                etag: Some("\"e1\"".to_owned()),
            }
        );
        let sent = fs::read_to_string(&args).unwrap();
        assert_eq!(
            sent.lines().collect::<Vec<_>>(),
            [
                "-fsS",
                "--max-time",
                "10",
                "-D",
                "-",
                "https://example.invalid/dagq"
            ]
        );
        assert_eq!(
            index.fetch(Some("\"e1\"")).unwrap(),
            IndexFetch::NotModified
        );
        assert!(
            fs::read_to_string(&args)
                .unwrap()
                .contains("If-None-Match: \"e1\"")
        );

        fs::write(
            &curl,
            "#!/bin/sh\necho 'curl: (28) timed out' >&2\nexit 28\n",
        )
        .unwrap();
        let error = index.fetch(None).unwrap_err().to_string();
        assert!(error.contains("timed out"), "{error}");

        let missing = CurlIndex {
            program: dir.path().join("no-curl"),
            ..index
        };
        assert!(missing.fetch(None).is_err());
        assert_eq!(CurlIndex::default().url, INDEX_URL);
    }
}
