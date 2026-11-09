//! Fixed search corpus shared by both adapters. Source rows and their index
//! maintenance use the store execution port, and every case rolls back.
use super::ContractStore;
use crate::domain::{DomainError, search::*};
use crate::infrastructure::{
    search_writes::{SearchWrite, maintain},
    sql_exec::{SqlDialect, SqlRead, SqlWrite, sql_params},
};
use anyhow::{Result, ensure};

fn query(terms: &str) -> SearchQuery {
    SearchQuery {
        terms: terms.into(),
        kinds: vec![],
        statuses: vec![],
        goal_id: None,
        limit: 100,
        full: true,
    }
}

/// Includes across-kind scoring, tied scores, short timestamps, literal LIKE
/// metacharacters, Unicode folding, boolean precedence and invalid syntax.
pub fn search_cases<S: ContractStore, F: Fn() -> Result<S>>(open: &F) -> Result<()> {
    let store = open()?;
    let tx = store.write_entry("contract_search")?;
    let goal = tx.insert_id(
        "INSERT INTO goals(id,title,created_at,updated_at) VALUES (700001,'全文検索','2026-01-01','2026-01-01')",
        &[],
    )?;
    maintain(
        &tx,
        SearchWrite::Goal {
            id: goal,
            previous_acceptance: None,
        },
    )?;
    let mut ids = Vec::new();
    for (i, (title, body, at)) in [
        ("全文検索", "", "2026-01-01"),
        (
            "全文検索",
            "全文検索 plan_review src/domain/search.rs 100% a_b c\\d CAFÉ",
            "2026-01-03",
        ),
        ("other", "planXreview aXb 1000 café", "2026-01-02"),
        ("全文検索", "", "2026-01-01"),
    ]
    .into_iter()
    .enumerate()
    {
        let id = tx.insert_id(
            "INSERT INTO tasks(id,title,description,acceptance,verification_commands,goal_id,created_at,updated_at) VALUES (?5,?1,?2,'','[]',?3,'2026-01-01',?4)",
            sql_params![title, body, goal, at, 700002 + i as i64],
        )?;
        maintain(&tx, SearchWrite::Task(id))?;
        ids.push(SearchRef::Id(id));
    }
    let note = tx.insert_id("INSERT INTO run_events(id,task_id,kind,payload,created_at) VALUES (700006,?1,'observation','{\"text\":\"全文検索\"}','2026-01-01')", sql_params![700002_i64])?;
    maintain(&tx, SearchWrite::Note(note))?;
    let commit = tx.insert_id("INSERT INTO landed_commits(id,run_id,task_id,commit_sha,message,landed_at) VALUES (700007,'search-case',?1,'abc123','全文検索','2026-01-01')", sql_params![700002_i64])?;
    maintain(&tx, SearchWrite::Commit(commit))?;
    // The five minimal documents tie. Their explicit source IDs fix the
    // inter-kind doc_order/rowid order independently of consumed sequences.
    let ranked = vec![
        SearchRef::Commit("abc123".into()),
        SearchRef::Id(note),
        ids[3].clone(),
        ids[0].clone(),
        SearchRef::Id(goal),
        ids[1].clone(),
    ];
    let ranked_kinds = vec![
        SearchKind::Commit,
        SearchKind::Note,
        SearchKind::Task,
        SearchKind::Task,
        SearchKind::Goal,
        SearchKind::Task,
    ];
    for (terms, expected) in [
        ("全文検索", ranked.clone()),
        (
            "全文",
            vec![
                ids[1].clone(),
                ranked[0].clone(),
                ranked[1].clone(),
                ranked[2].clone(),
                ranked[3].clone(),
                ranked[4].clone(),
            ],
        ),
        ("plan_review", vec![ids[1].clone()]),
        ("domain/search", vec![ids[1].clone()]),
        ("100%", vec![ids[1].clone()]),
        ("%", vec![ids[1].clone()]),
        ("_", vec![ids[1].clone()]),
        ("全文 plan_review", vec![ids[1].clone()]),
        ("a_b", vec![ids[1].clone()]),
        ("c\\d", vec![ids[1].clone()]),
        ("CAFÉ", vec![ids[2].clone(), ids[1].clone()]),
        ("café", vec![ids[2].clone(), ids[1].clone()]),
        (
            "plan_review OR planXreview",
            vec![ids[2].clone(), ids[1].clone()],
        ),
        ("(全文検索 NOT plan_review)", ranked[..5].to_vec()),
        ("全文検索 AND plan_review", vec![ids[1].clone()]),
    ] {
        let page = store.search_in(&tx, &query(terms))?;
        let actual: Vec<_> = page.hits.iter().map(|h| h.id.clone()).collect();
        ensure!(actual == expected, "{terms}: {actual:?} != {expected:?}");
        ensure!(page.total == expected.len());
        if terms == "全文検索" {
            ensure!(page.hits.iter().map(|h| h.kind).collect::<Vec<_>>() == ranked_kinds);
        }
    }
    let mut limited = query("全文検索");
    limited.limit = 1;
    let page = store.search_in(&tx, &limited)?;
    ensure!(page.hits.len() == 1 && page.total == 6);
    ensure!(page.hits[0].score.is_some() && page.hits[0].fields.is_some());
    limited.full = false;
    let page = store.search_in(&tx, &limited)?;
    ensure!(page.hits[0].score.is_none() && page.hits[0].fields.is_none());
    let mut filtered = query("全文検索");
    filtered.kinds = vec![SearchKind::Task];
    filtered.statuses = vec!["draft".into()];
    filtered.goal_id = Some(crate::domain::GoalId::new(goal));
    ensure!(
        store
            .search_in(&tx, &filtered)?
            .hits
            .iter()
            .map(|h| h.id.clone())
            .collect::<Vec<_>>()
            == vec![ids[3].clone(), ids[0].clone(), ids[1].clone()]
    );
    filtered.statuses = vec!["ready".into()];
    ensure!(store.search_in(&tx, &filtered)?.total == 0);
    for terms in [
        "全文 OR plan_review",
        "全文 NOT plan_review",
        "(全文)",
        "plan_review OR",
        "NOT plan_review",
        "()",
        "plan_review ()",
        "plan_review (other)",
        "(plan_review",
        "plan_review)",
        "plan_review AND AND other",
    ] {
        let error = store.search_in(&tx, &query(terms)).unwrap_err();
        ensure!(
            matches!(
                error.downcast_ref::<DomainError>(),
                Some(DomainError::SearchQuery { .. })
            ),
            "{terms}: {error:?}"
        );
    }
    Ok(())
}

/// Backend-specific expectation, deliberately excluded from the common hits.
/// Postgres CI's official image has en_US.utf8 LC_CTYPE; C is an ADR limit.
pub fn short_non_ascii<S: ContractStore, F: Fn() -> Result<S>>(open: &F) -> Result<()> {
    let store = open()?;
    let tx = store.write_entry("short_non_ascii")?;
    let id = tx.insert_id("INSERT INTO tasks(title,description,acceptance,verification_commands,created_at,updated_at) VALUES ('é','','','[]','2026-01-01','2026-01-01')", &[])?;
    maintain(&tx, SearchWrite::Task(id))?;
    let expected = usize::from(tx.dialect() == SqlDialect::Postgres);
    let page = store.search_in(&tx, &query("É"))?;
    ensure!(page.total == expected);
    if expected == 1 {
        ensure!(page.hits[0].id == SearchRef::Id(id));
        ensure!(page.hits[0].excerpt == "é");
    }
    Ok(())
}
