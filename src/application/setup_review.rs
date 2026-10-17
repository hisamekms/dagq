//! `setup-review` (ADR-t2259-1 decision 7): a coding agent reads the
//! project once and writes the review's settings (`dagq.toml`'s built-in
//! and program tables, `.dagq/review/builtin-<name>.md` and
//! `.dagq/review/programs/*.sh`) in a temporary place made from the main
//! checkout's HEAD, never in the checkout. The runtime then reads what it
//! wrote once ([`Generated`]) and checks those bytes
//! ([`crate::domain::setup_review`]: the files and tables it may
//! change, `dagq.toml` read as the runtime reads it, each program script's
//! root), shows their diff, and writes the same bytes to the checkout's
//! working tree only at a person's approval and only when none of them has
//! a change there not committed. It commits, pushes and changes the queue
//! in nothing; the place is removed whatever happened. A checkout with
//! settings gives them to the job as its input, so the same command
//! updates them. The files and the agent's process are reached through
//! [`SetupReviewHost`].

use std::path::{Path, PathBuf};

use anyhow::{Result, bail};
use serde_json::{Value, json};

use super::prompt::{CurrentSetup, setup_review_prompt};
use crate::domain::{
    ActorContext,
    review_builtins::{self, BuiltinAgent},
    review_programs::BuiltinProgram,
    setup_review::{
        CONFIG_FILE, Change, ChangeStatus, SETUP_COMMAND, check_config, check_paths,
        is_program_script, takes_root_from_cwd, visible,
    },
    toolchain::{ProfileRegistry, ToolchainDeclaration, recommend::Proposed},
};

/// The review's settings `init` recommends: each built-in with whether it
/// is on by default (what a repository without `dagq.toml` runs), and the
/// command that writes the project's rules for them and their settings;
/// and `toolchains`, what the profiles the repository declares, or its
/// files suggest, recommend ([`Proposed::view`]), shown and never written.
pub fn recommended(toolchains: &Proposed) -> Value {
    let mut builtins = vec![json!({
        "name": review_builtins::ACCEPTANCE,
        "enabled_by_default": true,
    })];
    builtins.extend(BuiltinAgent::ALL.into_iter().map(
        |agent| json!({"name": agent.name(), "enabled_by_default": agent.enabled_by_default()}),
    ));
    builtins.extend(BuiltinProgram::ALL.into_iter().map(|program| {
        json!({"name": program.name(), "enabled_by_default": program.enabled_by_default()})
    }));
    json!({
        "builtins": builtins,
        "setup": SETUP_COMMAND,
        "toolchains": toolchains_view(toolchains),
        "note": format!(
            "The review runs the built-ins that are on by default without any {CONFIG_FILE}. \
             Run `{SETUP_COMMAND}` to have a coding agent read the project once and write the \
             project's rules for them ({}/) and their settings ({CONFIG_FILE}); it shows the diff, \
             writes it to the working tree only when you approve, and you commit it.",
            review_builtins::RULES_DIR
        ),
    })
}

/// What `init`'s recommendation reads the repository it runs in through.
pub trait InitRepository {
    /// The repository's main checkout.
    fn checkout(&self) -> Result<PathBuf>;
    /// The toolchain profiles the `dagq.toml` of `checkout`'s working tree
    /// declares, as written (none without the file).
    fn declarations(&self, checkout: &Path) -> Result<Vec<ToolchainDeclaration>>;
    /// The files of `checkout`'s working tree, tracked or not ignored.
    fn files(&self, checkout: &Path) -> Result<Vec<String>>;
}

/// The settings `init` recommends ([`recommended`]), with the toolchain
/// profiles of `profiles` the main checkout's `dagq.toml` declares, or its
/// files suggest. Where no main checkout is found (outside a repository,
/// or a bare one), there is no file to read a declaration or a guess from,
/// so no profile is proposed and `init` goes on.
pub fn init_recommended(repository: &dyn InitRepository, profiles: &ProfileRegistry) -> Value {
    let error = |error: anyhow::Error| format!("{error:#}");
    let toolchains =
        match repository.checkout() {
            Ok(checkout) => crate::domain::toolchain::recommend::propose(
                profiles,
                repository.declarations(&checkout).map_err(error),
                || repository.files(&checkout).map_err(error),
            ),
            Err(_) => {
                crate::domain::toolchain::recommend::propose(profiles, Ok(Vec::new()), || {
                    Ok(Vec::new())
                })
            }
        };
    recommended(&toolchains)
}

/// [`Proposed::view`] with what to do with it: nothing of it works until
/// a person writes it.
fn toolchains_view(toolchains: &Proposed) -> Value {
    let mut view = toolchains.view();
    if view.get("error").is_none() {
        view["note"] = json!(format!(
            "Proposals only: nothing here works until you write it. `toolchains` is the \
             declaration of the profiles to add to {CONFIG_FILE}; `verify` is the commands to \
             give a task as its --verify when you register it; `programs` are review programs, \
             each a table of {CONFIG_FILE} and its script, which `{SETUP_COMMAND}` gives its \
             agent when `setup_review` is true; `config` is the tables together."
        ));
    }
    view
}

/// The temporary place of one generation: `tree`, a worktree of the main
/// checkout at its HEAD `commit`, where the job works, and `dir`, beside it,
/// where its output goes; `id` names it and the job. `git_dir` is the
/// worktree's own Git directory, outside the tree, read when the place was
/// made: Git is run on the tree through it, never through the tree's `.git`,
/// which the job could rewrite.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SetupPlace {
    pub id: String,
    pub tree: PathBuf,
    pub dir: PathBuf,
    pub commit: String,
    pub git_dir: PathBuf,
}

/// A file the job changed, read once from its place after the job ended:
/// the bytes checked, shown in the diff and, approved, written to the
/// checkout are these, never the place's file read again, so nothing
/// that changes the place after the job (a process it left behind)
/// reaches the checkout unchecked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Generated {
    pub change: Change,
    /// Its bytes and whether the job made it executable: `None` for a
    /// deleted file, and for one outside what the job may change
    /// ([`crate::domain::setup_review::writable`]), which is refused
    /// unread.
    pub content: Option<(Vec<u8>, bool)>,
}

impl Generated {
    /// The file's text: `None` without content, an error when it is not
    /// UTF-8.
    fn text(&self) -> Result<Option<&str>> {
        self.content
            .as_ref()
            .map(|(bytes, _)| {
                std::str::from_utf8(bytes).map_err(|_| {
                    anyhow::anyhow!("{} is not UTF-8, so nothing is applied", self.change.path)
                })
            })
            .transpose()
    }
}

/// How the person approves the changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Approval {
    /// Given beforehand by the command's flag.
    Given,
    /// Asked on the person's terminal after the diff is shown.
    Ask,
}

/// What `setup-review` reaches outside itself through.
pub trait SetupReviewHost {
    /// The main checkout of the repository the command runs in.
    fn checkout(&self) -> Result<PathBuf>;
    /// The review's settings `commit` of `checkout` has.
    fn current(&self, checkout: &Path, commit: &str) -> Result<CurrentSetup>;
    /// The commit `checkout`'s HEAD is now.
    fn head(&self, checkout: &Path) -> Result<String>;
    /// A new temporary place made from `checkout`'s HEAD.
    fn prepare(&self, checkout: &Path) -> Result<SetupPlace>;
    /// Start the job's agent as `actor` with `prompt` in the place and wait
    /// for it: its exit code, `None` when a signal ended it.
    fn run(&self, place: &SetupPlace, prompt: &str, actor: &ActorContext) -> Result<Option<i32>>;
    /// The files the job changed in the place against its commit, each
    /// read once ([`Generated`]), and their diff made from those bytes; an
    /// error for a changed path that is not a regular file.
    fn changes(&self, place: &SetupPlace) -> Result<(Vec<Generated>, String)>;
    /// Refuse a `dagq.toml` the runtime cannot read.
    fn check_config(&self, text: &str) -> Result<()>;
    /// The toolchain profiles the `dagq.toml` `text` declares, as the
    /// runtime reads them.
    fn toolchains(&self, text: &str) -> Result<Vec<ToolchainDeclaration>>;
    /// The paths of the files `commit` of `checkout` has.
    fn files(&self, checkout: &Path, commit: &str) -> Result<Vec<String>>;
    /// Those of `paths` the working tree of `checkout` has changes of that
    /// are not committed (untracked and ignored files included).
    fn uncommitted(&self, checkout: &Path, paths: &[String]) -> Result<Vec<String>>;
    /// Show `diff` and ask the person whether to apply it to `checkout`:
    /// whether they approved.
    fn approve(&self, checkout: &Path, diff: &str) -> Result<bool>;
    /// Refuse to write `path` in `checkout`'s working tree when it, or a
    /// directory on the way to it under the checkout, is a symbolic link:
    /// the write would land outside the checkout or on another file.
    fn check_target(&self, checkout: &Path, path: &str) -> Result<()>;
    /// Write `file`'s bytes as they were read after the job to
    /// `checkout`'s working tree (or remove it there when the job deleted
    /// it), never through a symbolic link.
    fn apply(&self, checkout: &Path, file: &Generated) -> Result<()>;
    /// Remove the place.
    fn remove(&self, checkout: &Path, place: &SetupPlace) -> Result<()>;
}

/// Run one generation and, approved, apply it (see the module). The
/// toolchain profiles of `profiles` the commit declares, or its files
/// suggest, add their review programs to the job's input; what else they
/// recommend is only shown.
pub fn setup_review(
    host: &dyn SetupReviewHost,
    approval: Approval,
    profiles: &ProfileRegistry,
) -> Result<Value> {
    let checkout = host.checkout()?;
    let place = host.prepare(&checkout)?;
    let result = host.current(&checkout, &place.commit).and_then(|current| {
        let toolchains = proposed(host, profiles, &checkout, &place.commit, &current);
        generate(host, &checkout, &place, &current, &toolchains, approval)
    });
    let removed = host.remove(&checkout, &place);
    match (result, removed) {
        (Ok(mut value), Err(error)) => {
            value["place_left"] = json!(format!("{error:#}"));
            Ok(value)
        }
        (Err(error), Err(left)) => Err(error.context(format!(
            "and the temporary checkout {} was left: {left:#}",
            place.tree.display()
        ))),
        (result, Ok(())) => result,
    }
}

/// The profiles `commit`'s `dagq.toml` declares, else those its files
/// suggest, and what they recommend.
fn proposed(
    host: &dyn SetupReviewHost,
    profiles: &ProfileRegistry,
    checkout: &Path,
    commit: &str,
    current: &CurrentSetup,
) -> Proposed {
    let declared = current
        .config
        .as_deref()
        .map_or_else(|| Ok(Vec::new()), |text| host.toolchains(text))
        .map_err(|error| format!("{error:#}"));
    crate::domain::toolchain::recommend::propose(profiles, declared, || {
        host.files(checkout, commit)
            .map_err(|error| format!("{error:#}"))
    })
}

fn generate(
    host: &dyn SetupReviewHost,
    checkout: &Path,
    place: &SetupPlace,
    current: &CurrentSetup,
    toolchains: &Proposed,
    approval: Approval,
) -> Result<Value> {
    let actor = ActorContext::setup_review_job(&place.id);
    let prompt = setup_review_prompt(current, &toolchains.setup_programs());
    match host.run(place, &prompt, &actor)? {
        Some(0) => {}
        exit => bail!(
            "the job that generates the review's settings did not succeed (exit {}); nothing \
             is applied, and its output was in {}",
            exit.map_or_else(|| "by a signal".to_owned(), |code| code.to_string()),
            place.dir.display()
        ),
    }
    let (files, diff) = host.changes(place)?;
    let changes: Vec<Change> = files.iter().map(|file| file.change.clone()).collect();
    // The diff is shown with what the job wrote made visible.
    let diff = visible(&diff);
    let mut value = json!({
        "checkout": checkout,
        "commit": place.commit,
        "updated": !current.is_empty(),
        "changes": changes,
        "diff": diff,
        "approved": null,
        "applied": [],
        "toolchains": toolchains_view(toolchains),
    });
    if changes.is_empty() {
        value["note"] = json!("the generation changed nothing");
        return Ok(value);
    }
    value["checks"] = check(host, current, &files)?;
    let paths: Vec<String> = changes.iter().map(|change| change.path.clone()).collect();
    check_checkout(host, checkout, place, &paths)?;
    let approved = match approval {
        Approval::Given => true,
        Approval::Ask => host.approve(checkout, &diff)?,
    };
    value["approved"] = json!(approved);
    if approved {
        // The person may have committed or edited while they read the diff:
        // the checkout is checked again, and every target before any write.
        check_checkout(host, checkout, place, &paths)?;
        for path in &paths {
            host.check_target(checkout, path)?;
        }
        for file in &files {
            host.apply(checkout, file)?;
        }
        value["applied"] = json!(paths);
    }
    Ok(value)
}

/// Refuse to apply to `checkout` when its HEAD is no longer the commit the
/// place was made from (a commit would be reverted by the older content) or
/// its working tree has changes not committed to any of `paths` (a
/// person's edit would be overwritten).
fn check_checkout(
    host: &dyn SetupReviewHost,
    checkout: &Path,
    place: &SetupPlace,
    paths: &[String],
) -> Result<()> {
    let head = host.head(checkout)?;
    if head != place.commit {
        bail!(
            "the HEAD of {} moved to {head} while the settings were generated from {}, so \
             nothing is applied: run it again",
            checkout.display(),
            place.commit
        );
    }
    let dirty = host.uncommitted(checkout, paths)?;
    if !dirty.is_empty() {
        bail!(
            "the working tree of {} has changes not committed to {}, which the generation \
             changes too, so nothing is applied: commit or set them aside and run it again",
            checkout.display(),
            dirty.join(", ")
        );
    }
    Ok(())
}

/// The checks of what the job wrote, on the bytes read after it (the ones
/// shown and written), before the person sees it: the files it changed,
/// `dagq.toml`'s tables and its reading by the runtime, and each program
/// script's root. A failure is an error, and nothing is applied.
fn check(host: &dyn SetupReviewHost, current: &CurrentSetup, files: &[Generated]) -> Result<Value> {
    let changes: Vec<Change> = files.iter().map(|file| file.change.clone()).collect();
    check_paths(&changes).map_err(anyhow::Error::msg)?;
    let mut config = Value::Null;
    if let Some(file) = files.iter().find(|file| file.change.path == CONFIG_FILE) {
        let after = file.text()?;
        check_config(current.config.as_deref(), after).map_err(anyhow::Error::msg)?;
        if let Some(text) = after {
            host.check_config(text).map_err(|error| {
                anyhow::anyhow!(
                    "the generated {CONFIG_FILE} is not one the runtime reads, so nothing is \
                     applied: {error:#}"
                )
            })?;
        }
        config = json!("read");
    }
    let mut scripts = Vec::new();
    for file in files.iter().filter(|file| {
        file.change.status != ChangeStatus::Deleted && is_program_script(&file.change.path)
    }) {
        if !takes_root_from_cwd(file.text()?.unwrap_or_default()) {
            bail!(
                "{} does not take its root from the working directory's Git work tree (git \
                 rev-parse --show-toplevel), as a program review runs it from elsewhere, so \
                 nothing is applied",
                file.change.path
            );
        }
        scripts.push(json!({"path": file.change.path, "root_from_cwd": true}));
    }
    Ok(json!({"paths": "ok", "config": config, "scripts": scripts}))
}

#[cfg(test)]
mod tests {
    use std::cell::{Cell, RefCell};
    use std::collections::BTreeMap;

    use super::*;
    use crate::domain::toolchain::{BUILTIN, fake::REGISTRY, recommend::propose};

    /// A host whose job "writes" `written` (a path to its text, `None` to
    /// delete it) in its place.
    #[derive(Default)]
    struct Fake {
        current: CurrentSetup,
        moved: Cell<bool>,
        /// What changes in the checkout while the person reads the diff:
        /// HEAD moves (`true`), or a path gets an edit not committed.
        while_asked: Option<(bool, Option<&'static str>)>,
        /// A target path of the checkout that is a symbolic link.
        linked: Option<&'static str>,
        /// The diff Git gives, "the diff" without.
        diff: Option<&'static str>,
        /// The diff the person was shown.
        shown: RefCell<Option<String>>,
        /// How many times the job's files were read.
        reads: Cell<usize>,
        written: BTreeMap<String, Option<String>>,
        exit: Option<i32>,
        unreadable: bool,
        dirty: RefCell<Vec<String>>,
        approves: bool,
        prompts: RefCell<Vec<String>>,
        asked: RefCell<bool>,
        applied: RefCell<Vec<String>>,
        removed: RefCell<bool>,
        /// The commit's files.
        files: Vec<String>,
        /// The profiles its `dagq.toml` declares.
        declared: Vec<ToolchainDeclaration>,
    }

    fn fake() -> Fake {
        Fake {
            exit: Some(0),
            ..Fake::default()
        }
    }

    impl SetupReviewHost for Fake {
        fn checkout(&self) -> Result<PathBuf> {
            Ok(PathBuf::from("/repo"))
        }
        fn current(&self, _: &Path, commit: &str) -> Result<CurrentSetup> {
            assert_eq!(commit, "c0");
            Ok(self.current.clone())
        }
        fn head(&self, _: &Path) -> Result<String> {
            Ok(if self.moved.get() { "c1" } else { "c0" }.into())
        }
        fn prepare(&self, _: &Path) -> Result<SetupPlace> {
            Ok(SetupPlace {
                id: "s1".into(),
                tree: PathBuf::from("/q/setup-review/s1/tree"),
                dir: PathBuf::from("/q/setup-review/s1/job"),
                commit: "c0".into(),
                git_dir: PathBuf::from("/repo/.git/worktrees/tree"),
            })
        }
        fn run(&self, _: &SetupPlace, prompt: &str, actor: &ActorContext) -> Result<Option<i32>> {
            assert_eq!(actor.actor_id(), "setup-review-job:s1");
            self.prompts.borrow_mut().push(prompt.to_owned());
            Ok(self.exit)
        }
        fn changes(&self, _: &SetupPlace) -> Result<(Vec<Generated>, String)> {
            self.reads.set(self.reads.get() + 1);
            let files = self
                .written
                .iter()
                .map(|(path, text)| Generated {
                    change: Change {
                        path: path.clone(),
                        status: match text {
                            None => ChangeStatus::Deleted,
                            Some(_) => ChangeStatus::Added,
                        },
                    },
                    content: text
                        .as_ref()
                        .map(|text| (text.as_bytes().to_vec(), is_program_script(path))),
                })
                .collect();
            Ok((files, self.diff.unwrap_or("the diff").into()))
        }
        fn check_config(&self, _: &str) -> Result<()> {
            if self.unreadable {
                bail!("dagq.toml:2: unknown key");
            }
            Ok(())
        }
        fn toolchains(&self, _: &str) -> Result<Vec<ToolchainDeclaration>> {
            Ok(self.declared.clone())
        }
        fn files(&self, _: &Path, commit: &str) -> Result<Vec<String>> {
            assert_eq!(commit, "c0");
            Ok(self.files.clone())
        }
        fn uncommitted(&self, _: &Path, paths: &[String]) -> Result<Vec<String>> {
            Ok(self
                .dirty
                .borrow()
                .iter()
                .filter(|path| paths.contains(path))
                .cloned()
                .collect())
        }
        fn approve(&self, _: &Path, diff: &str) -> Result<bool> {
            *self.shown.borrow_mut() = Some(diff.to_owned());
            *self.asked.borrow_mut() = true;
            if let Some((moves, edits)) = self.while_asked {
                self.moved.set(moves);
                self.dirty.borrow_mut().extend(edits.map(str::to_owned));
            }
            Ok(self.approves)
        }
        fn check_target(&self, _: &Path, path: &str) -> Result<()> {
            if self.linked == Some(path) {
                bail!("{path} is a symbolic link in the checkout");
            }
            Ok(())
        }
        fn apply(&self, _: &Path, file: &Generated) -> Result<()> {
            // What is written is what the job's files were read as once.
            let written = self.written[&file.change.path].as_ref();
            assert_eq!(
                file.content.as_ref().map(|(bytes, _)| bytes.as_slice()),
                written.map(String::as_bytes)
            );
            self.applied.borrow_mut().push(file.change.path.clone());
            Ok(())
        }
        fn remove(&self, _: &Path, _: &SetupPlace) -> Result<()> {
            *self.removed.borrow_mut() = true;
            Ok(())
        }
    }

    const SCRIPT: &str = "#!/bin/sh\nroot=$(git rev-parse --show-toplevel) || exit 2\n";

    fn writes(host: &mut Fake, files: &[(&str, Option<&str>)]) {
        for (path, text) in files {
            host.written
                .insert((*path).to_owned(), text.map(str::to_owned));
        }
    }

    fn generated() -> Fake {
        let mut host = fake();
        writes(
            &mut host,
            &[
                (
                    "dagq.toml",
                    Some("[review.subagents.\"builtin:tests\"]\nenabled = true\n"),
                ),
                (".dagq/review/builtin-tests.md", Some("Tests live in t/.")),
                (".dagq/review/programs/check-docs.sh", Some(SCRIPT)),
            ],
        );
        host
    }

    fn error_of(host: &Fake, approval: Approval) -> String {
        format!("{:#}", setup_review(host, approval, &BUILTIN).unwrap_err())
    }

    #[test]
    fn init_recommends_the_defaults_and_the_command() {
        let toolchains = propose(&REGISTRY, Ok(Vec::new()), || Ok(vec!["fake.build".into()]));
        let value = recommended(&toolchains);
        // What the guessed profile recommends is shown, with what to do with it.
        assert_eq!(value["toolchains"]["from"], "guessed", "{value}");
        assert_eq!(
            value["toolchains"]["toolchains"],
            "[toolchains.fake]\nprofile = \"fake\"\n"
        );
        assert_eq!(
            value["toolchains"]["verify"][0]["commands"],
            json!(["fakebuild check"])
        );
        assert!(
            value["toolchains"]["note"]
                .as_str()
                .unwrap()
                .contains("Proposals only")
        );
        let broken = recommended(&propose(&REGISTRY, Err("dagq.toml:1: bad".into()), || {
            Ok(Vec::new())
        }));
        assert_eq!(broken["toolchains"], json!({"error": "dagq.toml:1: bad"}));
        assert_eq!(value["setup"], "dagq setup-review");
        assert_eq!(
            value["builtins"],
            json!([
                {"name": "builtin:acceptance", "enabled_by_default": true},
                {"name": "builtin:security", "enabled_by_default": true},
                {"name": "builtin:tests", "enabled_by_default": false},
                {"name": "builtin:secrets", "enabled_by_default": true},
            ])
        );
    }

    #[test]
    fn an_approved_generation_is_applied_and_a_declined_one_is_not() {
        let mut host = generated();
        host.approves = true;
        let value = setup_review(&host, Approval::Ask, &BUILTIN).unwrap();
        assert!(*host.asked.borrow());
        assert_eq!(value["approved"], json!(true));
        assert_eq!(
            *host.applied.borrow(),
            [
                ".dagq/review/builtin-tests.md",
                ".dagq/review/programs/check-docs.sh",
                "dagq.toml"
            ]
        );
        assert_eq!(value["applied"], json!(*host.applied.borrow()));
        // The files were read once: the bytes checked and shown are the
        // ones written.
        assert_eq!(host.reads.get(), 1);
        assert_eq!(
            value["checks"],
            json!({"paths": "ok", "config": "read", "scripts": [
                {"path": ".dagq/review/programs/check-docs.sh", "root_from_cwd": true}]})
        );
        assert!(*host.removed.borrow());

        let declined = generated();
        let value = setup_review(&declined, Approval::Ask, &BUILTIN).unwrap();
        assert_eq!(value["approved"], json!(false));
        assert_eq!(value["applied"], json!([]));
        assert!(declined.applied.borrow().is_empty());
        assert!(*declined.removed.borrow());

        // The flag approves without asking.
        let flagged = generated();
        let value = setup_review(&flagged, Approval::Given, &BUILTIN).unwrap();
        assert!(!*flagged.asked.borrow());
        assert_eq!(flagged.applied.borrow().len(), 3, "{value}");
    }

    #[test]
    fn a_change_outside_the_settings_is_refused_and_nothing_is_applied() {
        let mut outside = generated();
        writes(&mut outside, &[("src/lib.rs", Some("fn main() {}"))]);
        let mut other_table = generated();
        other_table.current.config = Some("[run.env]\nA = \"1\"\n".into());
        writes(
            &mut other_table,
            &[("dagq.toml", Some("[run.env]\nA = \"2\"\n"))],
        );
        let mut dirty = generated();
        dirty.dirty = RefCell::new(vec![".dagq/review/builtin-tests.md".into()]);
        let moved = generated();
        moved.moved.set(true);
        for (host, expected) in [
            (
                &moved,
                "moved to c1 while the settings were generated from c0",
            ),
            (
                &outside,
                "outside dagq.toml and .dagq/review/, so nothing is applied: src/lib.rs",
            ),
            (
                &other_table,
                "built-in agents' and the programs' tables, so nothing is applied: [run.env]",
            ),
            (
                &dirty,
                "changes not committed to .dagq/review/builtin-tests.md",
            ),
        ] {
            let error = error_of(host, Approval::Given);
            assert!(error.contains(expected), "{error}");
            assert!(host.applied.borrow().is_empty());
            assert!(!*host.asked.borrow());
            assert!(*host.removed.borrow());
        }
    }

    /// The checkout is checked again after the person approved: a commit or
    /// an edit made while they read the diff, or a target that is a
    /// symbolic link, stops the apply before any file is written.
    #[test]
    fn what_changes_while_the_person_reads_the_diff_or_a_linked_target_stops_the_apply() {
        for (while_asked, expected) in [
            (
                (true, None),
                "moved to c1 while the settings were generated from c0",
            ),
            (
                (false, Some("dagq.toml")),
                "changes not committed to dagq.toml",
            ),
        ] {
            let mut host = generated();
            host.approves = true;
            host.while_asked = Some(while_asked);
            let error = error_of(&host, Approval::Ask);
            assert!(error.contains(expected), "{error}");
            assert!(*host.asked.borrow());
            assert!(host.applied.borrow().is_empty());
            assert!(*host.removed.borrow());
        }
        let mut linked = generated();
        linked.linked = Some(".dagq/review/programs/check-docs.sh");
        let error = error_of(&linked, Approval::Given);
        assert!(error.contains("is a symbolic link"), "{error}");
        assert!(linked.applied.borrow().is_empty());
    }

    /// What the job wrote cannot redraw the terminal: the diff the person
    /// is shown and the output's carry its control characters as escapes.
    #[test]
    fn the_diff_is_shown_with_its_control_characters_escaped() {
        let mut host = generated();
        host.diff = Some("+Tests\x1b[1A\x1b[2K\rok\n");
        let value = setup_review(&host, Approval::Ask, &BUILTIN).unwrap();
        let escaped = "+Tests\\x1b[1A\\x1b[2K\\rok\n";
        assert_eq!(host.shown.borrow().as_deref(), Some(escaped));
        assert_eq!(value["diff"], escaped);
    }

    #[test]
    fn a_config_the_runtime_cannot_read_or_a_script_rooted_elsewhere_is_an_error_before_applying() {
        let mut unreadable = generated();
        unreadable.unreadable = true;
        let error = error_of(&unreadable, Approval::Ask);
        assert!(
            error.contains(
                "not one the runtime reads, so nothing is applied: dagq.toml:2: unknown key"
            ),
            "{error}"
        );
        assert!(!*unreadable.asked.borrow());
        assert!(unreadable.applied.borrow().is_empty());

        let mut rooted = generated();
        writes(
            &mut rooted,
            &[(
                ".dagq/review/programs/check-docs.sh",
                Some("#!/bin/sh\ncd \"$(dirname \"$0\")\"\n"),
            )],
        );
        let error = error_of(&rooted, Approval::Given);
        assert!(
            error.contains("check-docs.sh does not take its root"),
            "{error}"
        );
        assert!(rooted.applied.borrow().is_empty());
    }

    #[test]
    fn the_current_settings_go_into_the_prompt_and_a_failed_job_applies_nothing() {
        let mut host = generated();
        host.current = CurrentSetup {
            config: Some("[review.subagents.\"builtin:security\"]\nenabled = false\n".into()),
            files: vec![(
                ".dagq/review/builtin-acceptance.md".into(),
                "Check the changelog.".into(),
            )],
        };
        host.exit = Some(1);
        let error = error_of(&host, Approval::Given);
        assert!(error.contains("did not succeed (exit 1)"), "{error}");
        let prompt = &host.prompts.borrow()[0];
        assert!(prompt.contains("[review.subagents.\"builtin:security\"]\nenabled = false"));
        assert!(prompt.contains("Check the changelog."));
        assert!(host.applied.borrow().is_empty());
        assert!(*host.removed.borrow());

        let unchanged = fake();
        let value = setup_review(&unchanged, Approval::Ask, &BUILTIN).unwrap();
        assert_eq!(value["note"], "the generation changed nothing");
        assert!(!*unchanged.asked.borrow());
    }

    /// The ToolchainDeclaration of the fake profile over `tool/**`.
    fn fake_declared() -> Vec<ToolchainDeclaration> {
        vec![ToolchainDeclaration {
            name: "tool".into(),
            profile: "fake".into(),
            paths: vec!["tool/**".into()],
            line: 1,
        }]
    }

    #[test]
    fn the_profiles_review_programs_go_into_the_prompt_and_the_rest_is_only_shown() {
        // Guessed from the commit's files, as nothing is declared.
        let mut host = generated();
        host.files = vec!["tool/fake.build".into()];
        let value = setup_review(&host, Approval::Ask, &REGISTRY).unwrap();
        let prompt = host.prompts.borrow()[0].clone();
        assert!(prompt.contains("[review.programs.fake-lint]"), "{prompt}");
        assert!(prompt.contains(".dagq/review/programs/fake-lint.sh"));
        // Outside what the job may write: never given to it.
        assert!(!prompt.contains("fake-outside"), "{prompt}");
        // Neither the declaration nor the verification goes to the job.
        assert!(!prompt.contains("fakebuild check"));
        assert!(!prompt.contains("[toolchains."));
        let toolchains = &value["toolchains"];
        assert_eq!(toolchains["from"], "guessed", "{value}");
        assert_eq!(
            toolchains["toolchains"],
            "[toolchains.fake]\nprofile = \"fake\"\npaths = [\"tool/**\"]\n"
        );
        assert_eq!(
            toolchains["verify"][0]["commands"],
            json!(["fakebuild check"])
        );
        assert_eq!(toolchains["programs"][1]["name"], "fake-outside");
        assert_eq!(toolchains["programs"][1]["setup_review"], false);
        assert_eq!(value["approved"], false);
        assert!(host.applied.borrow().is_empty());

        // Declared: the declaration's profile, and nothing guessed.
        let mut declared = generated();
        declared.current.config = Some("[toolchains.tool]\nprofile = \"fake\"\n".into());
        declared.declared = fake_declared();
        writes(
            &mut declared,
            &[(
                "dagq.toml",
                Some(
                    "[toolchains.tool]\nprofile = \"fake\"\n\n[review.subagents.\"builtin:tests\"]\nenabled = true\n",
                ),
            )],
        );
        let value = setup_review(&declared, Approval::Given, &REGISTRY).unwrap();
        assert_eq!(value["toolchains"]["from"], "declared");
        assert_eq!(value["toolchains"]["toolchains"], Value::Null);
        assert!(declared.prompts.borrow()[0].contains("[review.programs.fake-lint]"));
        assert_eq!(value["applied"].as_array().unwrap().len(), 3);

        // The built-in profiles alone do not know the fake one: no program.
        let mut builtin = generated();
        builtin.files = vec!["tool/fake.build".into()];
        let value = setup_review(&builtin, Approval::Ask, &BUILTIN).unwrap();
        assert_eq!(value["toolchains"]["from"], Value::Null);
        assert!(!builtin.prompts.borrow()[0].contains("fake-lint"));
    }

    #[test]
    fn a_recommended_program_written_outside_the_settings_is_refused() {
        let mut host = generated();
        host.declared = fake_declared();
        host.current.config = Some("[toolchains.tool]\nprofile = \"fake\"\n".into());
        writes(
            &mut host,
            &[("scripts/fake-outside.sh", Some("#!/bin/sh\nexit 0\n"))],
        );
        let error = format!(
            "{:#}",
            setup_review(&host, Approval::Given, &REGISTRY).unwrap_err()
        );
        assert!(error.contains("scripts/fake-outside.sh"), "{error}");
        assert!(host.applied.borrow().is_empty());
        assert!(*host.removed.borrow());
    }

    /// A repository for `init`: its checkout, declarations and files, or
    /// the failure of each.
    struct Repository {
        checkout: Option<PathBuf>,
        declarations: Result<Vec<ToolchainDeclaration>, &'static str>,
        files: Result<Vec<String>, &'static str>,
    }

    impl InitRepository for Repository {
        fn checkout(&self) -> Result<PathBuf> {
            self.checkout
                .clone()
                .ok_or_else(|| anyhow::anyhow!("not a checkout"))
        }
        fn declarations(&self, checkout: &Path) -> Result<Vec<ToolchainDeclaration>> {
            assert_eq!(checkout, Path::new("/repo"));
            self.declarations.clone().map_err(anyhow::Error::msg)
        }
        fn files(&self, checkout: &Path) -> Result<Vec<String>> {
            assert_eq!(checkout, Path::new("/repo"));
            self.files.clone().map_err(anyhow::Error::msg)
        }
    }

    #[test]
    fn init_proposes_from_the_checkout_and_nothing_without_one() {
        let repository = |checkout: bool, declarations, files| Repository {
            checkout: checkout.then(|| PathBuf::from("/repo")),
            declarations,
            files,
        };
        // Guessed from the files while nothing is declared.
        let guessed = init_recommended(
            &repository(true, Ok(Vec::new()), Ok(vec!["fake.build".into()])),
            &REGISTRY,
        );
        assert_eq!(guessed["toolchains"]["from"], "guessed", "{guessed}");
        assert_eq!(guessed["setup"], "dagq setup-review");
        // Declared: the declaration, and the files are not needed.
        let declared = init_recommended(
            &repository(true, Ok(fake_declared()), Err("not read")),
            &REGISTRY,
        );
        assert_eq!(declared["toolchains"]["from"], "declared", "{declared}");
        // A dagq.toml that cannot be read, or files that cannot be listed,
        // is shown as the error.
        let broken = init_recommended(
            &repository(true, Err("dagq.toml:1: bad"), Ok(Vec::new())),
            &REGISTRY,
        );
        assert_eq!(broken["toolchains"], json!({"error": "dagq.toml:1: bad"}));
        let unlisted = init_recommended(
            &repository(true, Ok(Vec::new()), Err("git failed")),
            &REGISTRY,
        );
        assert_eq!(unlisted["toolchains"], json!({"error": "git failed"}));
        // No checkout: no profile, no error, and init still recommends.
        let outside = init_recommended(
            &repository(false, Err("not read"), Err("not read")),
            &REGISTRY,
        );
        assert_eq!(outside["toolchains"]["from"], Value::Null, "{outside}");
        assert!(outside["toolchains"].get("error").is_none());
        assert_eq!(outside["toolchains"]["programs"], json!([]));
        assert_eq!(outside["builtins"][0]["name"], "builtin:acceptance");
    }
}
