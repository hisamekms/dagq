//! How much of a command an executor's step keeps, and the rules that put
//! a command down to a class when the ledger and a run's work breakdown
//! are made (ADR-t1662-3 decisions 16, 18 and 19 as ADR-t2268-1 amends
//! them, docs/design/measurement.md "コマンドの形と分類"). A step keeps the
//! command's generic shape ([`super::command_shape`]) at the detail
//! `[telemetry] command_detail` asks ([`CommandDetail`]), never a class: a
//! class is a view's, made from the rules that work when the view is made,
//! so other rules make it again.
//!
//! The rules that work ([`effective_rules`]) are, tried from the top, the
//! first that matches a part winning: the `[[telemetry.commands]]` of
//! `dagq.toml` (the project's, which add to or override what follows),
//! then each declared toolchain profile's own rules
//! ([`ToolchainProfile::command_rules`]) in the order declared, then the
//! runtime's common rules of no language's tools ([`common_rules`]). A
//! command's class is that of its part matched by the earliest rule; one
//! no rule matches is unclassified. The runtime gives a class no meaning,
//! except that the counts of `integrate`'s checks a work breakdown keeps
//! read the classes [`CHECK_CLASSES`] names.
//!
//! A run's work breakdown is classified by the same rules that work
//! ([`work_rules`]), whichever repository it is. The templates for
//! languages no profile knows are [`templates`]'s.
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Mutex, OnceLock, PoisonError};

use serde::Serialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use super::command_shape::{self, CommandShape, DigestKey, ShapeArg, ShapePart, Subcommands};
use super::session_step::Step;
use super::toolchain::{ToolchainProfile, Toolchains};

pub mod templates;

/// How much of a command an executor's step keeps (`[telemetry]
/// command_detail`, [`TelemetrySettings`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CommandDetail {
    /// `program`: the program of each part alone, as the shape keeps it
    /// (its name when a list of known subcommands names it, else its
    /// digest): no reserved word, no variable, no argument.
    Program,
    /// `shape` (the default): the whole generic shape
    /// ([`super::command_shape`]): the names of the flags the lists know
    /// in plain and of the others as digests, their values and the other
    /// words as digests, the known subcommands in plain at their
    /// positions, the names of the variables a part assigns (in plain
    /// when a list holds them, else as digests) and never their values.
    #[default]
    Shape,
    /// `redacted`: the step's kind and time alone: no shape, no exit, no
    /// output's size and no background.
    Redacted,
}

impl CommandDetail {
    pub const ALL: [Self; 3] = [Self::Program, Self::Shape, Self::Redacted];

    /// The value `dagq.toml` names it by.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Program => "program",
            Self::Shape => "shape",
            Self::Redacted => "redacted",
        }
    }

    /// The detail named `text`.
    pub fn parse(text: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|detail| detail.as_str() == text)
    }

    /// `shape` as a step of this detail keeps it: whole, each part's
    /// program alone (a part that runs none dropped), or none.
    pub fn apply(self, shape: CommandShape) -> Option<CommandShape> {
        match self {
            Self::Shape => Some(shape),
            Self::Redacted => None,
            Self::Program => Some(CommandShape {
                parts: shape
                    .parts
                    .into_iter()
                    .filter_map(|part| {
                        part.program.map(|program| ShapePart {
                            program: Some(program),
                            ..ShapePart::default()
                        })
                    })
                    .collect(),
                truncated: shape.truncated,
                programs_only: true,
            }),
        }
    }

    /// Clear what a command's `step` keeps beyond this detail: a redacted
    /// one keeps its kind, its tool and its time alone.
    pub fn redact(self, step: &mut Step) {
        if self == Self::Redacted {
            step.command_shape = None;
            step.exit_code = None;
            step.output_bytes = None;
            step.background = None;
        }
    }
}

/// `[telemetry]` and `[[telemetry.commands]]` of `dagq.toml`: how much of
/// its commands a session's steps keep and the project's rules that
/// classify them. Read with the rest of the file
/// (`crate::infrastructure::telemetry_config`); a mistake is an error with
/// its line.
///
/// ```toml
/// [telemetry]
/// command_detail = "shape"    # program, shape (the default) or redacted
/// failed_tests = false        # the default
///
/// [[telemetry.commands]]      # one table per rule, tried from the top
/// class = "full_test"
/// group = "test"
/// program = "cargo"
/// sub = ["test"]
/// flag_values = { "--test" = ["it"] }
/// takes_value = ["--test"]
/// positional = "none"
/// ```
///
/// - `command_detail`: what an executor's step keeps of a command
///   ([`CommandDetail`]); without it, `shape`.
/// - `failed_tests`: whether the readers of a session's commands read
///   the tests a command's output names as failed, by the declared
///   profiles (with none declared, none), and keep them for the work
///   breakdown ([`reads_failed_tests`]); without it, `false`, and none is
///   read or kept, in any repository.
/// - each `[[telemetry.commands]]` is a [`CommandRule`] (its keys are the
///   rule's fields of the same name). The project writes only the rules
///   that add to or override those of the profiles it declares, which
///   work by the declaration alone; [`effective_rules`] lays them out.
///
/// Without either table nothing changes: the default detail, no failed
/// test, and the profiles' and the common rules alone. A binary older than
/// the tables refuses them, so a repository writes them only once the
/// binary that runs its queue reads them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TelemetrySettings {
    pub command_detail: CommandDetail,
    pub failed_tests: bool,
    /// The project's rules, in file order.
    pub commands: Vec<CommandRule>,
}

/// Whether a rule asks a part for no word beside those it names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Positional {
    /// `any` (the default): any words.
    #[default]
    Any,
    /// `none`: no word but those the rule's `sub` matched, the values of
    /// its `flag_values` and `takes_value` flags, and those after `--`.
    None,
}

impl Positional {
    /// The value named `text` (`any` or `none`).
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "any" => Some(Self::Any),
            "none" => Some(Self::None),
            _ => None,
        }
    }

    const fn is_any(&self) -> bool {
        matches!(self, Self::Any)
    }
}

/// One rule of the classification: a part of a command matches it when
/// every item it has holds. A literal (a program, a subcommand, a value, a
/// word) matches a word kept in plain and the digest of the same word
/// alike, so a rule matches a program the shape keeps as its digest, and
/// its flags whether their value is written apart or after `=`. A
/// step kept at the `program` or `redacted` detail has no flag's value and
/// no word, so a rule that reads them does not match it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CommandRule {
    /// `class` (required): the name the rule gives, kebab- or snake-case
    /// (lowercase letters, digits, `_` and `-`, a letter first). The
    /// runtime gives it no meaning but [`CHECK_CLASSES`].
    pub class: String,
    /// `group`: the name of the coarser total it counts to (a work
    /// breakdown's category); without it, its class.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    /// `program` (required): the part's program. One a list of known
    /// subcommands names is matched by its name without a path; any other
    /// is matched by its first word as written, path included (`pytest`
    /// does not match `.venv/bin/pytest`), since the shape keeps that
    /// word's digest alone.
    pub program: String,
    /// `sub`: the part's subcommands from the first, as many as named
    /// (`["nextest", "run"]` matches `cargo nextest run …`); a word in a
    /// subcommand's position (before any flag) counts as one.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub sub: Vec<String>,
    /// `flags`: the names of the flags the part has, all of them.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub flags: Vec<String>,
    /// `none_of`: the names of flags the part has none of.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub none_of: Vec<String>,
    /// `flag_values` (`{ "--test" = ["it"] }`): flags the part has whose
    /// every value (`--flag=value`, or the word after it when
    /// `takes_value` names it) is one of the literals, in file order.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub flag_values: Vec<(String, Vec<String>)>,
    /// `takes_value`: flags whose next word is their value.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub takes_value: Vec<String>,
    /// `positional`: `any` (the default) or `none` ([`Positional`]).
    #[serde(skip_serializing_if = "Positional::is_any")]
    pub positional: Positional,
    /// `args_any`: literals one of the part's words is.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub args_any: Vec<String>,
    /// `lead`: reserved words of the shell one of which the part starts
    /// with (`until`, `do` …).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub lead: Vec<String>,
    /// The line of its `[[telemetry.commands]]` in `dagq.toml`; none for
    /// a profile's or a common rule.
    #[serde(skip)]
    pub line: Option<usize>,
}

fn owned(items: &[&str]) -> Vec<String> {
    items.iter().map(|item| (*item).to_owned()).collect()
}

impl CommandRule {
    /// A rule of `class` for `program` with nothing else asked.
    pub fn new(class: &str, program: &str) -> Self {
        Self {
            class: class.to_owned(),
            group: None,
            program: program.to_owned(),
            sub: Vec::new(),
            flags: Vec::new(),
            none_of: Vec::new(),
            flag_values: Vec::new(),
            takes_value: Vec::new(),
            positional: Positional::Any,
            args_any: Vec::new(),
            lead: Vec::new(),
            line: None,
        }
    }

    #[must_use]
    pub fn group(mut self, group: &str) -> Self {
        self.group = Some(group.to_owned());
        self
    }

    #[must_use]
    pub fn subcommands(mut self, sub: &[&str]) -> Self {
        self.sub = owned(sub);
        self
    }

    #[must_use]
    pub fn flags(mut self, flags: &[&str]) -> Self {
        self.flags = owned(flags);
        self
    }

    #[must_use]
    pub fn none_of(mut self, flags: &[&str]) -> Self {
        self.none_of = owned(flags);
        self
    }

    #[must_use]
    pub fn flag_values(mut self, flag: &str, literals: &[&str]) -> Self {
        self.flag_values.push((flag.to_owned(), owned(literals)));
        self
    }

    #[must_use]
    pub fn takes_value(mut self, flags: &[&str]) -> Self {
        self.takes_value = owned(flags);
        self
    }

    #[must_use]
    pub fn positional_none(mut self) -> Self {
        self.positional = Positional::None;
        self
    }

    #[must_use]
    pub fn args_any(mut self, literals: &[&str]) -> Self {
        self.args_any = owned(literals);
        self
    }

    #[must_use]
    pub fn lead(mut self, words: &[&str]) -> Self {
        self.lead = owned(words);
        self
    }

    /// Whether it reads nothing of a part but its program: the one kind of
    /// rule a shape of the `program` detail can match, whose absent words
    /// and flags are unknown, not absent.
    pub fn reads_program_alone(&self) -> bool {
        self.sub.is_empty()
            && self.flags.is_empty()
            && self.none_of.is_empty()
            && self.flag_values.is_empty()
            && self.takes_value.is_empty()
            && self.positional == Positional::Any
            && self.args_any.is_empty()
            && self.lead.is_empty()
    }

    /// The name of the total it counts to: its group, else its class.
    pub fn group_name(&self) -> &str {
        self.group.as_deref().unwrap_or(&self.class)
    }

    /// Every literal it compares a word with.
    fn literals(&self) -> impl Iterator<Item = &String> {
        std::iter::once(&self.program)
            .chain(&self.sub)
            .chain(&self.flags)
            .chain(&self.none_of)
            .chain(
                self.flag_values
                    .iter()
                    .flat_map(|(flag, values)| std::iter::once(flag).chain(values)),
            )
            .chain(&self.takes_value)
            .chain(&self.args_any)
    }

    /// The flags it names, whose words a part of an unknown program keeps
    /// as digests among its other words.
    fn named_flags(&self) -> impl Iterator<Item = &String> {
        self.flags
            .iter()
            .chain(&self.none_of)
            .chain(self.flag_values.iter().map(|(flag, _)| flag))
            .chain(&self.takes_value)
    }
}

/// The names of the work breakdown's own categories, which no rule's
/// class or group may take: its time would merge with theirs.
pub const RESERVED_CLASSES: [&str; 6] = [
    "model",
    "idle",
    "tool",
    "subagent",
    "chain",
    "other_command",
];

/// Whether `name` may be a class or a group: lowercase letters, digits,
/// `_` and `-`, a letter first, at most 64 bytes, and none of
/// [`RESERVED_CLASSES`].
pub fn valid_class(name: &str) -> bool {
    let mut chars = name.chars();
    !RESERVED_CLASSES.contains(&name)
        && chars.next().is_some_and(|c| c.is_ascii_lowercase())
        && name.len() <= 64
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
}

/// The shell's reserved words a rule's `lead` may name.
pub const LEAD_WORDS: [&str; 18] = [
    "if", "then", "else", "elif", "fi", "do", "done", "while", "until", "for", "case", "esac",
    "select", "function", "time", "!", "{", "}",
];

/// The classes whose commands a work breakdown counts as `integrate`'s
/// checks (`full_tests`, `llvm_cov_runs` and `verification_repeats`),
/// whichever rule gives them: the whole test suite, the coverage gate and
/// the e2e test.
pub const FULL_TEST: &str = "full_test";
pub const LLVM_COV: &str = "llvm_cov";
pub const E2E: &str = "e2e";
pub const CHECK_CLASSES: [&str; 3] = [FULL_TEST, LLVM_COV, E2E];

/// The classes of the common rules.
pub const WAIT: &str = "wait";
pub const DAGQ: &str = "dagq";
pub const GIT: &str = "git";

/// The runtime's common rules, of no language's tools: the waits, dagq
/// itself and git. Tried after every project's and profile's rule.
pub fn common_rules() -> Vec<CommandRule> {
    vec![
        CommandRule::new(WAIT, "sleep"),
        CommandRule::new(WAIT, "uptime"),
        CommandRule::new(WAIT, "sysctl").args_any(&["vm.loadavg"]),
        CommandRule::new(DAGQ, "dagq"),
        CommandRule::new(GIT, "git"),
    ]
}

/// Where a rule that works comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuleSource {
    /// `[[telemetry.commands]]` of `dagq.toml`.
    Project,
    /// A declared profile's own rules.
    Profile(&'static str),
    /// The runtime's common rules.
    Common,
}

impl RuleSource {
    /// How `doctor` names it: `dagq.toml`, `profile:<name>` or `common`.
    pub fn label(self) -> String {
        match self {
            Self::Project => "dagq.toml".to_owned(),
            Self::Profile(name) => format!("profile:{name}"),
            Self::Common => "common".to_owned(),
        }
    }
}

/// The rules that work, in the order they are tried, each with where it
/// comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EffectiveRules {
    rules: Vec<(RuleSource, CommandRule)>,
}

/// The rules of `project` (`[[telemetry.commands]]`), then those of each
/// of `profiles` in their order (a profile declared twice once), then the
/// common ones.
pub fn overlay<'a>(
    project: &[CommandRule],
    profiles: impl IntoIterator<Item = &'a dyn ToolchainProfile>,
) -> EffectiveRules {
    let mut rules: Vec<(RuleSource, CommandRule)> = project
        .iter()
        .map(|rule| (RuleSource::Project, rule.clone()))
        .collect();
    let mut seen = BTreeSet::new();
    for profile in profiles {
        if seen.insert(profile.name()) {
            rules.extend(
                profile
                    .command_rules()
                    .into_iter()
                    .map(|rule| (RuleSource::Profile(profile.name()), rule)),
            );
        }
    }
    rules.extend(
        common_rules()
            .into_iter()
            .map(|rule| (RuleSource::Common, rule)),
    );
    EffectiveRules { rules }
}

/// The rules that work for a repository whose `dagq.toml` resolves to
/// `toolchains` (its declared profiles and its `[telemetry]`): the
/// overlay itself. What the ledger classifies its steps by, and a run's
/// work breakdown ([`work_rules`]).
pub fn effective_rules(toolchains: &Toolchains) -> EffectiveRules {
    overlay(&toolchains.telemetry().commands, toolchains.profiles())
}

impl EffectiveRules {
    pub fn rules(&self) -> &[(RuleSource, CommandRule)] {
        &self.rules
    }

    /// The version of the classification they make: the first 16 hex
    /// digits of the SHA-256 of the rules and their sources, so that the
    /// same rules give the same version and any change another.
    pub fn version(&self) -> String {
        let listed: Vec<Value> = self
            .rules
            .iter()
            .map(|(source, rule)| json!([source.label(), rule]))
            .collect();
        let text = serde_json::to_string(&listed).expect("rules serialize");
        Sha256::digest(text.as_bytes())
            .iter()
            .take(8)
            .map(|byte| format!("{byte:02x}"))
            .collect()
    }

    /// Whether a rule gives one of [`CHECK_CLASSES`]: then a work
    /// breakdown counts `integrate`'s checks.
    pub fn counts_checks(&self) -> bool {
        self.rules
            .iter()
            .any(|(_, rule)| CHECK_CLASSES.contains(&rule.class.as_str()))
    }

    /// What `doctor` shows: each rule in the order tried, with its
    /// `source` and, for the project's, its `line`.
    pub fn view(&self) -> Value {
        Value::Array(
            self.rules
                .iter()
                .map(|(source, rule)| {
                    let mut value = serde_json::to_value(rule).expect("a rule serializes");
                    if !rule.flag_values.is_empty() {
                        value["flag_values"] = json!(
                            rule.flag_values
                                .iter()
                                .cloned()
                                .collect::<BTreeMap<String, Vec<String>>>()
                        );
                    }
                    value["source"] = json!(source.label());
                    if let Some(line) = rule.line {
                        value["line"] = json!(line);
                    }
                    value
                })
                .collect(),
        )
    }
}

/// A name for as long as the process lives: the classes and groups of the
/// rules are few, and the work breakdown keeps its categories as such.
fn intern(name: &str) -> &'static str {
    static NAMES: OnceLock<Mutex<BTreeSet<&'static str>>> = OnceLock::new();
    let mut names = NAMES
        .get_or_init(Mutex::default)
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    if let Some(known) = names.get(name) {
        return known;
    }
    let leaked: &'static str = Box::leak(name.to_owned().into_boxed_str());
    names.insert(leaked);
    leaked
}

/// The rule a part matched.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Matched {
    pub class: &'static str,
    pub group: &'static str,
    pub source: RuleSource,
    /// Its place in the rules that work: the earlier wins.
    pub index: usize,
}

/// The rules that work, ready to match the shapes made with `known`: its
/// key digests each literal, so a literal matches a word kept as its
/// digest under that key.
#[derive(Debug, Clone)]
pub struct CommandClassifier {
    rules: EffectiveRules,
    names: Vec<(&'static str, &'static str)>,
    known: Subcommands,
    digests: BTreeMap<String, String>,
    failed_tests: bool,
}

impl PartialEq for CommandClassifier {
    fn eq(&self, other: &Self) -> bool {
        self.rules == other.rules
            && self.digests == other.digests
            && self.failed_tests == other.failed_tests
    }
}

impl Eq for CommandClassifier {}

impl CommandClassifier {
    /// `rules` matching the shapes `known` makes (their words digested
    /// under its key; with none, a literal matches a plain word alone).
    pub fn new(rules: EffectiveRules, known: Subcommands) -> Self {
        let names = rules
            .rules
            .iter()
            .map(|(_, rule)| (intern(&rule.class), intern(rule.group_name())))
            .collect();
        let digests = rules
            .rules
            .iter()
            .flat_map(|(_, rule)| rule.literals())
            .filter_map(|literal| Some((literal.clone(), known.digest_word(literal)?)))
            .collect();
        Self {
            rules,
            names,
            known,
            digests,
            failed_tests: false,
        }
    }

    /// `rules` for commands read as text (a transcript's): shaped with the
    /// known subcommands of `toolchains`' profiles, whole, under a key of
    /// the process's own ([`DigestKey::local`]), since neither the shapes
    /// nor their digests leave it. It names the failed tests of a command
    /// only when `toolchains`' `[telemetry] failed_tests` is on and it
    /// declares a profile ([`Self::names_failed_tests`]).
    pub fn of_text(rules: EffectiveRules, toolchains: &Toolchains) -> Self {
        Self {
            failed_tests: reads_failed_tests(toolchains),
            ..Self::new(
                rules,
                Subcommands::of(toolchains)
                    .detailed(CommandDetail::Shape)
                    .keyed(Some(DigestKey::local())),
            )
        }
    }

    pub fn rules(&self) -> &EffectiveRules {
        &self.rules
    }

    /// Whether the work breakdown keeps the tests a command's output names
    /// as failed (read by the declared profiles): only when `[telemetry]
    /// failed_tests` is on and a profile is declared.
    pub const fn names_failed_tests(&self) -> bool {
        self.failed_tests
    }

    /// Whether `kept`, a word of a shape, is `literal` or its digest.
    fn same(&self, kept: &str, literal: &str) -> bool {
        kept == literal
            || self
                .digests
                .get(literal)
                .is_some_and(|digest| digest == kept)
    }

    /// The rule each part of `shape` matches first, in the parts' order.
    pub fn parts(&self, shape: &CommandShape) -> Vec<Option<Matched>> {
        shape
            .parts
            .iter()
            .map(|part| {
                self.rules
                    .rules
                    .iter()
                    .position(|(_, rule)| {
                        (!shape.programs_only || rule.reads_program_alone())
                            && self.matches(rule, part)
                    })
                    .map(|index| self.matched(index))
            })
            .collect()
    }

    fn matched(&self, index: usize) -> Matched {
        let (class, group) = self.names[index];
        Matched {
            class,
            group,
            source: self.rules.rules[index].0,
            index,
        }
    }

    /// The class of `shape`: that of its part matched by the earliest
    /// rule; `None`, unclassified, when no rule matches a part.
    pub fn classify(&self, shape: &CommandShape) -> Option<Matched> {
        self.parts(shape)
            .into_iter()
            .flatten()
            .min_by_key(|matched| matched.index)
    }

    /// The class of a step's shape as the store keeps it; `None` for one
    /// this binary cannot read.
    pub fn classify_value(&self, shape: &Value) -> Option<Matched> {
        serde_json::from_value::<CommandShape>(shape.clone())
            .ok()
            .and_then(|shape| self.classify(&shape))
    }

    /// The rule each part of a step's shape as the store keeps it matches
    /// first ([`Self::parts`]); none for one this binary cannot read.
    pub fn parts_value(&self, shape: &Value) -> Vec<Option<Matched>> {
        serde_json::from_value::<CommandShape>(shape.clone())
            .map(|shape| self.parts(&shape))
            .unwrap_or_default()
    }

    /// The shape of `command` as these rules read it.
    pub fn shape_text(&self, command: &str) -> CommandShape {
        command_shape::shape(command, &self.known).unwrap_or_default()
    }

    /// The class of `command`, a command's text.
    pub fn classify_text(&self, command: &str) -> Option<Matched> {
        self.classify(&self.shape_text(command))
    }

    /// The classes of the parts of `command` that a rule matches.
    pub fn part_classes(&self, command: &str) -> Vec<&'static str> {
        self.parts(&self.shape_text(command))
            .into_iter()
            .flatten()
            .map(|matched| matched.class)
            .collect()
    }

    /// Whether `part` matches `rule` ([`CommandRule`]).
    fn matches(&self, rule: &CommandRule, part: &ShapePart) -> bool {
        let Some(program) = &part.program else {
            return false;
        };
        if !self.same(program, &rule.program) {
            return false;
        }
        if !rule.lead.is_empty() && !part.lead.iter().any(|lead| rule.lead.contains(lead)) {
            return false;
        }
        let args = &part.args;
        // The subcommands' positions: the words before any flag, after a
        // toolchain's `+…`.
        let positions: Vec<usize> = args
            .iter()
            .enumerate()
            .skip_while(|(_, arg)| matches!(arg, ShapeArg::Plus { .. }))
            .take_while(|(_, arg)| word(arg).is_some())
            .map(|(at, _)| at)
            .collect();
        if rule.sub.len() > positions.len()
            || !rule
                .sub
                .iter()
                .zip(&positions)
                .all(|(literal, &at)| word(&args[at]).is_some_and(|w| self.same(w, literal)))
        {
            return false;
        }
        let mut consumed: BTreeSet<usize> = positions[..rule.sub.len()].iter().copied().collect();
        // Where a flag is: its name in plain or as its digest (a flag its
        // program's list does not know, ADR-t3201-1, or any flag's name of
        // a program no list names), or, for a program no list names, a
        // word that is its digest.
        let flag_at = |name: &str| -> Vec<usize> {
            args.iter()
                .enumerate()
                .filter(|(_, arg)| match arg {
                    ShapeArg::Flag { flag, .. } => self.same(flag, name),
                    ShapeArg::Word { word } => self.same(word, name),
                    _ => false,
                })
                .map(|(at, _)| at)
                .collect()
        };
        if rule.flags.iter().any(|name| flag_at(name).is_empty())
            || rule.none_of.iter().any(|name| !flag_at(name).is_empty())
        {
            return false;
        }
        let takes = |name: &str| rule.takes_value.iter().any(|flag| flag == name);
        // The value of the flag `name` at `at`, and where it is when it is
        // the next word.
        let value_of = |at: usize, name: &str| -> Option<(Option<usize>, &str)> {
            match &args[at] {
                ShapeArg::Flag {
                    value: Some(value), ..
                } => Some((None, value.as_str())),
                _ if takes(name) => match args.get(at + 1) {
                    Some(ShapeArg::Word { word }) => Some((Some(at + 1), word.as_str())),
                    _ => None,
                },
                _ => None,
            }
        };
        for name in &rule.takes_value {
            for at in flag_at(name) {
                if let Some((Some(next), _)) = value_of(at, name) {
                    consumed.insert(next);
                }
            }
        }
        for (name, literals) in &rule.flag_values {
            let found = flag_at(name);
            if found.is_empty() {
                return false;
            }
            for at in found {
                match value_of(at, name) {
                    Some((next, value)) if literals.iter().any(|l| self.same(value, l)) => {
                        consumed.extend(next);
                    }
                    _ => return false,
                }
            }
        }
        if !rule.args_any.is_empty()
            && !args
                .iter()
                .filter_map(word)
                .any(|kept| rule.args_any.iter().any(|literal| self.same(kept, literal)))
        {
            return false;
        }
        if rule.positional == Positional::None {
            let flags: BTreeSet<usize> =
                rule.named_flags().flat_map(|name| flag_at(name)).collect();
            let other_word = args.iter().enumerate().any(|(at, arg)| {
                word(arg).is_some() && !consumed.contains(&at) && !flags.contains(&at)
            });
            if other_word {
                return false;
            }
        }
        true
    }
}

/// The text of a word or a subcommand of a part; `None` for a flag, a
/// toolchain's `+…` or `--`.
fn word(arg: &ShapeArg) -> Option<&str> {
    match arg {
        ShapeArg::Sub { sub } => Some(sub.as_str()),
        ShapeArg::Word { word } => Some(word.as_str()),
        _ => None,
    }
}

/// How the work breakdown of a repository classifies its commands: by the
/// rules that work for its `dagq.toml` as `toolchains` resolves it
/// ([`effective_rules`]); with no declaration and no project rule, by the
/// common rules alone, so nothing counts `integrate`'s checks.
pub fn work_rules(toolchains: &Toolchains) -> CommandClassifier {
    CommandClassifier::of_text(effective_rules(toolchains), toolchains)
}

/// Whether the readers of a session's commands (a Claude transcript's tool
/// results, a Codex turn's commands) read their output for the tests it
/// names as failed, and so keep them: only when `[telemetry] failed_tests`
/// is on and `toolchains` declares a profile to read them. By default none
/// is read or kept.
pub fn reads_failed_tests(toolchains: &Toolchains) -> bool {
    toolchains.telemetry().failed_tests && toolchains.profiles().next().is_some()
}

/// What `doctor` shows of the classification: the rules that work, each
/// with its source (`rules`), which a run's work breakdown classifies by
/// too; the detail of the steps' commands (`command_detail`) and whether
/// they name failed tests (`failed_tests`).
pub fn doctor_view(toolchains: &Toolchains) -> Value {
    let settings = toolchains.telemetry();
    json!({
        "rules": effective_rules(toolchains).view(),
        "command_detail": settings.command_detail.as_str(),
        "failed_tests": settings.failed_tests,
    })
}

/// The project rules this repository's own `dagq.toml` has for its test
/// targets: `--test it` runs nearly the whole suite, `--test e2e` is
/// the e2e test.
#[cfg(test)]
pub(crate) fn project_rules() -> Vec<CommandRule> {
    const OTHER_TARGETS: &[&str] = &[
        "--lib",
        "--bin",
        "--bins",
        "--doc",
        "--example",
        "--package",
        "-p",
        "-E",
    ];
    vec![
        CommandRule::new("e2e", "cargo")
            .flag_values("--test", &["e2e"])
            .takes_value(&["--test"]),
        CommandRule::new("full_test", "cargo")
            .group("test")
            .subcommands(&["test"])
            .flag_values("--test", &["it"])
            .takes_value(&["--test"])
            .none_of(OTHER_TARGETS)
            .positional_none(),
        CommandRule::new("full_test", "cargo")
            .group("test")
            .subcommands(&["nextest", "run"])
            .flag_values("--test", &["it"])
            .takes_value(&["--test"])
            .none_of(OTHER_TARGETS)
            .positional_none(),
    ]
}

#[cfg(test)]
mod tests;
