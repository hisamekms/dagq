//! The agents' eval cases of this repository (ADR-t1728-1): every
//! `.dagq/agents/<name>/evals/` reads with the domain's reader
//! (`dagq::domain::agent_eval`), each case's patch is in the shared store
//! under the SHA-256 of its content, and the store has no patch no case
//! refers to. The reader's decisions are unit tests in the module; this
//! test is the boundary with the repository's files.
use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

use dagq::domain::agent_eval::{
    self, CaseFile, EVALS_DIR, PATCH_DIR, read_agent_cases, unreferenced_patches,
};
use dagq::domain::review_subagents::DEFINITION_DIR;
use sha2::{Digest, Sha256};

#[test]
fn every_case_list_of_this_repository_reads() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut patches = BTreeSet::new();
    for entry in fs::read_dir(root.join(PATCH_DIR)).unwrap() {
        let path = entry.unwrap().path();
        let name = path.file_name().unwrap().to_str().unwrap().to_owned();
        let hash = agent_eval::patch_hash(&name)
            .unwrap_or_else(|| panic!("{PATCH_DIR}/{name} is not <sha256>.patch"));
        let content = fs::read(&path).unwrap();
        assert_eq!(
            format!("{:x}", Sha256::digest(&content)),
            hash,
            "{PATCH_DIR}/{name} is not named after its content's SHA-256"
        );
        patches.insert(hash.to_owned());
    }

    let mut lists: Vec<CaseFile> = Vec::new();
    let mut agents = Vec::new();
    for entry in fs::read_dir(root.join(DEFINITION_DIR)).unwrap() {
        let dir = entry.unwrap().path().join(EVALS_DIR);
        if !dir.is_dir() {
            continue;
        }
        let agent = dir.parent().unwrap().file_name().unwrap().to_str().unwrap();
        let files: Vec<(String, String)> = fs::read_dir(&dir)
            .unwrap()
            .map(|file| {
                let path = file.unwrap().path();
                let name = path.file_name().unwrap().to_str().unwrap().to_owned();
                (name, fs::read_to_string(&path).unwrap())
            })
            .collect();
        let files: Vec<(&str, &str)> = files
            .iter()
            .map(|(name, text)| (name.as_str(), text.as_str()))
            .collect();
        let cases = read_agent_cases(agent, &files, &patches).unwrap_or_else(|problems| {
            let lines: Vec<String> = problems.iter().map(ToString::to_string).collect();
            panic!(
                "{DEFINITION_DIR}/{agent}/{EVALS_DIR}:\n{}",
                lines.join("\n")
            )
        });
        agents.push(agent.to_owned());
        lists.extend(cases.files);
    }

    assert!(
        !agents.is_empty(),
        "no agent of {DEFINITION_DIR} has {EVALS_DIR}/"
    );
    assert_eq!(
        unreferenced_patches(&lists, &patches),
        Vec::<String>::new(),
        "patches of {PATCH_DIR} no case refers to"
    );
}
