---
id: plan-goal-3-acceptance-ci
type: plan
title: goal 3 の受け入れ条件 (6) の証拠（全タスク着地後の main の CI と TOTAL 行、60d992ca 後の回帰テストの差分、e2e の根拠）
status: completed
created: 2026-10-20
owners:
  - hisamekms
tags:
  - measurement
  - testing
  - coverage
related:
  - adr-t1925-1
  - adr-t963-1
  - adr-t1233-2
  - adr-t2105-1
---

# goal 3 の受け入れ条件 (6) の証拠（全タスク着地後の main の CI と TOTAL 行、60d992ca 後の回帰テストの差分、e2e の根拠）

goal 3 の受け入れ条件 (6)「tests/cli.rs、tests/plugin.rs、tests/e2e.rs が変更なしで通り、cargo fmt / test / clippy / llvm-cov 80% が通る」の、全タスク着地後の main での証拠を記録する。goal 3 の最後の着地は 60d992ca（60d992ca16b163211891fe3fff27a562873faba0、2026-10-08T14:02:09+09:00、task 1845）で、goal 3 の全タスクはそれまでに着地している（goal review 114 の判定）。

測定は CI の 1 回の run と git の出力を読むだけで、周回数・交互実行は当たらない。値は CI の llvm-cov report の TOTAL 行と git・gh の出力をそのまま写し、計算はしない。確かめの時点は 2026-10-20、作業の起点の main は aff18ec286e6e5a54cbf909490285603337166f7。

## CI

coverage は [ADR-t1925-1](../adr/2026-10-07-t1925-1-landing-verifies-unit-tests-and-selected-integration-tests-and-ci-is-the-final-gate.md) 決定 1・4 と ask 608 の人の決定のとおり CI だけで測り、対象 commit の main の CI run の URL と coverage の割合で満たす（手元や integrate で llvm-cov を流していない）。

### 対象 commit の選び方

60d992ca を祖先に含む main の commit のうち、`.github/workflows/ci.yml` の `fmt / test / clippy / coverage` job が success の最新の run を持つものを対象にした。

```sh
gh run list --branch main --workflow ci.yml --limit 400 --status success --json databaseId,headSha,createdAt
gh run view <ID> --json headSha,jobs --jq '.headSha[:10]+" "+([.jobs[]|select(.name=="fmt / test / clippy / coverage")|.conclusion]|join(","))'
```

2026-10-20 の時点で、main の CI の run のうち workflow が success の最新は 39359475641（23d319332d、2026-10-20T02:32:30Z）だが、docs だけの差分で `fmt / test / clippy / coverage` は skipped。それより新しい run は failure か cancelled か実行中で、success の run を遡ると 38441250662 までの 10 本（39124325210・39123593807・39123023391・39104596715・39070564937・39069524267・39068687663・38929862823・38765216724・38763798470）も `fmt / test / clippy / coverage` が skipped だった。job が success の最新の run は 38441250662 で、その commit を対象にした。planner が 2026-10-09 に確かめた 5d3f0ab6 の run 37890053159（TOTAL の Lines 94.01%）より新しい。

6e16cd35 より後の main で `fmt / test / clippy / coverage` を流した run は failure（39379576026・39367216123・39363005106・39351238156 など。落ちた test はこの task の known failures の一覧が CI run とともに持つ）か cancelled で、job が success のものは無い（2026-10-13T17:45Z より後に作られた workflow が failure の run 79 本は、`gh run view <ID> --json jobs` でどれも `fmt / test / clippy / coverage` が failure）。

### 対象 commit と run

- 対象 commit: `6e16cd35b982b559e243d01cc9ddc43b1b5c93cb`（2026-10-14T02:45:36+09:00、main）
- `git merge-base --is-ancestor 60d992ca 6e16cd35b982b559e243d01cc9ddc43b1b5c93cb` の exit status: `0`（60d992ca は祖先）
- `git merge-base --is-ancestor 6e16cd35b982b559e243d01cc9ddc43b1b5c93cb origin/main` の exit status: `0`（main の commit）
- CI run: https://github.com/hisamekms/dagq/actions/runs/38441250662 （workflow の conclusion は success、2026-10-13T17:45:41Z）

`gh run view 38441250662 --json jobs --jq '.jobs[]|[.name,.status,.conclusion]|@tsv'`:

```text
docs / scripts checks	completed	success
docs-only diff	completed	success
fmt / test / clippy / coverage	completed	success
linux build / test	completed	success
```

`fmt / test / clippy / coverage` は `cargo fmt --all --check`・`cargo clippy --locked --all-targets -- -D warnings`・`cargo llvm-cov nextest --locked --workspace --fail-under-lines 80` を流す job で、test の失敗と line coverage の 80% 割れのどちらでも落ちる。

### TOTAL 行

`gh run view 38441250662 --log | grep TOTAL` の原文（列の見出しは同じ log の `Filename` の行）:

```text
fmt / test / clippy / coverage	UNKNOWN STEP	2026-10-13T18:22:56.0303910Z Filename                                             Regions    Missed Regions     Cover   Functions  Missed Functions  Executed       Lines      Missed Lines     Cover    Branches   Missed Branches     Cover
fmt / test / clippy / coverage	UNKNOWN STEP	2026-10-13T18:22:56.0896280Z TOTAL                                                 316417             19651    93.79%       19375              1328    93.15%      195751              8622    95.60%           0                 0         -
```

Lines の Cover は `95.60%` で、80% 以上。

## 回帰テストの差分

条件 (6) の「tests/cli.rs・tests/plugin.rs・tests/e2e.rs が変更なしで通る」は、goal 3 のリファクタリングが外部 API の回帰テストを変えないことの制約である。goal 3 の回帰の確認は 60d992ca までの差分が対象で、60d992ca 時点の維持は task 1845 の記録が CLI 分割後の 41 テスト・654 assertion 行で確かめた。60d992ca の後のこれらの path の変更は、下の表のとおりどれも goal 3 の後の他の goal の機能の変更による。tests/cli.rs は既に tests/it/cli_*.rs に分割されていて、60d992ca にも対象 commit にも存在しない（`git ls-tree 60d992ca -- tests/cli.rs` と `git ls-tree 6e16cd35 -- tests/cli.rs` は空）。

`git diff --stat 60d992ca 6e16cd35b982b559e243d01cc9ddc43b1b5c93cb -- tests/cli.rs tests/plugin.rs tests/e2e.rs 'tests/it/cli_*.rs'`:

```text
 tests/e2e.rs                    | 600 +++-------------------------------------
 tests/it/cli_broker.rs          | 289 -------------------
 tests/it/cli_candidates.rs      |  26 +-
 tests/it/cli_dialogue.rs        |   2 +-
 tests/it/cli_goals.rs           |   3 +-
 tests/it/cli_install_release.rs |  17 +-
 tests/it/cli_read.rs            |  65 +++++
 tests/it/cli_roles.rs           |  49 ++--
 tests/it/cli_stats.rs           |  21 ++
 tests/it/cli_status_runs.rs     |   9 +
 tests/it/cli_task_hold.rs       | 234 ++++++++++++++++
 tests/it/cli_tasks.rs           | 187 +++++++++----
 tests/it/cli_version.rs         |   6 +-
 tests/plugin.rs                 |  24 +-
 14 files changed, 578 insertions(+), 954 deletions(-)
```

`git log --format='%h %ad %s' --date=iso-strict 60d992ca..6e16cd35b982b559e243d01cc9ddc43b1b5c93cb -- tests/cli.rs tests/plugin.rs tests/e2e.rs 'tests/it/cli_*.rs'`:

```text
5d05161c 2026-10-14T02:25:39+09:00 runtime: task の hold と release を永続し、CLI と権限を足し、hold 中の task を claim・復旧 job・resume の候補から外す
5090da6a 2026-10-13T05:45:03+09:00 runtime: supervisor の周回に、終わった run と task の台帳を冪等に追いついて作る係を観測と分析の側に足し、今の stats の値と台帳から作った値を run ごとに並べて差を出す読み取りを足す
f9b2ff44 2026-10-13T03:34:16+09:00 refactor: goal 92 と goal 100 の状態の分割の後に、引き継ぎ（handoff）と待ちの段の判断を行き先の context の関数の unit test に移し、runtime_handoff・runtime_waiting・runtime_waiting_stages を境界の代表に絞る
89b6f5d2 2026-10-13T01:45:56+09:00 runtime: e2e と e2e の関門から cmux を外す（関門の cmux の ping・fixture の workspace と group の作成と片付け・CMUX_SOCKET_PASSWORD と CMUX_ の受け渡し・E2eSkip::cmux、tests/e2e の cmux の preflight と guard）
388d1d1e 2026-10-13T00:42:44+09:00 runtime: inbox の watch の ask の通知を cmux notify から host.toml の [push] に移し、infrastructure::adapters::Cmux・WorkspaceBackend・RecordingBackend の cmux の記録・FakeCmux・tests/it/lifecycle_cmux.rs を消す
5cfb3428 2026-10-09T19:20:01+09:00 runtime: task に Spike の実行区分（ADR-t1487-1）を足し、add・edit・show・list・lint・candidates・graph で扱い、Spike の run の receipt に根拠付きの判定と証拠の条件を必須にする
e4813469 2026-10-09T16:50:33+09:00 runtime: dagq inbox で inbox を人の terminal に開き、up は cmux の preflight と inbox の workspace の作成・再利用・見た目をやめ、down は cmux の workspace を閉じない
26184a7b 2026-10-09T07:17:00+09:00 runtime: crates/ の resource broker の 3 つの crate（dagq-broker・dagq-broker-client・dagq-broker-protocol）と [workspace] を外して dagq の単一の package に戻し、release.yml・README の Release 節・release skill を dagq だけの手順にし、dagq.toml の [areas] の broker を直し、ADR-t828-1 を置き換える ADR を書く
cf7d69ab 2026-10-08T21:17:49+09:00 feature: 運ばれない planner_question の答えの attention の next を対象で分け、worker の居ない draft には「request add --ref task:N --ref ask:M で計画の依頼にする」旨を出す
8e24119c 2026-10-08T19:37:02+09:00 Remove dagq ADR and task-number history from runtime-emitted fixed strings
cde598ae 2026-10-08T19:09:14+09:00 runtime: in-cmux mode を廃止し、launchd mode の cmux の preflight を外して supervisor を cmux なしで常駐させる
4cdd183b 2026-10-08T16:01:24+09:00 runtime: dagq（src/・tests/・build.rs・containers/broker）から resource broker を外し（supervisor の用意と health・run の token・[broker] の読み取り・dagq broker のコマンド・required の claim と resume の保留・up の podman の preflight・down の停止と gvproxy の片付け・install と auto-update の client・e2e の broker:: と関門の podman の確認）、build 識別子の規則を dagq に移し、地図と plugin を直し、ADR-t1162-1 を置き換え ADR-t2105-1・ADR-t1582-1 の broker の箇所を amends にする ADR を書く
```

どの commit も 60d992ca（2026-10-08T14:02:09+09:00）より後で、`60d992ca..6e16cd35` の範囲にある。各 commit の目的（task は commit の `Dagq-Task` trailer、ADR は件名・本文と、その commit が test の comment に書いた番号から）と、テストを消したか期待値を変えたかの区別は次のとおり。

| commit | task | ADR | 変更の目的 | 消えたテスト・変えた期待値の区別 |
| --- | --- | --- | --- | --- |
| 4cdd183b | 2125 | ADR-t2125-1（ADR-t1162-1 を置き換え） | resource broker の撤去 | 消えた機能のもの: `tests/it/cli_broker.rs` の全体（`dagq broker` のコマンドと podman）、`cli_tasks.rs` の status・doctor の `broker` 欄の assertion、`cli_install_release.rs` の `dagq-broker-client` の install の期待値と、client が `absent` で固定の dagq の横に残らないことの assertion、`cli_version.rs` の broker の binary に触れた comment（assertion は変えていない）、`cli_candidates.rs` の `broker_claims_held` の hold、`e2e.rs` の `mod broker`、`plugin.rs` の `dagq broker status` の案内。`cli_read.rs` に除いた broker の event を持つ queue が読めるテストを足した |
| cde598ae | 1443 | ADR-t1433-4 | in-cmux mode の廃止 | 消えた機能のもの: `e2e.rs` の `up_in_cmux_starts_a_supervisor_in_a_workspace_that_down_wait_stops_and_closes`（60d992ca で既に `#[cfg(any())]` で compile から外れていた、ADR-t1582-1）と、その helper（workspace の見た目・env）。残った `up_starts_a_launchd_supervisor_that_status_lists_and_down_wait_stops_it` からは `DAGQ_E2E_LAUNCHD=1` が無いと本体を流さずに返す早期 return（`launchd_e2e_enabled()`）を外し、`--ignored` で常に流れるようにした（強める変更） |
| 8e24119c | 2098 | ADR-t1433-3 | runtime が出す固定の文字列から ADR と task 番号の経緯を除く | 残った機能の期待値の変更: `cli_dialogue.rs` の blocked ask の拒否の理由の期待値を `"a blocked ask needs --recommend (ADR-t451-1 decision 2)"` から `"a blocked ask needs --recommend:"` で始まることに変えた。出力の文言が変わったのに合わせたもので、前置きの比べる長さは短くなった（下の「弱めたように読める変更」） |
| cf7d69ab | 2018 | — | 運ばれない planner_question の答えの attention の next を分ける | 足しただけ（`plugin.rs` の案内の assertion を追加） |
| 26184a7b | 2126 | ADR-t2126-1（ADR-t828-1 を置き換え） | broker の crate を外して単一の package に戻す | 残った機能のテストの準備の変更: `plugin.rs` の `check_plugin_version` が script の読む git work tree を用意するようにした。期待値は変えていない |
| e4813469 | 2160 | ADR-t2159-1（e2e の comment） | inbox を人の terminal に開き、up が cmux の inbox の workspace を作らない | 消えた機能のもの: `e2e.rs` の cmux の preflight・fixture・workspace の guard。残った `up_starts_a_launchd_supervisor_that_status_lists_and_down_wait_stops_it` は inbox の workspace の作成と再利用の assertion を、`inbox` が `not_opened` と案内を返し 2 回目も同じことの assertion に変えた（機能の変更に合わせた期待値） |
| 5cfb3428 | 1488 | ADR-t1487-1 | task の Spike の実行区分 | 足しただけ（`cli_tasks.rs` の `the_execution_class_is_added_shown_edited_and_drawn`、`cli_goals.rs` の graph の期待値に `execution_class` を足した） |
| 388d1d1e | 2161 | ADR-t2159-1（test の comment） | inbox の watch の ask の通知を cmux notify から `[push]` へ | 消えた機能のもの（cmux notify）を、`[push]` の送信の数・中身の assertion に置き換えた。`plan_is_refused_with_the_way_to_ask_the_inbox_and_opens_nothing` の `notifications(&db) == ""` の assertion は消した（下の「弱めたように読める変更」） |
| 89b6f5d2 | 2162 | ADR-t2159-1 | e2e と e2e の関門から cmux を外す | 名前と comment の変更: `happy_path_runs_a_stub_agent_through_cmux_and_lands_on_main` を `happy_path_runs_a_stub_agent_in_the_background_and_lands_on_main` に改名し、up の e2e から使われない `--cmux` の引数を外した。assertion は変えていない |
| f9b2ff44 | 1719 | — | handoff と待ちの段の判断を unit test に移す | 足しただけ（`cli_status_runs.rs` に state `returning`・cause `answered` の assertion を追加） |
| 5090da6a | 1667 | — | run と task の台帳と stats との差の読み取り | 足しただけ（`cli_stats.rs`） |
| 5d05161c | 1880 | ADR-t1879-1 | task の hold と release | 足しただけ（`cli_task_hold.rs` の新しいファイル） |

### 弱めたように読める変更

直さずに記録する。どちらも残った機能の期待値で、goal 3 の後の他の goal の出力の変更に合わせたもの。

- 8e24119c の `cli_dialogue.rs::the_observer_still_records_findings_and_raises_blocked_asks`: 拒否の理由の前置きの比べる文字列が、ADR の番号を含む全体から `--recommend:` までに短くなった。runtime の文言から ADR 番号を除いた変更（task 2098）に合わせたもの。
- 388d1d1e の `cli_roles.rs::plan_is_refused_with_the_way_to_ask_the_inbox_and_opens_nothing`: `dagq plan` の拒否で cmux の通知が出ないことの assertion（`notifications(&db) == ""`）を消し、`[push]` の送信が無いことの assertion を足していない（この test は `[push]` の stub を置かないので、消した assertion は `[push]` の後では常に成り立つ形だった）。cmux notify の通知を runtime から除いた変更（task 2161）に合わせたもので、拒否のエラーと案内の文言、planner が開かれないこと、event が増えないこと（`events --all` が前と同じ）の assertion は残っている。

## e2e

goal 3 の条件 (6) は tests/e2e.rs が通ることを求めるが、CI は e2e を `#[ignore]` のまま流さない。e2e は worker が流さず、review の pass の後に runtime が host で流す（[ADR-t963-1](../adr/2026-09-29-t963-1-e2e-required-by-diff-and-run-in-full-before-auto-update.md)・[ADR-t1233-2](../adr/2026-10-02-t1233-2-e2e-runs-on-the-host-after-review-passes.md)、[integrate](../design/supervisor-lifecycle/integrate.md) の「着地の前のe2e」）。

### 既存の記録

planner が照合した既存の `run_e2e_finished` の記録は次の 2 つ（値は planner の照合の値）。

| event | task | run | 検証 commit | outcome | skipped |
| --- | --- | --- | --- | --- | --- |
| 134163 | goal review 114 の根拠 | b80c3aa6 | 1c7ff1d82921e6699c914d27ef90d1b656745e83 | passed | なし |
| 145469 | 1555 | 148e32af-3681-4356-8a82-9816c4f1ddce | 5844971fe31f183773915a134d64e9eff4b78e81 | passed | `up_starts_a_launchd_supervisor_that_status_lists_and_down_wait_stops_it`（cmux が答えなかった） |

event 145469 の skipped は、cmux が答えないときに実 cmux を要る e2e を外して残りで判定し、外したことを記録する関門の仕様（[ADR-t2105-1](../adr/2026-10-08-t2105-1-e2e-gate-skips-cmux-e2e-only-when-cmux-does-not-answer.md) 決定 1）による。その後 89b6f5d2 が e2e と関門から cmux を外した。

それぞれの検証 commit と対象 commit の差分（`git diff --stat <検証 commit> 6e16cd35b982b559e243d01cc9ddc43b1b5c93cb -- src tests build.rs Cargo.toml Cargo.lock`、出力の全体は同じ名前の directory の text file）:

- 1c7ff1d8 → 6e16cd35: `536 files changed, 128827 insertions(+), 55549 deletions(-)`（[全体](goal-3-acceptance-ci/diff-stat-1c7ff1d8-6e16cd35.txt)）。`git diff --stat 1c7ff1d8 60d992ca -- src tests build.rs Cargo.toml Cargo.lock` は空で、event 134163 は 60d992ca とのコード差分が無いことに基づく根拠である。
- 5844971f → 6e16cd35: `427 files changed, 89072 insertions(+), 26086 deletions(-)`（[全体](goal-3-acceptance-ci/diff-stat-5844971f-6e16cd35.txt)）。5844971f は task 1555 の run branch の commit で、`git merge-base --is-ancestor 5844971f 6e16cd35` の exit status は `1`（対象 commit の祖先でない）。

### 既存の記録を使わない理由と、対象の証拠

どちらの検証 commit も、対象 commit とも作業の起点の main とも src/・tests/ に差分があり、その e2e の結果は対象の commit のコードの e2e の証拠にならない。対象に当てはまる既存の記録が無いので、この記録の task は `--evidence e2e` で登録され、runtime が review の pass の後に host でこの task の run の commit（作業の起点の main ＋この文書）で e2e を流す。その run の `run_e2e_finished` が e2e の証拠で、goal review がそれを読む。この文書は docs だけを変えるので、その run の commit の src/・tests/ は作業の起点の main と同じ。

### 作業の起点の main と CI の対象 commit の差分

作業の起点の main（`git merge-base HEAD origin/main`）は aff18ec286e6e5a54cbf909490285603337166f7 で、CI の対象 commit 6e16cd35 との src/・tests/ の差分はある。

- `git diff --stat 6e16cd35b982b559e243d01cc9ddc43b1b5c93cb aff18ec286e6e5a54cbf909490285603337166f7 -- src tests`: `581 files changed, 84413 insertions(+), 10867 deletions(-)`（[全体](goal-3-acceptance-ci/diff-stat-6e16cd35-aff18ec2.txt)）
- `git log --format='%h %ad %s' --date=iso-strict 6e16cd35..aff18ec2 -- src tests`: 124 commit（[一覧](goal-3-acceptance-ci/commits-6e16cd35-aff18ec2.txt)）

つまり coverage の証拠（6e16cd35 の CI）と e2e の証拠（aff18ec2 ＋この文書の run の e2e）は別の commit のもので、6e16cd35 より新しい main の commit で `fmt / test / clippy / coverage` が success の run は 2026-10-20 の時点で無い。
