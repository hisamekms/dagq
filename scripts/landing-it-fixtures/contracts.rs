// Runtime contracts for the landing shell's actual lib-gate failure output
// and this repository's review globs. Run after building the library:
// rustc --edition=2024 scripts/landing-it-fixtures/contracts.rs \
//   -L dependency=target/debug/deps --extern dagq=target/debug/libdagq.rlib \
//   -o "$TMPDIR/landing-contracts"
// "$TMPDIR/landing-contracts" "$PWD" "$TMPDIR/landing-lib-failure.log"
// This is separate from the shell's lightweight --self-test: no queue,
// nextest or extra test run is needed to exercise the real classifier.
use dagq::domain::{
    scope::glob_matches,
    toolchain::{BUILTIN, Toolchains},
    verify_failure::{FailureClass, classify},
};
use dagq::infrastructure::run_env::parse_config;
use std::{fs, path::PathBuf, process::Command};

fn main() {
    let mut args = std::env::args_os().skip(1);
    let root = PathBuf::from(args.next().expect("repository root"));
    let log = PathBuf::from(args.next().expect("failure log path"));
    let output = Command::new("sh")
        .args(["scripts/landing-it.sh", "--self-test"])
        .current_dir(&root)
        .env("LANDING_IT_SELF_TEST_FAILURE_LOG", &log)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let text = fs::read_to_string(&log).unwrap();
    assert!(text.contains("check-lib-test-time: dagq tests::slow took 6.250s, over 5s"));
    let config = parse_config(&fs::read_to_string(root.join("dagq.toml")).unwrap()).unwrap();
    let toolchains = Toolchains::resolve(&BUILTIN, &config.toolchains)
        .unwrap()
        .with_marks(config.verify_marks);
    let failure = classify(
        &toolchains,
        "sh scripts/landing-it.sh",
        Some(1),
        None,
        &text,
    );
    assert_eq!(failure.class, FailureClass::Unknown);
    assert!(!failure.class.is_environmental(), "{failure:?}");
    println!(
        "lib gate output: {} (code, returned to worker)",
        failure.class.as_str()
    );

    let rules = config
        .review_subagents
        .iter()
        .find(|a| a.name == "test-rules")
        .unwrap();
    for path in [
        ".config/lib-slow-allow.toml",
        ".config/lib-slow-allow.d/t4949.toml",
    ] {
        assert!(
            rules.paths.iter().any(|glob| glob_matches(glob, path)),
            "{path}"
        );
    }
    assert!(
        !rules
            .paths
            .iter()
            .any(|glob| glob_matches(glob, ".config/unrelated.toml"))
    );
    let gates = config.local_gates.unwrap();
    let lib = gates
        .gates
        .iter()
        .find(|g| g.name == "lib-test-time")
        .unwrap();
    assert_eq!(lib.kind, "lib-test-time");
    assert_eq!(lib.paths, ["src/**"]);
    println!("lib allow-list-only changes match test-rules; lib local gate is declared");
}
