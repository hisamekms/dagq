//! How the queue's migration files are named and numbered (ADR-0067).
//!
//! A migration is `migrations/NNNN_<name>.sql`, `NNNN` its schema version in
//! four digits. `build.rs` includes this file to list the migrations it
//! embeds and to refuse numbers that do not run from 0001 without a gap or a
//! repeat; `integrate` uses it to renumber a run's migration whose number
//! `main` took meanwhile. The library compiles it as a module so the rule is
//! tested like any other code.
//!
//! The Postgres series (ADR-t3986-1 decision 7) is `migrations-postgres/`,
//! outside those names and numbers: `base.sql`, which makes the schema of the
//! SQLite version on its first line, then one `<name>.sql` for each SQLite
//! migration after that version, matched by `<name>` and in SQLite's order.
//! A name is what survives renumbering, so a file there spells no number:
//! `integrate` will not move a migration whose number another changed file
//! mentions.

/// The directory of the migrations, relative to the repository root.
pub const DIRECTORY: &str = "migrations";

/// The directory of the Postgres series, relative to the repository root.
pub const POSTGRES_DIRECTORY: &str = "migrations-postgres";

/// The file of the Postgres series' base.
pub const POSTGRES_BASE: &str = "base.sql";

/// What the base's first line starts with, before the SQLite version it makes.
const POSTGRES_BASE_DECLARATION: &str = "-- dagq-postgres-base:";

/// The number of a migration file named `NNNN_<name>.sql`, or `None` when
/// `file_name` is not named so.
pub fn number(file_name: &str) -> Option<u32> {
    let (digits, rest) = file_name.split_at_checked(4)?;
    let name = rest.strip_prefix('_')?.strip_suffix(".sql")?;
    if name.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

/// `number` as the four digits a file name starts with.
pub fn digits(number: u32) -> String {
    format!("{number:04}")
}

/// `file_name` (a migration's) with its number replaced by `number`.
pub fn renumbered(file_name: &str, number: u32) -> String {
    format!("{}{}", digits(number), &file_name[4..])
}

/// The migration files among `file_names` (the entries of the migrations
/// directory) in order of their number, or why they cannot be: a `.sql` file
/// not named `NNNN_<name>.sql`, a number more than one file uses, or a
/// number missing between 0001 and the last. Files that are not `.sql` are
/// not migrations and are left out.
pub fn ordered(file_names: &[String]) -> Result<Vec<String>, String> {
    let mut problems = Vec::new();
    let mut numbered: Vec<(u32, &str)> = Vec::new();
    for name in file_names.iter().filter(|name| name.ends_with(".sql")) {
        match number(name) {
            Some(n) => numbered.push((n, name)),
            None => problems.push(format!(
                "{DIRECTORY}/{name} is not named NNNN_<name>.sql (four digits, an underscore, a name)"
            )),
        }
    }
    numbered.sort();
    let mut expected = 1;
    for (index, &(n, _)) in numbered.iter().enumerate() {
        if index > 0 && numbered[index - 1].0 == n {
            continue;
        }
        let sharing: Vec<String> = numbered
            .iter()
            .filter(|(m, _)| *m == n)
            .map(|(_, name)| format!("{DIRECTORY}/{name}"))
            .collect();
        if sharing.len() > 1 {
            problems.push(format!(
                "migration number {} is used by more than one file: {}",
                digits(n),
                sharing.join(", ")
            ));
        }
        if n != expected {
            let missing = if n > expected + 1 {
                format!("{} to {}", digits(expected), digits(n - 1))
            } else if n == expected + 1 {
                digits(expected)
            } else {
                // Only 0000 comes before the first expected number.
                String::new()
            };
            problems.push(if missing.is_empty() {
                format!(
                    "{DIRECTORY}/{} has number {}, but the numbers start at 0001",
                    numbered[index].1,
                    digits(n)
                )
            } else {
                format!(
                    "migration number {missing} is missing before {DIRECTORY}/{} (the numbers run from 0001 without a gap)",
                    numbered[index].1
                )
            });
        }
        expected = expected.max(n + 1);
    }
    if problems.is_empty() {
        Ok(numbered
            .into_iter()
            .map(|(_, name)| name.to_owned())
            .collect())
    } else {
        Err(problems.join("\n"))
    }
}

/// The SQLite version the Postgres base `text` makes, from its first line.
pub fn postgres_base_version(text: &str) -> Result<usize, String> {
    text.lines()
        .next()
        .and_then(|line| line.strip_prefix(POSTGRES_BASE_DECLARATION))
        .and_then(|version| version.trim().parse().ok())
        .filter(|version| *version > 0)
        .ok_or_else(|| {
            format!(
                "{POSTGRES_DIRECTORY}/{POSTGRES_BASE} does not start with \
                 `{POSTGRES_BASE_DECLARATION} <version>`"
            )
        })
}

/// The files of the Postgres series after its base, in the order of the
/// SQLite migrations they match, or why they cannot be: `sqlite` (the files
/// [`ordered`] gave) has a migration after `base_version` with no file of its
/// name in `postgres` (the entries of the directory), the directory has a
/// file no such migration names, or two of those files would have one name
/// (two migrations named alike, or one named as the base). Files that are not
/// `.sql` are left out; the names before the base are not looked at.
pub fn postgres_series(
    sqlite: &[String],
    base_version: usize,
    postgres: &[String],
) -> Result<Vec<String>, String> {
    let mut problems = Vec::new();
    if !postgres.iter().any(|name| name == POSTGRES_BASE) {
        problems.push(format!("{POSTGRES_DIRECTORY}/{POSTGRES_BASE} is missing"));
    }
    if base_version > sqlite.len() {
        problems.push(format!(
            "{POSTGRES_DIRECTORY}/{POSTGRES_BASE} makes version {base_version}, past the {} \
             migrations under {DIRECTORY}",
            sqlite.len()
        ));
    }
    let after = sqlite.get(base_version..).unwrap_or_default();
    let mut series: Vec<String> = Vec::new();
    for migration in after {
        // `ordered` only returns names `number` reads.
        let file = migration[5..].to_owned();
        if file == POSTGRES_BASE || series.contains(&file) {
            problems.push(format!(
                "{DIRECTORY}/{migration} has a name another file of {POSTGRES_DIRECTORY} \
                 has after the base; rename it"
            ));
        } else if !postgres.contains(&file) {
            problems.push(format!(
                "{DIRECTORY}/{migration} has no {POSTGRES_DIRECTORY}/{file}; add it \
                 (`-- postgres: none` alone when Postgres needs no change)"
            ));
        }
        series.push(file);
    }
    for name in postgres.iter().filter(|name| name.ends_with(".sql")) {
        if name != POSTGRES_BASE && !series.contains(name) {
            problems.push(format!(
                "{POSTGRES_DIRECTORY}/{name} matches no migration of {DIRECTORY} after the base"
            ));
        }
    }
    if problems.is_empty() {
        Ok(series)
    } else {
        Err(problems.join("\n"))
    }
}

/// A migration's number `text` spells, which a file of the Postgres series may
/// not have even in a comment: `own` (the digits of the migration the file
/// matches, if any) anywhere, as `integrate` searches for it, or else any four
/// digits with a leading zero, not within longer digits.
pub fn spelled_number<'a>(text: &'a str, own: Option<&'a str>) -> Option<&'a str> {
    own.filter(|digits| text.contains(*digits)).or_else(|| {
        text.split(|c: char| !c.is_ascii_digit())
            .find(|digits| digits.len() == 4 && digits.starts_with('0'))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(names: &[&str]) -> Vec<String> {
        names.iter().map(|name| (*name).to_owned()).collect()
    }

    #[test]
    fn only_four_digits_an_underscore_a_name_and_sql_make_a_migration() {
        assert_eq!(number("0001_queue.sql"), Some(1));
        assert_eq!(number("0032_run_env_program_events.sql"), Some(32));
        for name in [
            "001_queue.sql",
            "00a1_queue.sql",
            "0001queue.sql",
            "0001_.sql",
            "0001_queue.txt",
            "",
            "あいうえ_x.sql",
        ] {
            assert_eq!(number(name), None, "{name}");
        }
        assert_eq!(renumbered("0033_findings.sql", 35), "0035_findings.sql");
        assert_eq!(digits(7), "0007");
    }

    #[test]
    fn numbers_from_one_without_gaps_are_ordered_and_other_files_left_out() {
        assert_eq!(
            ordered(&names(&[
                "0002_b.sql",
                "README.md",
                "0001_a.sql",
                "0003_c.sql"
            ])),
            Ok(names(&["0001_a.sql", "0002_b.sql", "0003_c.sql"]))
        );
        assert_eq!(ordered(&[]), Ok(Vec::new()));
    }

    #[test]
    fn a_repeat_a_gap_a_zero_and_a_misnamed_file_name_the_files() {
        let error = ordered(&names(&[
            "0001_a.sql",
            "0002_b.sql",
            "0002_c.sql",
            "0003_d.sql",
        ]))
        .unwrap_err();
        assert_eq!(
            error,
            "migration number 0002 is used by more than one file: migrations/0002_b.sql, migrations/0002_c.sql"
        );
        let error = ordered(&names(&["0001_a.sql", "0003_c.sql"])).unwrap_err();
        assert_eq!(
            error,
            "migration number 0002 is missing before migrations/0003_c.sql (the numbers run from 0001 without a gap)"
        );
        let error = ordered(&names(&["0001_a.sql", "0005_e.sql"])).unwrap_err();
        assert!(error.contains("0002 to 0004 is missing before migrations/0005_e.sql"));
        let error = ordered(&names(&["0000_z.sql", "0001_a.sql"])).unwrap_err();
        assert_eq!(
            error,
            "migrations/0000_z.sql has number 0000, but the numbers start at 0001"
        );
        let error = ordered(&names(&["0001_a.sql", "2_b.sql"])).unwrap_err();
        assert!(error.contains("migrations/2_b.sql is not named NNNN_<name>.sql"));
        // Every problem is named, not only the first.
        let error = ordered(&names(&["0002_b.sql", "0002_c.sql", "x.sql"])).unwrap_err();
        assert_eq!(error.lines().count(), 3, "{error}");
    }

    /// The SQLite migrations `0001_a` to `0003_c`, with the base at 2.
    fn sqlite() -> Vec<String> {
        names(&["0001_a.sql", "0002_b.sql", "0003_c.sql"])
    }

    #[test]
    fn the_series_after_the_base_follows_sqlite_s_order_by_name() {
        assert_eq!(
            postgres_series(
                &names(&["0001_a.sql", "0002_b.sql", "0003_z.sql", "0004_c.sql"]),
                2,
                &names(&["c.sql", "README.md", "base.sql", "z.sql"])
            ),
            Ok(names(&["z.sql", "c.sql"]))
        );
        assert_eq!(
            postgres_series(&sqlite(), 3, &names(&["base.sql"])),
            Ok(Vec::new())
        );
    }

    #[test]
    fn a_renumbered_sqlite_migration_leaves_the_series_as_it_is() {
        let postgres = names(&["base.sql", "c.sql"]);
        assert_eq!(
            postgres_series(&sqlite(), 2, &postgres),
            postgres_series(
                &names(&["0001_a.sql", "0002_b.sql", "0007_c.sql"]),
                2,
                &postgres
            )
        );
    }

    #[test]
    fn a_migration_after_the_base_without_its_file_is_refused() {
        let error = postgres_series(&sqlite(), 2, &names(&["base.sql"])).unwrap_err();
        assert_eq!(
            error,
            "migrations/0003_c.sql has no migrations-postgres/c.sql; add it \
             (`-- postgres: none` alone when Postgres needs no change)"
        );
    }

    #[test]
    fn a_file_no_migration_after_the_base_names_is_refused() {
        for file in ["z.sql", "a.sql", "0003_c.sql"] {
            let error =
                postgres_series(&sqlite(), 2, &names(&["base.sql", "c.sql", file])).unwrap_err();
            assert_eq!(
                error,
                format!(
                    "migrations-postgres/{file} matches no migration of migrations after the base"
                )
            );
        }
    }

    #[test]
    fn names_that_meet_after_the_base_are_refused() {
        let error = postgres_series(
            &names(&["0001_a.sql", "0002_c.sql", "0003_c.sql"]),
            1,
            &names(&["base.sql", "c.sql"]),
        )
        .unwrap_err();
        assert!(
            error.contains("migrations/0003_c.sql has a name"),
            "{error}"
        );
        let error = postgres_series(
            &names(&["0001_a.sql", "0002_base.sql"]),
            1,
            &names(&["base.sql"]),
        )
        .unwrap_err();
        assert!(
            error.contains("migrations/0002_base.sql has a name"),
            "{error}"
        );
        // Before the base a name may repeat: nothing here matches it.
        assert_eq!(
            postgres_series(
                &names(&["0001_a.sql", "0002_a.sql"]),
                2,
                &names(&["base.sql"])
            ),
            Ok(Vec::new())
        );
    }

    #[test]
    fn a_series_without_its_base_or_past_the_migrations_is_refused() {
        let error = postgres_series(&sqlite(), 3, &names(&["c.sql"])).unwrap_err();
        assert!(error.contains("base.sql is missing"), "{error}");
        assert!(error.contains("matches no migration"), "{error}");
        let error = postgres_series(&sqlite(), 4, &names(&["base.sql"])).unwrap_err();
        assert!(error.contains("makes version 4, past the 3"), "{error}");
    }

    #[test]
    fn the_base_declares_its_version_on_its_first_line() {
        assert_eq!(
            postgres_base_version("-- dagq-postgres-base: 94\nCREATE TABLE t (x bigint);"),
            Ok(94)
        );
        for text in [
            "",
            "CREATE TABLE t (x bigint);",
            "-- note\n-- dagq-postgres-base: 94",
            "-- dagq-postgres-base: x",
            "-- dagq-postgres-base: 0",
        ] {
            assert!(postgres_base_version(text).is_err(), "{text}");
        }
    }

    #[test]
    fn only_a_number_spelled_as_a_migration_file_s_is_found() {
        assert_eq!(
            spelled_number("-- added by 0075_node_samples", None),
            Some("0075")
        );
        assert_eq!(spelled_number("0095", None), Some("0095"));
        for text in ["94", "x * 1000", "10095", "00951", "-- ADR-t3986-1"] {
            assert_eq!(spelled_number(text, None), None, "{text}");
        }
    }

    #[test]
    fn a_file_s_own_number_is_found_anywhere_as_the_renumbering_searches() {
        assert_eq!(spelled_number("x = 100950", Some("0095")), Some("0095"));
        assert_eq!(spelled_number("x = 100960", Some("0095")), None);
    }
}
