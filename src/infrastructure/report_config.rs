//! The `[report]` table of the host's `host.toml` (ADR-0051 decision 20):
//! how many daily and weekly KPI reports are kept. Read from the host-wide
//! `$XDG_CONFIG_HOME/dagq/host.toml` and the queue's `<queue dir>/host.toml`,
//! the queue's winning key by key; the other tables are skipped.
//!
//! ```toml
//! [report]
//! keep_daily_days = 90
//! keep_weekly_weeks = 104
//! ```
use anyhow::{Context, Result, bail, ensure};
use std::{fs, path::Path};

use super::kpi_config::HOST_FILE_NAME;
use super::run_env::{parse_positive, strip_comment};
use crate::domain::kpi::report::Keep;

/// What one file's `[report]` sets; a key it does not set is `None`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ReportSettings {
    pub keep_daily_days: Option<usize>,
    pub keep_weekly_weeks: Option<usize>,
}

/// The `[report]` table of a `host.toml`'s text, `label` naming the file in
/// errors.
pub fn parse_host_report(text: &str, label: &str) -> Result<ReportSettings> {
    let mut settings = ReportSettings::default();
    let mut in_report = false;
    let mut seen = false;
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
            in_report = name == "report";
            ensure!(
                !(in_report && seen),
                "{label}:{number}: [report] is defined twice"
            );
            seen |= in_report;
            continue;
        }
        if !in_report {
            continue;
        }
        let (key, rest) = line
            .split_once('=')
            .with_context(|| format!("{label}:{number}: expected KEY = value"))?;
        let key = key.trim();
        let slot = match key {
            "keep_daily_days" => &mut settings.keep_daily_days,
            "keep_weekly_weeks" => &mut settings.keep_weekly_weeks,
            _ => bail!(
                "{label}:{number}: unknown key {key} in [report]; the keys are keep_daily_days, keep_weekly_weeks"
            ),
        };
        ensure!(slot.is_none(), "{label}:{number}: {key} is defined twice");
        let value = parse_positive(rest.trim(), "number")
            .with_context(|| format!("{label}:{number}: value of {key}"))?;
        *slot = Some(usize::try_from(value).context("too large")?);
    }
    Ok(settings)
}

/// How many reports the host keeps: the defaults, under the host-wide
/// file's `[report]`, under the queue's.
pub fn load_host_report(queue_dir: &Path, host_wide: Option<&Path>) -> Result<Keep> {
    let mut keep = Keep::default();
    for path in host_wide
        .into_iter()
        .chain([queue_dir.join(HOST_FILE_NAME).as_path()])
    {
        let text = match fs::read_to_string(path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error).with_context(|| format!("read {}", path.display())),
        };
        let settings = parse_host_report(&text, &path.display().to_string())?;
        keep.daily_days = settings.keep_daily_days.unwrap_or(keep.daily_days);
        keep.weekly_weeks = settings.keep_weekly_weeks.unwrap_or(keep.weekly_weeks);
    }
    Ok(keep)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_report_and_skips_other_tables() {
        let text = "\u{feff}[push]\ncommand = [\"x\"]\n[report] # kept\nkeep_daily_days = 30 # a month\n[kpi]\nkeep_daily_days = oops\n";
        let settings = parse_host_report(text, "h").unwrap();
        assert_eq!(settings.keep_daily_days, Some(30));
        assert_eq!(settings.keep_weekly_weeks, None);
        assert_eq!(
            parse_host_report("", "h").unwrap(),
            ReportSettings::default()
        );
    }

    #[test]
    fn refuses_what_the_table_does_not_support() {
        let error = |text: &str| format!("{:#}", parse_host_report(text, "h").unwrap_err());
        assert!(error("[report]\nkeep = 1\n").contains("unknown key keep"));
        assert!(error("[report]\nkeep_daily_days = 0\n").contains("positive"));
        assert!(error("[report]\nkeep_daily_days = 1\nkeep_daily_days = 2\n").contains("twice"));
        assert!(error("[report]\n[report]\n").contains("defined twice"));
        assert!(error("[report]\nkeep_daily_days\n").contains("expected KEY = value"));
        assert!(error("[report\n").contains("unclosed"));
    }

    #[test]
    fn the_queue_file_wins_over_the_host_wide_one() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(load_host_report(dir.path(), None).unwrap(), Keep::default());
        let wide = dir.path().join("wide.toml");
        fs::write(
            &wide,
            "[report]\nkeep_daily_days = 10\nkeep_weekly_weeks = 5\n",
        )
        .unwrap();
        fs::write(
            dir.path().join(HOST_FILE_NAME),
            "[report]\nkeep_daily_days = 3\n",
        )
        .unwrap();
        let keep = load_host_report(dir.path(), Some(&wide)).unwrap();
        assert_eq!(
            keep,
            Keep {
                daily_days: 3,
                weekly_weeks: 5
            }
        );
        fs::write(
            dir.path().join(HOST_FILE_NAME),
            "[report]\nkeep_daily_days = x\n",
        )
        .unwrap();
        assert!(load_host_report(dir.path(), None).is_err());
    }
}
