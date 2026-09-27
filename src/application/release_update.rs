//! A supervisor's look at crates.io for a new dagq release (ADR-t618-1
//! decisions 1 to 3): only a release build looks, as the host's `[update]`
//! says, at most once per `check_interval_secs` for the whole queue, and
//! writes what it found as `release_checked` or `release_check_failed`.
//! Neither is an attention. The index is read through [`ReleaseIndex`]
//! (`curl` on the host, a stub in tests). What the supervisor does about a
//! new release (decisions 4 and 5: ask, or install without asking) is
//! [`next_action`].

use anyhow::Result;
use serde_json::{Value, json};

use super::update::{
    UPDATE_ANSWERED, UPDATE_FAILED, UPDATE_INSTALLED, UPDATE_RETRY, UPDATE_STARTED, step_release,
};
use super::{InstalledPlugin, Queue};
use crate::domain::release_update::{
    RELEASE_CHECK_FAILED, RELEASE_CHECK_KINDS, RELEASE_CHECKED, ReleaseMode, ReleaseUpdateConfig,
    check_due, is_newer, is_release_build, latest_release,
};
use crate::domain::{LeaseToken, RunEvent};

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
/// this is the process's first look, `plugin` the installed dagq plugin
/// whose version the look records too (`None` when the supervisor loads one
/// from `--plugin-dir`, or it cannot be read: `plugin` is null). Returns the
/// event written, `None` when nothing was due (a development build,
/// `release = "off"`, or a look by any supervisor within the interval).
#[allow(clippy::too_many_arguments)]
pub fn check(
    queue: &dyn Queue,
    index: &dyn ReleaseIndex,
    plugin: Option<&dyn InstalledPlugin>,
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
                // Unread, only the binary decides (ADR-t618-2 decision 4).
                "plugin": plugin.and_then(|plugin| plugin.version().ok().flatten()),
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

/// What a supervisor of a release build does about the releases on a pass
/// (ADR-t618-1 decisions 4 and 5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReleaseAction {
    /// Start the job that installs this release.
    Start(String),
    /// Open the `approve_release` ask about this release.
    Ask(String),
    /// Start the job that brings the installed plugin to this release, the
    /// binary being it already (ADR-t618-2 decision 4).
    StartPlugin(String),
    /// Open the `approve_release` ask about bringing the plugin to this
    /// release.
    AskPlugin(String),
    Nothing,
}

/// Whether `update` asks for a job of its release: a `retry`, or an
/// `install` answer.
fn is_request(update: &RunEvent) -> bool {
    update.kind == UPDATE_RETRY
        || (update.kind == UPDATE_ANSWERED
            && update.payload["answer"].as_str().map(str::trim) == Some("install"))
}

/// The next step of the release update: `mode` is the host's `release`,
/// `current` the supervisor's build, `latest` the release the last look
/// found, `updates` the queue's `update_*` newest first and `open` the
/// releases of the `approve_release` asks nobody closed. A request (an
/// `install` answer or a `retry`) that no job of its release followed
/// starts its release (the newest, when several wait); otherwise a release newer than `current` that no job
/// tried and no answer skipped is installed (`auto`) or asked about (`ask`,
/// unless its ask is open).
pub fn next_action(
    mode: ReleaseMode,
    current: &str,
    latest: Option<&str>,
    updates: &[RunEvent],
    open: &[String],
) -> ReleaseAction {
    if mode == ReleaseMode::Off || !is_release_build(current) {
        return ReleaseAction::Nothing;
    }
    // Each request no job of its release (or a newer one) started after;
    // of those, the newest release, so an answer about another release in the same pass
    // does not hide it.
    let pending = updates
        .iter()
        .enumerate()
        .filter(|(_, update)| is_request(update))
        .filter_map(|(at, update)| {
            let version = step_release(update)?;
            // A job of it, or of a newer release, answered it.
            let started = updates[..at].iter().any(|later| {
                later.kind == UPDATE_STARTED
                    && step_release(later)
                        .is_some_and(|started| started == version || is_newer(started, version))
            });
            (!started && is_newer(version, current)).then_some(version)
        })
        .reduce(|best, version| {
            if is_newer(version, best) {
                version
            } else {
                best
            }
        });
    if let Some(version) = pending {
        return ReleaseAction::Start(version.to_owned());
    }
    let Some(latest) = latest.filter(|latest| is_newer(latest, current)) else {
        return ReleaseAction::Nothing;
    };
    // A job tried it (its failure asks on its own), or an answer decided.
    let decided = updates.iter().any(|update| {
        step_release(update) == Some(latest)
            && matches!(update.kind.as_str(), UPDATE_STARTED | UPDATE_ANSWERED)
    });
    if decided {
        return ReleaseAction::Nothing;
    }
    match mode {
        ReleaseMode::Auto => ReleaseAction::Start(latest.to_owned()),
        _ if open.iter().any(|version| version == latest) => ReleaseAction::Nothing,
        _ => ReleaseAction::Ask(latest.to_owned()),
    }
}

/// The next step of the plugin's side of the release update (ADR-t618-2
/// decision 4), once [`next_action`] has nothing to do about the binary and
/// the supervisor does not load the plugin from `--plugin-dir`: only while
/// the binary is the latest release (the marketplace hands out the latest,
/// and a plugin newer than the binary could name what it lacks). A request
/// (an `install` answer or a `retry`) about the release the binary is
/// already that no job followed starts the plugin's job; otherwise an
/// installed `plugin` older than the release is brought to it (`auto`) or
/// asked about (`ask`), unless a job already tried it (the binary's job
/// updates the plugin after it, and its failure asks on its own) or an
/// answer decided.
pub fn next_plugin_action(
    mode: ReleaseMode,
    current: &str,
    latest: Option<&str>,
    plugin: Option<&str>,
    updates: &[RunEvent],
    open: &[String],
) -> ReleaseAction {
    if mode == ReleaseMode::Off || !is_release_build(current) || latest != Some(current) {
        return ReleaseAction::Nothing;
    }
    let requested = updates.iter().enumerate().any(|(at, update)| {
        is_request(update)
            && step_release(update) == Some(current)
            && !updates[..at].iter().any(|later| {
                later.kind == UPDATE_STARTED
                    && step_release(later)
                        .is_some_and(|started| started == current || is_newer(started, current))
            })
    });
    if requested {
        return ReleaseAction::StartPlugin(current.to_owned());
    }
    if !plugin.is_some_and(|plugin| is_newer(current, plugin)) {
        return ReleaseAction::Nothing;
    }
    let decided = updates.iter().any(|update| {
        step_release(update) == Some(current)
            && match update.kind.as_str() {
                UPDATE_STARTED | UPDATE_ANSWERED => update.payload["plugin_only"] == true,
                UPDATE_INSTALLED => update.payload.get("plugin").is_some(),
                UPDATE_FAILED => update.payload["stage"] == "plugin",
                _ => false,
            }
    });
    if decided {
        return ReleaseAction::Nothing;
    }
    match mode {
        ReleaseMode::Auto => ReleaseAction::StartPlugin(current.to_owned()),
        _ if open.iter().any(|version| version == current) => ReleaseAction::Nothing,
        _ => ReleaseAction::AskPlugin(current.to_owned()),
    }
}

/// The release `install --release` puts in place: `requested`, or the
/// newest one `index` lists.
pub fn resolve_version(index: &dyn ReleaseIndex, requested: Option<&str>) -> Result<String> {
    if let Some(version) = requested {
        anyhow::ensure!(
            is_release_build(version),
            "{version} is not a release version (X.Y.Z)"
        );
        return Ok(version.to_owned());
    }
    let body = match index.fetch(None)? {
        IndexFetch::Fetched { body, .. } => body,
        IndexFetch::NotModified => anyhow::bail!("the index answered 304 Not Modified to no ETag"),
    };
    latest_release(&body)
        .map_err(anyhow::Error::msg)?
        .ok_or_else(|| anyhow::anyhow!("crates.io lists no release of dagq"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::EventId;

    fn update(kind: &str, payload: Value) -> RunEvent {
        RunEvent {
            id: EventId::new(1),
            task_id: None,
            goal_id: None,
            run_id: None,
            kind: kind.to_owned(),
            payload,
            created_at: String::new(),
            actor: None,
        }
    }

    fn release(kind: &str, version: &str, answer: Option<&str>) -> RunEvent {
        update(
            kind,
            json!({"source": "release", "release": version, "answer": answer}),
        )
    }

    #[test]
    fn a_new_release_is_asked_about_once_and_installed_without_asking_in_auto() {
        use ReleaseAction::*;
        let ask = ReleaseMode::Ask;
        assert_eq!(
            next_action(ask, "0.3.0", Some("0.4.0"), &[], &[]),
            Ask("0.4.0".into())
        );
        assert_eq!(
            next_action(ask, "0.3.0", Some("0.4.0"), &[], &["0.4.0".into()]),
            Nothing
        );
        // An open ask of an older release gives way to the new one.
        assert_eq!(
            next_action(ask, "0.3.0", Some("0.5.0"), &[], &["0.4.0".into()]),
            Ask("0.5.0".into())
        );
        assert_eq!(
            next_action(ReleaseMode::Auto, "0.3.0", Some("0.4.0"), &[], &[]),
            Start("0.4.0".into())
        );
        for mode in [ask, ReleaseMode::Auto, ReleaseMode::Off] {
            assert_eq!(next_action(mode, "0.4.0", Some("0.4.0"), &[], &[]), Nothing);
            assert_eq!(next_action(mode, "0.3.0", None, &[], &[]), Nothing);
            // A development build never acts on a release.
            assert_eq!(
                next_action(mode, "0.3.0-dev+abc", Some("0.4.0"), &[], &[]),
                Nothing
            );
        }
        assert_eq!(
            next_action(ReleaseMode::Off, "0.3.0", Some("0.4.0"), &[], &[]),
            Nothing
        );
    }

    #[test]
    fn answers_start_skip_and_retry_a_release() {
        use ReleaseAction::*;
        let ask = ReleaseMode::Ask;
        let install = [release(UPDATE_ANSWERED, "0.4.0", Some("install"))];
        assert_eq!(
            next_action(ask, "0.3.0", Some("0.4.0"), &install, &[]),
            Start("0.4.0".into())
        );
        // Started: not again, whatever became of it.
        let started = [
            release(UPDATE_STARTED, "0.4.0", None),
            release(UPDATE_ANSWERED, "0.4.0", Some("install")),
        ];
        for mode in [ask, ReleaseMode::Auto] {
            assert_eq!(
                next_action(mode, "0.3.0", Some("0.4.0"), &started, &[]),
                Nothing
            );
        }
        // Skipped: not asked again, but the next release is.
        let skipped = [release(UPDATE_ANSWERED, "0.4.0", Some("skip"))];
        assert_eq!(
            next_action(ask, "0.3.0", Some("0.4.0"), &skipped, &[]),
            Nothing
        );
        assert_eq!(
            next_action(ReleaseMode::Auto, "0.3.0", Some("0.4.0"), &skipped, &[]),
            Nothing
        );
        assert_eq!(
            next_action(ask, "0.3.0", Some("0.5.0"), &skipped, &[]),
            Ask("0.5.0".into())
        );
        // A retry after a failed job starts it again, once.
        let retry = [
            release(UPDATE_RETRY, "0.4.0", Some("retry")),
            release(UPDATE_STARTED, "0.4.0", None),
        ];
        assert_eq!(
            next_action(ask, "0.3.0", Some("0.4.0"), &retry, &[]),
            Start("0.4.0".into())
        );
        // An install answer for a release this build is already.
        assert_eq!(
            next_action(ask, "0.4.0", Some("0.4.0"), &install, &[]),
            Nothing
        );
        // An install answer and a skip of another release's failure in
        // the same pass: the install still starts.
        let both = [
            release(UPDATE_ANSWERED, "0.4.0", Some("skip")),
            release(UPDATE_ANSWERED, "0.5.0", Some("install")),
            release(UPDATE_STARTED, "0.4.0", None),
        ];
        assert_eq!(
            next_action(ask, "0.3.0", Some("0.5.0"), &both, &[]),
            Start("0.5.0".into())
        );
        // A retry of an older release and an install of a newer one: the
        // newer is installed, and then neither starts again.
        let retry_and_install = [
            release(UPDATE_RETRY, "0.4.0", Some("retry")),
            release(UPDATE_ANSWERED, "0.5.0", Some("install")),
            release(UPDATE_STARTED, "0.4.0", None),
        ];
        assert_eq!(
            next_action(ask, "0.3.0", Some("0.5.0"), &retry_and_install, &[]),
            Start("0.5.0".into())
        );
        let mut after = vec![release(UPDATE_STARTED, "0.5.0", None)];
        after.extend(retry_and_install);
        assert_eq!(
            next_action(ask, "0.3.0", Some("0.5.0"), &after, &[]),
            Nothing
        );
        assert_eq!(
            next_action(ask, "0.5.0", Some("0.5.0"), &after, &[]),
            Nothing
        );
        // The automatic update's steps are not the release update's.
        let source = [update(UPDATE_RETRY, json!({"commit": "abc"}))];
        assert_eq!(
            next_action(ask, "0.3.0", Some("0.4.0"), &source, &[]),
            Ask("0.4.0".into())
        );
    }

    fn plugin_step(kind: &str, version: &str, extra: Value) -> RunEvent {
        let mut payload = json!({"source": "release", "release": version});
        if let (Some(payload), Value::Object(extra)) = (payload.as_object_mut(), extra) {
            payload.extend(extra);
        }
        update(kind, payload)
    }

    #[test]
    fn a_plugin_older_than_the_binary_is_asked_about_or_updated_once() {
        use ReleaseAction::*;
        let ask = ReleaseMode::Ask;
        let plugin = |mode, current, latest, plugin, updates: &[RunEvent], open: &[String]| {
            next_plugin_action(mode, current, latest, plugin, updates, open)
        };
        // Only the plugin is old: asked about, or updated without asking.
        assert_eq!(
            plugin(ask, "0.4.0", Some("0.4.0"), Some("0.3.0"), &[], &[]),
            AskPlugin("0.4.0".into())
        );
        assert_eq!(
            plugin(
                ReleaseMode::Auto,
                "0.4.0",
                Some("0.4.0"),
                Some("0.3.0"),
                &[],
                &[]
            ),
            StartPlugin("0.4.0".into())
        );
        // Not while its ask is open, the plugin is not older, unread, the
        // binary is not the latest, off, or a development build.
        for (mode, current, latest, version, open) in [
            (
                ask,
                "0.4.0",
                Some("0.4.0"),
                Some("0.3.0"),
                vec!["0.4.0".into()],
            ),
            (ask, "0.4.0", Some("0.4.0"), Some("0.4.0"), vec![]),
            (ask, "0.4.0", Some("0.4.0"), None, vec![]),
            (ask, "0.4.0", Some("0.4.0"), Some("unknown"), vec![]),
            (ask, "0.3.0", Some("0.4.0"), Some("0.2.0"), vec![]),
            (ask, "0.4.0", None, Some("0.2.0"), vec![]),
            (
                ReleaseMode::Off,
                "0.4.0",
                Some("0.4.0"),
                Some("0.3.0"),
                vec![],
            ),
            (
                ask,
                "0.4.0-dev+a",
                Some("0.4.0-dev+a"),
                Some("0.3.0"),
                vec![],
            ),
        ] {
            assert_eq!(
                plugin(mode, current, latest, version, &[], &open),
                Nothing,
                "{current} {latest:?} {version:?}"
            );
        }
        // Tried or decided: the binary's job updated it (or failed to), a
        // plugin job ran, or the plugin's ask was skipped.
        for decided in [
            plugin_step(
                UPDATE_INSTALLED,
                "0.4.0",
                json!({"plugin": {"updated": true}}),
            ),
            plugin_step(UPDATE_FAILED, "0.4.0", json!({"stage": "plugin"})),
            plugin_step(UPDATE_STARTED, "0.4.0", json!({"plugin_only": true})),
            plugin_step(
                UPDATE_ANSWERED,
                "0.4.0",
                json!({"plugin_only": true, "answer": "skip"}),
            ),
        ] {
            assert_eq!(
                plugin(ask, "0.4.0", Some("0.4.0"), Some("0.3.0"), &[decided], &[]),
                Nothing
            );
        }
        // The binary's own steps do not decide the plugin's: an older
        // runtime's install without `plugin`, a skip of the binary.
        for binary in [
            plugin_step(UPDATE_INSTALLED, "0.4.0", json!({})),
            plugin_step(UPDATE_STARTED, "0.4.0", json!({})),
            plugin_step(UPDATE_FAILED, "0.4.0", json!({"stage": "build"})),
        ] {
            assert_eq!(
                plugin(ask, "0.4.0", Some("0.4.0"), Some("0.3.0"), &[binary], &[]),
                AskPlugin("0.4.0".into())
            );
        }
        // An install answer or a retry about the release the binary is:
        // the plugin's job starts once, whatever the plugin reads.
        for request in [
            plugin_step(
                UPDATE_ANSWERED,
                "0.4.0",
                json!({"plugin_only": true, "answer": "install"}),
            ),
            plugin_step(UPDATE_RETRY, "0.4.0", json!({"answer": "retry"})),
        ] {
            let pending = [
                request.clone(),
                plugin_step(UPDATE_FAILED, "0.4.0", json!({"stage": "plugin"})),
                plugin_step(UPDATE_STARTED, "0.4.0", json!({})),
            ];
            assert_eq!(
                plugin(ask, "0.4.0", Some("0.4.0"), None, &pending, &[]),
                StartPlugin("0.4.0".into())
            );
            let started = [
                plugin_step(UPDATE_STARTED, "0.4.0", json!({"plugin_only": true})),
                request,
            ];
            assert_eq!(
                plugin(ask, "0.4.0", Some("0.4.0"), Some("0.4.0"), &started, &[]),
                Nothing
            );
        }
    }

    struct Index(Result<IndexFetch, String>);

    impl ReleaseIndex for Index {
        fn fetch(&self, etag: Option<&str>) -> Result<IndexFetch> {
            assert_eq!(etag, None);
            self.0.clone().map_err(anyhow::Error::msg)
        }
    }

    #[test]
    fn install_release_takes_the_version_given_or_the_newest_listed() {
        let listed = Index(Ok(IndexFetch::Fetched {
            body:
                "{\"vers\":\"0.3.0\"}\n{\"vers\":\"0.4.0\"}\n{\"vers\":\"0.5.0\",\"yanked\":true}\n"
                    .into(),
            etag: None,
        }));
        assert_eq!(resolve_version(&listed, None).unwrap(), "0.4.0");
        assert_eq!(resolve_version(&listed, Some("0.3.0")).unwrap(), "0.3.0");
        let error = resolve_version(&listed, Some("0.4.0-dev")).unwrap_err();
        assert!(
            error.to_string().contains("not a release version"),
            "{error}"
        );
        let down = Index(Err("offline".into()));
        assert!(
            resolve_version(&down, None)
                .unwrap_err()
                .to_string()
                .contains("offline")
        );
        let odd = Index(Ok(IndexFetch::NotModified));
        assert!(resolve_version(&odd, None).is_err());
        let none = Index(Ok(IndexFetch::Fetched {
            body: "{\"vers\":\"0.1.0\",\"yanked\":true}\n".into(),
            etag: None,
        }));
        assert!(
            resolve_version(&none, None)
                .unwrap_err()
                .to_string()
                .contains("no release")
        );
        let junk = Index(Ok(IndexFetch::Fetched {
            body: "<html>".into(),
            etag: None,
        }));
        assert!(resolve_version(&junk, None).is_err());
    }
}
