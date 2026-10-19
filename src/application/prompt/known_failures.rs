//! The tests that fail already on the landing branch's CI (ADR-t1920-1
//! decision 5), as the worker's prompt, its resolution requests and the
//! run's whole review carry them: one section within its own limits,
//! the whole list in a file of the run's directory.

use super::*;

/// The items the section keeps at most. Of the CI's red runs the watch
/// lists a few tests at a time; a run that broke many lists them all in
/// the file.
pub const KNOWN_FAILURES_COUNT: usize = 20;

/// The bytes of the section's item lines at most: a line takes about 250
/// bytes (a test's name, its CI run's URL and the task that fixes it), so
/// [`KNOWN_FAILURES_COUNT`] such lines fit. The section's fixed sentences
/// and the note of what it left out take under
/// [`KNOWN_FAILURES_TEXT_BYTES`] more; each prompt that carries the section
/// adds both to its whole limit.
pub const KNOWN_FAILURES_BYTES: usize = 6_000;

/// The bytes of the section's headings, instructions and note of what it
/// left out (about 900 with the longest file path of a run directory).
pub const KNOWN_FAILURES_TEXT_BYTES: usize = 1_500;

/// The bytes the section takes at most: what each prompt that carries it
/// (the worker's, a resolution request, the whole review's) adds to its
/// whole limit, so the sections' limits still fit it. The unit test
/// `a_thousand_known_failures_stay_within_each_prompts_limit_and_name_their_file`
/// holds 1,000 items of 1 KB within each of those limits.
pub const KNOWN_FAILURES_LIMIT: usize = KNOWN_FAILURES_BYTES + KNOWN_FAILURES_TEXT_BYTES;

/// The bytes of one item's name and of its URL: cut past these, keeping
/// their start, so one huge name does not take the section.
pub const KNOWN_FAILURE_NAME_BYTES: usize = 500;
pub const KNOWN_FAILURE_URL_BYTES: usize = 300;

/// One item of the list: a test, or a failed job and step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KnownFailure {
    pub name: String,
    /// The CI run that added it to the list.
    pub url: Option<String>,
    /// The tasks that fix its finding.
    pub fix_tasks: Vec<TaskId>,
}

/// The list as a run of a task reads it: `others`, the tests it does not
/// fix, the CI run that failed last first; `fixing`, those of the CI
/// failures the task fixes; `file`, the file in the run's directory that
/// holds the whole list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KnownFailures {
    pub others: Vec<KnownFailure>,
    pub fixing: Vec<KnownFailure>,
    pub file: String,
}

impl KnownFailures {
    /// The list of `view` (`ci failures --task` with each item's
    /// `fix_tasks`), written whole to `file`; `None` when it holds no item
    /// for the run, and the prompts carry no section.
    pub fn from_view(view: &Value, file: &str) -> Option<Self> {
        let items = |key: &str| -> Vec<KnownFailure> {
            let mut items: Vec<(i64, KnownFailure)> = view[key]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|item| {
                    Some((
                        item["added"]["run_id"].as_i64().unwrap_or(0),
                        KnownFailure {
                            name: item["name"].as_str()?.to_owned(),
                            url: item["added"]["url"].as_str().map(str::to_owned),
                            fix_tasks: item["fix_tasks"]
                                .as_array()
                                .into_iter()
                                .flatten()
                                .filter_map(Value::as_i64)
                                .map(TaskId::new)
                                .collect(),
                        },
                    ))
                })
                .collect();
            // The newest CI run first; a stable sort keeps the names' order.
            items.sort_by_key(|(run, _)| std::cmp::Reverse(*run));
            items.into_iter().map(|(_, item)| item).collect()
        };
        let known = Self {
            others: items("failures"),
            fixing: items("kept_for_task"),
            file: file.to_owned(),
        };
        (!known.others.is_empty() || !known.fixing.is_empty()).then_some(known)
    }
}

/// Who reads the section, which decides what it asks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Reader {
    /// The worker, in its prompt or a resolution request.
    Worker,
    /// The run's whole review.
    Review,
}

/// The line of `item`; its name and URL cut past their limits (said read
/// whole in `file`), and whether either was. The tasks that fix it are
/// named unless `own`, the run's task's own item.
fn item_line(item: &KnownFailure, file: &str, own: bool) -> (String, bool) {
    let read = format!("{file} holds it whole");
    let (name, name_cut) =
        prompt_fit::cut_part(&item.name, KNOWN_FAILURE_NAME_BYTES, Keep::Start, &read);
    let url = item
        .url
        .as_deref()
        .map(|url| prompt_fit::cut_part(url, KNOWN_FAILURE_URL_BYTES, Keep::Start, &read));
    let url_cut = url.as_ref().is_some_and(|(_, cut)| *cut);
    let ci = url.map_or_else(|| "no URL".to_owned(), |(url, _)| url);
    let fixed = if own {
        String::new()
    } else if item.fix_tasks.is_empty() {
        "; no task fixes it yet".to_owned()
    } else {
        let tasks: Vec<String> = item.fix_tasks.iter().map(|t| format!("task {t}")).collect();
        format!("; fixed by {}", tasks.join(", "))
    };
    (
        format!("- {name} (CI run: {ci}{fixed})\n"),
        name_cut || url_cut,
    )
}

/// The section on `known` for `reader`, counted in `fit` as
/// `known_failures`; empty without a list. The task's own items come
/// first, then the others the CI run that failed last first, within
/// [`KNOWN_FAILURES_COUNT`] and [`KNOWN_FAILURES_BYTES`]; what is left out
/// is counted, with the file that holds it (`known.file`).
pub(super) fn known_failures_section(
    fit: &mut Fit,
    known: Option<&KnownFailures>,
    reader: Reader,
) -> String {
    let Some(known) = known else {
        return String::new();
    };
    let items: Vec<(&KnownFailure, bool)> = known
        .fixing
        .iter()
        .map(|item| (item, true))
        .chain(known.others.iter().map(|item| (item, false)))
        .collect();
    let lines: Vec<(String, bool)> = items
        .iter()
        .map(|(item, own)| item_line(item, &known.file, *own))
        .collect();
    let (kept, left_out) = super::worker::picked_lines(
        fit,
        "known_failures",
        &lines,
        0..lines.len(),
        (KNOWN_FAILURES_COUNT, KNOWN_FAILURES_BYTES),
    );
    let mut text = String::new();
    let fixing = known.fixing.len();
    if fixing > 0 {
        text.push_str(match reader {
            Reader::Worker => {
                "This task fixes these tests, which fail already on the landing branch's CI: make them pass:\n"
            }
            Reader::Review => {
                "This task fixes these tests, which fail already on the landing branch's CI: judge whether the diff makes them pass:\n"
            }
        });
        for (line, _) in lines
            .iter()
            .zip(&kept)
            .take(fixing)
            .filter(|(_, k)| **k)
            .map(|(l, _)| l)
        {
            text.push_str(line);
        }
    }
    // The others' lines only when one is kept; the note counts the rest.
    if kept.iter().skip(fixing).any(|k| *k) {
        text.push_str("Tests that fail already on the landing branch's CI, each with the CI run that failed and the task that fixes it:\n");
        for (line, _) in lines
            .iter()
            .zip(&kept)
            .skip(fixing)
            .filter(|(_, k)| **k)
            .map(|(l, _)| l)
        {
            text.push_str(line);
        }
    }
    if !left_out.is_empty() {
        // Counted, not named: a name may be long, and the file has them all.
        text.push_str(&format!(
            "({} failing tests left out by this section's limit. To read them: {} lists them all, with the tasks that fix each as fix_tasks.)\n",
            left_out.len(),
            known.file
        ));
    }
    // Said whenever the list holds others' tests, kept here or not: it
    // holds for those the file alone lists too.
    if !known.others.is_empty() {
        text.push_str(match reader {
            Reader::Worker => {
                "If a test of this list or of its file fails in your checks and your change does not cause it, do not fix it: name it in the receipt's summary, since the task that fixes it owns it.\n"
            }
            Reader::Review => {
                "Do not send the run back for a failure of a test of this list or of its file unless the diff causes it: the task that fixes it owns it.\n"
            }
        });
    }
    fit.section("known_failures", &text);
    text
}

/// File stems that name no test of their own (a module's or a package's
/// index): a path ending in one is named by its directory.
const INDEX_STEMS: [&str; 7] = ["mod", "lib", "main", "index", "tests", "test", "__init__"];

/// The names a path of `paths` (a changed file, or a declared path or
/// glob) gives a test's name: its file's stem, or its directory's for an
/// index file ([`INDEX_STEMS`]); a glob's last plain part. The runtime
/// knows no project's layout, so a test is matched by these alone.
pub(crate) fn touched_names(paths: &[String]) -> BTreeSet<String> {
    paths
        .iter()
        .filter_map(|path| {
            let parts: Vec<&str> = path
                .split('/')
                .filter(|part| !part.is_empty() && !part.contains(['*', '?', '[']))
                .collect();
            let (last, parent) = parts.split_last()?;
            let stem = last.split('.').next().unwrap_or(last);
            let name = if INDEX_STEMS.contains(&stem) {
                parent.last()?
            } else {
                stem
            };
            (!name.is_empty()).then(|| name.to_owned())
        })
        .collect()
}

/// Whether the test `name` is one of `touched` ([`touched_names`]): a
/// word of its module path is one of them (ADR-t2852-1 decision 5). What comes before a space is
/// the test binary's or the package's name (nextest's `<binary-id>
/// <test>`), which every test of that binary shares, and the path's last
/// part is the test's own name: neither is matched, so touching a file
/// named like the package (`dagq.toml`) or like the binary (the `it`
/// binary's `tests/it/main.rs`) keeps no test by itself, nor does a file
/// named like a test (`docs/check_status.md` for `check_status`). A test
/// with no module path is matched by nothing.
fn touches(name: &str, touched: &BTreeSet<String>) -> bool {
    let path = name.split_once(' ').map_or(name, |(_, path)| path).trim();
    let Some((module, _test)) = path.rsplit_once("::") else {
        return false;
    };
    module
        .split("::")
        .flat_map(|part| part.split(|c: char| !(c.is_alphanumeric() || c == '_' || c == '-')))
        .any(|word| touched.contains(word))
}

/// `known` with only the others' tests that `touched` names
/// ([`touched_names`]; the task's own are all kept), and how many others
/// were left out.
pub(crate) fn touched_known_failures(
    known: &KnownFailures,
    touched: &BTreeSet<String>,
) -> (KnownFailures, usize) {
    let others: Vec<KnownFailure> = known
        .others
        .iter()
        .filter(|item| touches(&item.name, touched))
        .cloned()
        .collect();
    let left_out = known.others.len() - others.len();
    (
        KnownFailures {
            others,
            fixing: known.fixing.clone(),
            file: known.file.clone(),
        },
        left_out,
    )
}
