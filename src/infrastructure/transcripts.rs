//! Claude Code's transcripts (ADR-0048 decision 9): where the transcript of
//! a session is and how its lines are read. The format is Claude Code's
//! own and changes with its versions; this is the only module that knows
//! it (with `domain::transcript`), so a new format is fixed here.
//!
//! Every session's transcript is first looked for as the host's Claude
//! Code's. The worker's sessions of a run in a container are not there but
//! under its run directory's agent home, named after its clone
//! ([`crate::domain::run_container::container_transcripts`]); those are
//! the container's files, read without following a link, and one that
//! cannot be read so is missing (ADR-t2179-1 decision 9). The host is
//! looked at first because the run's other sessions (its review) run on
//! the host with the clone as their span's cwd, and the container cannot
//! write there; a host session whose cwd only looks like a clone is read
//! as before.

use std::{
    fs,
    io::Read,
    path::{Path, PathBuf},
};

use tracing::debug;

use crate::{
    application::{TranscriptSource, Transcripts},
    domain::{
        run_clone::RUN_FILE_LIMIT,
        run_container::{CLAUDE_DIR, ContainerTranscripts, container_transcripts},
        toolchain::Toolchains,
        transcript::{
            SESSION_UNKNOWN, TRANSCRIPT_MISSING, TRANSCRIPT_UNPARSABLE, Transcript, Unreadable,
        },
    },
    infrastructure::agent_dir::Directory,
};

/// The transcripts under Claude Code's configuration directory, and those
/// of the container runs under their run directories.
#[derive(Debug, Clone)]
pub struct ClaudeTranscripts {
    config_dir: Option<PathBuf>,
}

#[cfg(test)]
thread_local! {
    /// The configuration directory a unit test reads, instead of the
    /// environment's.
    static TEST_CONFIG_DIR: std::cell::RefCell<Option<PathBuf>> =
        const { std::cell::RefCell::new(None) };
}

impl ClaudeTranscripts {
    pub fn new(config_dir: Option<PathBuf>) -> Self {
        Self { config_dir }
    }

    /// `$CLAUDE_CONFIG_DIR`, else `~/.claude`, for the sessions of the
    /// host.
    pub fn from_env() -> Self {
        #[cfg(test)]
        if let Some(dir) = TEST_CONFIG_DIR.with(|dir| dir.borrow().clone()) {
            return Self::new(Some(dir));
        }
        let dir = std::env::var_os("CLAUDE_CONFIG_DIR")
            .filter(|dir| !dir.is_empty())
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|home| Path::new(&home).join(".claude")));
        Self::new(dir)
    }

    /// Make [`ClaudeTranscripts::from_env`] read `dir` on this thread.
    #[cfg(test)]
    pub(crate) fn use_config_dir_in_test(dir: &Path) {
        TEST_CONFIG_DIR.with(|cell| *cell.borrow_mut() = Some(dir.to_owned()));
    }

    /// The file of the transcript: the recorded path, else
    /// `projects/<cwd encoded>/<session id>.jsonl`, else that file name in
    /// any project directory.
    fn path(&self, source: &TranscriptSource, session_id: &str) -> Option<PathBuf> {
        if let Some(path) = &source.transcript_path {
            return Some(PathBuf::from(path));
        }
        let projects = self.config_dir.as_ref()?.join("projects");
        let file = format!("{session_id}.jsonl");
        if let Some(cwd) = &source.cwd {
            let path = projects.join(encode_cwd(cwd)).join(&file);
            if path.is_file() {
                return Some(path);
            }
        }
        fs::read_dir(&projects)
            .ok()?
            .filter_map(Result::ok)
            .map(|entry| entry.path().join(&file))
            .find(|path| path.is_file())
    }
}

impl ClaudeTranscripts {
    /// Whether the transcript of the session `session_id` started in `cwd`
    /// exists: the host's, else, for a container run's clone, a regular
    /// file under its agent home reached without a link.
    pub fn exists(&self, cwd: &str, session_id: &str) -> bool {
        let source = TranscriptSource {
            session_id: Some(session_id.to_owned()),
            cwd: Some(cwd.to_owned()),
            transcript_path: None,
        };
        self.path(&source, session_id)
            .is_some_and(|path| path.is_file())
            || container_transcripts(Path::new(cwd))
                .is_some_and(|place| in_container(&place, session_id).is_some())
    }

    /// The bytes of the transcript of `session_id` and where they are: from
    /// [`Self::path`] when the host has it, else, for a container run's
    /// clone, from its agent home ([`in_container`]).
    fn bytes(
        &self,
        source: &TranscriptSource,
        session_id: &str,
    ) -> Result<(PathBuf, Vec<u8>), Unreadable> {
        let missing = |detail: String| Unreadable {
            code: TRANSCRIPT_MISSING,
            version: None,
            detail,
        };
        if let Some(path) = self.path(source, session_id) {
            let bytes =
                fs::read(&path).map_err(|error| missing(format!("{}: {error}", path.display())))?;
            return Ok((path, bytes));
        }
        let place = source
            .cwd
            .as_deref()
            .and_then(|cwd| container_transcripts(Path::new(cwd)));
        if let Some(place) = place {
            let (path, file) = in_container(&place, session_id).ok_or_else(|| {
                missing(format!(
                    "no transcript of session {session_id} was found on the host or as a regular file under {}",
                    place.home.display()
                ))
            })?;
            let mut bytes = Vec::new();
            file.take(RUN_FILE_LIMIT + 1)
                .read_to_end(&mut bytes)
                .map_err(|error| missing(format!("{}: {error}", path.display())))?;
            if bytes.len() as u64 > RUN_FILE_LIMIT {
                return Err(missing(format!(
                    "{} is larger than {RUN_FILE_LIMIT} bytes",
                    path.display()
                )));
            }
            return Ok((path, bytes));
        }
        Err(missing(format!(
            "no transcript of session {session_id} was found"
        )))
    }
}

/// The transcript of `session_id` of a container run, opened under its
/// agent home without following a link at any level (the home, `.claude`,
/// `projects`, the project directory, the file) and only when it is a
/// regular file: in the project directory of the container's working
/// directory, else in any project directory, as [`ClaudeTranscripts::path`]
/// finds one on the host. `None` when there is none so.
fn in_container(place: &ContainerTranscripts, session_id: &str) -> Option<(PathBuf, fs::File)> {
    let projects = Directory::open(&place.home)
        .ok()?
        .child(CLAUDE_DIR)
        .ok()?
        .child("projects")
        .ok()?;
    let file = format!("{session_id}.jsonl");
    let named = place.cwd.to_str().map(encode_cwd);
    let open = |project: &str| {
        let opened = projects.child(project).ok()?.read_file(&file).ok()?;
        let path = place
            .home
            .join(CLAUDE_DIR)
            .join("projects")
            .join(project)
            .join(&file);
        Some((path, opened))
    };
    // The other project directories are listed only when the clone's has
    // none: a `projects` that cannot be listed hides no transcript of it.
    if let Some(found) = named.as_deref().and_then(open) {
        return Some(found);
    }
    projects
        .names()
        .ok()?
        .into_iter()
        .filter_map(|name| name.into_string().ok())
        .find_map(|project| open(&project))
}

/// The directory name Claude Code gives the project of `cwd`: every
/// character but an ASCII letter or digit is `-`.
pub fn encode_cwd(cwd: &str) -> String {
    cwd.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

impl Transcripts for ClaudeTranscripts {
    fn read(
        &self,
        source: &TranscriptSource,
        toolchains: &Toolchains,
    ) -> Result<Transcript, Unreadable> {
        let Some(session_id) = source.session_id.as_deref() else {
            return Err(Unreadable {
                code: SESSION_UNKNOWN,
                version: None,
                detail: "the span has no session id".into(),
            });
        };
        let (path, bytes) = self.bytes(source, session_id)?;
        let Ok(text) = String::from_utf8(bytes) else {
            return Err(Unreadable {
                code: TRANSCRIPT_UNPARSABLE,
                version: None,
                detail: format!("{} is not UTF-8", path.display()),
            });
        };
        let transcript =
            Transcript::parse(&text, session_id, toolchains).map_err(|mut error| {
                error.detail = format!("{}: {}", path.display(), error.detail);
                error
            })?;
        if transcript.skipped > 0 {
            debug!(
                "transcript {}: {} line(s) that are not JSON skipped",
                path.display(),
                transcript.skipped
            );
        }
        Ok(transcript)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::transcript::{SESSION_MISMATCH, TRANSCRIPT_UNSUPPORTED};

    const SESSION: &str = "33333333-3333-4333-8333-333333333333";

    fn record(session: &str) -> String {
        format!(
            r#"{{"type":"user","timestamp":"2026-09-26T00:00:00.000Z","sessionId":"{session}","message":{{"content":"hi"}}}}"#
        )
    }

    #[test]
    fn transcripts_are_found_by_cwd_by_session_id_or_by_path() {
        let dir = tempfile::tempdir().unwrap();
        let transcripts = ClaudeTranscripts::new(Some(dir.path().to_owned()));
        let source = TranscriptSource {
            session_id: Some(SESSION.into()),
            cwd: Some("/work/tree.x".into()),
            transcript_path: None,
        };
        assert_eq!(
            transcripts
                .read(&source, &Toolchains::this_repository())
                .unwrap_err()
                .code,
            TRANSCRIPT_MISSING
        );

        let project = dir.path().join("projects").join("-work-tree-x");
        fs::create_dir_all(&project).unwrap();
        fs::write(project.join(format!("{SESSION}.jsonl")), record(SESSION)).unwrap();
        assert_eq!(
            transcripts
                .read(&source, &Toolchains::this_repository())
                .unwrap()
                .records
                .len(),
            1
        );
        // Another cwd: found in whatever project holds the session.
        let elsewhere = TranscriptSource {
            cwd: Some("/private/work/tree.x".into()),
            ..source.clone()
        };
        assert_eq!(
            transcripts
                .read(&elsewhere, &Toolchains::this_repository())
                .unwrap()
                .records
                .len(),
            1
        );

        let path = dir.path().join("given.jsonl");
        fs::write(&path, record("44444444-4444-4444-8444-444444444444")).unwrap();
        let given = TranscriptSource {
            transcript_path: Some(path.display().to_string()),
            ..source.clone()
        };
        assert_eq!(
            transcripts
                .read(&given, &Toolchains::this_repository())
                .unwrap_err()
                .code,
            SESSION_MISMATCH
        );
        fs::write(&path, "{\"unknown\":1}\n").unwrap();
        assert_eq!(
            transcripts
                .read(&given, &Toolchains::this_repository())
                .unwrap_err()
                .code,
            TRANSCRIPT_UNSUPPORTED
        );
        fs::write(&path, [0xff, 0xfe]).unwrap();
        assert_eq!(
            transcripts
                .read(&given, &Toolchains::this_repository())
                .unwrap_err()
                .code,
            TRANSCRIPT_UNPARSABLE
        );
        fs::write(&path, format!("{}\nnot json\n", record(SESSION))).unwrap();
        assert_eq!(
            transcripts
                .read(&given, &Toolchains::this_repository())
                .unwrap()
                .skipped,
            1
        );

        let unknown = TranscriptSource::default();
        assert_eq!(
            transcripts
                .read(&unknown, &Toolchains::this_repository())
                .unwrap_err()
                .code,
            SESSION_UNKNOWN
        );
        let nowhere = ClaudeTranscripts::new(None);
        assert_eq!(
            nowhere
                .read(&source, &Toolchains::this_repository())
                .unwrap_err()
                .code,
            TRANSCRIPT_MISSING
        );
    }

    #[test]
    fn a_session_exists_once_its_transcript_does() {
        let dir = tempfile::tempdir().unwrap();
        let transcripts = ClaudeTranscripts::new(Some(dir.path().to_owned()));
        assert!(!transcripts.exists("/work/tree.x", SESSION));
        let project = dir.path().join("projects").join("-work-tree-x");
        fs::create_dir_all(&project).unwrap();
        fs::write(project.join(format!("{SESSION}.jsonl")), "{}\n").unwrap();
        assert!(transcripts.exists("/work/tree.x", SESSION));
        assert!(!transcripts.exists("/work/tree.x", "other"));
        assert!(!ClaudeTranscripts::new(None).exists("/work/tree.x", SESSION));
    }

    /// A container run's worker sessions are read from its agent home,
    /// named after its clone, when the host has none of them; a link at any
    /// level of it is not followed. A session the host has (the run's
    /// review, whose span's cwd is the clone too) is the host's, whatever
    /// the agent home holds under its name.
    #[test]
    fn a_container_run_s_transcripts_are_read_from_its_home_without_links() {
        const REVIEW: &str = "44444444-4444-4444-8444-444444444444";
        let dir = tempfile::tempdir().unwrap();
        let clone = dir.path().join("q/clones/r1");
        let home = dir.path().join("q/runs/r1/container-home");
        let cwd = clone.to_str().unwrap();
        let host = dir.path().join("host");
        let transcripts = ClaudeTranscripts::new(Some(host.clone()));
        let source = |session: &str| TranscriptSource {
            session_id: Some(session.into()),
            cwd: Some(cwd.to_owned()),
            transcript_path: None,
        };
        let records = |session: &str| {
            transcripts
                .read(&source(session), &Toolchains::this_repository())
                .map(|transcript| transcript.records.len())
                .map_err(|unreadable| unreadable.code)
        };
        assert!(!transcripts.exists(cwd, SESSION));
        assert_eq!(records(SESSION), Err(TRANSCRIPT_MISSING));
        // The review ran on the host, in the run's landing worktree.
        let landing = host.join("projects").join(encode_cwd(
            dir.path().join("q/landing/r1").to_str().unwrap(),
        ));
        fs::create_dir_all(&landing).unwrap();
        fs::write(landing.join(format!("{REVIEW}.jsonl")), record(REVIEW)).unwrap();
        assert!(transcripts.exists(cwd, REVIEW));
        assert_eq!(records(REVIEW), Ok(1));

        let project = home.join(".claude/projects").join(encode_cwd(cwd));
        fs::create_dir_all(&project).unwrap();
        let file = project.join(format!("{SESSION}.jsonl"));
        fs::write(&file, format!("{}\n{}\n", record(SESSION), record(SESSION))).unwrap();
        assert!(transcripts.exists(cwd, SESSION));
        assert!(!transcripts.exists(cwd, "other"));
        assert!(!transcripts.exists(cwd, "../x"));
        assert_eq!(records(SESSION), Ok(2));
        let planted = format!("{0}\n{0}\n{0}\n", record(REVIEW));
        fs::write(project.join(format!("{REVIEW}.jsonl")), planted).unwrap();
        assert_eq!(records(REVIEW), Ok(1));
        // In another project directory of the home, it is found too.
        let moved = home.join(".claude/projects/-elsewhere");
        fs::create_dir_all(&moved).unwrap();
        fs::rename(&file, moved.join(format!("{SESSION}.jsonl"))).unwrap();
        assert!(transcripts.exists(cwd, SESSION));

        // A link as the file, the project directory, `projects`, `.claude`
        // or the home is missing.
        let elsewhere = dir.path().join("elsewhere");
        fs::create_dir_all(elsewhere.join(".claude/projects").join(encode_cwd(cwd))).unwrap();
        fs::write(
            elsewhere
                .join(".claude/projects")
                .join(encode_cwd(cwd))
                .join(format!("{SESSION}.jsonl")),
            record(SESSION),
        )
        .unwrap();
        fs::remove_dir_all(&home).unwrap();
        fs::create_dir_all(&project).unwrap();
        std::os::unix::fs::symlink(
            elsewhere
                .join(".claude/projects")
                .join(encode_cwd(cwd))
                .join(format!("{SESSION}.jsonl")),
            &file,
        )
        .unwrap();
        let links = |at: &Path, to: &Path| {
            fs::remove_dir_all(&home).unwrap();
            fs::create_dir_all(at.parent().unwrap()).unwrap();
            std::os::unix::fs::symlink(to, at).unwrap();
        };
        let missing = || {
            assert!(!transcripts.exists(cwd, SESSION));
            assert_eq!(records(SESSION), Err(TRANSCRIPT_MISSING));
        };
        missing();
        links(
            &project,
            &elsewhere.join(".claude/projects").join(encode_cwd(cwd)),
        );
        missing();
        links(
            &home.join(".claude/projects"),
            &elsewhere.join(".claude/projects"),
        );
        missing();
        links(&home.join(".claude"), &elsewhere.join(".claude"));
        missing();
        fs::remove_dir_all(&home).unwrap();
        std::os::unix::fs::symlink(&elsewhere, &home).unwrap();
        missing();
        // A directory where the file would be is no transcript either.
        fs::remove_file(&home).unwrap();
        fs::create_dir_all(project.join(format!("{SESSION}.jsonl"))).unwrap();
        missing();
    }

    /// A host session whose cwd is in a directory named like the clones
    /// (a project checked out at `.../clones/<name>`) is read from the
    /// host as before.
    #[test]
    fn a_host_session_in_a_directory_named_clones_is_the_host_s() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().join("me/clones/app");
        let cwd = cwd.to_str().unwrap();
        let host = dir.path().join("host");
        let project = host.join("projects").join(encode_cwd(cwd));
        fs::create_dir_all(&project).unwrap();
        fs::write(project.join(format!("{SESSION}.jsonl")), record(SESSION)).unwrap();
        let transcripts = ClaudeTranscripts::new(Some(host));
        assert!(transcripts.exists(cwd, SESSION));
        let source = TranscriptSource {
            session_id: Some(SESSION.into()),
            cwd: Some(cwd.to_owned()),
            transcript_path: None,
        };
        assert_eq!(
            transcripts
                .read(&source, &Toolchains::this_repository())
                .unwrap()
                .records
                .len(),
            1
        );
    }

    #[test]
    fn from_env_reads_the_test_directory() {
        let dir = tempfile::tempdir().unwrap();
        ClaudeTranscripts::use_config_dir_in_test(dir.path());
        assert_eq!(
            ClaudeTranscripts::from_env().config_dir.as_deref(),
            Some(dir.path())
        );
        assert_eq!(encode_cwd("/Users/a/.local/x_y"), "-Users-a--local-x-y");
    }
}
