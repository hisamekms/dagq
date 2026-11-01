//! The layers read from the ledger's run rows: any key of the claim's
//! attributes, and the derived layers by their rules whatever the
//! attributes hold under their names.

use super::*;
use crate::domain::claim_facts;
use crate::domain::ledger;

/// The runs of one day that cover the cases of the derived layers, and
/// the run ids in their order.
fn layered_queue() -> (Queue, Vec<RunId>) {
    let mut queue = Queue::default();
    let at = |index: i64| MONDAY + HOUR * index;
    let mut runs = Vec::new();
    // 1: Claude, in the trial, with `rustc`.
    let mut run = Run::new(1, None, at(1), 100);
    run.session = Some(("claude-opus-5-5", "medium", Some("control")));
    run.rustc = Some("1.90.0");
    runs.push(run);
    // 2: claimed on Codex: its claim keeps the step, its turn names the
    // model; a full supervisor under load.
    let mut run = Run::new(2, None, at(2), 100);
    run.provider = "codex";
    run.session = Some(("claude-opus-5-5", "high", Some("treatment")));
    run.build = "b2";
    run.slots = 3;
    run.load = 6.0;
    runs.push(run);
    // 3: claude → codex in the middle, no Codex turn.
    let mut run = Run::new(3, None, at(3), 100);
    run.session = Some(("claude-sonnet-5", "medium", Some("treatment")));
    run.switched_to = Some("codex");
    runs.push(run);
    // 4: codex → claude in the middle, after a Codex turn.
    let mut run = Run::new(4, None, at(4), 100);
    run.provider = "codex";
    run.session = Some(("claude-opus-5-5", "medium", None));
    run.switched_to = Some("claude");
    runs.push(run);
    // 5: Claude, claimed before the worker session was recorded.
    runs.push(Run::new(5, None, at(5), 100));
    // 6: Codex, claimed before the worker session was recorded.
    let mut run = Run::new(6, None, at(6), 100);
    run.provider = "codex";
    runs.push(run);
    // 7: moved to Codex at the claim; the claim's model is not the one
    // Codex ran.
    let mut run = Run::new(7, None, at(7), 100);
    run.session = Some(("claude-sonnet-5", "medium", Some("treatment")));
    runs.push(run);
    for run in &runs {
        queue.run(run);
    }
    let ids: Vec<RunId> = runs.iter().map(run_id).collect();
    let seven = Some(ids[6].as_str());
    queue.push(
        Some(7),
        seven,
        "provider_switched",
        json!({"from": "claude", "to": "codex", "reason": "usage_limit", "phase": "claim"}),
        at(7) - 1,
    );
    queue.push(
        Some(7),
        seven,
        "turn_finished",
        json!({"turn": 1, "outcome": "succeeded", "provider": "codex", "model": "gpt-6-astra"}),
        at(7) + 30,
    );
    for event in &mut queue.events {
        if event.kind != EventKind::RunClaimed {
            continue;
        }
        let run = event.run_id.clone().unwrap();
        if run == ids[6] {
            event.payload["requested_provider"] = json!("claude");
            event.payload["provider"] = json!("codex");
            event.payload["model"] = json!("gpt-5-codex");
        }
        // Runs 1, 2 and 4 recorded their attributes; the others are
        // claims from before, read as legacy rows.
        if [&ids[0], &ids[1], &ids[3]].contains(&&run) {
            let attributes = claim_facts::attributes(event.payload.as_object().unwrap());
            event.payload[claim_facts::ATTRIBUTES] = Value::Object(attributes);
        }
    }
    queue.sort();
    (queue, ids)
}

/// The ledger's row of each run, as the builder folds it.
fn ledger_rows(queue: &Queue, ids: &[RunId], now_ms: i64) -> Vec<ledger::LedgerRow> {
    ids.iter()
        .map(|id| {
            let events: Vec<RunEvent> = queue
                .events
                .iter()
                .filter(|event| event.run_id.as_ref() == Some(id))
                .cloned()
                .collect();
            ledger::run_row(id.as_str(), &events, &[], now_ms, "t")
        })
        .collect()
}

/// The twelve layers the claim was split by before the ledger, each with
/// the strata it gave the seven runs of [`layered_queue`].
const TWELVE: [(&str, &[(&str, f64)]); 12] = [
    ("build", &[("b1", 6.0), ("b2", 1.0)]),
    ("parallel", &[("3", 7.0)]),
    ("slot", &[("low", 6.0), ("full", 1.0)]),
    ("load", &[("low", 6.0), ("mid", 1.0)]),
    (
        "toolchain",
        &[("1.90.0 aarch64-apple-darwin", 1.0), ("unknown", 6.0)],
    ),
    ("claude", &[("2.1.0", 1.0), ("none", 5.0), ("unknown", 1.0)]),
    ("provider", &[("claude", 3.0), ("codex", 4.0)]),
    ("route", &[("interactive", 4.0), ("headless", 3.0)]),
    ("codex", &[("0.46.0", 3.0), ("unknown", 4.0)]),
    (
        "group",
        &[("control", 1.0), ("none", 4.0), ("unknown", 2.0)],
    ),
    (
        "model",
        &[
            ("claude-opus-5-5", 1.0),
            ("gpt-6-astra", 3.0),
            ("unknown", 3.0),
        ],
    ),
    (
        "effort",
        &[("medium", 4.0), ("high", 1.0), ("unknown", 2.0)],
    ),
];

/// The strata of `layer` in `strata`, by value.
fn strata_of(strata: &BTreeMap<String, Measure>, layer: &str) -> BTreeMap<String, Option<f64>> {
    strata
        .iter()
        .filter_map(|(stratum, measure)| {
            let value = stratum.strip_prefix(&format!("{layer}="))?;
            Some((value.to_owned(), measure.value))
        })
        .collect()
}

fn expected(values: &[(&str, f64)]) -> BTreeMap<String, Option<f64>> {
    values
        .iter()
        .map(|(value, n)| ((*value).to_owned(), Some(*n)))
        .collect()
}

/// The twelve layers keep their names and values when read from the
/// ledger's rows: a provider moved at the claim, switched both ways in the
/// middle, a Codex model unlike the claim's, Codex runs out of Claude and
/// the trial (`group=none`, and `unknown` without an effort), legacy rows
/// beside rows with recorded attributes, and rows that hold none. An attribute named as a
/// derived layer is not read for it; any other key, known to no claim or
/// not, splits the runs by the attribute and `--compare` splits by it too.
#[test]
fn the_layers_read_any_attribute_and_derive_the_twelve_as_before() {
    let (mut queue, ids) = layered_queue();
    let now = MONDAY + DAY + HOUR;
    let mut by: Vec<Axis> = TWELVE
        .iter()
        .map(|(layer, _)| layer.parse().unwrap())
        .collect();
    by.extend(["instructions_prompt", "nobody_records"].map(|key| key.parse().unwrap()));
    let query = KpiQuery {
        last: 1,
        at: Some(Cursor::Time(MONDAY * 1000)),
        by,
        ..KpiQuery::default()
    };
    // Read from the claims as the runs' events give them.
    let from_claims = queue.kpi(now, &KpiConfig::default(), &query);
    let mut rows = ledger_rows(&queue, &ids, now * 1000);
    // The rows' attributes hold keys named as the derived layers, and one
    // run an attribute no claim records today.
    for key in claim_facts::RESERVED {
        rows[0].attributes.insert(key.to_owned(), json!("bogus"));
    }
    rows[0]
        .attributes
        .insert("instructions_prompt".to_owned(), json!("p1"));
    // A row folded of the phases whose claim came before the attributes
    // holds none: the claim's fields are read instead.
    rows[2].attributes.clear();
    rows[4].attributes.clear();
    queue.rows = rows;
    let from_rows = queue.kpi(now, &KpiConfig::default(), &query);
    let strata = &from_rows.periods[0].window.kpis["landings"];
    for (layer, values) in TWELVE {
        assert_eq!(strata_of(strata, layer), expected(values), "{layer}");
        let before = &from_claims.periods[0].window.kpis["landings"];
        assert_eq!(strata_of(before, layer), expected(values), "{layer}");
    }
    assert!(
        strata.keys().all(|stratum| !stratum.contains("bogus")),
        "{strata:?}"
    );
    assert_eq!(
        strata_of(strata, "instructions_prompt"),
        expected(&[("p1", 1.0), ("unknown", 6.0)])
    );
    assert_eq!(
        strata_of(strata, "nobody_records"),
        expected(&[("unknown", 7.0)])
    );
    // `--compare` splits by the same layers next to its own.
    let spec: CompareSpec = format!(
        "@{}..@{},@{}..@{}",
        MONDAY - DAY,
        MONDAY,
        MONDAY,
        MONDAY + DAY
    )
    .parse()
    .unwrap();
    let compared = queue.kpi(
        now,
        &KpiConfig::default(),
        &KpiQuery {
            compare: Some(spec),
            by: vec![Axis::PROVIDER, "instructions_prompt".parse().unwrap()],
            ..KpiQuery::default()
        },
    );
    let compare = compared.compare.unwrap();
    let after = |stratum: &str| compare.strata["landings"][stratum].after.value;
    assert_eq!(after("provider=codex"), Some(4.0));
    assert_eq!(after("instructions_prompt=p1"), Some(1.0));
    assert_eq!(after("instructions_prompt=unknown"), Some(6.0));
    assert_eq!(after("parallel=3"), Some(7.0));
    assert_eq!(after("build=b2"), Some(1.0));
}

/// `--by` takes any key a claim's attribute may have, the known names as
/// themselves, and refuses what a stratum could not read back and the
/// retired kind; the derived
/// layers are the names a claim's attribute never takes.
#[test]
fn a_layer_is_any_attribute_key_and_the_derived_ones_are_reserved() {
    assert_eq!("provider".parse::<Axis>(), Ok(Axis::PROVIDER));
    assert_eq!("build".parse::<Axis>().unwrap().derived(), None);
    assert!("k".repeat(64).parse::<Axis>().is_ok());
    assert!("rustc_v2.x".parse::<Axis>().is_ok());
    let key: Axis = "instructions_prompt".parse().unwrap();
    assert_eq!((key.as_str(), key.derived()), ("instructions_prompt", None));
    let long = "k".repeat(65);
    for text in [
        "",
        "a=b",
        "a|b",
        "a b",
        "cross:x",
        "kind",
        "Build",
        "1st",
        "a-b",
        long.as_str(),
    ] {
        assert!(text.parse::<Axis>().is_err(), "{text:?}");
    }
    let mut derived: Vec<&str> = Derived::ALL.map(Derived::as_str).to_vec();
    let mut reserved = claim_facts::RESERVED.to_vec();
    derived.sort_unstable();
    reserved.sort_unstable();
    assert_eq!(derived, reserved);
    assert_eq!(
        serde_json::to_value(&key).unwrap(),
        json!("instructions_prompt")
    );
    assert_eq!(
        serde_json::from_value::<Axis>(json!("group")).unwrap(),
        Axis::GROUP
    );
}
