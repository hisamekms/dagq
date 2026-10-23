//! The templates of `[[telemetry.commands]]` the binary carries for the
//! tools no toolchain profile knows yet (npm and pnpm, go, pytest,
//! gradle, make): `dagq init` proposes those whose files the repository
//! has ([`suggested`]), and a person writes what they keep into
//! `dagq.toml`. They are proposals, never rules the runtime applies on
//! its own. Once a language has a profile, its template becomes the
//! profile's own rules ([`super::super::toolchain::ToolchainProfile::command_rules`]),
//! which work by the declaration alone: cargo has none here, since the
//! Rust profile holds its rules.

use std::collections::BTreeSet;

use serde_json::{Value, json};

/// One template: the names of the files whose presence (at any depth)
/// suggests it, and its tables as `dagq.toml`'s text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Template {
    pub name: &'static str,
    pub markers: &'static [&'static str],
    pub config: &'static str,
}

/// Every template, in the order proposed.
pub const TEMPLATES: [Template; 5] = [
    Template {
        name: "npm",
        markers: &["package.json"],
        config: r#"[[telemetry.commands]]
class = "test"
program = "npm"
sub = ["test"]

[[telemetry.commands]]
class = "test"
program = "npm"
sub = ["run", "test"]

[[telemetry.commands]]
class = "lint"
program = "npm"
sub = ["run", "lint"]

[[telemetry.commands]]
class = "build"
program = "npm"
sub = ["run", "build"]

[[telemetry.commands]]
class = "test"
program = "pnpm"
sub = ["test"]

[[telemetry.commands]]
class = "test"
program = "pnpm"
sub = ["run", "test"]

[[telemetry.commands]]
class = "lint"
program = "pnpm"
sub = ["lint"]

[[telemetry.commands]]
class = "build"
program = "pnpm"
sub = ["build"]
"#,
    },
    Template {
        name: "go",
        markers: &["go.mod"],
        config: r#"[[telemetry.commands]]
class = "test"
program = "go"
sub = ["test"]

[[telemetry.commands]]
class = "lint"
program = "go"
sub = ["vet"]

[[telemetry.commands]]
class = "lint"
program = "golangci-lint"

[[telemetry.commands]]
class = "fmt"
program = "gofmt"

[[telemetry.commands]]
class = "build"
program = "go"
sub = ["build"]
"#,
    },
    Template {
        name: "pytest",
        markers: &["pyproject.toml", "pytest.ini"],
        config: r#"[[telemetry.commands]]
class = "test"
program = "pytest"

[[telemetry.commands]]
class = "test"
program = "python"
flag_values = { "-m" = ["pytest"] }
takes_value = ["-m"]

[[telemetry.commands]]
class = "lint"
program = "ruff"
sub = ["check"]

[[telemetry.commands]]
class = "fmt"
program = "ruff"
sub = ["format"]
"#,
    },
    Template {
        name: "gradle",
        markers: &["build.gradle", "build.gradle.kts"],
        config: r#"[[telemetry.commands]]
class = "test"
program = "./gradlew"
sub = ["test"]

[[telemetry.commands]]
class = "test"
program = "gradle"
sub = ["test"]

[[telemetry.commands]]
class = "build"
program = "./gradlew"
sub = ["build"]

[[telemetry.commands]]
class = "build"
program = "gradle"
sub = ["build"]
"#,
    },
    Template {
        name: "make",
        markers: &["Makefile"],
        config: r#"[[telemetry.commands]]
class = "test"
program = "make"
sub = ["test"]

[[telemetry.commands]]
class = "lint"
program = "make"
sub = ["lint"]

[[telemetry.commands]]
class = "build"
program = "make"
positional = "none"
"#,
    },
];

/// The templates whose markers are among `files`, a repository's paths,
/// in the order of [`TEMPLATES`].
pub fn suggested(files: &[String]) -> Vec<&'static Template> {
    let names: BTreeSet<&str> = files
        .iter()
        .map(|file| {
            file.rsplit_once('/')
                .map_or(file.as_str(), |(_, name)| name)
        })
        .collect();
    TEMPLATES
        .iter()
        .filter(|template| template.markers.iter().any(|marker| names.contains(marker)))
        .collect()
}

/// What `init` shows of the templates `files` suggest: each one's `name`
/// and `markers` (`templates`), and `config`, their tables together as
/// `dagq.toml`'s text (`null` with none).
pub fn view(files: &[String]) -> Value {
    let templates = suggested(files);
    json!({
        "templates": templates
            .iter()
            .map(|template| json!({"name": template.name, "markers": template.markers}))
            .collect::<Vec<_>>(),
        "config": (!templates.is_empty()).then(|| {
            templates
                .iter()
                .map(|template| template.config)
                .collect::<Vec<_>>()
                .join("\n")
        }),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn files(paths: &[&str]) -> Vec<String> {
        paths.iter().map(|path| (*path).to_owned()).collect()
    }

    fn names(paths: &[&str]) -> Vec<&'static str> {
        suggested(&files(paths))
            .into_iter()
            .map(|template| template.name)
            .collect()
    }

    #[test]
    fn the_repositorys_files_choose_the_templates() {
        assert_eq!(names(&["package.json", "src/a.ts"]), ["npm"]);
        assert_eq!(names(&["go.mod"]), ["go"]);
        assert_eq!(names(&["pyproject.toml"]), ["pytest"]);
        assert_eq!(names(&["app/build.gradle.kts"]), ["gradle"]);
        assert_eq!(names(&["Makefile"]), ["make"]);
        assert_eq!(
            names(&[
                "Makefile",
                "web/package.json",
                "go.mod",
                "pytest.ini",
                "build.gradle"
            ]),
            ["npm", "go", "pytest", "gradle", "make"]
        );
    }

    #[test]
    fn a_cargo_repository_is_given_no_template() {
        assert!(names(&["Cargo.toml", "src/main.rs", "Cargo.lock"]).is_empty());
        let view = view(&files(&["Cargo.toml"]));
        assert_eq!(view["templates"], json!([]));
        assert_eq!(view["config"], Value::Null);
        assert!(TEMPLATES.iter().all(|t| !t.config.contains("\"cargo\"")));
    }

    #[test]
    fn the_view_joins_the_tables_of_the_templates_suggested() {
        let view = view(&files(&["go.mod", "Makefile"]));
        assert_eq!(view["templates"][0]["name"], "go");
        assert_eq!(view["templates"][1]["markers"], json!(["Makefile"]));
        let config = view["config"].as_str().unwrap();
        assert!(config.starts_with(TEMPLATES[1].config), "{config}");
        assert!(config.ends_with(TEMPLATES[4].config), "{config}");
    }
}
