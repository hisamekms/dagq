use crate::domain::command_rules::{
    CommandClassifier, CommandDetail, CommandRule, Positional, overlay, templates::TEMPLATES,
};
use crate::domain::command_shape::{DigestKey, Subcommands};
use crate::infrastructure::run_env::parse_config;

fn error(text: &str) -> String {
    format!("{:#}", parse_config(text).unwrap_err())
}

#[test]
fn the_tables_are_read_with_every_key() {
    let text = r#"[run.env]
A = 'x'

[telemetry]
command_detail = "program"  # less
failed_tests = true

[[telemetry.commands]]
class = "full_test"
group = "test"
program = "cargo"
sub = ["test"]
flags = ["--locked"]
none_of = ["--lib", "-p"]
flag_values = { "--test" = ["it"], -E = ['all()', "x"] }
takes_value = ["--test", "-E"]
positional = "none"
args_any = ["a"]
lead = ["do", "until"]

[[ telemetry.commands ]] # spaced
class = "wait"
program = "sleep"

[stall]
send_confirm_secs = 30
"#;
    let config = parse_config(text).unwrap();
    let telemetry = config.telemetry;
    assert_eq!(telemetry.command_detail, CommandDetail::Program);
    assert!(telemetry.failed_tests);
    let mut expected = CommandRule::new("full_test", "cargo")
        .group("test")
        .subcommands(&["test"])
        .flags(&["--locked"])
        .none_of(&["--lib", "-p"])
        .flag_values("--test", &["it"])
        .flag_values("-E", &["all()", "x"])
        .takes_value(&["--test", "-E"])
        .positional_none()
        .args_any(&["a"])
        .lead(&["do", "until"]);
    expected.line = Some(8);
    assert_eq!(telemetry.commands[0], expected);
    assert_eq!(telemetry.commands[1].line, Some(21));
    assert_eq!(telemetry.commands[1].positional, Positional::Any);
    assert_eq!(
        config.stall,
        parse_config("[stall]\nsend_confirm_secs = 30\n")
            .unwrap()
            .stall
    );
}

#[test]
fn without_the_tables_the_defaults_hold() {
    let telemetry = parse_config("[run.env]\nA = 'x'\n").unwrap().telemetry;
    assert_eq!(telemetry.command_detail, CommandDetail::Shape);
    assert!(!telemetry.failed_tests);
    assert!(telemetry.commands.is_empty());
}

/// (5) Every mistake of the tables is an error with its line.
#[test]
fn a_mistake_is_an_error_with_its_line() {
    for (text, wanted) in [
        (
            "[telemetry]\ncommand_detail = \"full\"\n",
            "dagq.toml:2: command_detail is \"full\"",
        ),
        (
            "[telemetry]\nfailed_tests = yes\n",
            "dagq.toml:2: value of failed_tests",
        ),
        (
            "[telemetry]\ncolor = 1\n",
            "dagq.toml:2: unknown key color in [telemetry]",
        ),
        (
            "[telemetry]\nfailed_tests = true\nfailed_tests = false\n",
            "dagq.toml:3: failed_tests is defined twice",
        ),
        (
            "[telemetry]\n[telemetry]\n",
            "dagq.toml:2: [telemetry] is defined twice",
        ),
        (
            "[telemetry.commands]\nclass = \"x\"\n",
            "dagq.toml:1: [telemetry.commands] holds one rule per table",
        ),
        (
            "[[telemetry.commands]]\nprogram = \"make\"\n",
            "dagq.toml:1: [[telemetry.commands]] has no class",
        ),
        (
            "[[telemetry.commands]]\nclass = \"x\"\n[run.env]\nA = 'x'\n",
            "dagq.toml:1: [[telemetry.commands]] has no program",
        ),
        (
            "[[telemetry.commands]]\nclass = \"Full Test\"\nprogram = \"make\"\n",
            "dagq.toml:2: class \"Full Test\" is not a class's form",
        ),
        (
            "[[telemetry.commands]]\nclass = \"x\"\ngroup = \"9\"\nprogram = \"make\"\n",
            "dagq.toml:3: group \"9\" is not a class's form",
        ),
        (
            "[[telemetry.commands]]\nclass = \"x\"\ngroup = \"model\"\nprogram = \"make\"\n",
            "dagq.toml:3: group \"model\" is not a class's form",
        ),
        (
            "[[telemetry.commands]]\nclass = \"x\"\nprogram = \"make\"\ncolor = \"red\"\n",
            "dagq.toml:4: unknown key color in [[telemetry.commands]]",
        ),
        (
            "[[telemetry.commands]]\nclass = \"x\"\nclass = \"y\"\n",
            "dagq.toml:3: class is defined twice",
        ),
        (
            "[[telemetry.commands]]\nclass = \"x\"\nprogram = \"make\"\npositional = \"some\"\n",
            "dagq.toml:4: positional is \"some\"",
        ),
        (
            "[[telemetry.commands]]\nclass = \"x\"\nprogram = \"make\"\nlead = [\"then\", \"go\"]\n",
            "dagq.toml:4: lead names \"go\"",
        ),
        (
            "[[telemetry.commands]]\nclass = \"x\"\nprogram = \"make\"\nflags = [\"locked\"]\n",
            "dagq.toml:4: flags names \"locked\", which is no flag's name",
        ),
        (
            "[[telemetry.commands]]\nclass = \"x\"\nprogram = \"make\"\nsub = \"test\"\n",
            "dagq.toml:4: value of sub",
        ),
        (
            "[[telemetry.commands]]\nclass = \"x\"\nprogram = \"make\"\nflag_values = { \"--test\" = [] }\n",
            "dagq.toml:4: flag_values gives --test no literal",
        ),
        (
            "[[telemetry.commands]]\nclass = \"x\"\nprogram = \"make\"\nflag_values = { \"--test\" = [\"a\"], --test = [\"b\"] }\n",
            "dagq.toml:4: value of flag_values",
        ),
        (
            "[[telemetry.commands]]\nclass = \"x\"\nprogram = \"make\"\nflag_values = [\"--test\"]\n",
            "dagq.toml:4: value of flag_values",
        ),
        (
            "[[telemetry.commands]]\nclass = \"x\"\nprogram = \"make\"\n\n[[telemetry.commands]]\nclass = \"x\"\nprogram = \"make\"\n",
            "dagq.toml:5: [[telemetry.commands]] repeats the rule of line 1",
        ),
        (
            "[[telemetry.commands]]\nclass = \"x\"\nprogram = \"my tool\"\n",
            "dagq.toml:3: program \"my tool\" is no program's name",
        ),
    ] {
        let message = error(text);
        assert!(message.contains(wanted), "{text}\n{message}");
    }
}

/// (3) Every template the binary carries is read as `dagq.toml` reads it,
/// and its rules classify the commands of its tools.
#[test]
fn every_template_is_valid_and_classifies_its_tools() {
    let classes = |config: &str, commands: &[(&str, Option<&str>)]| {
        let rules = parse_config(config).unwrap().telemetry.commands;
        assert!(!rules.is_empty(), "{config}");
        let classifier = CommandClassifier::new(
            overlay(&rules, []),
            Subcommands::common().keyed(Some(DigestKey::test())),
        );
        for (command, class) in commands {
            assert_eq!(
                classifier.classify_text(command).map(|m| m.class),
                *class,
                "{command}"
            );
        }
    };
    let by_name = |name: &str| TEMPLATES.iter().find(|t| t.name == name).unwrap().config;
    classes(
        by_name("npm"),
        &[
            ("npm test", Some("test")),
            ("npm run test -- --watch", Some("test")),
            ("pnpm lint", Some("lint")),
            ("npm run build", Some("build")),
            ("npm install", None),
        ],
    );
    classes(
        by_name("go"),
        &[
            ("go test ./...", Some("test")),
            ("go vet ./...", Some("lint")),
            ("gofmt -l .", Some("fmt")),
            ("go build ./cmd/x", Some("build")),
            ("go mod tidy", None),
        ],
    );
    classes(
        by_name("pytest"),
        &[
            ("pytest -x tests/", Some("test")),
            ("python -m pytest -q", Some("test")),
            ("python -m http.server", None),
            ("ruff check .", Some("lint")),
        ],
    );
    classes(
        by_name("gradle"),
        &[
            ("./gradlew test", Some("test")),
            ("gradle build", Some("build")),
            ("./gradlew clean", None),
        ],
    );
    classes(
        by_name("make"),
        &[
            ("make test", Some("test")),
            ("make lint", Some("lint")),
            ("make", Some("build")),
            ("make -j4", Some("build")),
            ("make clean", None),
        ],
    );
    assert_eq!(TEMPLATES.len(), 5);
}
