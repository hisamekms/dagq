//! The observer's input as its prompt carries it (ADR-t1566-1, task 1567):
//! the decision material within per-section and whole-prompt limits, chosen
//! in a fixed order, with what each section left out and how to read it,
//! and `observe --input`, the read of an observation's whole input that
//! the observer's `dagq` can run (`JobAccess::QueueCli`).
//!
//! The sections go in priority order. The required ones (the KPI breaches,
//! the open asks, the alerts) have no limit of their own: only the whole
//! prompt's limit cuts them, before any other section takes a byte. The
//! others share what is left, each within its own limits. The instructions
//! (the role, the window and cursor, what the observer may write, how to
//! read) are never cut.
use anyhow::{Context, Result, bail};
use serde::Serialize;
use serde_json::{Map, Value, json};

/// The whole prompt's bytes, the language instruction included: about 15%
/// of the host's `ARG_MAX` (1 MiB on macOS) and above the 127 KB or so
/// the production input of 2026-10-03 comes to once summarized.
pub const PROMPT_LIMIT: usize = 160_000;
/// Kept for the language instruction appended after the prompt is built.
pub const LANGUAGE_RESERVE: usize = 1_000;
/// Characters a string inside an item keeps in the prompt.
pub const TEXT_CHARS: usize = 300;
/// Entries a list inside an item keeps in the prompt: the last ones, the
/// newest of the queue's lists (evidence ids, marks, periods).
pub const NESTED_ITEMS: usize = 10;
/// Items an `observe --input` answer gives at a time by default.
pub const INPUT_PAGE: usize = 10;
/// Bytes an `observe --input` answer carries before it pages a list or
/// lists an object's keys instead.
pub const INPUT_READ_BYTES: usize = 24_000;

/// One section of the input as the prompt carries it.
struct Spec {
    /// The path of the material in the input (and `observe --input
    /// --section`), its name in the prompt.
    path: &'static str,
    /// A list of items, or an object whose keys are the entries.
    object: bool,
    required: bool,
    max_items: usize,
    /// The bytes the section's entries take.
    max_bytes: usize,
    /// The bytes one entry of an object may take.
    max_entry: usize,
    /// The command that reads the material as it is now, besides the
    /// observation's snapshot (`{dagq}` and `{since}` filled in).
    cli: &'static str,
}

const NONE: usize = usize::MAX;

/// The sections in priority order.
const SECTIONS: &[Spec] = &[
    Spec {
        path: "kpi.breaches",
        object: false,
        required: true,
        max_items: NONE,
        max_bytes: NONE,
        max_entry: NONE,
        cli: "`{dagq} kpi`",
    },
    Spec {
        path: "open_asks",
        object: false,
        required: true,
        max_items: NONE,
        max_bytes: NONE,
        max_entry: NONE,
        cli: "`{dagq} asks --open`",
    },
    Spec {
        path: "stats.alerts",
        object: false,
        required: true,
        max_items: NONE,
        max_bytes: NONE,
        max_entry: NONE,
        cli: "`{dagq} stats{since}`",
    },
    Spec {
        path: "stats.running_alerts",
        object: false,
        required: true,
        max_items: NONE,
        max_bytes: NONE,
        max_entry: NONE,
        cli: "`{dagq} stats{since}`",
    },
    Spec {
        path: "stats",
        object: true,
        required: false,
        max_items: NONE,
        max_bytes: 40_000,
        max_entry: 8_000,
        cli: "`{dagq} stats{since}`",
    },
    Spec {
        path: "kpi",
        object: true,
        required: false,
        max_items: NONE,
        max_bytes: 32_000,
        max_entry: 16_000,
        cli: "`{dagq} kpi`",
    },
    Spec {
        path: "findings",
        object: false,
        required: false,
        max_items: 100,
        max_bytes: 48_000,
        max_entry: NONE,
        cli: "`{dagq} findings` and `{dagq} findings ID --full`",
    },
    Spec {
        path: "improvements",
        object: true,
        required: false,
        max_items: NONE,
        max_bytes: 4_000,
        max_entry: 4_000,
        cli: "`{dagq} findings`",
    },
    Spec {
        path: "notes",
        object: false,
        required: false,
        max_items: 20,
        max_bytes: 12_000,
        max_entry: NONE,
        cli: "`{dagq} notes`",
    },
    Spec {
        path: "graph.critical",
        object: false,
        required: false,
        max_items: 50,
        max_bytes: 2_000,
        max_entry: NONE,
        cli: "`{dagq} graph`",
    },
    Spec {
        path: "graph.candidates",
        object: false,
        required: false,
        max_items: 200,
        max_bytes: 4_000,
        max_entry: NONE,
        cli: "`{dagq} candidates`",
    },
];

/// The keys of `stats` that go first, in this order; the rest follow by
/// name. `alerts` and `running_alerts` are sections of their own.
const STATS_ORDER: &[&str] = &[
    "next_cursor",
    "overall",
    "stall_thresholds",
    "stall_config",
    "failed_tests",
    "updates",
    "asks",
    "waiting",
    "jobs",
    "recommendations",
    "escalations",
    "auto_repairs",
    "review_reasons",
    "reason_codes",
    "conflict_hotspots",
    "landing_utilization",
];

/// The keys of `kpi` that go first; `breaches` is a section of its own.
const KPI_ORDER: &[&str] = &["error", "config", "targets", "trend", "forecast"];

/// What the prompt's sections came to, as `observe_started` records it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SectionSize {
    pub name: String,
    pub bytes: usize,
    /// Items (keys of an object section) in the input; 0 for the
    /// instructions.
    pub total: usize,
    pub kept: usize,
    pub omitted: usize,
}

/// The prompt's text and what each of its sections came to.
#[derive(Debug, Clone)]
pub struct Fitted {
    pub text: String,
    pub sections: Vec<SectionSize>,
}

/// Where the read methods point: the observation's directory name, the
/// `dagq` the agent runs and the cursor the window starts past.
pub struct Reading<'a> {
    pub observation: &'a str,
    pub dagq: &'a str,
    pub since: Option<i64>,
}

struct Material<'a> {
    spec: &'a Spec,
    /// Each item rendered, or each entry as `"key":value`.
    entries: Vec<(String, String)>,
    /// Object entries the prompt carries summarized.
    summarized: Vec<String>,
    /// List items [`clip`] shortened.
    clipped: Vec<usize>,
    kept: Vec<usize>,
}

impl Material<'_> {
    fn render(&self, reading: &Reading<'_>) -> String {
        let spec = self.spec;
        let total = self.entries.len();
        let unit = if spec.object { "keys" } else { "items" };
        let required = if spec.required { " (required)" } else { "" };
        let mut text = format!(
            "### {}{required}: {} of {total} {unit}\n",
            spec.path,
            self.kept.len()
        );
        if !self.kept.is_empty() {
            let (open, close) = if spec.object { ("{", "}") } else { ("[", "]") };
            let lines: Vec<&str> = self
                .kept
                .iter()
                .map(|&index| self.entries[index].1.as_str())
                .collect();
            text.push_str(&format!(
                "```json\n{open}\n{}\n{close}\n```\n",
                lines.join(",\n")
            ));
        }
        let snapshot = format!(
            "`{} observe --input {} --section {}",
            reading.dagq, reading.observation, spec.path
        );
        let cli = spec.cli.replace("{dagq}", reading.dagq).replace(
            "{since}",
            &reading
                .since
                .map(|since| format!(" --since {since}"))
                .unwrap_or_default(),
        );
        let omitted = total - self.kept.len();
        let unit = match (omitted, spec.object) {
            (1, true) => "key",
            (1, false) => "item",
            _ => unit,
        };
        if omitted > 0 {
            if spec.object {
                let names: Vec<&str> = (0..total)
                    .filter(|index| !self.kept.contains(index))
                    .map(|index| self.entries[index].0.as_str())
                    .collect();
                text.push_str(&format!(
                    "Left out {omitted} {unit} ({}): read each with {snapshot}.<key>`, or now with {cli}.\n",
                    names.join(", ")
                ));
            } else {
                let from = self.kept.len();
                text.push_str(&format!(
                    "Left out {omitted} {unit} (from offset {from}): read them with {snapshot} --offset {from}`, or now with {cli}.\n"
                ));
            }
        }
        let summarized: Vec<&str> = self
            .summarized
            .iter()
            .filter(|key| {
                self.kept
                    .iter()
                    .any(|&index| self.entries[index].0 == **key)
            })
            .map(String::as_str)
            .collect();
        if !summarized.is_empty() {
            text.push_str(&format!(
                "Summarized here: {}; read the whole with {snapshot}.<key>`.\n",
                summarized.join(", ")
            ));
        }
        // Only a list's items are clipped; an object's entries go whole or
        // not at all.
        if !spec.object && self.kept.iter().any(|index| self.clipped.contains(index)) {
            text.push_str(&format!(
                "Strings are cut at {TEXT_CHARS} characters and lists inside an item keep their last {NESTED_ITEMS} (`<key>_omitted` counts the rest): read item I whole with {snapshot} --offset I --limit 1` (I counts from 0 in the list above).\n"
            ));
        }
        text
    }

    fn size(&self, reading: &Reading<'_>) -> SectionSize {
        let total = self.entries.len();
        SectionSize {
            name: self.spec.path.to_owned(),
            bytes: self.render(reading).len(),
            total,
            kept: self.kept.len(),
            omitted: total - self.kept.len(),
        }
    }
}

/// The value at `path` (keys joined by `.`, an index into a list as a
/// number) of `value`.
fn at<'a>(value: &'a Value, path: &str) -> Option<&'a Value> {
    path.split('.').try_fold(value, |value, key| match value {
        Value::Object(object) => object.get(key),
        Value::Array(items) => key.parse::<usize>().ok().and_then(|index| items.get(index)),
        _ => None,
    })
}

fn compact(value: &Value) -> String {
    serde_json::to_string(value).unwrap_or_default()
}

/// `value` with its strings cut at [`TEXT_CHARS`] characters and the lists
/// of its objects cut to their last [`NESTED_ITEMS`], each with a
/// `<key>_omitted` count of what was cut.
pub fn clip(value: &Value) -> Value {
    match value {
        Value::String(text) if text.chars().count() > TEXT_CHARS => {
            let kept: String = text.chars().take(TEXT_CHARS).collect();
            let rest = text.chars().count() - TEXT_CHARS;
            Value::String(format!("{kept}… ({rest} more characters)"))
        }
        Value::Array(items) => Value::Array(items.iter().map(clip).collect()),
        Value::Object(object) => {
            let mut clipped = Map::new();
            for (key, value) in object {
                match value {
                    Value::Array(items) if items.len() > NESTED_ITEMS => {
                        let skip = items.len() - NESTED_ITEMS;
                        clipped.insert(
                            key.clone(),
                            Value::Array(items[skip..].iter().map(clip).collect()),
                        );
                        clipped.insert(format!("{key}_omitted"), json!(skip));
                    }
                    _ => {
                        clipped.insert(key.clone(), clip(value));
                    }
                }
            }
            Value::Object(clipped)
        }
        other => other.clone(),
    }
}

/// `kpi.trend` without each period's marks and worsened KPIs beyond the
/// first [`NESTED_ITEMS`]: the counts stand in for them.
fn trend_summary(trend: &Value) -> Value {
    let Some(periods) = trend.as_object() else {
        return trend.clone();
    };
    let summary = periods
        .iter()
        .map(|(period, rows)| {
            let rows = rows.as_array().cloned().unwrap_or_default();
            let rows: Vec<Value> = rows
                .iter()
                .map(|row| {
                    let worsened = row["worsened"].as_array().cloned().unwrap_or_default();
                    json!({
                        "label": row["label"],
                        "partial": row["partial"],
                        "runs": row["runs"],
                        "marks": row["marks"].as_array().map_or(0, Vec::len),
                        "worsened": worsened.iter().take(NESTED_ITEMS).collect::<Vec<_>>(),
                        "worsened_omitted": worsened.len().saturating_sub(NESTED_ITEMS),
                    })
                })
                .collect();
            (period.clone(), Value::Array(rows))
        })
        .collect::<Map<_, _>>();
    Value::Object(summary)
}

/// `kpi.forecast` with each period's label and counts, without the
/// `forecast.*` KPIs (a bias is judged through `kpi.breaches` only).
fn forecast_summary(forecast: &Value) -> Value {
    let Some(periods) = forecast.as_object() else {
        return forecast.clone();
    };
    let summary = periods
        .iter()
        .map(|(period, rows)| {
            let rows: Vec<Value> = rows
                .as_array()
                .cloned()
                .unwrap_or_default()
                .iter()
                .map(|row| json!({"label": row["label"], "partial": row["partial"], "details": row["details"]}))
                .collect();
            (period.clone(), Value::Array(rows))
        })
        .collect::<Map<_, _>>();
    Value::Object(summary)
}

fn material<'a>(spec: &'a Spec, input: &Value) -> Material<'a> {
    let value = at(input, spec.path).cloned().unwrap_or(Value::Null);
    let mut summarized = Vec::new();
    let mut clipped = Vec::new();
    let entries = if spec.object {
        let object = value.as_object().cloned().unwrap_or_default();
        let (order, skip): (&[&str], &[&str]) = match spec.path {
            "stats" => (STATS_ORDER, &["alerts", "running_alerts"]),
            "kpi" => (KPI_ORDER, &["breaches"]),
            _ => (&[], &[]),
        };
        let mut keys: Vec<&String> = object
            .keys()
            .filter(|key| !skip.contains(&key.as_str()))
            .collect();
        keys.sort_by_key(|key| {
            (
                order
                    .iter()
                    .position(|first| first == key)
                    .unwrap_or(order.len()),
                (*key).clone(),
            )
        });
        keys.into_iter()
            .map(|key| {
                let value = &object[key];
                let value = match (spec.path, key.as_str()) {
                    ("kpi", "trend") => {
                        summarized.push(key.clone());
                        trend_summary(value)
                    }
                    ("kpi", "forecast") => {
                        summarized.push(key.clone());
                        forecast_summary(value)
                    }
                    _ => value.clone(),
                };
                (
                    key.clone(),
                    format!("{}:{}", compact(&json!(key)), compact(&value)),
                )
            })
            .collect()
    } else {
        value
            .as_array()
            .cloned()
            .unwrap_or_default()
            .iter()
            .enumerate()
            .map(|(index, item)| {
                let short = clip(item);
                if short != *item {
                    clipped.push(index);
                }
                (index.to_string(), compact(&short))
            })
            .collect()
    };
    Material {
        spec,
        entries,
        summarized,
        clipped,
        kept: Vec::new(),
    }
}

/// The prompt: `head` (the instructions, never cut) and the sections of
/// `input` in priority order within `limit` bytes less
/// [`LANGUAGE_RESERVE`]. Each section keeps its entries in order while
/// they fit its own limits and what the whole has left; a list stops at
/// the first that does not fit, so what it left out is one run from an
/// offset, and an object skips a key that does not fit and goes on. What a
/// section left out is named with how to read it.
pub fn fit(head: &str, input: &Value, reading: &Reading<'_>, limit: usize) -> Fitted {
    let limit = limit.saturating_sub(LANGUAGE_RESERVE);
    let mut materials: Vec<Material<'_>> =
        SECTIONS.iter().map(|spec| material(spec, input)).collect();
    let empty: Vec<usize> = materials
        .iter()
        .map(|material| material.render(reading).len())
        .collect();
    for index in 0..materials.len() {
        let others: usize = materials[..index]
            .iter()
            .map(|material| material.render(reading).len())
            .sum::<usize>()
            + empty[index + 1..].iter().sum::<usize>();
        let mut left = limit.saturating_sub(head.len() + others + empty[index]);
        let material = &mut materials[index];
        let spec = material.spec;
        let mut used = 0;
        for entry in 0..material.entries.len() {
            let cost = material.entries[entry].1.len() + 2;
            let fits = material.kept.len() < spec.max_items
                && used + cost <= spec.max_bytes
                && cost <= spec.max_entry.saturating_add(2)
                && cost <= left;
            if fits {
                material.kept.push(entry);
                used += cost;
                left -= cost;
            } else if !spec.object {
                break;
            }
        }
    }
    // The estimate leaves out the counts' digits and the lines a section
    // adds when it carries entries: drop the last entries of the latest
    // sections until the whole fits.
    let total = |materials: &[Material<'_>]| {
        head.len()
            + materials
                .iter()
                .map(|material| material.render(reading).len())
                .sum::<usize>()
    };
    while total(&materials) > limit {
        let Some(material) = materials
            .iter_mut()
            .rev()
            .find(|material| !material.kept.is_empty())
        else {
            break;
        };
        material.kept.pop();
    }
    let mut text = head.to_owned();
    let mut sections = vec![SectionSize {
        name: "instructions".to_owned(),
        bytes: head.len(),
        total: 0,
        kept: 0,
        omitted: 0,
    }];
    for material in &materials {
        text.push_str(&material.render(reading));
        sections.push(material.size(reading));
    }
    Fitted { text, sections }
}

/// Whether `observation` can name a directory of `<queue dir>/observer/`:
/// a unix second with an optional `-N` suffix, nothing that leaves it.
fn check_observation(observation: &str) -> Result<()> {
    if observation.is_empty()
        || !observation
            .chars()
            .all(|character| character.is_ascii_digit() || character == '-')
    {
        bail!(
            "{observation:?} names no observation: give the directory name `observe --history` shows in `dir` (a unix second, perhaps with -N)"
        );
    }
    Ok(())
}

/// `observe --input OBSERVATION [--section PATH] [--offset N] [--limit N]`:
/// the observation's whole input (its `input.json`) as the observer's
/// `dagq` reads it, the observation's name checked before `text` (what
/// `read` gives of the file) is read. Without a section, the paths of the
/// input (two levels deep) with their bytes and items; a list, from
/// `offset` up to `limit` items within [`INPUT_READ_BYTES`] (at least one)
/// with the next offset; an object over [`INPUT_READ_BYTES`], its keys
/// with their bytes; any other value, as it is.
pub fn read_input(
    observation: &str,
    read: impl FnOnce() -> Result<String>,
    section: Option<&str>,
    offset: usize,
    limit: usize,
) -> Result<Value> {
    check_observation(observation)?;
    let text = read().with_context(|| format!("observation {observation} has no input to read"))?;
    let input: Value = serde_json::from_str(&text)
        .with_context(|| format!("read the input of observation {observation}"))?;
    let Some(section) = section else {
        let mut sections = Vec::new();
        if let Some(object) = input.as_object() {
            for (key, value) in object {
                sections.push(outline(key, value));
                if let Some(inner) = value.as_object() {
                    for (child, value) in inner {
                        sections.push(outline(&format!("{key}.{child}"), value));
                    }
                }
            }
        }
        return Ok(json!({"observation": observation, "sections": sections}));
    };
    let Some(value) = at(&input, section) else {
        bail!(
            "observation {observation}'s input has no {section:?}: `observe --input {observation}` lists its sections"
        );
    };
    Ok(match value {
        Value::Array(items) => {
            let mut page = Vec::new();
            let mut bytes = 0;
            for item in items.iter().skip(offset).take(limit) {
                let size = compact(item).len();
                if !page.is_empty() && bytes + size > INPUT_READ_BYTES {
                    break;
                }
                bytes += size;
                page.push(item.clone());
            }
            let next = offset + page.len();
            json!({
                "observation": observation,
                "section": section,
                "total": items.len(),
                "offset": offset,
                "items": page,
                "next_offset": (next < items.len()).then_some(next),
            })
        }
        Value::Object(object) if compact(value).len() > INPUT_READ_BYTES => {
            let keys: Vec<Value> = object
                .iter()
                .map(|(key, value)| outline(&format!("{section}.{key}"), value))
                .collect();
            json!({
                "observation": observation,
                "section": section,
                "bytes": compact(value).len(),
                "keys": keys,
                "hint": format!("over {INPUT_READ_BYTES} bytes: read one key with --section {section}.<key>"),
            })
        }
        _ => json!({"observation": observation, "section": section, "value": value}),
    })
}

fn outline(path: &str, value: &Value) -> Value {
    let items = match value {
        Value::Array(items) => Some(items.len()),
        Value::Object(object) => Some(object.len()),
        _ => None,
    };
    json!({"section": path, "bytes": compact(value).len(), "items": items})
}

/// Whether `read` names an observation `observe --input` can read.
pub fn check_read(observation: &str, limit: usize) -> Result<()> {
    check_observation(observation)?;
    if limit == 0 {
        bail!("observe --input's limit is 1 or more");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reading() -> Reading<'static> {
        Reading {
            observation: "1791005872",
            dagq: "dagq",
            since: Some(12),
        }
    }

    #[test]
    fn a_long_string_and_a_long_inner_list_are_cut_with_counts() {
        let item = json!({"summary": "x".repeat(TEXT_CHARS + 5), "evidence": (1..=15).collect::<Vec<_>>(), "id": 3});
        let clipped = clip(&item);
        assert_eq!(clipped["id"], 3);
        assert!(
            clipped["summary"]
                .as_str()
                .unwrap()
                .ends_with("… (5 more characters)")
        );
        assert_eq!(clipped["evidence"], json!((6..=15).collect::<Vec<_>>()));
        assert_eq!(clipped["evidence_omitted"], 5);
    }

    #[test]
    fn the_read_of_a_section_rejects_a_path_out_of_the_observer_directory() {
        for name in ["", "..", "../x", "1/2", "a"] {
            assert!(check_read(name, 1).is_err(), "{name:?}");
        }
        assert!(check_read("1791005872-1", 1).is_ok());
        assert!(check_read("1791005872", 0).is_err());
    }

    #[test]
    fn the_trend_and_the_forecast_are_summarized_with_counts() {
        let input = json!({"kpi": {
            "trend": {"day": [{"label": "d", "partial": false, "runs": 3, "marks": [1, 2, 3],
                                "worsened": (0..12).map(|n| json!({"kpi": n})).collect::<Vec<_>>()}]},
            "forecast": {"day": [{"label": "d", "partial": true, "details": {"samples": 2}, "kpis": {"big": "x"}}]},
        }});
        let fitted = fit("head\n", &input, &reading(), PROMPT_LIMIT);
        assert!(
            fitted.text.contains(r#""marks":3"#) && fitted.text.contains(r#""worsened_omitted":2"#),
            "{}",
            fitted.text
        );
        assert!(!fitted.text.contains(r#""big""#), "{}", fitted.text);
        assert!(
            fitted.text.contains("Summarized here: trend, forecast; read the whole with `dagq observe --input 1791005872 --section kpi.<key>`."),
            "{}",
            fitted.text
        );
    }

    /// An item whose string alone was cut says how to read it whole.
    #[test]
    fn an_item_cut_only_in_its_strings_names_its_whole_read() {
        let input =
            json!({"findings": [{"id": 1}, {"id": 2, "detail": "d".repeat(TEXT_CHARS + 1)}]});
        let fitted = fit("head\n", &input, &reading(), PROMPT_LIMIT);
        assert!(
            fitted.text.contains("read item I whole with `dagq observe --input 1791005872 --section findings --offset I --limit 1`"),
            "{}",
            fitted.text
        );
        let whole = fit(
            "head\n",
            &json!({"findings": [{"id": 1}]}),
            &reading(),
            PROMPT_LIMIT,
        );
        assert!(!whole.text.contains("Strings are cut"), "{}", whole.text);
    }
}
