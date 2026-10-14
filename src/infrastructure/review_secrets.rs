//! `dagq review-program builtin:secrets BASE HEAD [PATH...]`: the process
//! of the built-in program `builtin:secrets` (ADR-t2259-1 decision 1). It
//! runs `git` in the working directory (the run's worktree) for what
//! `domain::review_programs::secrets` reads, keeps its output in memory
//! (no file holds the change's lines, so a job stopped by a signal leaves
//! none behind), prints the findings on stdout and returns the exit code:
//! 0 nothing found, 1 something found, 2 the range could not be read.

use std::io::Write;
use std::process::{Command, Stdio};

use crate::domain::review_programs::{BuiltinProgram, secrets};

/// Run the built-in program `name` with `args`, printing on stdout and
/// stderr; its exit code.
pub fn run(name: &str, args: &[String]) -> i32 {
    match BuiltinProgram::named(name) {
        Ok(Some(BuiltinProgram::Secrets)) => secrets_run(args),
        Ok(None) => fail(&format!("{name} is no built-in program")),
        Err(why) => fail(&why),
    }
}

fn secrets_run(args: &[String]) -> i32 {
    let [base, head, only @ ..] = args else {
        return fail(
            "builtin:secrets: usage: dagq review-program builtin:secrets BASE HEAD [PATH...]",
        );
    };
    let diff_args = match secrets::diff_args(base, head) {
        Ok(diff_args) => diff_args,
        Err(why) => return fail(&format!("builtin:secrets: {why}")),
    };
    let output = match Command::new("git")
        .args(&diff_args)
        .stdin(Stdio::null())
        .output()
    {
        Ok(output) => output,
        Err(error) => return fail(&format!("builtin:secrets: git could not start: {error}")),
    };
    if !output.status.success() {
        return fail(&format!(
            "builtin:secrets: the diff {base}..{head} could not be read ({}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim_end()
        ));
    }
    let diff = String::from_utf8_lossy(&output.stdout);
    let (text, code) = secrets::outcome(&secrets::scan(&diff, only));
    let _ = std::io::stdout().lock().write_all(text.as_bytes());
    code
}

fn fail(why: &str) -> i32 {
    eprintln!("{why}");
    2
}
