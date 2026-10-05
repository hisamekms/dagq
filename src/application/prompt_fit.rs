//! Holding the prompts of the headless jobs and the runtime's planners to
//! their limits (task 1571, ADR-t1566-1 decisions 4 to 6), as the plan
//! review's (task 1561) and the observer's (task 1567) are: a section's
//! items are chosen in a fixed order within its count and bytes, a long
//! text or JSON value is cut to its bytes, and what was left out is
//! counted and named with how to read it. [`Fit`] adds up what each
//! section takes into the [`PromptBytes`] the job's event records.

use serde_json::Value;

use super::prompt::{FittedPrompt, PromptBytes};
use crate::domain::language::{Language, with_instruction};

/// What a prompt keeps free for the language's instruction its caller adds
/// last ([`FittedPrompt::with_language`]): the instruction is about 450
/// bytes.
pub(crate) const LANGUAGE_ROOM: usize = 1_000;

/// The end of a prompt cut at its whole limit keeps this many bytes, where
/// the instructions and the answer's schema usually are.
const KEPT_END: usize = 8_000;

/// IDs a note of what was left out names at most.
const NOTE_IDS: usize = 40;

/// The shortest a string is cut to when a JSON value is shrunk.
const SHORTEST_CUT: usize = 80;

/// Characters of a title a stub keeps.
const STUB_TITLE_CHARS: usize = 200;

/// How to read what cannot be read in a file or with a command the job may
/// run (ADR-t1566-1 decision 3): nothing is named as a way to read it.
pub(crate) const NOT_READABLE: &str = "it is in no file you can read";

/// `text` cut on a character boundary to at most `max` bytes from its
/// start.
fn head(text: &str, max: usize) -> &str {
    if text.len() <= max {
        return text;
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// `text` cut on a character boundary to at most `max` bytes from its end.
fn end(text: &str, max: usize) -> &str {
    if text.len() <= max {
        return text;
    }
    let mut start = text.len() - max;
    while !text.is_char_boundary(start) {
        start += 1;
    }
    &text[start..]
}

/// The note a cut text ends (or starts) with.
fn cut_note(left_out: usize, read: &str) -> String {
    format!("[… {left_out} bytes left out by the prompt's limit; {read}]")
}

/// Which part of a long text to keep.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Keep {
    /// The start (a description, the newest turns first). The end of a
    /// worker's screen was the other choice until task 1437 retired it.
    Start,
}

/// `text` within `max` bytes, its note of what was left out included, and
/// the bytes left out; `None` when it fits.
pub(crate) fn cut(text: &str, max: usize, keep: Keep, read: &str) -> Option<(String, usize)> {
    if text.len() <= max {
        return None;
    }
    // The note's own length depends on the count it names: take the
    // longest it can be.
    let note_room = cut_note(text.len(), read).len() + 1;
    let room = max.saturating_sub(note_room);
    let kept = match keep {
        Keep::Start => head(text, room),
    };
    let left_out = text.len() - kept.len();
    let note = cut_note(left_out, read);
    Some((format!("{kept}\n{note}"), left_out))
}

/// The longest string in `value`.
fn longest(value: &mut Value) -> Option<&mut String> {
    match value {
        Value::String(text) => Some(text),
        Value::Array(items) => items
            .iter_mut()
            .filter_map(longest)
            .max_by_key(|text| text.len()),
        Value::Object(fields) => fields
            .values_mut()
            .filter_map(longest)
            .max_by_key(|text| text.len()),
        _ => None,
    }
}

/// The stub of a value that could not be shrunk into `max`: its `id`,
/// `kind` and `title` when it has them, its size and how to read it.
fn stub(value: &Value, bytes: usize, read: &str) -> Value {
    let mut stub = serde_json::json!({"left_out_bytes": bytes, "read_with": read});
    for key in ["id", "kind"] {
        if let Some(field) = value.get(key).filter(|field| {
            !field.is_string()
                || field
                    .as_str()
                    .is_some_and(|text| text.len() <= STUB_TITLE_CHARS)
        }) {
            stub[key] = field.clone();
        }
    }
    if let Some(title) = value.get("title").and_then(Value::as_str) {
        stub["title"] = super::health::truncate(title, STUB_TITLE_CHARS)
            .unwrap_or_else(|| title.to_owned())
            .into();
    }
    stub
}

/// `value` shrunk to at most `max` bytes as one JSON line, cutting its
/// longest strings first, with `cut` saying how many bytes were left out
/// and how to read them; when it cannot be shrunk so, a stub
/// ([`stub`]). `None` when it fits.
pub(crate) fn shrink(value: &Value, max: usize, read: &str) -> Option<(Value, usize)> {
    let size = value.to_string().len();
    if size <= max {
        return None;
    }
    let marker = format!(",\"cut\":\"{} bytes left out; {read}\"", size).len() + 2;
    let target = max.saturating_sub(marker);
    let mut shrunk = value.clone();
    loop {
        let now = shrunk.to_string().len();
        if now <= target {
            break;
        }
        let over = now - target;
        let Some(text) = longest(&mut shrunk).filter(|text| text.len() > SHORTEST_CUT) else {
            let stub = stub(value, size, read);
            return Some((stub, size));
        };
        let keep = text.len().saturating_sub(over + 8).max(SHORTEST_CUT);
        let cut = format!("{}…", head(text, keep));
        // A string escaped in JSON may take more than its bytes: cut at
        // least a little each time so that the loop ends.
        *text = if cut.len() >= text.len() {
            format!("{}…", head(text, text.len() / 2))
        } else {
            cut
        };
    }
    let left_out = size.saturating_sub(shrunk.to_string().len());
    if let Value::Object(fields) = &mut shrunk {
        fields.insert(
            "cut".to_owned(),
            Value::String(format!("{left_out} bytes left out; {read}")),
        );
    }
    if shrunk.to_string().len() > max {
        return Some((stub(value, size, read), size));
    }
    Some((shrunk, left_out))
}

/// Which items of a section to keep: taken in `order` (the most relevant
/// first) within `count` items and `bytes` (each `sizes[i]` with its
/// newline); one that does not fit is skipped and the next tried, so one
/// huge item does not hide the rest. The kept indices come back in the
/// items' own order.
pub(crate) fn pick(
    sizes: &[usize],
    order: impl IntoIterator<Item = usize>,
    count: usize,
    bytes: usize,
) -> Vec<bool> {
    let mut kept = vec![false; sizes.len()];
    let (mut taken, mut used) = (0, 0);
    for index in order {
        if taken == count {
            break;
        }
        let size = sizes[index] + 1;
        if used + size <= bytes {
            kept[index] = true;
            taken += 1;
            used += size;
        }
    }
    kept
}

/// The note of a section that left `ids` out: how many, which (at most
/// [`NOTE_IDS`]) and how to read them.
pub(crate) fn left_out_note(what: &str, ids: &[String], read: &str) -> String {
    let mut named = ids
        .iter()
        .take(NOTE_IDS)
        .cloned()
        .collect::<Vec<_>>()
        .join(", ");
    if ids.len() > NOTE_IDS {
        named.push_str(&format!(" and {} more", ids.len() - NOTE_IDS));
    }
    format!(
        "({n} {what} left out by this section's limit: {named}. To read them: {read}.)\n",
        n = ids.len(),
    )
}

/// One headless prompt being held to its whole `limit`: what each section
/// takes and what each left out, recorded as it is built.
#[derive(Debug)]
pub(crate) struct Fit {
    bytes: PromptBytes,
}

impl Fit {
    pub(crate) fn new(limit: usize) -> Self {
        Self {
            bytes: PromptBytes {
                limit,
                ..PromptBytes::default()
            },
        }
    }

    /// Count `text` as (more of) section `name`.
    pub(crate) fn section(&mut self, name: &'static str, text: &str) {
        *self.bytes.sections.entry(name).or_default() += text.len();
    }

    /// Count `n` items of section `name` left out or cut.
    pub(crate) fn omit(&mut self, name: &'static str, n: usize) {
        if n > 0 {
            *self.bytes.omitted.entry(name).or_default() += n;
        }
    }

    /// Say why material the prompt cannot do without was cut.
    pub(crate) fn over(&mut self, why: String) {
        match &mut self.bytes.over_limit {
            Some(said) => {
                said.push_str("; ");
                said.push_str(&why);
            }
            None => self.bytes.over_limit = Some(why),
        }
    }

    /// `text` of section `name` within `max` bytes (see [`cut`]), counted
    /// as one item cut when it was.
    pub(crate) fn text(
        &mut self,
        name: &'static str,
        text: &str,
        max: usize,
        keep: Keep,
        read: &str,
    ) -> String {
        match cut(text, max, keep, read) {
            Some((cut, _)) => {
                self.omit(name, 1);
                cut
            }
            None => text.to_owned(),
        }
    }

    /// [`Self::text`] of material the prompt cannot do without: a cut is
    /// also said in `over_limit`.
    pub(crate) fn required(
        &mut self,
        name: &'static str,
        text: &str,
        max: usize,
        read: &str,
    ) -> String {
        match cut(text, max, Keep::Start, read) {
            Some((cut, left_out)) => {
                self.omit(name, 1);
                self.over(format!(
                    "{name}: {left_out} of {} bytes left out by its limit of {max}",
                    text.len()
                ));
                cut
            }
            None => text.to_owned(),
        }
    }

    /// `value` of section `name` as one JSON line within `max` bytes (see
    /// [`shrink`]), counted as one item cut when it was.
    pub(crate) fn json(
        &mut self,
        name: &'static str,
        value: &Value,
        max: usize,
        read: &str,
    ) -> String {
        match shrink(value, max, read) {
            Some((shrunk, _)) => {
                self.omit(name, 1);
                shrunk.to_string()
            }
            None => value.to_string(),
        }
    }

    /// The items of section `name` chosen by [`pick`], each first shrunk to
    /// `each` bytes, within `count` and `bytes`: the kept lines in their
    /// own order and the indices left out. Count each item once: either
    /// left out, or kept and cut.
    pub(crate) fn lines(
        &mut self,
        name: &'static str,
        items: &[Value],
        order: impl IntoIterator<Item = usize>,
        (count, bytes, each): (usize, usize, usize),
        read: &str,
    ) -> (Vec<String>, Vec<usize>) {
        let lines: Vec<(String, bool)> = items
            .iter()
            .map(|item| match shrink(item, each, read) {
                Some((shrunk, _)) => (shrunk.to_string(), true),
                None => (item.to_string(), false),
            })
            .collect();
        let sizes: Vec<usize> = lines.iter().map(|(line, _)| line.len()).collect();
        let kept = pick(&sizes, order, count, bytes);
        let left_out: Vec<usize> = (0..items.len()).filter(|index| !kept[*index]).collect();
        let kept_cuts = lines
            .iter()
            .zip(&kept)
            .filter(|((_, cut), kept)| *cut && **kept)
            .count();
        self.omit(name, left_out.len() + kept_cuts);
        (
            lines
                .into_iter()
                .zip(&kept)
                .filter(|(_, kept)| **kept)
                .map(|((line, _), _)| line)
                .collect(),
            left_out,
        )
    }

    /// The prompt `text` and what it takes. Its sections were held to
    /// limits whose sum keeps it within [`PromptBytes::limit`] less
    /// [`LANGUAGE_ROOM`]; should it still be over, its middle is cut out
    /// (its start and its last [`KEPT_END`] bytes, the instructions and
    /// the schema, are kept) and `over_limit` says so; the sections then
    /// say what each took before the cut. What no section counted is
    /// `instructions`.
    pub(crate) fn finish(mut self, text: String) -> FittedPrompt {
        let room = self.bytes.limit.saturating_sub(LANGUAGE_ROOM);
        let text = if text.len() > room {
            let note = cut_note(
                text.len(),
                "the sections above say how to read their material",
            );
            let start = head(&text, room.saturating_sub(KEPT_END + note.len() + 2));
            let tail = end(&text, KEPT_END);
            self.over(format!(
                "the prompt took {} bytes, past its limit of {room}: its middle was cut",
                text.len()
            ));
            format!("{start}\n{note}\n{tail}")
        } else {
            text
        };
        let counted: usize = self.bytes.sections.values().sum();
        self.bytes
            .sections
            .insert("instructions", text.len().saturating_sub(counted));
        self.bytes.total = text.len();
        FittedPrompt {
            text,
            bytes: self.bytes,
        }
    }
}

impl FittedPrompt {
    /// The prompt with the language's instruction as its last paragraph
    /// (ADR-t616-2), counted in its bytes as `language`.
    pub fn with_language(mut self, language: Option<&Language>) -> Self {
        let before = self.text.len();
        self.text = with_instruction(self.text, language);
        let added = self.text.len().saturating_sub(before);
        if added > 0 {
            self.bytes.sections.insert("language", added);
        }
        self.bytes.total = self.text.len();
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_cut_text_stays_within_its_bytes_and_says_how_to_read_the_rest() {
        let text = "あ".repeat(10_000);
        let (start, left_out) = cut(&text, 1_000, Keep::Start, "read X").unwrap();
        assert!(start.len() <= 1_000, "{}", start.len());
        assert!(start.starts_with('あ'));
        assert!(start.ends_with("read X]"), "{start}");
        assert_eq!(left_out, text.len() - start.lines().next().unwrap().len());
        assert!(cut("short", 1_000, Keep::Start, "read X").is_none());
    }

    #[test]
    fn a_shrunk_value_keeps_its_short_fields_and_names_its_cut() {
        let value = json!({"id": 7, "title": "t", "description": "d".repeat(50_000), "acceptance": "a".repeat(5_000)});
        let (shrunk, left_out) = shrink(&value, 4_000, "dagq show 7 --full").unwrap();
        assert!(
            shrunk.to_string().len() <= 4_000,
            "{}",
            shrunk.to_string().len()
        );
        assert_eq!(shrunk["id"], 7);
        assert_eq!(shrunk["title"], "t");
        assert!(
            shrunk["cut"]
                .as_str()
                .unwrap()
                .ends_with("dagq show 7 --full")
        );
        assert!(left_out > 50_000);
        // A value of many short fields becomes a stub.
        let many: serde_json::Map<String, Value> =
            (0..1_000).map(|i| (format!("k{i}"), json!("v"))).collect();
        let mut many = Value::Object(many);
        many["id"] = json!(3);
        let (stub, _) = shrink(&many, 300, "read it").unwrap();
        assert_eq!(stub["id"], 3);
        assert_eq!(stub["read_with"], "read it");
        assert!(stub.to_string().len() <= 300);
    }

    #[test]
    fn items_are_picked_in_the_order_given_and_kept_in_their_own() {
        let kept = pick(&[10, 500, 10, 10], [3, 1, 2, 0], 2, 100);
        // 3 first, 1 does not fit, 2 next; the count stops before 0.
        assert_eq!(kept, vec![false, false, true, true]);
    }

    #[test]
    fn lines_count_each_kept_cut_or_left_out_item_once() {
        let long = json!({"id": 1, "description": "x".repeat(2_000)});
        let short = json!({"id": 2});
        for (item, count, cut) in [(long.clone(), 1, true), (long, 0, true), (short, 0, false)] {
            assert_eq!(shrink(&item, 300, "read it").is_some(), cut);
            let mut fit = Fit::new(10_000);
            let (lines, left_out) =
                fit.lines("items", &[item], [0], (count, 1_000, 300), "read it");
            assert_eq!(lines.len(), count);
            assert_eq!(left_out.len(), 1 - count);
            if count == 1 {
                assert!(lines[0].contains("bytes left out; read it"));
            }
            assert_eq!(fit.finish(lines.join("\n")).bytes.omitted["items"], 1);
        }
    }

    #[test]
    fn a_prompt_past_its_whole_limit_keeps_its_start_and_its_end() {
        let mut fit = Fit::new(20_000);
        let body = format!("START{}END", "x".repeat(100_000));
        fit.section("material", &body);
        let fitted = fit.finish(body);
        assert!(fitted.text.len() <= 20_000 - LANGUAGE_ROOM);
        assert!(fitted.text.starts_with("START") && fitted.text.ends_with("END"));
        assert!(fitted.bytes.over_limit.unwrap().contains("past its limit"));
        assert_eq!(fitted.bytes.total, fitted.text.len());
    }
}
