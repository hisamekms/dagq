use super::*;
use crate::domain::host_metrics::{DiskFree, DiskSpace, HOST_NODE, local_day, summarize};
use crate::domain::kpi::HostReader;
use crate::domain::ledger::{
    LEDGER_V,
    settle::{SETTLE_GRACE_MS, node_settlement},
    window::{WindowQuery, select},
};
use crate::domain::stats::Cursor;

const BUILT: &str = "2030-01-01T00:00:00.000Z";
/// The start of an hour, and of a UTC day.
const T0: i64 = 1_900_022_400;
const JST: i64 = 9 * 3600;

/// A sample at `unix` as the store keeps it: loads, the CPU per kind, the
/// memory, the pageouts and the disk, each with more decimals than a file
/// keeps, as written; `blank` leaves the CPU and the load empty.
fn sample(unix: i64, step: i64, blank: bool) -> HostSample {
    #[allow(clippy::cast_precision_loss)]
    let x = step as f64;
    let mut sample = HostSample::new(unix).with_disk(Some(DiskSpace {
        free_bytes: 50_000_000_000 - u64::try_from(step).unwrap() * 7_777_777,
        total_bytes: 100_000_000_000,
    }));
    if !blank {
        sample.set("load1", Some(1.0 + (x * 0.37) % 5.0 + 0.004_9));
        sample.set("load5", Some(0.5 + x / 1000.0));
        sample.set("cpu_total", Some(100.0 + (x * 13.3) % 300.0));
        sample.set("cpu_rustc", Some((x * 7.1) % 200.0));
        sample.set("cpu_claude", Some(12.345_6));
    }
    sample.set("mem_used_mb", Some(8000.0 + x));
    sample.set("pageouts", Some(1000.0 + x * 3.0));
    sample.as_written()
}

/// The NodeSampleStore's rows of the host: every 30 s for four hours and a
/// half from `T0`, the second hour imported from a CSV file, every 7th
/// sample with empty cells, and no record for 40 minutes in the third hour.
fn store() -> Vec<NodeSample> {
    let mut rows = Vec::new();
    let mut id = 0;
    for step in 0..540_i64 {
        let unix = T0 + step * 30 + i64::from(step > 100);
        if (2 * 3600 + 600..2 * 3600 + 3000).contains(&(unix - T0)) {
            continue;
        }
        id += 1;
        rows.push(NodeSample {
            id,
            node_id: HOST_NODE.to_owned(),
            local_day: local_day(unix, JST),
            source: if (3600..7200).contains(&(unix - T0)) {
                SampleSource::CsvImport
            } else {
                SampleSource::Sampled
            },
            sample: sample(unix, step, step % 7 == 3),
        });
    }
    rows
}

fn host_samples(rows: &[NodeSample]) -> Vec<HostSample> {
    rows.iter().map(|row| row.sample.clone()).collect()
}

/// The rows fold one per node and hour, and keep each sample whole: its
/// second, local day, source and values (empty cells and more decimals
/// than JSON numbers keep included) read back as the store gave them,
/// through the row's JSON too.
#[test]
fn a_node_row_keeps_its_hours_samples_whole() {
    let store = store();
    let rows = node_rows(&store, BUILT);
    assert_eq!(rows.len(), 5);
    let first = &rows[0];
    assert_eq!(first.kind, LedgerKind::Node);
    assert_eq!(first.key, format!("host/{T0}"));
    assert_eq!(first.started_at, Some(T0 * 1000));
    assert_eq!(first.ended_at, Some((T0 + 3600) * 1000));
    assert_eq!(first.subject.as_ref().unwrap().id, HOST_NODE);
    assert_eq!(first.ledger_v, LEDGER_V);
    assert!(!first.is_final && first.legacy.is_none() && first.records.is_empty());
    assert_eq!(first.inputs_through.node_samples, Some(120));
    let back: Vec<NodeSample> = rows.iter().flat_map(samples).collect();
    assert_eq!(back, store);
    assert!(
        samples(&rows[1])
            .iter()
            .all(|row| row.source == SampleSource::CsvImport)
    );
    // Through the JSON the store keeps it in.
    let mut odd = store[3].clone();
    odd.sample.set("load15", Some(0.1 + 0.2));
    let row = node_row(HOST_NODE, T0, &[&odd], BUILT);
    let text = serde_json::to_string(&row.detail).unwrap();
    let mut read = row.clone();
    read.detail = serde_json::from_str(&text).unwrap();
    assert_eq!(samples(&read), vec![odd]);
    // An unknown column (a newer binary's) is left out; a row of another
    // kind keeps no sample.
    read.detail["samples"][0]["values"]["gpu"] = json!("1");
    assert!(!samples(&read)[0].sample.values.contains_key("gpu"));
    read.kind = LedgerKind::Queue;
    assert!(samples(&read).is_empty());
}

/// The summary made of the rows is the one the samples make, column by
/// column (the minimum, mean, median, maximum and p90), the CPU seconds
/// (the seconds covered, the largest gap, the total and per kind) and the
/// disk's free space: across hours, within one, starting at an hour's
/// first sample (which stands for the seconds since the window's start,
/// not since the hour before's last sample), across the stretch with no
/// record, and over a kpi day's period.
#[test]
fn the_rows_summarize_any_window_as_the_samples_do() {
    let store = store();
    let rows = node_rows(&store, BUILT);
    let samples = host_samples(&store);
    let day_reader = |from: i64, until: i64| summarize(&samples, from, until);
    let day = HostReader(&day_reader).between((T0 - JST) * 1000, (T0 - JST + 86_400) * 1000);
    let windows = [
        (T0 + 1234, T0 + 3 * 3600 + 77),
        (T0 + 3600 + 100, T0 + 3600 + 2000),
        (T0 + 3600 + 1, T0 + 3600 + 900),
        (T0 + 2 * 3600 + 300, T0 + 2 * 3600 + 3500),
        (T0 - 5000, T0 + 40),
        (T0 - JST + 1, T0 - JST + 86_400),
        (day.from, day.until),
    ];
    for (from, until) in windows {
        let expected = summarize(&samples, from, until);
        let made = host_summary(&rows, from, until);
        assert_eq!(made, expected, "{from}..{until}");
        assert_eq!(DiskFree::of(&made), DiskFree::of(&expected));
        let (made_cpu, cpu) = (made.cpu_secs.unwrap(), expected.cpu_secs.unwrap());
        assert_eq!(made_cpu.covered_secs, cpu.covered_secs);
        assert_eq!(made_cpu.max_gap_secs, cpu.max_gap_secs);
        assert_eq!(made_cpu.total, cpu.total);
        assert_eq!(made_cpu.by_kind, cpu.by_kind);
        for column in ["load1", "cpu_total", "mem_used_mb", "pageouts_per_min"] {
            let (made, expected) = (
                made.metrics[column].unwrap(),
                expected.metrics[column].unwrap(),
            );
            assert_eq!(
                (made.min, made.mean, made.median, made.max, made.p90),
                (
                    expected.min,
                    expected.mean,
                    expected.median,
                    expected.max,
                    expected.p90
                )
            );
        }
    }
    // The day's period holds every sample; the empty cells count in no
    // summary; the hour's first sample stands for the seconds since the
    // window's start.
    assert_eq!(day.samples, store.len());
    let load = day.metrics["load1"].unwrap();
    assert!(load.samples < day.samples);
    let starting = host_summary(&rows, T0 + 3600 - 5, T0 + 3600 + 31);
    assert_eq!(starting.first, Some(T0 + 3600 + 1));
    // A window before the records, and rows of another node, read none.
    assert_eq!(host_summary(&rows, T0 - 100, T0 - 1).samples, 0);
    let mut other = store.clone();
    for row in &mut other {
        row.node_id = "remote".to_owned();
    }
    assert_eq!(
        host_summary(&node_rows(&other, BUILT), T0, T0 + 9999).samples,
        0
    );
}

/// The common read takes the node rows of the hours its times reach,
/// whatever its filters, and reads the host's load over its seconds as
/// `stats` does and over each span as `kpi`'s periods and `--compare`
/// windows do.
#[test]
fn a_window_takes_the_node_rows_of_its_times() {
    let store = store();
    let rows = node_rows(&store, BUILT);
    let samples = host_samples(&store);
    let query = |since: i64, until: i64| WindowQuery {
        since: Some(Cursor::Time(since * 1000)),
        until: Some(Cursor::Time(until * 1000)),
        now_ms: (T0 + 86_400) * 1000,
        ..WindowQuery::default()
    };
    let window = select(rows.clone(), &query(T0 + 3000, T0 + 2 * 3600 + 10));
    let keys: Vec<&str> = window
        .of(LedgerKind::Node)
        .map(|row| row.key.as_str())
        .collect();
    assert_eq!(
        keys,
        [T0, T0 + 3600, T0 + 7200].map(|start| key(HOST_NODE, start))
    );
    assert_eq!(
        window.host(),
        summarize(&samples, T0 + 3000, T0 + 2 * 3600 + 10)
    );
    let (start, end) = ((T0 + 3000) * 1000, (T0 + 5000) * 1000);
    let reader = |from: i64, until: i64| summarize(&samples, from, until);
    assert_eq!(
        window.host_between(start, end),
        HostReader(&reader).between(start, end)
    );
    // A window inside one hour takes that hour alone; one past the
    // records takes none and reads none.
    let inside = select(rows.clone(), &query(T0 + 3700, T0 + 3800));
    assert_eq!(inside.of(LedgerKind::Node).count(), 1);
    let past = select(rows.clone(), &query(T0 + 86_000, T0 + 86_100));
    assert_eq!(past.of(LedgerKind::Node).count(), 0);
    assert_eq!(past.host().samples, 0);
    // The filters choose no node row away.
    let goal = WindowQuery {
        goal_id: Some(crate::domain::GoalId::new(3)),
        change: Some("fix".parse().unwrap()),
        area: Some("runtime".to_owned()),
        ..query(T0, T0 + 3599)
    };
    let filtered = select(rows.clone(), &goal);
    assert_eq!(filtered.of(LedgerKind::Node).count(), 1);
    assert_eq!(filtered.host(), summarize(&samples, T0, T0 + 3599));
    // Without `--since`, from the window's first event, as `stats`.
    let event = crate::domain::RunEvent {
        id: crate::domain::EventId::new(1),
        task_id: None,
        goal_id: None,
        run_id: None,
        kind: "supervisor_started".into(),
        payload: json!({}),
        created_at: crate::domain::marks::utc_text((T0 + 4000) * 1000),
        actor: None,
    };
    let mut all = rows;
    all.extend(crate::domain::ledger::build(
        std::slice::from_ref(&event),
        (T0 + 9000) * 1000,
        BUILT,
    ));
    let open = select(
        all,
        &WindowQuery {
            now_ms: (T0 + 9000) * 1000,
            ..WindowQuery::default()
        },
    );
    assert_eq!(open.host(), summarize(&samples, T0 + 4000, T0 + 9000));
    assert_eq!(open.of(LedgerKind::Node).count(), 2);
}

/// A node row ends with its hour and settles past the grace; a sample of
/// its hour that came in after what it folded (taken late, or imported
/// from a CSV file after the hour) makes it dirty, a settled one too; a
/// row of an older fold is dirty.
#[test]
fn a_node_row_ends_with_its_hour_and_a_later_sample_makes_it_dirty() {
    let store = store();
    let row = node_row(
        HOST_NODE,
        T0,
        &store[..120].iter().collect::<Vec<_>>(),
        BUILT,
    );
    let end = (T0 + 3600) * 1000;
    let within = node_settlement(&row, Some(120), end - 1);
    assert_eq!(
        (within.ended, within.dirty, within.settles),
        (false, false, false)
    );
    let ended = node_settlement(&row, Some(120), end);
    assert_eq!(
        (ended.ended, ended.dirty, ended.settles),
        (true, false, false)
    );
    let settles = node_settlement(&row, Some(120), end + SETTLE_GRACE_MS);
    assert_eq!(
        (settles.ended, settles.dirty, settles.settles),
        (true, false, true)
    );
    // An imported row of the hour, with a later id.
    let mut late = store[5].clone();
    late.id = 9_999;
    late.sample.unix = T0 + 3599;
    late.source = SampleSource::CsvImport;
    let dirty = node_settlement(&row, Some(late.id), end + 2 * SETTLE_GRACE_MS);
    assert_eq!(
        (dirty.ended, dirty.dirty, dirty.settles),
        (true, true, false)
    );
    let mut settled = row.clone();
    settled.is_final = true;
    assert!(node_settlement(&settled, Some(late.id), end + SETTLE_GRACE_MS).dirty);
    let mut refolded: Vec<&NodeSample> = store[..120].iter().collect();
    refolded.push(&late);
    let again = node_row(HOST_NODE, T0, &refolded, BUILT);
    assert!(!node_settlement(&again, Some(late.id), end + SETTLE_GRACE_MS).dirty);
    assert_eq!(samples(&again).last(), Some(&late));
    let mut older = row.clone();
    older.ledger_v = LEDGER_V - 1;
    assert!(node_settlement(&older, Some(120), end + SETTLE_GRACE_MS).dirty);
    // Another kind is never ended here.
    let mut queue = row;
    queue.kind = LedgerKind::Queue;
    assert!(!node_settlement(&queue, Some(120), end + SETTLE_GRACE_MS).ended);
}

/// The retention drops the samples of a local day before the store's
/// oldest kept day, and a row left with none, so a window past it reads
/// no host's load, as the store and the files do.
#[test]
fn the_retention_drops_the_samples_and_rows_of_days_before_it() {
    // An hour that straddles the host's local midnight (at +09:30).
    let offset = 9 * 3600 + 1800;
    let at = |id: i64, unix: i64| NodeSample {
        id,
        node_id: HOST_NODE.to_owned(),
        local_day: local_day(unix, offset),
        source: SampleSource::Sampled,
        sample: sample(unix, id, false),
    };
    let midnight = T0 + 86_400 - offset;
    let store = [
        at(1, midnight - 7200),
        at(2, midnight - 1200),
        at(3, midnight - 30),
        at(4, midnight + 30),
        at(5, midnight + 600),
    ];
    let rows = node_rows(&store, BUILT);
    assert_eq!(rows.len(), 2);
    let today = local_day(midnight, offset);
    let kept: Vec<LedgerRow> = rows.iter().filter_map(|row| retained(row, today)).collect();
    assert_eq!(kept.len(), 1);
    assert_eq!(samples(&kept[0]), store[3..].to_vec());
    assert_eq!(kept[0].key, rows[1].key);
    assert_eq!(
        host_summary(&kept, midnight - 3600, midnight - 1).samples,
        0
    );
    assert!(
        rows.iter()
            .all(|row| retained(row, today - 1).as_ref() == Some(row))
    );
    assert!(rows.iter().all(|row| retained(row, today + 1).is_none()));
    // A row whose samples do not read is left as it is.
    let mut unread = rows[0].clone();
    unread.detail["samples"] = json!("not samples");
    assert_eq!(retained(&unread, today + 1), Some(unread));
}
