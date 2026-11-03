---
id: plan-team-version-sqlite-dialect
type: plan
title: queue の store の SQLite に固有の書き方の棚卸し（種類ごとの数・ファイル・PostgreSQL での候補・扱う層）
status: active
created: 2026-11-02
owners:
  - hisamekms
tags:
  - architecture
  - persistence
related:
  - adr-t3981-1
  - adr-t3982-1
  - design-persistence
---

# queue の store の SQLite に固有の書き方の棚卸し

チームの版の段1（goal 249）で、queue の store に PostgreSQL の adapter を足せる形にする前に、SQLite に固有の書き方がどこに何か所あるかを数えた。どれをどの層で扱い、段1で何を切り出すかは [ADR-t3982-1](../adr/2026-11-02-t3982-1-store-adapter-shares-ports-row-mapping-and-standard-sql-and-splits-dialect-by-functions-and-per-adapter-statements.md) が決める。この文書は数えた結果と、種類ごとの PostgreSQL での書き方の候補だけを持つ。

## 数えた commit と数え方

- 数えた commit: `1f2d8341a65a9a46ea498c176fcf21d588297647`（2026-11-03 の main）。この commit には段1の方言の関数（`infrastructure/sql_dialect.rs`）があり、src の `json_extract` の一部と日時の関数の全部は文が綴らずその関数が返す。式は SQL の文字列を数えるので、呼び出しは数えず、関数の本体と unit test の期待の文字列を数える（呼び出しの数は下の「方言の関数の呼び出し」）。
- 数えたのは tracked の `src/` と `migrations/` で、どちらも test のコード（`#[cfg(test)]` の module と `*_tests.rs`）とコメントの行を含む。行数は一致した行の数（1 行に 2 つあっても 1）、ファイル数は一致した行を持つファイルの数。
- 1 つの種類について次の形で数えた（`<式>` は下の表の grep の式）。

```sh
git grep -h -E '<式>' -- src | wc -l          # src の行数
git grep -l -E '<式>' -- src | wc -l          # src のファイル数
git grep -c -E '<式>' -- src                  # src のファイルごとの行数
git grep -h -E '<式>' -- migrations | wc -l   # migrations の行数
git grep -h -o -E '<式>' -- src | wc -l       # 出現の数（行の数と違うものだけ書く）
```

- goal 249 の記述の数（json_extract 236 など）は同じ日の別の数え方で、ここの数とずれる。ここでは上の式を正とする。

## 一覧

扱う層の語は ADR-t3982-1 の語で、共有の SQL（両方の adapter が同じ文を流す）・方言の関数（共有の SQL に埋める断片を adapter ごとに返す）・adapter の中（adapter ごとに別の文やコードを持つ）・migration（schema の系列が持ち、Postgres の系列は後の ADR）の 4 つ。

| 種類 | grep の式 | src | migrations | PostgreSQL での候補 | 扱う層 |
| --- | --- | --- | --- | --- | --- |
| transaction の始め方（`WriteTx::begin` の引数） | `TransactionBehavior::` | 219 行 / 34（出現 220） | 0 | `BEGIN` と書き込みの係の lease の確かめ（ADR-t3981-1 決定4。SQLite の入口も同じ確かめを持つ）、読み取りは `BEGIN READ ONLY` | adapter の中（名前付きの入口の実装） |
| JSON 関数 | `json_(extract\|valid\|each\|type\|set\|object\|group_array\|remove\|array\|insert\|replace\|patch\|tree)\(\|[^a-z_.]json\((\?\|')` | 190 行 / 27（出現 199） | 90 行 / 27（出現 104） | `->` / `->>`、`jsonb_array_elements`、`jsonb_typeof`、`jsonb_set`、`jsonb_build_object`、`jsonb_agg`、`::jsonb` の検証と cast | src の `json_extract` は方言の関数、ほかの JSON 関数（`json(...)` を含む）は決定4 が段1で切り出さない種類で Postgres の adapter の段が当てる、migrations は migration |
| うち `json(...)` | `[^a-z_.]json\((\?\|')` | 3 行 / 2 | 0 | `?::jsonb`、`'...'::jsonb` | 決定4 が段1で切り出さない種類（ほかの JSON 関数）。Postgres の adapter の段で方言の関数か adapter の中 |
| うち `json_extract` | `json_extract\(` | 150 行 / 17（出現 155） | 55 行 / 14（出現 56） | `(col::jsonb ->> 'key')` に読む型の cast | 方言の関数 |
| CREATE TRIGGER | `CREATE (TEMP \|TEMPORARY )?TRIGGER` | 16 行 / 5（全て test と doc comment） | 47 行 / 8 | 下の「CREATE TRIGGER の内訳」 | migration（移し先は後の ADR） |
| REFERENCES | `REFERENCES ` | 2 行 / 2（test） | 99 行 / 29 | そのまま書ける（`FOREIGN KEY` は標準） | migration |
| datetime/strftime と日時の文字列 | `datetime\(\|strftime\(\|julianday\(` | 16 行 / 3 | 30 行 / 16（出現 33） | `now()`・`to_char`・`extract(epoch from ...)`・`timestamptz` の算術 | src は方言の関数、migrations は migration |
| 日時の列の比較 | `_at [<>]=? ?\?` | 22 行 / 8 | 0 | 比べる列は全て INTEGER（ミリ秒）の列なので `bigint` で同じ比較が書ける。TEXT の日時の列の比較は「型の緩さに頼る所」 | 共有の SQL |
| FTS5 の検索一式 | `fts5\|search_index\| MATCH \|bm25\(\|snippet\(\|highlight\(` | 20 行 / 7（出現 31） | 64 行 / 7（出現 65） | `tsvector` と GIN、`pg_trgm` の trigram 索引、`ts_rank` | adapter の中（検索の adapter） |
| INSERT OR | `INSERT OR (IGNORE\|REPLACE)` | 9 行 / 7 | 9 行 / 8 | `ON CONFLICT DO NOTHING`、`ON CONFLICT (...) DO UPDATE` | 共有の SQL に書き直す（段1では切り出さない） |
| `unixepoch()` | `unixepoch\(` | 5 行 / 3（出現 8） | 22 行 / 15 | `extract(epoch from now())::bigint` | 決定4 が段1で切り出す日時の関数（`datetime`・`strftime`・`julianday`）に入らない。src は Postgres の adapter の段で方言の関数、migrations（列の `DEFAULT`）は migration |
| `ifnull(...)` | `ifnull\(` | 14 行 / 3（出現 29） | 20 行 / 11（出現 34） | `COALESCE`（SQLite も受ける）。索引の式の `ifnull(col, 0)` は `COALESCE` の式の索引か `UNIQUE NULLS NOT DISTINCT` | 決定4 が段1で切り出さない種類。src は共有の SQL に書き直す、migrations は migration |
| ON CONFLICT | `ON CONFLICT` | 19 行 / 11 | 0 | 同じ構文（`excluded.` も同じ） | 共有の SQL |
| RETURNING | `RETURNING` | 0 | 0 | — | — |
| rowid の並び順と行の選び方 | `BY (r\.)?rowid\|\(rowid\)\|\.rowid\|MAX\(rowid\)\|, rowid"\|rowid IN\|SELECT rowid` | 34 行 / 10（出現 39） | 7 行 / 5 | 挿入の順を持つ列（identity か作成時刻）で並べる、主キーか `ctid` で行を選ぶ | 決定4 が段1で切り出さない種類。共有の SQL（並べる列と選ぶ鍵は Postgres の migration が決める）、migrations は migration |
| PRAGMA | `pragma_(update\|query_value\|foreign_key_check)\|PRAGMA ` | 15 行 / 2（出現 16） | 0 | 接続の設定・`schema` の版の表・`pg_catalog` | adapter の中 |
| last_insert_rowid | `last_insert_rowid` | 26 行 / 17（全て rusqlite の呼び出しで SQL でない） | 0 | `INSERT ... RETURNING id` を `query_row` で読む | adapter の中（挿入して ID を返す関数） |
| SQLite の system の表 | `sqlite_(sequence\|master\|schema)` | 3 行 / 1 | 25 行 / 2（出現 36） | `information_schema.tables`・`pg_catalog`、identity の列の sequence（`pg_get_serial_sequence`・`setval`） | 決定4 が段1で切り出さない種類。src は adapter の中（schema の検査と `next_id`）、migrations は migration |
| AUTOINCREMENT | `AUTOINCREMENT` | 2 行 / 2（test） | 51 行 / 28 | `GENERATED ALWAYS AS IDENTITY` | migration |
| 型の緩さに頼る所 | 下の「型の緩さに頼る所」 | — | — | — | 種類ごと |

### ファイル

上の表の種類ごとに、`git grep -c -E '<式>' -- src` と `-- migrations` の結果を全て並べる（数はファイルごとの行数、多い順。src は `src/` からの、migrations は `migrations/` からの path）。種類の中で SQL でない行や test の行を分けたものは、その下に書く。

- `TransactionBehavior::`（src、219 行 / 34 ファイル）: `infrastructure/sqlite.rs` 32、`infrastructure/runtime_store/transitions.rs` 31、`infrastructure/runtime_store/recovery.rs` 23、`infrastructure/runtime_store/coordination.rs` 15、`infrastructure/asks.rs` 13、`infrastructure/plan_reviews.rs` 11、`infrastructure/sessions.rs` 10、`infrastructure/replans.rs` 8、`infrastructure/runtime_store/run_log.rs` 8、`infrastructure/draft_planners.rs` 6、`infrastructure/goal_reviews.rs` 6、`infrastructure/runtime_store/session_steps.rs` 5、`context_boundaries/write_tx.rs` 4、`infrastructure/runtime_store/attributes.rs` 4、`infrastructure/task_holds.rs` 4、`infrastructure/ci_watch_store.rs` 3、`infrastructure/finding_planners.rs` 3、`infrastructure/findings.rs` 3、`infrastructure/plan_requests.rs` 3、`infrastructure/replan_apply.rs` 3、`infrastructure/runtime_store/eval_rounds.rs` 3、`infrastructure/runtime_store/session_registry.rs` 3、`infrastructure/follow_up_membership.rs` 2、`infrastructure/headless_jobs.rs` 2、`infrastructure/kpi_push.rs` 2、`infrastructure/planners.rs` 2、`infrastructure/runtime_store/ledger.rs` 2、`infrastructure/runtime_store/queue_records.rs` 2、`infrastructure/queue_service.rs` 1、`infrastructure/recurring_drafts.rs` 1、`infrastructure/runtime_store/mod.rs` 1、`infrastructure/runtime_store/node_samples.rs` 1、`infrastructure/runtime_store/run_resources.rs` 1、`infrastructure/session_tokens.rs` 1。
  - `Immediate` が出現 206、`Deferred` が 13（全て読み取りの transaction）、`Exclusive` が 1（`WriteTx::begin` の中の判定）。`context_boundaries/write_tx.rs` の 4 は境界の test。
- JSON 関数（src、190 行 / 27 ファイル）: `infrastructure/sql_dialect.rs` 67、`infrastructure/draft_planners.rs` 15、`infrastructure/finding_planners.rs` 13、`infrastructure/asks.rs` 9、`infrastructure/plan_requests.rs` 8、`infrastructure/runtime_store/ledger.rs` 8、`infrastructure/ci_watch_store.rs` 6、`infrastructure/follow_up_membership.rs` 6、`infrastructure/related.rs` 6、`infrastructure/replans.rs` 6、`infrastructure/sqlite.rs` 6、`infrastructure/proposals.rs` 5、`infrastructure/runtime_store/run_log.rs` 5、`infrastructure/planners.rs` 4、`infrastructure/task_holds.rs` 4、`infrastructure/plan_reviews.rs` 3、`infrastructure/queue_service/supervisor_tests.rs` 3、`domain/goal_review.rs` 2、`infrastructure/goal_reviews.rs` 2、`infrastructure/recurring_drafts.rs` 2、`infrastructure/replan_apply.rs` 2、`infrastructure/runtime_store/eval_rounds.rs` 2、`infrastructure/session_tokens.rs` 2、`domain/plan_review.rs` 1、`domain/recovery.rs` 1、`domain/replan_detect.rs` 1、`infrastructure/sessions.rs` 1。
  - `infrastructure/sql_dialect.rs` の 67 行は方言の関数の本体 1 と、その unit test が期待する式 66。`domain/` の 4 ファイル 5 行は Rust の関数 `parse_json_object` の名前に当たったもので SQL でない。両方を除けば、文が綴る SQL は 118 行 / 22 ファイル。
  - この一覧に在って下の `json_extract` の一覧に無い src のファイルは、文が綴る JSON 関数が `json_extract` 以外だけの `infrastructure/runtime_store/ledger.rs`（`json_each` 5、`json_group_array` 2、`json_type` 1）・`infrastructure/runtime_store/run_log.rs`（`json_each` 5）・`infrastructure/runtime_store/eval_rounds.rs`（`json_each` 2）・`infrastructure/session_tokens.rs`（`json_set`・`json_remove`）・`infrastructure/sessions.rs`（`json_set` と `json(...)` の 1 行）と、`infrastructure/replan_apply.rs`（`json_each` 2）と、上の SQL でない `domain/` の 4 ファイルである。
  - 方言の関数の外で SQL の文字列が綴る `json_extract` の出現 88 のうち `payload` を読むものが 69 で、ほかは `material`・`records`・`json_each` の `value`。方言の関数の呼び出しは下の「方言の関数の呼び出し」。
- うち `json(...)`（src、3 行 / 2 ファイル）: `infrastructure/queue_service/supervisor_tests.rs` 2、`infrastructure/sessions.rs` 1。
  - 3 行とも `json_set` の値に JSON の文字列を JSON として渡す（`json(?2)`・`json('{...}')`）。`supervisor_tests.rs` の 2 行は test の入力。式の `[^a-z_.]` は、Rust の関数（`fn json(`・`.json(`）と `json_` の関数を除くためのもの。
- JSON 関数（migrations、90 行 / 27 ファイル）: `0064_follow_up_membership.sql` 13、`0026_search.sql` 9、`0028_draft_planners.sql` 7、`0039_open_kinds.sql` 7、`0030_findings.sql` 6、`0032_run_env_program_events.sql` 5、`0035_session_events.sql` 5、`0036_change_marks.sql` 5、`0050_no_check_constraints.sql` 4、`0022_follow_up_triage.sql` 3、`0070_origin_and_human_priority.sql` 3、`0001_queue.sql` 2、`0016_observer.sql` 2、`0025_stalled_ask.sql` 2、`0027_plan_review.sql` 2、`0029_ask_reasons.sql` 2、`0058_events_by_opened_event.sql` 2、`0093_events_by_supervisor.sql` 2、`0008_goals.sql` 1、`0012_queue_events.sql` 1、`0014_asks.sql` 1、`0017_stuck_exit_ask.sql` 1、`0021_proposals.sql` 1、`0033_auto_update.sql` 1、`0041_draft_reopens.sql` 1、`0042_goal_reviews.sql` 1、`0057_worker_mode_default.sql` 1。
- うち `json_extract`（src、150 行 / 17 ファイル）: `infrastructure/sql_dialect.rs` 67、`infrastructure/finding_planners.rs` 13、`infrastructure/draft_planners.rs` 11、`infrastructure/asks.rs` 7、`infrastructure/plan_requests.rs` 7、`infrastructure/follow_up_membership.rs` 6、`infrastructure/replans.rs` 6、`infrastructure/sqlite.rs` 6、`infrastructure/proposals.rs` 5、`infrastructure/related.rs` 4、`infrastructure/task_holds.rs` 4、`infrastructure/ci_watch_store.rs` 3、`infrastructure/plan_reviews.rs` 3、`infrastructure/planners.rs` 3、`infrastructure/goal_reviews.rs` 2、`infrastructure/recurring_drafts.rs` 2、`infrastructure/queue_service/supervisor_tests.rs` 1。
  - `infrastructure/sql_dialect.rs` の 67 は上の JSON 関数の行と同じ（本体 1、test の期待 66）。残る 16 ファイル 83 行が、まだ方言の関数を通さず `json_extract` を綴る文である。
- うち `json_extract`（migrations、55 行 / 14 ファイル）: `0026_search.sql` 9、`0064_follow_up_membership.sql` 8、`0028_draft_planners.sql` 4、`0030_findings.sql` 4、`0032_run_env_program_events.sql` 4、`0035_session_events.sql` 4、`0036_change_marks.sql` 4、`0039_open_kinds.sql` 4、`0050_no_check_constraints.sql` 4、`0070_origin_and_human_priority.sql` 3、`0022_follow_up_triage.sql` 2、`0058_events_by_opened_event.sql` 2、`0093_events_by_supervisor.sql` 2、`0057_worker_mode_default.sql` 1。
- CREATE TRIGGER（src、16 行 / 5 ファイル）: `infrastructure/schema.rs` 10、`infrastructure/runtime_store/coordination.rs` 2、`infrastructure/runtime_store/ledger.rs` 2、`context_boundaries/sql.rs` 1、`infrastructure/sessions.rs` 1。
  - 内訳は下の「CREATE TRIGGER の内訳」。
- CREATE TRIGGER（migrations、47 行 / 8 ファイル）: `0026_search.sql` 12、`0050_no_check_constraints.sql` 12、`0030_findings.sql` 4、`0032_run_env_program_events.sql` 4、`0035_session_events.sql` 4、`0036_change_marks.sql` 4、`0039_open_kinds.sql` 4、`0064_follow_up_membership.sql` 3。
- REFERENCES（src、2 行 / 2 ファイル）: `infrastructure/schema.rs` 1、`infrastructure/sessions.rs` 1。
  - どちらも test の入力。
- REFERENCES（migrations、99 行 / 29 ファイル）: `0050_no_check_constraints.sql` 20、`0030_findings.sql` 8、`0039_open_kinds.sql` 6、`0001_queue.sql` 5、`0016_observer.sql` 5、`0025_stalled_ask.sql` 5、`0008_goals.sql` 4、`0028_draft_planners.sql` 4、`0064_follow_up_membership.sql` 4、`0012_queue_events.sql` 3、`0021_proposals.sql` 3、`0022_follow_up_triage.sql` 3、`0027_plan_review.sql` 3、`0032_run_env_program_events.sql` 3、`0035_session_events.sql` 3、`0036_change_marks.sql` 3、`0014_asks.sql` 2、`0017_stuck_exit_ask.sql` 2、`0019_task_goal_dependencies.sql` 2、`0029_ask_reasons.sql` 2、`0002_supervisor.sql` 1、`0004_integration.sql` 1、`0005_run_leases.sql` 1、`0006_merge_queue.sql` 1、`0023_planners.sql` 1、`0049_open_run_providers.sql` 1、`0078_spike_replanning.sql` 1、`0079_task_replanning.sql` 1、`0081_task_holds.sql` 1。
- datetime/strftime/julianday（src、16 行 / 3 ファイル）: `infrastructure/sql_dialect.rs` 14、`domain/marks.rs` 1、`domain/stats.rs` 1。
  - `infrastructure/sql_dialect.rs` の 14 行は方言の関数の本体 5 と unit test の期待 9、`domain/` の 2 行は doc comment。文が綴る日時の関数は無く、全て方言の関数の呼び出しを通る。
- datetime/strftime/julianday（migrations、30 行 / 16 ファイル）: `0050_no_check_constraints.sql` 5、`0064_follow_up_membership.sql` 5、`0001_queue.sql` 4、`0008_goals.sql` 3、`0021_proposals.sql` 2、`0004_integration.sql` 1、`0006_merge_queue.sql` 1、`0012_queue_events.sql` 1、`0016_observer.sql` 1、`0025_stalled_ask.sql` 1、`0030_findings.sql` 1、`0032_run_env_program_events.sql` 1、`0035_session_events.sql` 1、`0036_change_marks.sql` 1、`0039_open_kinds.sql` 1、`0049_open_run_providers.sql` 1。
- 日時の列の比較（src、22 行 / 8 ファイル）: `infrastructure/runtime_store/session_steps.rs` 10、`infrastructure/runtime_store/ledger.rs` 4、`infrastructure/runtime_store/recovery.rs` 2、`infrastructure/task_holds.rs` 2、`infrastructure/headless_jobs.rs` 1、`infrastructure/plan_reviews.rs` 1、`infrastructure/runtime_store/mod.rs` 1、`infrastructure/runtime_store/run_log.rs` 1。
  - 比べる列（`heartbeat_at`・`received_at`・`registered_at`・`started_at`・`ended_at`・`finished_at`）は全て INTEGER の列で、TEXT の日時の列を比べるものは無い。TEXT の日時の列の比較は下の「型の緩さに頼る所」。
- FTS5 の検索一式（src、20 行 / 7 ファイル）: `infrastructure/search.rs` 10、`domain/search.rs` 3、`context_boundaries/sql.rs` 2、`infrastructure/related.rs` 2、`context_boundaries.rs` 1、`context_boundaries/ownership.rs` 1、`infrastructure/schema.rs` 1。
  - 動くときに FTS5 を読む SQL は `infrastructure/search.rs` と `infrastructure/related.rs` だけ。`domain/search.rs` の 3 行は FTS5 の問い合わせの式を組む関数の test、`context_boundaries/` の 4 行は所有の表と doc comment と test、`infrastructure/schema.rs` の 1 行は test の入力。
- FTS5 の検索一式（migrations、64 行 / 7 ファイル）: `0026_search.sql` 22、`0050_no_check_constraints.sql` 17、`0030_findings.sql` 5、`0032_run_env_program_events.sql` 5、`0035_session_events.sql` 5、`0036_change_marks.sql` 5、`0039_open_kinds.sql` 5。
  - `0026` と `0050` は索引の表と trigger、ほかは trigger の作り直し。
- INSERT OR（src、9 行 / 7 ファイル）: `infrastructure/draft_planners.rs` 2、`infrastructure/schema.rs` 2、`context_boundaries/sql.rs` 1、`infrastructure/plan_requests.rs` 1、`infrastructure/replan_apply.rs` 1、`infrastructure/runtime_store/coordination.rs` 1、`infrastructure/sessions.rs` 1。
  - 本番の SQL は `draft_planners.rs` 2、`plan_requests.rs`・`replan_apply.rs`・`runtime_store/coordination.rs`・`sessions.rs` 各 1 の 6 行。`schema.rs` の 2 行は test の入力、`context_boundaries/sql.rs` の 1 行はコメント。
- INSERT OR（migrations、9 行 / 8 ファイル）: `0026_search.sql` 2、`0028_draft_planners.sql` 1、`0030_findings.sql` 1、`0032_run_env_program_events.sql` 1、`0035_session_events.sql` 1、`0036_change_marks.sql` 1、`0039_open_kinds.sql` 1、`0050_no_check_constraints.sql` 1。
- `unixepoch()`（src、5 行 / 3 ファイル）: `infrastructure/runtime_store/eval_rounds.rs` 2、`infrastructure/runtime_store/session_steps.rs` 2、`infrastructure/headless_jobs.rs` 1。
  - 本番の SQL は `runtime_store/session_steps.rs` の 2 行（`unixepoch() - s.heartbeat_at` で heartbeat からの秒を読む）。`eval_rounds.rs` と `headless_jobs.rs` の 3 行は test の入力。
- `unixepoch()`（migrations、22 行 / 15 ファイル）: `0050_no_check_constraints.sql` 5、`0002_supervisor.sql` 2、`0007_supervisors.sql` 2、`0028_draft_planners.sql` 2、`0005_run_leases.sql` 1、`0011_session_workspaces.sql` 1、`0014_asks.sql` 1、`0016_observer.sql` 1、`0017_stuck_exit_ask.sql` 1、`0022_follow_up_triage.sql` 1、`0025_stalled_ask.sql` 1、`0027_plan_review.sql` 1、`0029_ask_reasons.sql` 1、`0033_auto_update.sql` 1、`0039_open_kinds.sql` 1。
  - `0028_draft_planners.sql` の 1 行が移す行の値で、ほかは INTEGER の日時の列の `DEFAULT (unixepoch())`。
- `ifnull(...)`（src、14 行 / 3 ファイル）: `infrastructure/asks.rs` 7、`infrastructure/findings.rs` 4、`infrastructure/runtime_store/run_log.rs` 3。
  - `asks.rs`・`findings.rs` は NULL になる列を `ifnull(col,0)=ifnull(?n,0)`・`ifnull(col,'')=ifnull(?n,'')` で比べて開いた ask・finding を探す（下の索引と同じ鍵）。`asks.rs` の 2 行は NULL を通す否定の比較 `ifnull(subject,'') <> 'disk'`。`run_log.rs` は `ifnull(max(id),0)` と、NULL になる欄を `ifnull(...,'')<>'skipped'` で比べる。
- `ifnull(...)`（migrations、20 行 / 11 ファイル）: `0030_findings.sql` 4、`0050_no_check_constraints.sql` 4、`0039_open_kinds.sql` 3、`0029_ask_reasons.sql` 2、`0014_asks.sql` 1、`0016_observer.sql` 1、`0017_stuck_exit_ask.sql` 1、`0022_follow_up_triage.sql` 1、`0025_stalled_ask.sql` 1、`0027_plan_review.sql` 1、`0028_draft_planners.sql` 1。
  - 全て一意の索引（`asks_open` と `findings_unsettled`）の式で、NULL になる列を一意の鍵に入れるために使う。
- ON CONFLICT（src、19 行 / 11 ファイル）: `infrastructure/sqlite.rs` 4、`infrastructure/runtime_store/ledger.rs` 3、`infrastructure/runtime_store/session_steps.rs` 3、`infrastructure/runtime_store/node_samples.rs` 2、`context_boundaries/fixtures.rs` 1、`context_boundaries/sql.rs` 1、`infrastructure/draft_planners.rs` 1、`infrastructure/proposals.rs` 1、`infrastructure/runtime_store/coordination.rs` 1、`infrastructure/runtime_store/session_registry.rs` 1、`infrastructure/runtime_store/transitions.rs` 1。
- rowid の並び順と行の選び方（src、34 行 / 10 ファイル）: `infrastructure/runtime_store/run_log.rs` 11、`infrastructure/runtime_store/recovery.rs` 8、`infrastructure/sqlite.rs` 4、`infrastructure/replans.rs` 2、`infrastructure/runtime_store/mod.rs` 2、`infrastructure/runtime_store/session_steps.rs` 2、`infrastructure/task_holds.rs` 2、`infrastructure/follow_up_membership.rs` 1、`infrastructure/runtime_store/attributes.rs` 1、`infrastructure/runtime_store/coordination.rs` 1。
  - 31 行は `task_runs` の挿入の順に頼る SQL（`ORDER BY rowid`・`MAX(rowid)`・`r.rowid`）。残る 3 行は、`runtime_store/mod.rs` の supervisor の一覧の並べ替え（`ORDER BY started_at, rowid`、`supervisors` の挿入の順で同じ時刻を分ける）と、`runtime_store/session_steps.rs` の `REMOVE_VOID_RANGES` が消す行を選ぶ 2 行（`DELETE ... WHERE rowid IN (SELECT rowid FROM session_step_removed ...)`、並べて `LIMIT` で絞った行を、整数の主キーの無い（主キーが複合の `(session_id, from_seq)` の）表で rowid で選ぶ。Postgres では複合の主キーで選べる）。
  - 式に当たらない rowid の使い方に、test の `EXPLAIN QUERY PLAN` の期待の文（`rowid=?`）、FTS5 の結果の並び（`infrastructure/search.rs` の `rowid DESC`）と test の入力の FTS5 の trigger（`infrastructure/schema.rs`）、`domain/ids.rs` の doc comment がある。
- rowid の並び順と行の選び方（migrations、7 行 / 5 ファイル）: `0050_no_check_constraints.sql` 3、`0004_integration.sql` 1、`0006_merge_queue.sql` 1、`0021_proposals.sql` 1、`0049_open_run_providers.sql` 1。
  - 全て表を作り直す migration が `SELECT rowid, ...` で rowid（挿入の順）を新しい表へ写す行。式に当たらない migrations の rowid は、その前の行の `INSERT INTO ... (rowid, ...)`、FTS5 の索引を埋める文と trigger の `rowid`（検索の種類の行）、コメント、`0091_event_records.sql` の表の `WITHOUT ROWID`（SQLite だけの表の指定で、Postgres の migration では書かない）。
- PRAGMA（src、15 行 / 2 ファイル）: `infrastructure/sqlite.rs` 13、`context_boundaries/write_tx.rs` 2。
  - `infrastructure/sqlite.rs` は `journal_mode`・`foreign_keys`・`user_version`・`application_id`・`foreign_key_check`、`context_boundaries/write_tx.rs` は test の入力。
- last_insert_rowid（src、26 行 / 17 ファイル）: `infrastructure/plan_requests.rs` 4、`infrastructure/sqlite.rs` 4、`infrastructure/asks.rs` 3、`infrastructure/replans.rs` 2、`infrastructure/ci_watch_store.rs` 1、`infrastructure/draft_planners.rs` 1、`infrastructure/finding_planners.rs` 1、`infrastructure/findings.rs` 1、`infrastructure/goal_reviews.rs` 1、`infrastructure/headless_jobs.rs` 1、`infrastructure/plan_reviews.rs` 1、`infrastructure/planners.rs` 1、`infrastructure/runtime_store/queue_records.rs` 1、`infrastructure/runtime_store/run_log.rs` 1、`infrastructure/runtime_store/run_resources.rs` 1、`infrastructure/sessions.rs` 1、`infrastructure/task_holds.rs` 1。
  - 全て rusqlite の `Connection::last_insert_rowid` の呼び出しで、SQL の文ではない。
- SQLite の system の表（src、3 行 / 1 ファイル）: `infrastructure/sqlite.rs` 3。
  - `sqlite_master` を読む schema の検査 2 行と、`next_id` が `sqlite_sequence` を読む 1 行（下の「型の緩さに頼る所」の `MAX(a, b)` の行と同じ文）。
- SQLite の system の表（migrations、25 行 / 2 ファイル）: `0050_no_check_constraints.sql` 22、`0021_proposals.sql` 3。
  - 全て表を作り直す migration が AUTOINCREMENT の次の値（`sqlite_sequence`）を新しい表へ写す行。
- AUTOINCREMENT（src、2 行 / 2 ファイル）: `infrastructure/runtime_store/run_log.rs` 1、`infrastructure/sqlite.rs` 1。
  - `runtime_store/run_log.rs` は test の入力、`sqlite.rs` は `next_id` の doc comment。
- AUTOINCREMENT（migrations、51 行 / 28 ファイル）: `0050_no_check_constraints.sql` 12、`0021_proposals.sql` 3、`0001_queue.sql` 2、`0008_goals.sql` 2、`0016_observer.sql` 2、`0025_stalled_ask.sql` 2、`0027_plan_review.sql` 2、`0030_findings.sql` 2、`0039_open_kinds.sql` 2、`0075_node_samples.sql` 2、`0082_session_steps.sql` 2、`0084_environment_resources.sql` 2、`0012_queue_events.sql` 1、`0014_asks.sql` 1、`0017_stuck_exit_ask.sql` 1、`0022_follow_up_triage.sql` 1、`0023_planners.sql` 1、`0026_search.sql` 1、`0028_draft_planners.sql` 1、`0029_ask_reasons.sql` 1、`0032_run_env_program_events.sql` 1、`0033_auto_update.sql` 1、`0035_session_events.sql` 1、`0036_change_marks.sql` 1、`0042_goal_reviews.sql` 1、`0043_headless_jobs.sql` 1、`0064_follow_up_membership.sql` 1、`0083_session_owners_and_resources.sql` 1。
- `IS` で値と比べる（src、11 行 / 4 ファイル）: `infrastructure/runtime_store/run_log.rs` 6、`infrastructure/finding_planners.rs` 3、`infrastructure/ci_watch_store.rs` 1、`infrastructure/follow_up_membership.rs` 1。
- `IS` で値と比べる（migrations、1 行 / 1 ファイル）: `0064_follow_up_membership.sql` 1。
- `IS NOT` で値と比べる（src、5 行 / 5 ファイル）: `infrastructure/goal_reviews.rs` 1、`infrastructure/plan_reviews.rs` 1、`infrastructure/proposals.rs` 1、`infrastructure/schema.rs` 1、`infrastructure/sessions.rs` 1。
  - `?` の引数と比べるのは `proposals.rs` の `proposal_id IS NOT ?1`、リテラルと比べるのは `goal_reviews.rs`・`plan_reviews.rs` の `outcome IS NOT 'interrupted'` と `sessions.rs` の `IS NOT '{HEADLESS_ROUTE}'`。`schema.rs` の 1 行は test の入力の trigger（`old.status IS NOT new.status`）。
- `IS NOT` で値と比べる（migrations、11 行 / 9 ファイル、出現 18）: `0064_follow_up_membership.sql` 2、`0070_origin_and_human_priority.sql` 2、`0026_search.sql` 1、`0030_findings.sql` 1、`0032_run_env_program_events.sql` 1、`0035_session_events.sql` 1、`0036_change_marks.sql` 1、`0039_open_kinds.sql` 1、`0050_no_check_constraints.sql` 1。
  - 7 ファイルの 1 行は検索の索引の trigger の `WHEN old.status IS NOT new.status OR old.goal_id IS NOT new.goal_id`、`0064` は trigger の `old.acceptance IS NOT new.acceptance` とリテラルとの `valid IS NOT 1`、`0070` は `planner_origin IS NOT 'person'` と `IS NOT json_extract(...)`。
- 方言の関数の呼び出し（src）: JSON の欄の読み出し `json_field(_spaced|_at)?\(` は 171 行 / 10 ファイル（`infrastructure/sql_dialect.rs` 72、`infrastructure/sessions.rs` 31、`infrastructure/runtime_store/run_log.rs` 20、`infrastructure/runtime_store/ledger.rs` 12、`infrastructure/runtime_store/session_steps.rs` 9、`infrastructure/session_tokens.rs` 8、`infrastructure/runtime_store/recovery.rs` 6、`infrastructure/runtime_store/eval_rounds.rs` 5、`infrastructure/runtime_store/queue_records.rs` 4、`infrastructure/runtime_store/session_registry.rs` 4）、日時の関数 `time_value(_spaced|_shifted)?\(|formatted_time\(` は 29 行 / 5 ファイル（`infrastructure/sql_dialect.rs` 17、`infrastructure/runtime_store/run_log.rs` 7、`infrastructure/sessions.rs` 3、`infrastructure/proposals.rs` 1、`infrastructure/runtime_store/session_steps.rs` 1）。
  - `sql_dialect.rs` の行は関数の定義と unit test。外の呼び出しは JSON が 99 行 / 9（うち `payload` を読むもの 88）、日時が 12 行 / 4。方言の関数の SQLite の版は `json_extract`・`strftime`・`julianday` の式を返すが、式は呼び出しの名前に当たらないので、上の JSON 関数と日時の関数の行の数にこの呼び出しは入らない。

## CREATE TRIGGER の内訳

migrations の 47 行は、名前で 15 本の trigger を作る（表を作り直す migration が同じ名前を作り直すため行が多い）。src の 16 行 / 5 ファイルは全て test の入力と schema の検査の doc comment で、本番の schema の trigger は migrations にだけある。内訳は `infrastructure/schema.rs` 10（migration の trigger を検査するコードの doc comment 1 と、その test の入力 9）、`infrastructure/runtime_store/coordination.rs` 2 と `infrastructure/runtime_store/ledger.rs` 2（test が書き込みを途中で失敗させる `CREATE TEMP TRIGGER`）、`infrastructure/sessions.rs` 1（test の失敗の注入）、`context_boundaries/sql.rs` 1（書き込みの判定の test の入力）。

| 分け | trigger | 行 / ファイル | PostgreSQL での候補 |
| --- | --- | --- | --- |
| 検索の索引の維持 | `search_task_*`（4）・`search_goal_*`（3）・`search_note_*`（2）・`search_run_integrated`・`search_commit_*`（2）の 12 本 | 44 行 / 7（`0026`・`0030`・`0032`・`0035`・`0036`・`0039`・`0050`） | `tsvector` の生成列、PL/pgSQL の trigger、または書き込みのコードが索引を書く |
| 記録の変更の禁止 | `judgement_no_update`・`judgement_no_delete`（`RAISE(ABORT, ...)`） | 2 行 / 1（`0064`） | PL/pgSQL の trigger で `RAISE EXCEPTION`、または権限（UPDATE・DELETE を与えない） |
| その他 | `goal_acceptance_version`（goal の受け入れ条件が変われば版を進める） | 1 行 / 1（`0064`） | PL/pgSQL の trigger、または書き込みの port が同じ transaction で版を進める |

どれに移すか（DB の trigger のまま方言ごとに書くか、書き込みのコードへ移すか）は ADR-t3982-1 が決めず、後の ADR が決める。

## 型の緩さに頼る所

SQLite は列の型を値ごとに持ち、宣言と違う型の値も入る。次は PostgreSQL では型を決めないと同じに動かない所で、grep で数えられるものだけ数を書く。

| 頼り方 | grep の式と数 | PostgreSQL での候補 | 扱う層 |
| --- | --- | --- | --- |
| JSON を `TEXT` の列に置き、`json_extract` が値の型（整数・文字列）を返す | 上の JSON 関数の行 | `jsonb` の列と `->>` の cast | 方言の関数（読む型を受ける） |
| 日時を `TEXT` の RFC 3339 で置き、文字列で比べる・`julianday` で引く | 下の「真偽の列と TEXT の日時の列の分類」で、TEXT の日時の列を SQL で大小に比べるのは 1 か所 / 1（`infrastructure/session_tokens.rs` の `run_events.created_at >= ?1`）。`julianday`・`strftime('%s', ...)` で引くのは方言の関数 `time_value` などの呼び出し 12 行 / 4（`runtime_store/run_log.rs` 7、`sessions.rs` 3、`runtime_store/session_steps.rs` 1、`proposals.rs` 1） | `timestamptz` か、文字列のままの比較 | 比較は共有の SQL、`julianday` などは方言の関数（列の型は Postgres の migration） |
| 真偽を `INTEGER` の 0/1 で置く | 下の「真偽の列と TEXT の日時の列の分類」で真偽の列は 8 本（`asks.answer_approval`、`supervisors.auto_update`・`handoff_accepted`、`session_steps.background`、`ledger_rows.final`・`legacy`、`run_resources.is_last`、`tasks.wait_for_build`）。SQL でその列を 0/1 の数と比べる・書くのは 12 行 / 4（`runtime_store/ledger.rs` 6、`runtime_store/coordination.rs` 4、`runtime_store/run_resources.rs` 1、`sqlite.rs` 1）。JSON の真偽を読むのは方言の関数 `json_field(..., JsonType::Boolean)` の呼び出し 4 行 / 2（`session_tokens.rs` 2、`sessions.rs` 2、どちらも `$.final`。SQLite の版は `json_extract` の 0/1 を返す） | `boolean` の列と `true` / `false`、または `integer` のまま。JSON は `(col->>'final')::boolean` | 列の型は migration、比べる式は方言の関数か共有の SQL の書き直し |
| `IS` で NULL でない値と比べる（NULL を等しいとみなす比較） | ` IS (\?\|[a-z])`: src 11 行 / 4（`run_log.rs` 6、`finding_planners.rs` 3、`ci_watch_store.rs` 1、`follow_up_membership.rs` 1）、migrations 1 行 / 1 | `IS NOT DISTINCT FROM` | 方言の関数か共有の SQL の書き直し |
| `IS NOT` で NULL でない値（`?` の引数・リテラル・列）と比べる（NULL を等しいとみなす否定の比較） | ` IS NOT (\?\|'\|[a-z0-9])`: src 5 行 / 5（`goal_reviews.rs`・`plan_reviews.rs`・`sessions.rs` はリテラル、`proposals.rs` は `?1`、`schema.rs` は test の入力）、migrations 11 行 / 9（出現 18。trigger の `WHEN` と、リテラル・式との比較）。上の「ファイル」の `IS NOT` の項 | `IS DISTINCT FROM`（PostgreSQL の `IS NOT` は `NULL`・`TRUE`・`FALSE`・`UNKNOWN` しか受けない） | 決定4 が段1で切り出さない「`IS` で値を比べる所」に入る。src は共有の SQL の書き直し（SQLite も `IS DISTINCT FROM` を受ける）か方言の関数、migrations は migration |
| NULL になる列を含む主キー（一意を保たない） | grep で数えない（[persistence](../design/persistence.md) の「Transactions and constraints」が挙げる `replans.rs` の `write_mappings`） | 主キーの列は NOT NULL になるので schema を変える | migration |
| 引数 2 つの `MAX(a, b)` / `MIN(a, b)`（スカラーの関数） | 下の「SQL の文の手での分類」で 2 か所 / 2: `infrastructure/sqlite.rs` の `next_id`（`sqlite_sequence` も読む）、`infrastructure/runtime_store/ledger.rs` の `ledger_tasks_open_since_at` | `GREATEST` / `LEAST`、`next_id` は identity の列の次の値か `RETURNING` | 方言の関数（`next_id` は adapter の中） |
| 真偽の式を集約する（`max(a = b)` で真偽を 0/1 の整数として読む） | 手での分類で 1 文 / 1（`infrastructure/plan_reviews.rs` の提出済みの proposal を選ぶ文の `max(...)` 2 つ） | `bool_or` | 方言の関数か共有の SQL の書き直し |
| 集約しない列を `GROUP BY` で読む | 手での分類で `GROUP BY` を持つ文 7 / 4（`runtime_store/run_log.rs` 4、`plan_reviews.rs` 1、`runtime_store/run_resources.rs` 1、`sqlite.rs` 1）のうち 1 文（`plan_reviews.rs` の同じ文が `p.submitted_at` を `GROUP BY p.id` で読む）。`p.id` が主キーなので PostgreSQL も関数従属で受ける | そのまま（主キーで GROUP BY しているので書き直しは要らない） | 共有の SQL |

### SQL の文の手での分類

スカラーの `MAX` / `MIN`、真偽の集約、`GROUP BY` は Rust の `max(a, b)` などと grep で分けられないので、`src/` の `.rs` の文字列の literal（通常と raw）のうち `SELECT`・`INSERT`・`UPDATE`・`DELETE` を含むものを取り出し、次を拾って 1 件ずつ読んだ（`format!` や `concat!` で組む文も literal の断片として拾う。実行時に組み立てて literal に現れない SQL は拾えない）。

- スカラーの `MAX` / `MIN`: `MAX(`・`MIN(`（大文字・小文字）の括弧の中に深さ 1 の `,` があるもの。2 件で、どちらも上の表の 2 か所。
- 真偽の集約: `MAX`・`MIN`・`SUM`・`TOTAL` の引数に比較（`=`・`<>`・`IS`・`IN (`・`LIKE`・`NOT`）を含むもの。3 件のうち 1 件は `next_id` のスカラーの `max` で、残る 2 件が `plan_reviews.rs` の 1 文。
- `GROUP BY`: `GROUP BY` を含む文 7 件を読み、SELECT の列が GROUP BY の式か集約か主キーに従属する列かを見た。主キーに従属しない列を読む文は無い。

### 真偽の列と TEXT の日時の列の分類

真偽の列は、migrations が `INTEGER` で宣言した列の名前（`id` を除く 120 本）を集め、src でその列を `= 0` / `= 1`（`<>`・`!=`・`IS` も）と比べる所と、Rust の `bool` として読み書きする所を拾って 1 本ずつ分けた。

- 0/1 と比べる所を持つ列は 15 本。真偽の列は `final`・`handoff_accepted`・`is_last`・`wait_for_build` の 4 本。残る 11 本は真偽でない: `follow_up_depth` は数で、SQL の 2 か所（`draft_planners.rs`・`sqlite.rs`）は 0 を書く。`singleton` は 1 行だけの表の鍵で、SQL の 4 か所（`runtime_store/coordination.rs` 3、`sessions.rs` 1）は `singleton=1` で行を選ぶ。`acceptance_version`・`attempt`・`bytes`・`exit_code`・`generation`・`revise_count`・`rows`・`revised_at`・`started_at` は、同じ名前の Rust の変数か欄の比較に当たったもので SQL でない。
- 0/1 と比べないが Rust の `bool` として読み書きする列が 4 本ある: `asks.answer_approval`（`Option<bool>`）、`supervisors.auto_update`（`Some(1)` と比べて読む）、`session_steps.background`（`Option<bool>`）、`ledger_rows.legacy`（`bool` を `i64` にして書く）。
- 一覧の表の「真偽を `INTEGER` の 0/1 で置く」は、この 8 本である。`INTEGER NOT NULL DEFAULT 0` の 22 行 / 6（migrations）のうち真偽の列は `final`・`legacy`・`wait_for_build` の 3 本で、残りは時間（`*_ms`）・数（`attempts`・`revise_count`・`follow_up_depth`）・位置（`inputs_from`）である。

TEXT の日時の列は、migrations の最後の宣言で `TEXT` の `_at` の列（`tasks`・`goals`・`task_runs`・`run_events`・`proposals` の `created_at` など、`ledger_rows.built_at`）で、`asks`・`planners`・`task_holds` などの `created_at` と、`heartbeat_at`・`started_at`・`ended_at` などは `INTEGER` である。src の Rust の文字列で、TEXT になりうる名前の列（`built_at`・`closed_at`・`created_at`・`landed_at`・`submitted_at`・`updated_at`）を `<`・`>`・`<=`・`>=`・`BETWEEN` で比べる所を拾い、表を辿って型を見た。SQL の 4 か所のうち TEXT の列は `infrastructure/session_tokens.rs` の `run_events.created_at` の 1 か所だけで、`asks.rs`・`finding_planners.rs`・`plan_requests.rs` の 3 か所は `INTEGER` の列である（残りの当たりは Rust のコードの比較）。
