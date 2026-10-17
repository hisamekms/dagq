//! The settings a profile recommends (ADR-t2268-1 decision 7): proposals a
//! person writes into `dagq.toml` and commits, never defaults of the
//! runtime's. A profile gives them through
//! [`super::ToolchainProfile::recommendation`]; [`propose`] takes the
//! profiles a repository declares, or, when it declares none, those its
//! files suggest ([`guess`]), and [`Proposed`] says what `dagq init` and
//! `dagq setup-review` show. Of the recommendations, `setup-review` gives
//! its job only the review programs within what it may write
//! ([`RecommendedProgram::within_setup`]); the declaration and the
//! verification commands are shown, not written.

use std::collections::BTreeSet;

use serde_json::{Value, json};

use super::{ProfileRegistry, ToolchainDeclaration, ToolchainProfile, Toolchains};
use crate::domain::setup_review::{is_program_script, writable};

/// What a profile recommends a repository that uses it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Recommendation {
    /// The names of the files whose presence, at any depth, suggests the
    /// profile: what a declaration is guessed from ([`guess`]), never a
    /// profile turned on (ADR-t2257-1 decision 1).
    pub markers: &'static [&'static str],
    /// The verification commands recommended together: a task's
    /// `--verify` values when it is registered, not a key of `dagq.toml`.
    pub verify: &'static [&'static str],
    /// The review programs (`[review.programs.<name>]` and their scripts).
    pub programs: &'static [RecommendedProgram],
}

/// A review program a profile recommends: the table
/// `[review.programs.<name>]` with `script` and `paths`, and the script's
/// text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecommendedProgram {
    pub name: &'static str,
    /// The script's repository-relative path.
    pub script: &'static str,
    pub paths: &'static [&'static str],
    /// The script's text.
    pub body: &'static str,
}

impl RecommendedProgram {
    /// Whether `setup-review` may write it: a program script under
    /// `.dagq/review/programs/` (ADR-t2259-1 decision 7). One elsewhere is
    /// shown and never given to its job, which would be refused writing it.
    pub fn within_setup(&self) -> bool {
        writable(self.script) && is_program_script(self.script)
    }

    /// Its table as `dagq.toml` would have it.
    pub fn table(&self) -> String {
        format!(
            "[review.programs.{}]\nscript = {}\npaths = {}\n",
            key(self.name),
            quoted(self.script),
            list(self.paths.iter().copied())
        )
    }
}

/// A string as a TOML basic string.
fn quoted(text: &str) -> String {
    serde_json::to_string(text).expect("a string serializes")
}

/// A key of a table header: bare when TOML allows it, quoted otherwise.
fn key(name: &str) -> String {
    if !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        name.to_owned()
    } else {
        quoted(name)
    }
}

/// Strings as a TOML array.
fn list<'a>(items: impl Iterator<Item = &'a str>) -> String {
    format!("[{}]", items.map(quoted).collect::<Vec<_>>().join(", "))
}

/// Where the proposed profiles come from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Origin {
    /// `dagq.toml`'s `[toolchains.<name>]`.
    Declared,
    /// The repository's files, while it declares none.
    Guessed,
}

/// A profile proposed with the name and the paths of its declaration.
#[derive(Clone)]
struct Entry {
    name: String,
    profile: &'static dyn ToolchainProfile,
    paths: Vec<String>,
}

impl Entry {
    /// The declaration as `dagq.toml` would have it.
    fn table(&self) -> String {
        let mut table = format!(
            "[toolchains.{}]\nprofile = {}\n",
            key(&self.name),
            quoted(self.profile.name())
        );
        if !self.paths.is_empty() {
            table.push_str(&format!(
                "paths = {}\n",
                list(self.paths.iter().map(String::as_str))
            ));
        }
        table
    }
}

/// The profiles proposed for a repository and what they recommend.
#[derive(Clone)]
pub struct Proposed {
    from: Option<Origin>,
    entries: Vec<Entry>,
    error: Option<String>,
}

/// The profiles `files`, a repository's paths, suggest, each named after
/// its profile, in the order registered: a profile whose marker is at the
/// root applies to the whole repository, one whose markers are only below
/// it to each of their directories (`<dir>/**`). A directory whose name
/// a glob would read otherwise (`*`, `?`, `[`, `]`, `{`, `}`, `\\`) is left
/// out.
fn guess(registry: &ProfileRegistry, files: &[String]) -> Vec<Entry> {
    registry
        .profiles()
        .filter_map(|profile| {
            let markers = profile.recommendation().markers;
            let dirs: BTreeSet<&str> = files
                .iter()
                .filter_map(|file| {
                    let (dir, name) = file.rsplit_once('/').unwrap_or(("", file));
                    let plain = !dir.contains(['*', '?', '[', ']', '{', '}', '\\']);
                    (markers.contains(&name) && plain).then_some(dir)
                })
                .collect();
            if dirs.is_empty() {
                return None;
            }
            let paths = if dirs.contains("") {
                Vec::new()
            } else {
                dirs.iter().map(|dir| format!("{dir}/**")).collect()
            };
            Some(Entry {
                name: profile.name().to_owned(),
                profile,
                paths,
            })
        })
        .collect()
}

/// The profiles `declarations` name, resolved against `registry`; when
/// there are none, those the repository's `files` suggest. A declaration
/// that cannot be read, or names a profile `registry` does not have,
/// proposes nothing and says why; `files` is read only when nothing is
/// declared, and one that cannot be listed proposes nothing.
pub fn propose(
    registry: &ProfileRegistry,
    declarations: Result<Vec<ToolchainDeclaration>, String>,
    files: impl FnOnce() -> Result<Vec<String>, String>,
) -> Proposed {
    let failed = |error| Proposed {
        from: None,
        entries: Vec::new(),
        error: Some(error),
    };
    let declarations = match declarations {
        Ok(declarations) => declarations,
        Err(error) => return failed(error),
    };
    if !declarations.is_empty() {
        if let Err(error) = Toolchains::resolve(registry, &declarations) {
            return failed(error);
        }
        let entries = declarations
            .iter()
            .filter_map(|declaration| {
                registry.get(&declaration.profile).map(|profile| Entry {
                    name: declaration.name.clone(),
                    profile,
                    paths: declaration.paths.clone(),
                })
            })
            .collect();
        return Proposed {
            from: Some(Origin::Declared),
            entries,
            error: None,
        };
    }
    match files() {
        Ok(files) => {
            let entries = guess(registry, &files);
            Proposed {
                from: (!entries.is_empty()).then_some(Origin::Guessed),
                entries,
                error: None,
            }
        }
        Err(error) => failed(error),
    }
}

impl Proposed {
    /// The review programs of the proposed profiles, each once (the first
    /// of a name), in the order of the profiles.
    fn programs(&self) -> Vec<RecommendedProgram> {
        let mut seen = BTreeSet::new();
        self.entries
            .iter()
            .flat_map(|entry| entry.profile.recommendation().programs.iter().copied())
            .filter(|program| seen.insert(program.name))
            .collect()
    }

    /// The review programs `setup-review` gives its job: those within what
    /// it may write ([`RecommendedProgram::within_setup`]).
    pub fn setup_programs(&self) -> Vec<RecommendedProgram> {
        self.programs()
            .into_iter()
            .filter(RecommendedProgram::within_setup)
            .collect()
    }

    /// What `init` and `setup-review` show: where the profiles come from
    /// (`from`, `null` with none), the declarations to write when they
    /// were guessed (`toolchains`), each profile's verification commands
    /// (`verify`), each review program with its table, its script and
    /// whether `setup-review` gives it its job (`setup_review`), and
    /// `config`, the tables proposed as `dagq.toml`'s text; or `error`
    /// alone when the declaration cannot be read.
    pub fn view(&self) -> Value {
        if let Some(error) = &self.error {
            return json!({"error": error});
        }
        let guessed = self.from == Some(Origin::Guessed);
        let toolchains: String = if guessed {
            self.entries
                .iter()
                .map(Entry::table)
                .collect::<Vec<_>>()
                .join("\n")
        } else {
            String::new()
        };
        let programs = self.programs();
        let mut config: Vec<String> = Vec::new();
        if !toolchains.is_empty() {
            config.push(toolchains.clone());
        }
        config.extend(programs.iter().map(RecommendedProgram::table));
        let mut verify = Vec::new();
        let mut profiles_seen = BTreeSet::new();
        for entry in &self.entries {
            if profiles_seen.insert(entry.profile.name()) {
                verify.push(json!({
                    "profile": entry.profile.name(),
                    "commands": entry.profile.recommendation().verify,
                }));
            }
        }
        json!({
            "from": self.from.map(|origin| match origin {
                Origin::Declared => "declared",
                Origin::Guessed => "guessed",
            }),
            "profiles": self.entries.iter().map(|entry| json!({
                "name": entry.name,
                "profile": entry.profile.name(),
                "paths": entry.paths,
            })).collect::<Vec<_>>(),
            "toolchains": (!toolchains.is_empty()).then_some(toolchains),
            "verify": verify,
            "programs": programs.iter().map(|program| json!({
                "name": program.name,
                "script": program.script,
                "paths": program.paths,
                "table": program.table(),
                "body": program.body,
                "setup_review": program.within_setup(),
            })).collect::<Vec<_>>(),
            "config": (!config.is_empty()).then(|| config.join("\n")),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::super::{
        fake::{FAKE, REGISTRY},
        rust::RUST,
    };
    use super::*;
    use crate::domain::setup_review::takes_root_from_cwd;

    fn declaration(name: &str, profile: &str, paths: &[&str]) -> ToolchainDeclaration {
        ToolchainDeclaration {
            name: name.to_owned(),
            profile: profile.to_owned(),
            paths: paths.iter().map(|path| (*path).to_owned()).collect(),
            line: 1,
        }
    }

    fn files(paths: &[&str]) -> Result<Vec<String>, String> {
        Ok(paths.iter().map(|path| (*path).to_owned()).collect())
    }

    #[test]
    fn the_rust_profile_recommends_a_declaration_its_verification_and_a_review_program() {
        let recommendation = RUST.recommendation();
        assert_eq!(recommendation.markers, ["Cargo.toml"]);
        let verify = recommendation.verify.join("\n");
        for tool in ["cargo fmt", "cargo clippy", "cargo nextest"] {
            assert!(verify.contains(tool), "{tool}: {verify}");
        }
        assert!(!recommendation.programs.is_empty());
        for program in recommendation.programs {
            assert!(program.within_setup(), "{}", program.name);
            assert!(program.body.starts_with("#!"), "{}", program.name);
            assert!(takes_root_from_cwd(program.body), "{}", program.name);
            assert!(!program.paths.is_empty());
        }

        let proposed = propose(&super::super::BUILTIN, Ok(Vec::new()), || {
            files(&["Cargo.toml", "src/main.rs"])
        });
        let view = proposed.view();
        assert_eq!(view["from"], "guessed", "{view}");
        assert_eq!(
            view["toolchains"],
            "[toolchains.rust]\nprofile = \"rust\"\n"
        );
        assert_eq!(view["verify"][0]["profile"], "rust");
        assert_eq!(view["programs"][0]["setup_review"], true);
        let config = view["config"].as_str().unwrap();
        assert!(config.starts_with("[toolchains.rust]\n"), "{config}");
        assert!(config.contains("[review.programs."), "{config}");
        assert_eq!(proposed.setup_programs(), RUST.recommendation().programs);
    }

    #[test]
    fn a_guess_takes_the_directories_of_the_markers_below_the_root() {
        let proposed = propose(&REGISTRY, Ok(Vec::new()), || {
            files(&[
                "crates/a/Cargo.toml",
                "crates/b/Cargo.toml",
                "tool/fake.build",
                "odd[1]/Cargo.toml",
                "README.md",
            ])
        });
        let view = proposed.view();
        assert_eq!(
            view["profiles"],
            json!([
                {"name": "rust", "profile": "rust", "paths": ["crates/a/**", "crates/b/**"]},
                {"name": "fake", "profile": "fake", "paths": ["tool/**"]},
            ])
        );
        assert_eq!(
            view["toolchains"],
            "[toolchains.rust]\nprofile = \"rust\"\npaths = [\"crates/a/**\", \"crates/b/**\"]\n\n\
             [toolchains.fake]\nprofile = \"fake\"\npaths = [\"tool/**\"]\n"
        );
        // Nothing suggests a profile: nothing is proposed.
        let none = propose(&REGISTRY, Ok(Vec::new()), || files(&["README.md"])).view();
        assert_eq!(none["from"], Value::Null, "{none}");
        assert_eq!(none["programs"], json!([]));
        assert_eq!(none["config"], Value::Null);
    }

    #[test]
    fn the_declared_profiles_are_proposed_and_nothing_is_guessed() {
        let proposed = propose(
            &REGISTRY,
            Ok(vec![declaration("tool", "fake", &["tool/**"])]),
            || panic!("the files are not read when a profile is declared"),
        );
        let view = proposed.view();
        assert_eq!(view["from"], "declared", "{view}");
        // Declared already: no declaration to write.
        assert_eq!(view["toolchains"], Value::Null);
        assert_eq!(
            view["verify"],
            json!([{"profile": "fake", "commands": ["fakebuild check"]}])
        );
        let names: Vec<&str> = view["programs"]
            .as_array()
            .unwrap()
            .iter()
            .map(|program| program["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, ["fake-lint", "fake-outside"]);
        // The program outside `.dagq/review/` is shown, never given to the job.
        assert_eq!(view["programs"][1]["setup_review"], false);
        let given: Vec<&str> = proposed
            .setup_programs()
            .iter()
            .map(|program| program.name)
            .collect();
        assert_eq!(given, ["fake-lint"]);
        assert!(FAKE.recommendation().programs.len() > given.len());
    }

    #[test]
    fn an_unreadable_or_unknown_declaration_proposes_nothing() {
        let unknown = propose(
            &super::super::BUILTIN,
            Ok(vec![declaration("tool", "fake", &[])]),
            || panic!("not read"),
        );
        assert!(
            unknown.view()["error"]
                .as_str()
                .unwrap()
                .contains("unknown profile")
        );
        assert!(unknown.setup_programs().is_empty());
        let unreadable = propose(&REGISTRY, Err("broken".to_owned()), || panic!("not read"));
        assert_eq!(unreadable.view(), json!({"error": "broken"}));
        let unlisted = propose(&REGISTRY, Ok(Vec::new()), || Err("no git".to_owned()));
        assert_eq!(unlisted.view(), json!({"error": "no git"}));
    }

    #[test]
    fn a_program_outside_the_review_settings_is_not_within_setup() {
        let program = |script| RecommendedProgram {
            name: "x",
            script,
            paths: &["**"],
            body: "#!/bin/sh\n",
        };
        assert!(program(".dagq/review/programs/x.sh").within_setup());
        for script in [
            "scripts/x.sh",
            ".dagq/review/x.sh",
            ".dagq/review/programs/x.py",
            ".dagq/review/programs/../x.sh",
        ] {
            assert!(!program(script).within_setup(), "{script}");
        }
        // A name TOML does not take bare is quoted.
        let dotted = RecommendedProgram {
            name: "a.b",
            ..program(".dagq/review/programs/x.sh")
        };
        assert!(dotted.table().starts_with("[review.programs.\"a.b\"]\n"));
        assert_eq!(
            program(".dagq/review/programs/x.sh").table(),
            "[review.programs.x]\nscript = \".dagq/review/programs/x.sh\"\npaths = [\"**\"]\n"
        );
    }
}
