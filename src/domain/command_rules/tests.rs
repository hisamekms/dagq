use serde_json::json;

use super::*;
use crate::domain::command_shape::{DigestKey, shape_value_with};
use crate::domain::session_step::StepKind;
use crate::domain::toolchain::{
    BUILTIN, ProfileRegistry, ToolchainDeclaration, Toolchains, fake::REGISTRY,
};
use crate::domain::worktime;

fn declare(registry: &ProfileRegistry, profiles: &[&str]) -> Toolchains {
    let declarations: Vec<ToolchainDeclaration> = profiles
        .iter()
        .enumerate()
        .map(|(at, profile)| ToolchainDeclaration {
            name: (*profile).to_owned(),
            profile: (*profile).to_owned(),
            paths: Vec::new(),
            line: at + 1,
        })
        .collect();
    Toolchains::resolve(registry, &declarations).unwrap()
}

fn rust() -> Toolchains {
    declare(&BUILTIN, &["rust"])
}

fn with_rules(toolchains: Toolchains, commands: Vec<CommandRule>) -> Toolchains {
    toolchains.with_telemetry(TelemetrySettings {
        commands,
        ..TelemetrySettings::default()
    })
}

fn classifier(toolchains: &Toolchains) -> CommandClassifier {
    CommandClassifier::of_text(effective_rules(toolchains), toolchains)
}

/// The class and the group of `command`, `None` unclassified.
fn class_of(classifier: &CommandClassifier, command: &str) -> Option<(&'static str, &'static str)> {
    classifier
        .classify_text(command)
        .map(|matched| (matched.class, matched.group))
}

/// The representative commands: each with the class and the group the Rust
/// profile's rules, the common ones and the project's for its test targets
/// give it (`None` unclassified, the work breakdown's `other_command`).
const TABLE: &[(&str, Option<(&str, &str)>)] = &[
    ("cargo test --locked --test it", Some(("full_test", "test"))),
    ("cargo test --locked --test plugin", Some(("test", "test"))),
    (
        "cargo test --locked --test it runtime_claim::",
        Some(("test", "test")),
    ),
    ("cargo test --locked --test e2e", Some(("e2e", "e2e"))),
    ("cargo test --locked --test=e2e", Some(("e2e", "e2e"))),
    ("cargo test --locked", Some(("full_test", "test"))),
    ("cargo nextest run --locked", Some(("full_test", "test"))),
    (
        "cargo test --locked -- --nocapture",
        Some(("full_test", "test")),
    ),
    (
        "cargo test --locked 2>&1 | tail -5",
        Some(("full_test", "test")),
    ),
    ("cargo test --lib", Some(("test", "test"))),
    ("cargo test -p dagq", Some(("test", "test"))),
    (
        "cargo llvm-cov nextest --locked --workspace --fail-under-lines 80",
        Some(("llvm_cov", "llvm_cov")),
    ),
    ("cargo +nightly fmt --all --check", Some(("fmt", "fmt"))),
    (
        "cargo clippy --locked --all-targets -- -D warnings",
        Some(("build", "build")),
    ),
    ("cargo build", Some(("build", "build"))),
    ("sleep 30", Some(("wait", "wait"))),
    ("until test -f x; do sleep 1; done", Some(("wait", "wait"))),
    ("sysctl -n vm.loadavg", Some(("wait", "wait"))),
    ("dagq status", Some(("dagq", "dagq"))),
    ("git log", Some(("git", "git"))),
    ("ls", None),
];

/// (2b) Every row of the representative table takes its class and group
/// from the Rust profile's rules and the common ones with the project's
/// rules for its test targets over them, as this repository's `dagq.toml`
/// declares: the group its work breakdown's category, and the classes of
/// `integrate`'s checks those of its class.
#[test]
fn the_representative_commands_take_the_classes_of_this_repositorys_rules() {
    let toolchains = with_rules(rust(), project_rules());
    let classifier = classifier(&toolchains);
    let rules = work_rules(&toolchains);
    for (command, expected) in TABLE {
        assert_eq!(class_of(&classifier, command), *expected, "{command}");
        let group = expected.map_or(worktime::OTHER_COMMAND, |(_, group)| group);
        assert_eq!(worktime::label(command, &rules).0, group, "{command}");
        let checks: Vec<&str> = expected
            .map(|(class, _)| class)
            .filter(|class| CHECK_CLASSES.contains(class))
            .into_iter()
            .collect();
        assert_eq!(
            worktime::check_classes(command, &rules),
            checks,
            "{command}"
        );
    }
}

/// The Rust profile's own rules know cargo alone, never a project's test
/// target.
#[test]
fn the_rust_profile_names_no_test_target() {
    for rule in crate::domain::toolchain::rust::cargo_rules() {
        assert_eq!(rule.program, "cargo");
        for (_, values) in &rule.flag_values {
            assert!(!values.iter().any(|v| v == "it" || v == "e2e"), "{rule:?}");
        }
        assert!(rule.args_any.is_empty(), "{rule:?}");
    }
    // Without the project's rules `--test it` and `--test e2e` are tests.
    let classifier = classifier(&rust());
    assert_eq!(
        class_of(&classifier, "cargo test --locked --test it"),
        Some(("test", "test"))
    );
    assert_eq!(
        class_of(&classifier, "cargo test --locked --test e2e"),
        Some(("test", "test"))
    );
}

fn shape(command: &str, known: &Subcommands) -> CommandShape {
    serde_json::from_value(shape_value_with(command, known).unwrap()).unwrap()
}

/// A program no list names keeps every word as a digest, and its
/// `--name=value` as a flag of digests, up to a `--` it keeps as a known
/// program's: a rule's flag means the same with its value written apart or
/// after `=`, matched or refused alike, and no word after `--` is a flag or
/// a positional word.
#[test]
fn an_unknown_programs_flag_holds_written_apart_or_with_its_value() {
    let known = Subcommands::of(&rust()).keyed(Some(DigestKey::test()));
    let matches = |rule: CommandRule, command: &str| {
        let classifier = CommandClassifier::new(
            EffectiveRules {
                rules: vec![(RuleSource::Project, rule)],
            },
            known.clone(),
        );
        classifier.classify(&shape(command, &known)).is_some()
    };
    let mode = || {
        CommandRule::new("x", "mytool")
            .flag_values("--mode", &["fast"])
            .takes_value(&["--mode"])
    };
    for command in ["mytool --mode fast", "mytool --mode=fast"] {
        assert!(matches(mode(), command), "{command}");
        assert!(matches(mode().positional_none(), command), "{command}");
        assert!(
            matches(CommandRule::new("x", "mytool").flags(&["--mode"]), command),
            "{command}"
        );
        assert!(
            !matches(
                CommandRule::new("x", "mytool").none_of(&["--mode"]),
                command
            ),
            "{command}"
        );
    }
    for command in ["mytool --mode slow", "mytool --mode=slow"] {
        assert!(!matches(mode(), command), "{command}");
    }
    assert!(matches(
        CommandRule::new("x", "mytool").none_of(&["--mode"]),
        "mytool --level=fast"
    ));
    assert!(!matches(
        CommandRule::new("x", "mytool").flags(&["--mode"]),
        "mytool -- --mode=fast"
    ));
    assert!(!matches(
        CommandRule::new("x", "mytool").flags(&["--mode"]),
        "mytool -- --mode fast"
    ));
    // positional none: the words after `--` are not counted, as a known
    // program's are not.
    let bare = || {
        CommandRule::new("x", "mytool")
            .subcommands(&["test"])
            .positional_none()
    };
    assert!(matches(bare(), "mytool test"));
    assert!(matches(bare(), "mytool test -- filter"));
    assert!(matches(bare(), "mytool test --mode=fast -- a b"));
    assert!(!matches(bare(), "mytool test filter"));
    assert!(!matches(bare(), "mytool test filter -- a"));
    // Every word stays a digest.
    let shaped = serde_json::to_string(&shape("mytool --mode=fast", &known)).unwrap();
    assert!(
        !shaped.contains("mode") && !shaped.contains("fast"),
        "{shaped}"
    );
}

/// A listed program's flag its list does not know is kept as its name's
/// digest (ADR-t3201-1), and a rule's `flags`, `none_of`, `flag_values`
/// and `takes_value` match it as they match a known one in plain.
#[test]
fn a_rules_flags_match_a_listed_programs_flags_kept_as_digests() {
    let known = Subcommands::of(&rust()).keyed(Some(DigestKey::test()));
    let matches = |rule: CommandRule, command: &str| {
        let classifier = CommandClassifier::new(
            EffectiveRules {
                rules: vec![(RuleSource::Project, rule)],
            },
            known.clone(),
        );
        classifier.classify(&shape(command, &known)).is_some()
    };
    let test = || CommandRule::new("x", "cargo").subcommands(&["test"]);
    let shaped = shape("cargo test --shard ci", &known);
    assert_eq!(
        shaped.parts[0].args[1],
        ShapeArg::Flag {
            flag: DigestKey::test().digest("--shard"),
            value: None
        }
    );
    // flags and none_of.
    assert!(matches(test().flags(&["--shard"]), "cargo test --shard ci"));
    assert!(!matches(test().flags(&["--shard"]), "cargo test --locked"));
    assert!(!matches(
        test().none_of(&["--shard"]),
        "cargo test --shard ci"
    ));
    assert!(matches(test().none_of(&["--shard"]), "cargo test --locked"));
    // flag_values and takes_value, written apart and after `=`.
    let shard = || {
        test()
            .flag_values("--shard", &["ci"])
            .takes_value(&["--shard"])
            .positional_none()
    };
    for command in [
        "cargo test --shard ci",
        "cargo test --shard=ci",
        "cargo test --locked --shard ci",
    ] {
        assert!(matches(shard(), command), "{command}");
    }
    for command in ["cargo test --shard local", "cargo test --shard=local"] {
        assert!(!matches(shard(), command), "{command}");
    }
    // A short one's letter too.
    assert!(matches(
        test().flag_values("-X", &["ci"]).takes_value(&["-X"]),
        "cargo test -X ci"
    ));
    assert!(matches(
        test().flag_values("-X", &["ci"]),
        "cargo test -Xci"
    ));
}

/// (2) Each item of a rule: the part matches only when every item holds,
/// and a literal matches the word in plain and as its digest alike.
#[test]
fn each_item_of_a_rule_must_hold() {
    let known = Subcommands::of(&rust()).keyed(Some(DigestKey::test()));
    let matches = |rule: CommandRule, command: &str| {
        let classifier = CommandClassifier::new(
            EffectiveRules {
                rules: vec![(RuleSource::Project, rule)],
            },
            known.clone(),
        );
        classifier.classify(&shape(command, &known)).is_some()
    };
    let cargo = || CommandRule::new("x", "cargo");
    // program: plain (cargo is known) and as a digest (an unknown tool).
    assert!(matches(cargo(), "cargo test"));
    assert!(!matches(cargo(), "make test"));
    assert!(matches(CommandRule::new("x", "npm"), "npm test"));
    assert!(matches(
        CommandRule::new("x", "./gradlew"),
        "./gradlew test"
    ));
    // sub: a prefix of the subcommands, a word at a subcommand's position
    // too.
    assert!(matches(
        cargo().subcommands(&["nextest", "run"]),
        "cargo nextest run -p a"
    ));
    assert!(matches(
        cargo().subcommands(&["nextest"]),
        "cargo nextest run"
    ));
    assert!(!matches(
        cargo().subcommands(&["nextest", "run"]),
        "cargo nextest list"
    ));
    assert!(!matches(
        cargo().subcommands(&["test"]),
        "cargo +nightly fmt"
    ));
    assert!(matches(cargo().subcommands(&["fmt"]), "cargo +nightly fmt"));
    assert!(matches(
        CommandRule::new("x", "go").subcommands(&["test"]),
        "go test ./..."
    ));
    assert!(!matches(
        CommandRule::new("x", "go").subcommands(&["test"]),
        "go vet"
    ));
    // flags: all of them; none_of: none of them.
    assert!(matches(
        cargo().flags(&["--locked", "--lib"]),
        "cargo test --lib --locked"
    ));
    assert!(!matches(
        cargo().flags(&["--locked", "--lib"]),
        "cargo test --locked"
    ));
    assert!(matches(cargo().none_of(&["--lib"]), "cargo test --locked"));
    assert!(!matches(
        cargo().none_of(&["--lib", "-p"]),
        "cargo test -p a"
    ));
    // flag_values: the flag is there and every value of it is listed, its
    // value attached by `=` or the next word when it takes one.
    let it = || {
        cargo()
            .flag_values("--test", &["it"])
            .takes_value(&["--test"])
    };
    assert!(matches(it(), "cargo test --test it"));
    assert!(matches(it(), "cargo test --test=it"));
    assert!(!matches(it(), "cargo test --test it --test plugin"));
    assert!(!matches(it(), "cargo test --lib"));
    assert!(!matches(
        cargo().flag_values("--test", &["it"]),
        "cargo test --test it"
    ));
    // takes_value: the next word is the flag's value, not a positional.
    assert!(matches(
        it().subcommands(&["test"]).positional_none(),
        "cargo test --test it"
    ));
    // A subcommand the rule does not name is a word.
    assert!(!matches(it().positional_none(), "cargo test --test it"));
    assert!(!matches(
        cargo()
            .subcommands(&["test"])
            .flags(&["--test"])
            .positional_none(),
        "cargo test --test it"
    ));
    assert!(matches(
        cargo()
            .subcommands(&["test"])
            .flags(&["--test"])
            .takes_value(&["--test"])
            .positional_none(),
        "cargo test --test it"
    ));
    // positional: none but what the rule names, and what follows `--`.
    let bare = || cargo().subcommands(&["test"]).positional_none();
    assert!(matches(bare(), "cargo test --locked -- --nocapture filter"));
    assert!(!matches(bare(), "cargo test filter"));
    assert!(matches(cargo().subcommands(&["test"]), "cargo test filter"));
    // args_any: one of the words.
    assert!(matches(
        CommandRule::new("x", "sysctl").args_any(&["vm.loadavg"]),
        "sysctl -n vm.loadavg"
    ));
    assert!(!matches(
        CommandRule::new("x", "sysctl").args_any(&["vm.loadavg"]),
        "sysctl -a"
    ));
    // lead: the part starts with one of the reserved words.
    let lead = || CommandRule::new("x", "sleep").lead(&["do"]);
    assert!(matches(lead(), "until test -f x; do sleep 1; done"));
    assert!(!matches(lead(), "sleep 1"));
}

/// (2) The first rule that matches a part gives its class and its group
/// (the class without one), and a command takes the class of its part
/// matched by the earliest rule.
#[test]
fn the_first_rule_that_matches_wins_with_its_group() {
    let rules = vec![
        CommandRule::new("first", "cargo")
            .subcommands(&["test"])
            .group("tests"),
        CommandRule::new("second", "cargo").subcommands(&["test"]),
        CommandRule::new("lint", "cargo").subcommands(&["clippy"]),
    ];
    let toolchains = with_rules(rust(), rules);
    let classifier = classifier(&toolchains);
    assert_eq!(
        class_of(&classifier, "cargo test"),
        Some(("first", "tests"))
    );
    assert_eq!(
        class_of(&classifier, "cargo clippy"),
        Some(("lint", "lint"))
    );
    // The parts of one command: the earlier rule wins, whatever the order
    // of the parts.
    assert_eq!(
        class_of(&classifier, "cargo clippy && cargo test"),
        Some(("first", "tests"))
    );
    assert_eq!(
        class_of(&classifier, "sleep 1; git status"),
        Some(("wait", "wait"))
    );
    assert_eq!(class_of(&classifier, "echo hi"), None);
}

/// (2a) The overlay: dagq.toml's rules first (overriding a profile's),
/// then each declared profile's in the order declared, then the common
/// ones; a second profile's rules work by its implementation and its
/// registration alone.
#[test]
fn the_project_rules_come_before_the_profiles_and_the_profiles_before_the_common_ones() {
    let overriding = vec![
        CommandRule::new("check", "cargo").subcommands(&["clippy"]),
        CommandRule::new("busy", "sleep"),
    ];
    let toolchains = with_rules(declare(&REGISTRY, &["rust", "fake"]), overriding);
    let rules = effective_rules(&toolchains);
    let sources: Vec<String> = rules
        .rules()
        .iter()
        .map(|(source, _)| source.label())
        .collect();
    let first = |label: &str| sources.iter().position(|s| s == label).unwrap();
    let last = |label: &str| sources.iter().rposition(|s| s == label).unwrap();
    assert_eq!(first("dagq.toml"), 0);
    assert!(last("dagq.toml") < first("profile:rust"));
    assert!(last("profile:rust") < first("profile:fake"));
    assert!(last("profile:fake") < first("common"));
    assert_eq!(last("common"), sources.len() - 1);
    let classifier = classifier(&toolchains);
    // dagq.toml overrides the profile's and the common rule.
    assert_eq!(
        class_of(&classifier, "cargo clippy"),
        Some(("check", "check"))
    );
    assert_eq!(class_of(&classifier, "sleep 5"), Some(("busy", "busy")));
    // The profile's comes before the common one.
    assert_eq!(
        class_of(&classifier, "cargo build"),
        Some(("build", "build"))
    );
    // The fake profile's rule works by its declaration.
    assert_eq!(
        class_of(&classifier, "fakebuild check"),
        Some(("fake_check", "fake_check"))
    );
    let undeclared = classifier_of_rules(&declare(&REGISTRY, &["rust"]));
    assert_eq!(class_of(&undeclared, "fakebuild check"), None);
    // A profile declared twice gives its rules once.
    let twice = effective_rules(&declare(&BUILTIN, &["rust", "rust"]));
    assert_eq!(
        twice.rules().len(),
        crate::domain::toolchain::rust::cargo_rules().len() + common_rules().len()
    );
}

fn classifier_of_rules(toolchains: &Toolchains) -> CommandClassifier {
    classifier(toolchains)
}

/// (2a) The final form, called directly with no case of the move: a
/// repository that declares nothing and has no project rule classifies by
/// the common rules alone, and any other command is unclassified.
#[test]
fn with_no_declaration_and_no_project_rule_only_the_common_rules_classify() {
    let none = Toolchains::none();
    let rules = effective_rules(&none);
    assert!(
        rules
            .rules()
            .iter()
            .all(|(source, _)| *source == RuleSource::Common)
    );
    assert!(!rules.counts_checks());
    let classifier = classifier(&none);
    for (command, expected) in TABLE {
        let common = expected.filter(|(class, _)| [WAIT, DAGQ, GIT].contains(class));
        assert_eq!(class_of(&classifier, command), common, "{command}");
    }
}

/// Checks of `integrate` that a breakdown counts the repeats of: the
/// whole test suite, the e2e test and the llvm-cov gate.
const VERIFICATION: [&str; 3] = [
    "cargo test --locked",
    "cargo test --locked --test e2e",
    "cargo llvm-cov nextest --locked --workspace --fail-under-lines 80",
];

/// The counts of each row of [`TABLE`] run once in this repository:
/// `full_tests`, `llvm_cov_runs` and `verification_repeats` against
/// [`VERIFICATION`].
const SOURCE_COUNTS: [(usize, usize, usize); 21] = [
    (1, 0, 1),
    (0, 0, 0),
    (0, 0, 0),
    (0, 0, 1),
    (0, 0, 1),
    (1, 0, 1),
    (1, 0, 1),
    (1, 0, 1),
    (1, 0, 1),
    (0, 0, 0),
    (0, 0, 0),
    (0, 1, 1),
    (0, 0, 0),
    (0, 0, 0),
    (0, 0, 0),
    (0, 0, 0),
    (0, 0, 0),
    (0, 0, 0),
    (0, 0, 0),
    (0, 0, 0),
    (0, 0, 0),
];

const SESSION: &str = "11111111-1111-4111-8111-111111111111";
const BASE: i64 = 1_790_000_000_000;

/// The work breakdowns of `command` run once for ten seconds, by `rules`:
/// that of a Claude transcript and that of a Codex turn.
fn breakdowns(command: &str, rules: &CommandClassifier) -> [worktime::Breakdown; 2] {
    use crate::domain::transcript::{Transcript, millis_text};
    let line = |secs: i64, kind: &str, message: serde_json::Value| {
        json!({
            "type": kind,
            "timestamp": millis_text(BASE + secs * 1000),
            "sessionId": SESSION,
            "isSidechain": false,
            "version": "2.1.283",
            "message": message,
        })
        .to_string()
    };
    let lines = [
        line(0, "user", json!({"content": "go"})),
        line(
            1,
            "assistant",
            json!({"content": [{"type": "tool_use", "id": "a", "name": "Bash",
                "input": {"command": command}}]}),
        ),
        line(
            11,
            "user",
            json!({"content": [{"type": "tool_result", "tool_use_id": "a",
                "content": "ok", "is_error": false}]}),
        ),
    ];
    let records = Transcript::parse(&lines.join("\n"), SESSION, &rust())
        .unwrap()
        .records;
    let verification = VERIFICATION.map(str::to_owned);
    let claude = worktime::breakdown(&records, BASE, BASE + 20_000, &verification, rules);
    let items = [crate::domain::turn::TurnCommand {
        id: "a".into(),
        tool: worktime::SHELL_ITEM.into(),
        command: Some(command.into()),
        started: Some(BASE + 1_000),
        ended: Some(BASE + 11_000),
        exit_code: Some(0),
        ..Default::default()
    }];
    let turns = [worktime::HeadlessTurn {
        start: BASE,
        end: BASE + 20_000,
        commands: &items,
    }];
    let codex = worktime::headless_breakdown(&turns, BASE, BASE + 20_000, &verification, rules);
    [claude, codex]
}

/// (2c), (3) One function of the domain makes every work breakdown's
/// rules, whichever repository it is: by this repository's `dagq.toml`
/// every row of the table takes the category, the time and the counts
/// ([`SOURCE_COUNTS`]) it had, in a Claude transcript's breakdown and a
/// Codex turn's alike; without a declaration or a project rule cargo's
/// commands are other commands with nothing counted, in both alike.
#[test]
fn every_breakdown_is_classified_by_the_rules_that_work() {
    let rust_rules = with_rules(rust(), project_rules());
    for toolchains in [Toolchains::none(), rust(), rust_rules.clone()] {
        assert_eq!(
            *work_rules(&toolchains).rules(),
            effective_rules(&toolchains)
        );
    }
    let own = work_rules(&rust_rules);
    assert_eq!(TABLE.len(), SOURCE_COUNTS.len());
    for ((command, expected), counts) in TABLE.iter().zip(SOURCE_COUNTS) {
        let group = expected.map_or(worktime::OTHER_COMMAND, |(_, group)| group);
        assert_eq!(worktime::label(command, &own).0, group, "{command}");
        // Its ten seconds in its group and the counts it had, of a Claude
        // transcript and a Codex turn alike.
        for work in breakdowns(command, &own) {
            let (full_tests, llvm_cov_runs, repeats) = counts;
            assert_eq!(work.payload()["secs"][group], 10, "{command}");
            assert_eq!(work.full_tests, Some(full_tests), "{command}");
            assert_eq!(work.llvm_cov_runs, Some(llvm_cov_runs), "{command}");
            assert_eq!(work.verification_repeats, Some(repeats), "{command}");
        }
    }
    // Nothing declared, no project rule: the common rules alone, cargo's
    // build and fmt unclassified and nothing counted, of a Claude
    // transcript and a Codex turn alike.
    assert!(Toolchains::none().profiles().next().is_none());
    let plain = work_rules(&Toolchains::none());
    for command in [
        "cargo build",
        "cargo clippy --locked --all-targets -- -D warnings",
        "cargo check",
        "cargo run",
        "cargo +nightly fmt --all --check",
        "cargo test --locked",
        "cargo llvm-cov nextest --locked --workspace --fail-under-lines 80",
        "cargo test --locked --test e2e",
    ] {
        assert_eq!(
            worktime::label(command, &plain),
            (worktime::OTHER_COMMAND, false),
            "{command}"
        );
        for work in breakdowns(command, &plain) {
            let payload = work.payload();
            assert_eq!(payload["secs"]["other_command"], 10, "{command}");
            for category in ["build", "fmt", "test", "llvm_cov", "e2e"] {
                assert!(payload["secs"].get(category).is_none(), "{command}");
            }
            assert_eq!(payload["commands"], json!({}), "{command}");
            assert_eq!(payload["heavy"], json!([]), "{command}");
            for key in ["verification_repeats", "full_tests", "llvm_cov_runs"] {
                assert!(payload.get(key).is_none(), "{command}: {key}");
            }
        }
    }
    assert!(!plain.rules().counts_checks());
    // Project rules alone come before the common ones.
    let own = with_rules(
        Toolchains::none(),
        vec![CommandRule::new("build", "cargo").subcommands(&["build"])],
    );
    assert_eq!(
        worktime::label("cargo build", &work_rules(&own)),
        ("build", true)
    );
    assert_eq!(
        worktime::label("cargo fmt", &work_rules(&own)).0,
        worktime::OTHER_COMMAND
    );
}

/// (2a) What `doctor` shows: each rule with its source, the project's with
/// its line; no source of an implied profile, since the resolution of a
/// repository that declares nothing implies none.
#[test]
fn doctor_shows_each_rule_with_its_source() {
    let mut rule = CommandRule::new("full_test", "cargo")
        .group("test")
        .flag_values("--test", &["it"]);
    rule.line = Some(12);
    let toolchains = with_rules(rust(), vec![rule]);
    let view = doctor_view(&toolchains);
    let rules = view["rules"].as_array().unwrap();
    assert_eq!(
        rules[0],
        json!({
            "source": "dagq.toml",
            "line": 12,
            "class": "full_test",
            "group": "test",
            "program": "cargo",
            "flag_values": {"--test": ["it"]},
        })
    );
    assert_eq!(rules[1]["source"], "profile:rust");
    assert_eq!(rules.last().unwrap()["source"], "common");
    assert!(view.get("work_breakdown").is_none());
    assert_eq!(view["command_detail"], "shape");
    assert_eq!(view["failed_tests"], false);
    for toolchains in [Toolchains::none(), rust()] {
        let view = doctor_view(&toolchains);
        let labels: Vec<&str> = view["rules"]
            .as_array()
            .unwrap()
            .iter()
            .map(|rule| rule["source"].as_str().unwrap())
            .collect();
        assert!(
            labels
                .iter()
                .all(|label| ["dagq.toml", "profile:rust", "common"].contains(label)),
            "{labels:?}"
        );
    }
    let none = doctor_view(&Toolchains::none());
    assert!(
        none["rules"]
            .as_array()
            .unwrap()
            .iter()
            .all(|rule| rule["source"] == "common")
    );
}

/// (1) The three details of the same command: the whole shape, the
/// programs alone, nothing; a variable's value never, its name only in the
/// shape.
#[test]
fn the_detail_decides_what_a_command_keeps() {
    let command = "RUST_LOG=debug cargo test --locked --features=secret --test it 2>&1 | tail -n 5";
    let known = |detail: CommandDetail| {
        Subcommands::of(&rust())
            .detailed(detail)
            .keyed(Some(DigestKey::test()))
    };
    let d = |word: &str| DigestKey::test().digest(word);
    let whole = shape_value_with(command, &known(CommandDetail::Shape)).unwrap();
    assert_eq!(
        whole,
        json!({"parts": [
            {"assigns": ["RUST_LOG"], "program": "cargo", "args": [
                {"sub": "test"}, {"flag": "--locked"},
                {"flag": "--features", "value": d("secret")},
                {"flag": "--test"}, {"word": d("it")}]},
            {"program": "tail", "args": [{"flag": "-n"}, {"word": d("5")}]}]})
    );
    let programs = shape_value_with(command, &known(CommandDetail::Program)).unwrap();
    assert_eq!(
        programs,
        json!({"parts": [{"program": "cargo", "args": []}, {"program": "tail", "args": []}],
               "programs_only": true})
    );
    assert_eq!(
        shape_value_with(command, &known(CommandDetail::Redacted)),
        None
    );
    for kept in [whole.to_string(), programs.to_string()] {
        assert!(!kept.contains("debug"), "{kept}");
        assert!(!kept.contains("secret"), "{kept}");
    }
    assert!(!programs.to_string().contains("RUST_LOG"));
    // The detail is the one dagq.toml asks.
    let settings = rust().with_telemetry(TelemetrySettings {
        command_detail: CommandDetail::Program,
        ..TelemetrySettings::default()
    });
    assert_eq!(Subcommands::of(&settings).detail(), CommandDetail::Program);
    assert_eq!(Subcommands::of(&rust()).detail(), CommandDetail::Shape);
    // A redacted step keeps its kind, its tool and its time alone.
    let mut step = Step::new(3, StepKind::ToolCall, 10, 20);
    step.tool = Some("Bash".into());
    step.command_shape = Some(whole);
    step.exit_code = Some(1);
    step.output_bytes = Some(40);
    step.background = Some(false);
    let kept = step.clone();
    CommandDetail::Shape.redact(&mut step);
    assert_eq!(step, kept);
    CommandDetail::Redacted.redact(&mut step);
    assert_eq!(
        step,
        Step {
            tool: Some("Bash".into()),
            ..Step::new(3, StepKind::ToolCall, 10, 20)
        }
    );
}

/// (2b) A rule that reads a flag's value or a word does not match a step
/// kept at the `program` or `redacted` detail, which has neither; one of
/// the program alone does.
#[test]
fn a_rule_of_values_does_not_match_a_step_without_them() {
    let toolchains = with_rules(rust(), project_rules());
    let classifier = CommandClassifier::new(
        effective_rules(&toolchains),
        Subcommands::of(&toolchains).keyed(Some(DigestKey::test())),
    );
    let known = |detail| {
        Subcommands::of(&toolchains)
            .detailed(detail)
            .keyed(Some(DigestKey::test()))
    };
    let command = "cargo test --locked --test it";
    let whole = shape_value_with(command, &known(CommandDetail::Shape)).unwrap();
    assert_eq!(classifier.classify_value(&whole).unwrap().class, FULL_TEST);
    let programs = shape_value_with(command, &known(CommandDetail::Program)).unwrap();
    assert_eq!(classifier.classify_value(&programs), None);
    assert_eq!(
        shape_value_with(command, &known(CommandDetail::Redacted)),
        None
    );
    // A rule that asks for no word or no flag does not match either: at
    // the program's detail they are unknown, not absent.
    let make = with_rules(
        rust(),
        vec![CommandRule::new("build", "make").positional_none()],
    );
    let make_classifier = CommandClassifier::new(
        effective_rules(&make),
        Subcommands::of(&make).keyed(Some(DigestKey::test())),
    );
    let make_known = Subcommands::of(&make)
        .detailed(CommandDetail::Program)
        .keyed(Some(DigestKey::test()));
    for command in ["make", "make test"] {
        let kept = shape_value_with(command, &make_known).unwrap();
        assert_eq!(make_classifier.classify_value(&kept), None, "{command}");
    }
    let none_of = CommandClassifier::new(
        overlay(&[CommandRule::new("x", "cargo").none_of(&["--lib"])], []),
        Subcommands::of(&toolchains).keyed(Some(DigestKey::test())),
    );
    assert_eq!(none_of.classify_value(&programs), None);
    // A rule of the program alone matches at the program's detail.
    let program_only = CommandClassifier::new(
        overlay(&[CommandRule::new("cargo", "cargo")], []),
        Subcommands::of(&toolchains).keyed(Some(DigestKey::test())),
    );
    assert_eq!(
        program_only.classify_value(&programs).unwrap().class,
        "cargo"
    );
}

/// (2) The version of the rules moves with any change of them.
#[test]
fn the_version_moves_with_the_rules() {
    let base = effective_rules(&rust());
    assert_eq!(base.version(), effective_rules(&rust()).version());
    assert_eq!(base.version().len(), 16);
    let changed = effective_rules(&with_rules(rust(), vec![CommandRule::new("x", "make")]));
    assert_ne!(base.version(), changed.version());
    let regrouped = effective_rules(&with_rules(
        rust(),
        vec![CommandRule::new("x", "make").group("y")],
    ));
    assert_ne!(changed.version(), regrouped.version());
}
