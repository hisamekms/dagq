//! A supervisor's look at crates.io for a new dagq release (ADR-t618-1
//! decisions 1 to 3): only a release build looks, as the host's `[update]`
//! says, at most once per `check_interval_secs` for the whole queue, and
//! writes what it found as `release_checked` or `release_check_failed`.
//! Neither is an attention. The index is read through [`ReleaseIndex`]
//! (`curl` on the host, a stub in tests).

use anyhow::Result;
use serde_json::{Value, json};

use super::Queue;
use crate::domain::LeaseToken;
use crate::domain::release_update::{
    RELEASE_CHECK_FAILED, RELEASE_CHECK_KINDS, RELEASE_CHECKED, ReleaseMode, ReleaseUpdateConfig,
    check_due, is_release_build, latest_release,
};

/// What one read of the index gave.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IndexFetch {
    /// `304 Not Modified` to the `If-None-Match` sent: the last result
    /// stands.
    NotModified,
    /// The index's text and its `ETag`, if it had one.
    Fetched { body: String, etag: Option<String> },
}

/// Reads dagq's file in crates.io's sparse index, sending `etag` (the one
/// of the last read) as `If-None-Match`.
pub trait ReleaseIndex: Send + Sync {
    fn fetch(&self, etag: Option<&str>) -> Result<IndexFetch>;
}

/// Look for a new release when one is due: `current` is the supervisor's
/// build identifier, `now` its clock in unix seconds, `first_pass` whether
/// this is the process's first look. Returns the event written, `None`
/// when nothing was due (a development build, `release = "off"`, or a look
/// by any supervisor within the interval).
pub fn check(
    queue: &dyn Queue,
    index: &dyn ReleaseIndex,
    config: &ReleaseUpdateConfig,
    current: &str,
    now: i64,
    first_pass: bool,
    supervisor: &LeaseToken,
) -> Result<Option<(&'static str, Value)>> {
    if !is_release_build(current) || config.release == ReleaseMode::Off {
        return Ok(None);
    }
    let last = queue.latest_queue_event(&RELEASE_CHECK_KINDS)?;
    if !check_due(
        last.as_ref(),
        now,
        config.check_interval_secs,
        first_pass,
        current,
    ) {
        return Ok(None);
    }
    let previous = match last {
        Some(event) if event.kind == RELEASE_CHECKED => Some(event),
        _ => queue.latest_queue_event(&[RELEASE_CHECKED])?,
    };
    let previous_etag = previous
        .as_ref()
        .and_then(|event| event.payload["etag"].as_str().map(str::to_owned));
    let found = match index.fetch(previous_etag.as_deref()) {
        Ok(IndexFetch::NotModified) => match &previous {
            Some(event) if previous_etag.is_some() => Ok((
                event.payload["latest"].as_str().map(str::to_owned),
                previous_etag.clone(),
                true,
            )),
            _ => Err("the index answered 304 Not Modified to no ETag".to_owned()),
        },
        Ok(IndexFetch::Fetched { body, etag }) => {
            latest_release(&body).map(|latest| (latest, etag, false))
        }
        Err(error) => Err(format!("{error:#}")),
    };
    let (kind, payload) = match found {
        Ok((latest, etag, not_modified)) => (
            RELEASE_CHECKED,
            json!({
                "latest": latest,
                "current": current,
                // The plugin's version is read by a later step (ADR-t618-2).
                "plugin": null,
                "checked_at": now,
                "etag": etag,
                "not_modified": not_modified,
                "supervisor": supervisor,
            }),
        ),
        Err(error) => (
            RELEASE_CHECK_FAILED,
            json!({
                "error": error,
                "current": current,
                "checked_at": now,
                "supervisor": supervisor,
            }),
        ),
    };
    queue.record_queue_event(kind, payload.clone())?;
    Ok(Some((kind, payload)))
}

/// `status`'s `release_update` of `queue` with the host's `config`:
/// whether a build that looks runs (a live supervisor's `binary_version`,
/// else this binary's), and the last looks.
pub fn status(
    queue: &dyn Queue,
    config: &ReleaseUpdateConfig,
    live_builds: &[String],
) -> Result<Value> {
    let release_build = if live_builds.is_empty() {
        is_release_build(crate::VERSION)
    } else {
        live_builds.iter().any(|build| is_release_build(build))
    };
    let last = queue.latest_queue_event(&RELEASE_CHECK_KINDS)?;
    let last_checked = match &last {
        Some(event) if event.kind == RELEASE_CHECKED => Some(event.clone()),
        _ => queue.latest_queue_event(&[RELEASE_CHECKED])?,
    };
    Ok(crate::domain::release_update::status(
        config,
        release_build,
        last_checked.as_ref(),
        last.as_ref(),
    ))
}
