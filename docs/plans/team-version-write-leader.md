---
id: plan-team-version-write-leader
type: plan
title: チームの版の書き込みの係と PostgreSQL の store の具体案（リーダーの行の列・確かめの SQL の形・lease の値の案・一意の索引・migration の系列の置き方・古い event と台帳の移し方）
status: proposed
created: 2026-11-03
owners:
  - hisamekms
tags:
  - architecture
  - persistence
  - queue-service
related:
  - adr-t3986-1
  - adr-t3981-1
  - adr-t3982-1
  - plan-team-version-sqlite-dialect
  - design-persistence
---

# チームの版の書き込みの係と PostgreSQL の store の具体案

[ADR-t3986-1](../adr/2026-11-03-t3986-1-write-leader-fencing-trigger-placement-postgres-migration-series-archive-local-import-and-store-tests.md) の決定を実装する段の設計資料。方針・不変条件・退けた案は ADR が持ち、ここは表の列・SQL の形・値の案だけを持つ。どれも案で、実装の task が決めて定義のそばの doc comment（表は migration のコメント、値は定数の doc comment）へ移し、着地した分は `docs/design/`（[SQLite persistence](../design/persistence.md)・[Queue service](../design/queue-service.md)）が今の形として書く。ここの名前は仮のもの。

## (a) リーダーの行

### 列の案

表 `write_leader`（1 行だけ）:

| 列 | 型（SQLite / Postgres） | 意味 |
| --- | --- | --- |
| `singleton` | `INTEGER` / `integer`、主キー、値は常に 1 | 1 行だけにする鍵（ADR 決定5 の「リーダーの行は 1 つ」を主キーが守る） |
| `holder` | `TEXT` / `text` | 係の識別子。serviceのプロセスが起動ごとに作る乱数の値（pid やホスト名を含めない。同じ host で起動し直しても別になる） |
| `generation` | `INTEGER` / `bigint` | 世代の番号。引き継ぐたびに 1 進める。0 は「まだ誰も係になっていない」 |
| `lease_until_ms` | `INTEGER` / `bigint` | lease の期限（UNIX 時刻のミリ秒、DB の時計）。手放したときは 0 |
| `acquired_at_ms` | `INTEGER` / `bigint` | 今の世代を取った時刻（観測用） |

行は表を作る migration が `generation = 0`・`lease_until_ms = 0` で 1 行入れる。期限を整数のミリ秒で持つのは、既存の INTEGER の時刻の列（`heartbeat_at` など）と同じ扱いにし、TEXT の日時の比較を増やさないため（[棚卸し](team-version-sqlite-dialect.md)の「型の緩さに頼る所」）。

### 確かめの SQL の形

DB の今の時刻は方言の関数（ADR-t3982-1 決定3）で埋める。SQLite は `CAST((julianday('now') - 2440587.5) * 86400000 AS INTEGER)`、Postgres は `(extract(epoch from clock_timestamp()) * 1000)::bigint` を返す（`now()` は transaction の始めの時刻なので使わない）。下の `:now` はその断片。

書き込みの transaction の最初の文（全部の書き込みの名前付きの入口が流す）:

```sql
UPDATE write_leader
   SET lease_until_ms = :now + ?3
 WHERE singleton = 1 AND holder = ?1 AND generation = ?2
```

変わった行が 0 なら、その transaction は何も書かずに rollback し、`not_leader`（「交代中」）で拒む。期限は条件に入れない（ADR 決定2）。

引き継ぎ（係になる。書き込みの列の外で、係でないプロセスが試す）:

```sql
UPDATE write_leader
   SET holder = ?1, generation = generation + 1,
       lease_until_ms = :now + ?2, acquired_at_ms = :now
 WHERE singleton = 1 AND (lease_until_ms <= :now OR ?3)
```

取った世代は、変わった行が 1 のときに同じ transaction で `SELECT generation FROM write_leader WHERE singleton = 1` で読む（共有の SQL。`RETURNING` を使うかは ADR-t3982-1 決定3 の adapter の関数に合わせて段2 が決める）。`?3` はローカルの証（DB のファイルと同じ queue のディレクトリの serviceの lock のファイルを持っている SQLite の store）が立つときだけ真にする。Postgres の store では常に偽。返った世代を係が覚え、以後の確かめに渡す。

手放し（予定した停止で列を空にした後）:

```sql
UPDATE write_leader SET lease_until_ms = 0
 WHERE singleton = 1 AND holder = ?1 AND generation = ?2
```

延ばし: 書き込みの最初の文が延ばすので、書き込みの無い間だけ係が延ばしの書き込み（最初の文だけの transaction）を列に入れる。

Postgres の分離のレベルは `READ COMMITTED` で足りる見込み: 最初の UPDATE が行の lock を取り、後から来た別のプロセスの UPDATE は先の commit まで待ってから条件を評価し直して 0 行になる。その後の文は文ごとの snapshot で先の commit を見る。`SERIALIZABLE` を要するかは段3 の共通の test で確かめる。

### 値の案

| 値 | 案 | 根拠 |
| --- | --- | --- |
| lease の期限 | 30 秒 | 延ばす間隔の 3 倍。RDS の Multi-AZ の failover（目安 1〜2 分）の間は誰も DB に書けないので、期限を failover より長くしても止まる時間は縮まない |
| 延ばす間隔（書き込みが無いとき） | 10 秒 | 2 回続けて遅れても係を失わない |
| 引き継ぐ側が試す間隔 | 5 秒 | 落ちたときに書き込みが止まる時間の上限は約 35 秒 |
| 予定した停止で列を空にする上限 | 20 秒 | ECS の停止の猶予（既定 30 秒）の内に手放す |

ローカルは serviceの lock のファイルが証になるので、期限の値は起動し直しの時間に効かない。

### 交代の答え

拒んだ書き込みは `not_leader` の code の答えにし、呼び出し元に送り直してよいことを示す。health は、DB に届けば係でも引き継ぎを待つ側でも `ok` と答え、答えに `leader: true|false` を添える。拒まれて係でなくなったプロセスは列の残りに同じ答えを返して終了する（ECS が入れ替える）。

参考の構成の入れ替え: ECS の service を `minimumHealthyPercent = 0`・`maximumPercent = 100` にし、古い task を止めてから新しい task を起動する。

migration との順:

1. 互換の migration: 配置する側が migrate のコマンド（新しい build の image の別の task）を流す。係を引き継がず、動いている前の build の係と並ぶ。終わってから service を入れ替える。migrate は transaction の外で今の係の `holder` と `generation` を読み、migration の transaction の最初の文で同じ行を鎖す。

   ```sql
   UPDATE write_leader SET generation = generation
    WHERE singleton = 1 AND holder = ?1 AND generation = ?2
   ```

   0 行なら（読んだ後に引き継ぎがあった）何も適用せず、読み直して試し直す。1 行なら同じ transaction で schema の版（Postgres は `dagq_schema`、SQLite は `user_version`）を読み直し、もう適用されていれば何もしない。Postgres では UPDATE が行の lock を commit まで持つので、係の書き込みはその間待つ。SQLite では `BEGIN IMMEDIATE` が同じ順を作る。係がまだ居ない（`generation = 0`）queue でも同じ文で鎖せる。
2. 非互換の migration: 前の係を止め（drain）、migrate のコマンドが係を引き継いで世代を進めてから適用し、手放してから新しい build の service を起動する。

### 一意の索引（ADR 決定5）

| 不変条件 | SQLite の今の索引 | Postgres の形 |
| --- | --- | --- |
| queue 全体で `integrating` の run は高々 1 つ | `one_integrating_run_per_queue ON task_runs((1)) WHERE status = 'integrating'` | 同じ式の部分索引 |
| task ごとに未完了の run は 1 つ | `one_unfinished_run_per_task` | 同じ |
| task ごとに `integrated` の run は 1 つ | `one_integrated_run_per_task` | 同じ |
| 走っている plan review は高々 1 つ | `plan_reviews_running ON plan_reviews(finished_at IS NULL) WHERE finished_at IS NULL` | 同じ |
| 同じ対象と種類の開いた ask は 1 つ | `asks_open`（`ifnull` の式） | `coalesce` の式 |
| 同じ対象の片付いていない finding は 1 つ | `findings_unsettled`（`ifnull` の式） | `coalesce` の式 |
| リーダーの行は 1 つ | 新しい表の主キー | 同じ |

## (b) trigger の移し先の対応

| trigger（棚卸しの分け） | SQLite | Postgres |
| --- | --- | --- |
| 検索の索引の維持（`search_task_*`・`search_goal_*`・`search_note_*`・`search_commit_*` の 11 本） | 残す | 検索の port の書き込み（`index_task`・`index_goal`・`index_note`・`index_commit` の形）を、元の行を書く transaction から呼ぶ。task の状態と goal の写し（`search_task_moved` に当たる）も同じ。索引の型（`tsvector`・`pg_bigm`・`pg_trgm`）は Spike の結論 |
| 着地の commit の記録（`search_run_integrated`。`run_integrated` の event から `landed_commits` に行を入れる） | 残す | run の着地を書く adapter の文が、同じ transaction で `landed_commits` に入れる（棚卸しは検索の分けに入れたが、記録を作るのでここに分ける） |
| `judgement_no_update`・`judgement_no_delete` | 残す | 置かない。所属の判断の port が追記だけを持つことを境界の test（`src/context_boundaries/sql.rs` の形）で確かめる。配置する側の文書に、アプリの DB の役に UPDATE・DELETE を与えない選択を書く |
| `goal_acceptance_version` | 残す | 受け入れ条件を書く adapter の文が同じ transaction で `acceptance_version = acceptance_version + 1` を書く（adapter ごとの文） |
| test の `CREATE TEMP TRIGGER`（失敗の注入） | 残す | 作らない。Postgres の組では注入しない |

## (c) Postgres の migration の系列

- 置き場: repository の root の `migrations-postgres/`（`migrations/` の直下の `.sql` だけを読む ADR-0067 の一覧と検査の外）。`Cargo.toml` の `include` に足す。
- 基点: `base.sql` 1 本。基点の版 B（SQLite の `SCHEMA_VERSION` の値）を基点のファイルの先頭行に書き、Postgres の DB の版の表に B を入れる。基点は段3 の着地の時の main の SQLite の schema から作り、型は次のとおり写す。
  - JSON の `TEXT` の列は `jsonb`（`payload` など、`json_extract` で読む列）。
  - TEXT の日時の列は `text` のまま（比較は共有の SQL。`timestamptz` への変更は後の段で決める）。
  - 真偽の 8 本（棚卸しの「真偽の列」）は `boolean`。比べる式は方言の関数か共有の SQL の書き直し。
  - `AUTOINCREMENT` は `bigint GENERATED BY DEFAULT AS IDENTITY`（import が ID を保てるよう `ALWAYS` にしない）。
  - NULL になる列を含む主キー（`replans.rs` の `write_mappings` の表）は、NOT NULL の列と部分一意の索引に分ける。
- 基点の後: SQLite の `NNNN_<name>.sql` の 1 本ごとに `migrations-postgres/<name>.sql` を 1 本。Postgres で要らない変更は `-- postgres: none` だけの 1 本。互換の宣言は SQLite の側の先頭行を読む。
- build の検査: `build.rs` が、基点より後の SQLite の migration の `<name>` ごとに Postgres の側のファイルがあること、Postgres の側に対応の無いファイルが無いこと、基点より後の `<name>` が重ならないことを確かめ、欠ければ build を失敗させる。
- 版の表: `dagq_schema(version bigint, floor bigint)` の 1 行（SQLite の `user_version` と `schema_floor` に当たる）。
- 着地の振り直し（ADR-0067 決定3）: SQLite のファイルの番号だけが変わり、`<name>` は変わらないので Postgres の側は書き換えない。振り直しは run の他の変更の中身に元の 4 桁が現れるかを見る（`integrate.rs` の `paths_containing`）ので、Postgres の側のファイルの中に SQLite の番号を書かない（書けば `needs_session` になる）。
- 検査の範囲: `scripts/check-migration-numbers.sh` の CHECK の禁止（ADR-t876-1）と、リリースした migration を変えない検査（ADR-t614-2）を `migrations-postgres/` にも当てる。

## (d) 古い event と台帳の移し方

- 境目の案: 閉じた（completed か canceled）task の run の event と、`final` の台帳の行のうち、90 日より古いもの。制御側の判断と表示が読む一番長い窓（stats・kpi の週次と 30 日の窓）より後に置く。
- 置き場の形: Parquet。鍵は `<prefix>/events/year=YYYY/month=MM/part-<最初の id>-<最後の id>.parquet` と `<prefix>/ledger/year=YYYY/month=MM/part-....parquet`。同じ区切りは同じ鍵に置き直すので、移し直しで重ならない。
- 移す係: 制御側の supervisor のループが日に 1 回、係の書き込みの列で区切りごとに (1) 行を読む → (2) 置き場に置く → (3) 置いた写しの行数と id の範囲を確かめる → (4) 書き込みの transaction で DB の行を消し、`archive_written` の event を書く。
- 移さない kind の案: 属性の畳み込み（`domain::ledger::attributes::fold` が読む属性の event）、所属の判断、`run_integrated`（着地の commit の記録の元）、goal と task の開き直しが読む event。一覧は移す係の定義のそばの定数に置く。
- 保存期間: 写しは境目より古い event の正本なので、dagq は消さず、参考の構成の文書は置き場に期限（lifecycle の expiration）を付けない形を書く。付けるのは配置する側の選択。
- 集計: DuckDB の `read_parquet('s3://<bucket>/<prefix>/events/**/*.parquet')` で読む。境目より新しい分は service の読み取りのユースケースで読み、DuckDB は置き場と service の答えを読む側（人の手元か job）で合わせる。
- 読み戻し: 境目より古い run の `show` は、置き場の区切りを id の範囲で引いて読む。開き直した task の event は DB に戻し、写しは残す。DuckDB で合わせるときは `run_events.id` で重なりを除く。経路は実装の段が決める。

## (e) ローカルから移す手順の形

1. ローカルで `down --wait` を打つ。supervisor が drain して止まり、`down` は supervisor・broker・service の順に止める（[`up` / `down`](../design/supervisor-lifecycle/up-down.md)）。
2. `dagq service start`（`service.lifecycle`）で service だけを起動する。supervisor と broker は止めたままにし、inbox も開かない（書き込みの主が人の CLI だけになる）。`dagq status` で未完了の run が無いことを確かめる。
3. 人の CLI が export のユースケース（専用の capability `queue.transfer`。user だけ）を呼ぶ。export は係の書き込みの列に並ぶ 1 つの transaction（SQLite の `BEGIN IMMEDIATE`）で、まず未完了の run・`run_leases` の行・生きている `supervisors` の登録が無いことを確かめ（あれば `export_refused`）、`queue_exported` の event を書き、同じ transaction の snapshot から下の「移す表」を表ごとに JSON Lines で書き出し、表ごとの行数と中身の hash の一覧を添える。export を終えた service は、起動し直すまで書き込みを `queue_exported` の答えで拒む（export の後の書き込みが移らずに失われないように）。
4. `dagq service stop` で service を止める。
5. export の版とチームの版の build の版が同じことを確かめる（違えば 2 に戻り、ローカルを先に `migrate` してから export し直す）。チームの版の `init` が表の無い Postgres の database に基点から最新まで schema を作る（ADR-t3102-2 決定1 の DB の無い所に作る例外）。`init` が作るのは版の表・リーダーの行（`generation = 0`）・空の表で、チームの版の service が起動して係になる。
6. その service の import のユースケース（`queue.transfer`。チームの版では管理者の役の user だけ）が係として「移す表」だけを入れる。import の transaction は、リーダーの行の確かめ（係の識別子と世代が自分のもの）の次に、「移す表」がどれも 0 行で `queue_imports` の行が無いことを確かめ、違えば `import_refused`（理由つき）で何も書かない。行は ID を保って入れ、identity の次の値を移した行の最大の ID から合わせ、境目より古い行は (d) の形で置き場へ直接置き、本文が閾値を超える event は大きな本文の置き場の port（ADR-t3982-1 決定5・6）に置く。検索の索引は移した task・goal・note・着地の commit から検索の port が書く。終えたら `queue_imports`（出どころの queue の `application_id` と版、表ごとの行数と hash、import した人の principal と時刻）に 1 行書き、2 回目は拒む。大きな queue で 1 つの transaction に収まらないときは、表ごとの transaction に分け、最初の transaction で `queue_imports` に「進行中」の行を書いて以後の import の開始を拒む形にする（どちらにするかは Spike の結果で決める）。
7. 「移す表」の表ごとの行数と hash を export の一覧と照合し、合えばチームの版の supervisor を始める。合わなければ Postgres を捨て、ローカルを `up` し直す（起動し直した service は書き込みを受ける）。

止める時間 = drain + export + import + 照合。Spike は import の実装の後に、本番の queue.db の形（表ごとの行数と本文の大きさの分布）で作った合成のデータを CI の Postgres の job の環境で流し、3・6・7 の秒を測る。

### 表の分け（今の schema の表で）

| 分け | 表 | 扱い |
| --- | --- | --- |
| 移す表（queue の記録） | `tasks`・`task_dependencies`・`task_goal_dependencies`・`task_holds`・`task_runs`・`run_events`・`event_records`・`goals`・`goal_reviews`・`asks`・`proposals`・`plan_requests`・`plan_request_proposals`・`plan_reviews`・`planners`・`headless_jobs`・`draft_bundles`・`draft_bundle_members`・`draft_origins`・`draft_reopens`・`draft_revisits`・`findings`・`follow_up_judgements`・`landed_commits`・`replan_plans`・`replan_operations`・`replan_mappings`・`replan_replacements`・`spike_cycles`・`session_steps`・`session_step_removed`・`session_owners`・`session_resources`・`run_resources`・`node_samples`・`node_sample_imports`・`ledger_rows` | export して import し、照合する |
| 移し先が持つ（書き換えない） | リーダーの行（`write_leader`）、版と下限（SQLite の `user_version`・`schema_floor`、Postgres の `dagq_schema`）、`application_id`、`queue_imports`、`sqlite_sequence`（Postgres は identity の次の値を合わせる）、`search_index` と FTS5 の内部の表 | export しない。移し先の `init`・係・検索の port が作る |
| 移さない（host の実行の状態と束縛） | `supervisors`・`run_leases`・`run_processes`・`session_workspaces`・`queue_repository`・`binary_updates` | export しない。前の 3 つは export の確かめで空か終わった run の行だけ。`queue_repository` はチームの版の `init` が自分の repository に束ねる |

表を足す migration は、同じ変更でこの分けのどれに入るかを export の定義のそばの定数に足す（分けの無い表があれば export を build の test で落とす）。

## (f) test の組の形

- 組: `src/infrastructure/store_contract/` に、adapter を引数に取る case の関数を置く（`fn writes_of_an_old_generation_are_refused(store: &dyn ...)` の形）。module は `#[cfg(any(test, feature = "store-contract"))]` で、既定の build に入らない。
- SQLite: `src/infrastructure` の `#[cfg(test)]` から memdb で組を呼ぶ（ADR-t2895-1）。
- Postgres: `examples/store_contract_postgres.rs`（`[[example]]`、`required-features = ["store-contract", "postgres"]`）が `DAGQ_TEST_POSTGRES_URL` の接続先で組の case を順に流し、1 件でも落ちれば終了 1。変数が無ければ何も流さず終了 1。test binary でないので nextest と `cargo llvm-cov nextest` は流さない。
- CI: `ci.yml` に job `postgres`（Linux、`services: postgres` の container、`cargo run --locked --example store_contract_postgres --features store-contract,postgres`）。docs だけの差分では Rust の job と同じく飛ばす。着地の検証（`scripts/landing-it.sh`）には足さない。
