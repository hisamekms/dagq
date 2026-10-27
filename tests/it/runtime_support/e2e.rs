//! The e2e the runtime runs on the host before a run lands (ADR-t1233-2)
//! in the runtime tests: the `[e2e] paths` of the main checkout and a stub
//! in place of `cargo test --locked --test e2e -- --ignored`.
use super::*;

/// Write `[e2e] paths` of `dagq.toml` in the main checkout and commit it
/// (ADR-t963-1 decision 2).
pub fn with_e2e_paths(repo: &Path, globs: &str) {
    fs::write(repo.join("dagq.toml"), format!("[e2e]\npaths = {globs}\n")).unwrap();
    git(repo, &["add", "dagq.toml"]);
    git(repo, &["commit", "-q", "-m", "e2e paths"]);
}

/// A stub e2e: it appends what it ran on (the directory, the head and the
/// tests rerun by name) to `ran`, passes in a worktree with `fixed.txt`
/// and otherwise fails `a_test` as libtest prints it.
pub fn stub_e2e(ran: &Path) -> String {
    format!(
        "printf 'pwd=%s head=%s rerun=%s\\n' \"$PWD\" \"$(git rev-parse HEAD)\" \"${{DAGQ_E2E_RERUN:-}}\" >> {ran}; \
         if [ -f fixed.txt ]; then echo 'test result: ok. 1 passed; 0 failed'; exit 0; fi; \
         echo 'test a_test ... FAILED'; echo; echo 'test result: FAILED. 0 passed; 1 failed'; exit 101",
        ran = shell_join(&[ran.display().to_string()]),
    )
}

/// The options of a supervisor (4 slots, once) whose e2e is `command`.
pub fn e2e_options(command: String) -> SuperviseOptions {
    SuperviseOptions {
        run_e2e: runtime::RunE2eOptions {
            command: Some(command),
            ..Default::default()
        },
        ..supervise_options(4, true)
    }
}
