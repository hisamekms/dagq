//! The cases of the built-in agents' eval (ADR-t2259-1 decision 10): a
//! dev and a holdout list for each of [`BuiltinAgent::ALL`] (no
//! `builtin:docs`, which is not carried, ADR-t2389-1; no
//! [`super::ACCEPTANCE`], whose verdict is the whole review's and no agent
//! job's), carried by the binary beside the definitions, in
//! `src/domain/review_builtins/evals/`:
//!
//! - `<short>.dev.json` and `<short>.holdout.json`: lists in the shape of a
//!   project's `.dagq/agents/<name>/evals/` ([`crate::domain::agent_eval`]'s
//!   `CaseFile`), each case with a `baseline` in place of `base_commit`.
//! - `baselines/<name>/`: the files of a baseline, the small project a
//!   case's patch applies to. Cases share them.
//! - `patches/<sha256>.patch`: each patch once, named by its content's
//!   SHA-256, as a project's patches are.
//!
//! The cases are generic changes of two small projects (a Python service
//! and a JavaScript library): each list has cases a built-in must flag
//! and cases it must not, none assumes a language, tool or document rule
//! of dagq's own development (the words the test beside them lists). Dev has one
//! violation per rule code and two clean cases, run once each (`k` 1, six
//! runs a round); holdout has two violations and two clean cases of other
//! changes, run twice each (`k` 2, eight runs), so its once-only round is
//! less a matter of one answer. A round of these cases is judged against
//! [`THRESHOLD`], not the project's `[eval] threshold`: the built-ins are
//! measured the same in every project.
//!
//! A case's tree is no commit of the repository the eval runs in: the
//! supervisor makes a repository of its own from the baseline and the
//! patch for each case (`Repository::add_baseline_case_tree`), so the cases
//! run in a repository that has none of dagq's history. The digest of a
//! round's cases ([`case_bytes`]) is made of each one's baseline's files
//! and its patch.

use std::collections::BTreeSet;

use super::BuiltinAgent;
use crate::domain::agent_eval::{
    AgentCases, CaseProblem, DEFAULT_THRESHOLD, Known, Split, read_case_file_in,
};

/// The threshold a built-in's round is judged against, whatever the
/// project's `[eval]` says.
pub const THRESHOLD: f64 = DEFAULT_THRESHOLD;

/// Each baseline by name: its files, each a path relative to the
/// baseline's root and the text.
const BASELINES: &[(&str, &[(&str, &str)])] = &[
    (
        "notes-service",
        &[
            (
                "README.md",
                include_str!("evals/baselines/notes-service/README.md"),
            ),
            (
                "notes/api.py",
                include_str!("evals/baselines/notes-service/notes/api.py"),
            ),
            (
                "notes/settings.py",
                include_str!("evals/baselines/notes-service/notes/settings.py"),
            ),
            (
                "notes/store.py",
                include_str!("evals/baselines/notes-service/notes/store.py"),
            ),
            (
                "tests/test_store.py",
                include_str!("evals/baselines/notes-service/tests/test_store.py"),
            ),
        ],
    ),
    (
        "stock-ledger",
        &[
            (
                "README.md",
                include_str!("evals/baselines/stock-ledger/README.md"),
            ),
            (
                "package.json",
                include_str!("evals/baselines/stock-ledger/package.json"),
            ),
            (
                "src/ledger.js",
                include_str!("evals/baselines/stock-ledger/src/ledger.js"),
            ),
            (
                "test/ledger.test.js",
                include_str!("evals/baselines/stock-ledger/test/ledger.test.js"),
            ),
        ],
    ),
];

/// Each patch by its content's SHA-256.
const PATCHES: &[(&str, &str)] = &[
    (
        "0036ffcdbdb004d927ec5f3b283ef7ecc18367fe52623a92864f916cb5b5fd32",
        include_str!(
            "evals/patches/0036ffcdbdb004d927ec5f3b283ef7ecc18367fe52623a92864f916cb5b5fd32.patch"
        ),
    ),
    (
        "23c8fdf03bd5e56f8d1f371fe31bd197b938fbf04d06296ed49f69125014678f",
        include_str!(
            "evals/patches/23c8fdf03bd5e56f8d1f371fe31bd197b938fbf04d06296ed49f69125014678f.patch"
        ),
    ),
    (
        "2c34dae5092223a779a41cd897dbb187b616d34c2e434ffaaa979833d08902da",
        include_str!(
            "evals/patches/2c34dae5092223a779a41cd897dbb187b616d34c2e434ffaaa979833d08902da.patch"
        ),
    ),
    (
        "333065ee6943235a4d0ae408c916bff6a872322fb343bb68eb557c44dd001eac",
        include_str!(
            "evals/patches/333065ee6943235a4d0ae408c916bff6a872322fb343bb68eb557c44dd001eac.patch"
        ),
    ),
    (
        "3655f2d9f377275a45ddf299422d94a4bc15dc225b5b03567b309b60e6477a41",
        include_str!(
            "evals/patches/3655f2d9f377275a45ddf299422d94a4bc15dc225b5b03567b309b60e6477a41.patch"
        ),
    ),
    (
        "4852c02f1a71baea7648494ef41393575d6f90dd4512c0b48d3c46454c17d320",
        include_str!(
            "evals/patches/4852c02f1a71baea7648494ef41393575d6f90dd4512c0b48d3c46454c17d320.patch"
        ),
    ),
    (
        "4ea3c79e4e7e2a607a111b8cf7677df6cab5c9d4390ec27ed2e391d2ecd62e4a",
        include_str!(
            "evals/patches/4ea3c79e4e7e2a607a111b8cf7677df6cab5c9d4390ec27ed2e391d2ecd62e4a.patch"
        ),
    ),
    (
        "58a92a221c22bcb7add0603e9552ac34a763748f371b5289d93de551b5a9127f",
        include_str!(
            "evals/patches/58a92a221c22bcb7add0603e9552ac34a763748f371b5289d93de551b5a9127f.patch"
        ),
    ),
    (
        "6cdb7d920c989518ae159323912585c956820d5a650d9f808dc0978f98044822",
        include_str!(
            "evals/patches/6cdb7d920c989518ae159323912585c956820d5a650d9f808dc0978f98044822.patch"
        ),
    ),
    (
        "7721f0a3d727b6ffdbcd5e4e2f5bf92537fc8ff46ca516d1190551cdf87c6ac3",
        include_str!(
            "evals/patches/7721f0a3d727b6ffdbcd5e4e2f5bf92537fc8ff46ca516d1190551cdf87c6ac3.patch"
        ),
    ),
    (
        "7b84feaa00d3e16cae5215a9ebe0fd4a97fbd6a23fc0d9fccab76243f80527fd",
        include_str!(
            "evals/patches/7b84feaa00d3e16cae5215a9ebe0fd4a97fbd6a23fc0d9fccab76243f80527fd.patch"
        ),
    ),
    (
        "7e42097fb014ff781440003f99f95db404e45a6ce75a2919c528032f3be45778",
        include_str!(
            "evals/patches/7e42097fb014ff781440003f99f95db404e45a6ce75a2919c528032f3be45778.patch"
        ),
    ),
    (
        "8464112909a437787839431ebece6854fb65900bec9f11c2e4513a05fc3851be",
        include_str!(
            "evals/patches/8464112909a437787839431ebece6854fb65900bec9f11c2e4513a05fc3851be.patch"
        ),
    ),
    (
        "a47de4ea233aafed9731cd4c26075c06af4ddee0c737a34ee4d84deb41f61dd1",
        include_str!(
            "evals/patches/a47de4ea233aafed9731cd4c26075c06af4ddee0c737a34ee4d84deb41f61dd1.patch"
        ),
    ),
    (
        "adb58b5cfe67dc007f8fc105ecef20fe49516e9d0a9b7729bc472d474b45a3c7",
        include_str!(
            "evals/patches/adb58b5cfe67dc007f8fc105ecef20fe49516e9d0a9b7729bc472d474b45a3c7.patch"
        ),
    ),
    (
        "b58c4fdd86fff80c66ee932e309879119eac926be42e4c65283dd2f85d0a0e6a",
        include_str!(
            "evals/patches/b58c4fdd86fff80c66ee932e309879119eac926be42e4c65283dd2f85d0a0e6a.patch"
        ),
    ),
    (
        "bcae051af58694af494832522db1dd5e4785da4b4c6cd1ba16a6ed6d76247032",
        include_str!(
            "evals/patches/bcae051af58694af494832522db1dd5e4785da4b4c6cd1ba16a6ed6d76247032.patch"
        ),
    ),
    (
        "d5263daf97e85b59033a9606f2bb096f99d9809d50f99fd53a507f57f9099b66",
        include_str!(
            "evals/patches/d5263daf97e85b59033a9606f2bb096f99d9809d50f99fd53a507f57f9099b66.patch"
        ),
    ),
    (
        "e1c96f9ceee423bf7c793b21180b93873b1d161e68efdfb4de664d4362fb34f0",
        include_str!(
            "evals/patches/e1c96f9ceee423bf7c793b21180b93873b1d161e68efdfb4de664d4362fb34f0.patch"
        ),
    ),
    (
        "e2b90e534264370a4daaa9f14ec06bf387d477d1722d4416de1a40405bd6f24f",
        include_str!(
            "evals/patches/e2b90e534264370a4daaa9f14ec06bf387d477d1722d4416de1a40405bd6f24f.patch"
        ),
    ),
];

/// The splits the binary carries for each built-in.
pub const SPLITS: [Split; 2] = [Split::Dev, Split::Holdout];

/// The text of `agent`'s list of `split`, `None` for a split not carried
/// (production: a built-in's production cases are a set of the queue's
/// side, `agent eval --set`).
pub const fn list_text(agent: BuiltinAgent, split: Split) -> Option<&'static str> {
    match (agent, split) {
        (BuiltinAgent::Security, Split::Dev) => Some(include_str!("evals/security.dev.json")),
        (BuiltinAgent::Security, Split::Holdout) => {
            Some(include_str!("evals/security.holdout.json"))
        }
        (BuiltinAgent::Tests, Split::Dev) => Some(include_str!("evals/tests.dev.json")),
        (BuiltinAgent::Tests, Split::Holdout) => Some(include_str!("evals/tests.holdout.json")),
        (_, Split::Production) => None,
    }
}

/// The files of the baseline `name`, `None` for none of that name.
pub fn baseline(name: &str) -> Option<&'static [(&'static str, &'static str)]> {
    BASELINES
        .iter()
        .find(|(known, _)| *known == name)
        .map(|(_, files)| *files)
}

/// The patch whose SHA-256 is `hash`, `None` for none.
pub fn patch(hash: &str) -> Option<&'static str> {
    PATCHES
        .iter()
        .find(|(known, _)| *known == hash)
        .map(|(_, text)| *text)
}

/// `agent`'s carried lists, read as a project's are, their cases' patches
/// and baselines among those carried.
pub fn cases(agent: BuiltinAgent) -> Result<AgentCases, Vec<CaseProblem>> {
    let patches: BTreeSet<String> = PATCHES.iter().map(|(hash, _)| (*hash).to_owned()).collect();
    let baselines: BTreeSet<String> = BASELINES
        .iter()
        .map(|(name, _)| (*name).to_owned())
        .collect();
    let known = Known {
        patches: &patches,
        baselines: &baselines,
    };
    let mut problems = Vec::new();
    let mut files = Vec::new();
    for split in SPLITS {
        let text = list_text(agent, split).expect("a carried split");
        match read_case_file_in(&agent.name(), split.file_name(), text, known) {
            Ok(file) => files.push(file),
            Err(found) => problems.extend(found),
        }
    }
    if problems.is_empty() {
        Ok(AgentCases {
            agent: agent.name(),
            files,
        })
    } else {
        Err(problems)
    }
}

/// What a case of `files` (a baseline's) and `patch` weighs in a round's
/// case set digest: each file's path and text, in the paths' order, then
/// the patch, each with its length, so that changing any of them changes
/// the digest.
pub fn case_bytes(files: &[(&str, &str)], patch: &str) -> Vec<u8> {
    let mut sorted: Vec<&(&str, &str)> = files.iter().collect();
    sorted.sort_by_key(|(path, _)| *path);
    let mut bytes = Vec::new();
    for (path, text) in sorted {
        for part in [*path, *text] {
            bytes.extend_from_slice(format!("{}\n", part.len()).as_bytes());
            bytes.extend_from_slice(part.as_bytes());
        }
    }
    bytes.extend_from_slice(format!("patch {}\n", patch.len()).as_bytes());
    bytes.extend_from_slice(patch.as_bytes());
    bytes
}

#[cfg(test)]
mod tests {
    use sha2::{Digest, Sha256};

    use super::*;
    use crate::domain::agent_eval::review::Judgement;
    use crate::domain::agent_eval::{CaseBase, unreferenced_patches};

    /// Words no carried case's text may hold, lowercase: the language,
    /// tools and document rules of dagq's own development (goal 188's
    /// constraint), which a built-in must not be measured on.
    pub(super) const FORBIDDEN: &[&str] = &[
        "rust",
        "cargo",
        "clippy",
        "nextest",
        ".rs",
        "crate",
        "dagq",
        "adr-",
        "adr ",
        "docs/design",
        "docs/development",
        "agents.md",
        "claude.md",
        "receipt",
        "worker",
    ];

    /// Each built-in has a dev and a holdout list that read, each with
    /// cases it must flag and cases it must not, every case on a carried
    /// baseline and patch, never a commit (acceptance 1).
    #[test]
    fn each_builtin_carries_dev_and_holdout_with_both_kinds_of_cases() {
        for agent in BuiltinAgent::ALL {
            let read = cases(agent).unwrap_or_else(|problems| {
                panic!(
                    "{}: {}",
                    agent.name(),
                    problems
                        .iter()
                        .map(ToString::to_string)
                        .collect::<Vec<_>>()
                        .join("; ")
                )
            });
            for split in SPLITS {
                let list = read.split(split).expect("a carried split");
                assert_eq!(list.agent, agent.name());
                let verdicts: BTreeSet<&str> = list
                    .cases
                    .iter()
                    .map(|case| case.review().unwrap().expected.verdict.as_str())
                    .collect();
                assert_eq!(
                    verdicts,
                    BTreeSet::from([Judgement::Violation.as_str(), Judgement::Clean.as_str()]),
                    "{} {}",
                    agent.name(),
                    split.as_str()
                );
                for case in &list.cases {
                    let CaseBase::Baseline(name) = &case.base else {
                        panic!("{} has a base commit", case.id)
                    };
                    assert!(baseline(name).is_some(), "{name}");
                    assert!(patch(&case.patch).is_some(), "{}", case.id);
                }
            }
            assert!(list_text(agent, Split::Production).is_none());
        }
    }

    /// Each carried patch is named by its content's SHA-256 and some case
    /// uses it; each baseline is some case's.
    #[test]
    fn every_carried_patch_and_baseline_is_used_and_named_by_its_content() {
        let lists: Vec<_> = BuiltinAgent::ALL
            .into_iter()
            .flat_map(|agent| cases(agent).unwrap().files)
            .collect();
        for (hash, text) in PATCHES {
            assert_eq!(*hash, format!("{:x}", Sha256::digest(text.as_bytes())));
        }
        let hashes: BTreeSet<String> = PATCHES.iter().map(|(hash, _)| (*hash).to_owned()).collect();
        assert_eq!(unreferenced_patches(&lists, &hashes), Vec::<String>::new());
        for (name, _) in BASELINES {
            assert!(
                lists
                    .iter()
                    .flat_map(|list| &list.cases)
                    .any(|case| matches!(&case.base, CaseBase::Baseline(used) if used == name)),
                "{name} is no case's"
            );
        }
    }

    /// No carried text (a list with its expectations, a patch, a
    /// baseline's file or its path) holds a word of [`FORBIDDEN`]
    /// (acceptance 7).
    #[test]
    fn the_carried_cases_assume_nothing_of_dagqs_own_development() {
        let mut texts: Vec<(String, &str)> = Vec::new();
        for agent in BuiltinAgent::ALL {
            for split in SPLITS {
                texts.push((
                    format!("{} {}", agent.name(), split.as_str()),
                    list_text(agent, split).unwrap(),
                ));
            }
        }
        for (hash, text) in PATCHES {
            texts.push(((*hash).to_owned(), text));
        }
        for (name, files) in BASELINES {
            for (path, text) in *files {
                texts.push((format!("{name}/{path}"), path));
                texts.push((format!("{name}/{path}"), text));
            }
        }
        for (place, text) in texts {
            // The lists name their agent `builtin:<short>`, which is no
            // project's word.
            let lower = text.to_lowercase();
            for word in FORBIDDEN {
                assert!(!lower.contains(word), "{word:?} in {place}");
            }
        }
    }

    /// A case's weight in the digest changes with any file of its baseline
    /// and with its patch (acceptance 3).
    #[test]
    fn a_cases_bytes_change_with_its_baseline_and_its_patch() {
        let files = [("a.py", "x = 1\n"), ("b.py", "y = 2\n")];
        let same = case_bytes(&[files[1], files[0]], "p");
        assert_eq!(
            case_bytes(&files, "p"),
            same,
            "the files' order is the paths'"
        );
        assert_ne!(case_bytes(&[("a.py", "x = 2\n"), files[1]], "p"), same);
        assert_ne!(case_bytes(&[("c.py", "x = 1\n"), files[1]], "p"), same);
        assert_ne!(case_bytes(&files, "q"), same);
        assert_ne!(
            case_bytes(&[("a.py", "x = 1\nb.py")], "p"),
            case_bytes(&[("a.py", "x = 1\n"), ("b.py", "")], "p")
        );
    }
}
