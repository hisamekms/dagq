---
id: design-supervisor-lifecycle-landing-e2e
type: design
title: "着地の前のe2e"
status: current
created: 2026-10-08
scope: runtime
related:
  - design-supervisor-lifecycle-review
  - design-supervisor-lifecycle-validation
  - design-supervisor-lifecycle-auto-update
  - adr-t1233-2
  - adr-t1165-1
  - adr-t1582-1
  - adr-t2105-1
---

# 着地の前のe2e

[Review](review.md)の流れの中で、passの後・着地の前に置く工程。

## 入口の地図

| 知りたいこと | コードの入口 |
| --- | --- |
| 着地の前のe2e | `src/application/supervise/e2e.rs`、`src/domain/run_e2e.rs`、`src/infrastructure/e2e_gate.rs` |

## 着地の前のe2e

[ADR-t1233-2](../../adr/2026-10-02-t1233-2-e2e-runs-on-the-host-after-review-passes.md)。
e2eはworkerが流さず、reviewがpassしたrun（`land`の答え・jobの`land`の推奨・resumeから着地へ進むものも）のうち要るものに、着地の前にsupervisorがhostで流す（`application::supervise::e2e`）。
要否はvalidatingが決めて記録する（[Validation](validation.md#runtimeが流すe2e)）。

- **いつ**: 着地スロットを待つ段で、着地の前提（`[run.env]`のプログラム・landing branch・空き容量）がそろってから、reviewしたcommitにまだ通ったe2eが無ければ流す（`run_e2e::due`）。
  列に並んだleaseの無いrunは、leaseを取ってslotに入る。
  着地のrebaseの後と人の`integrate`では流さない。
- **流し方**: 自動更新の関門（[Auto-update](auto-update.md)）と同じ`e2e_gate::run`を、runのworktreeでworkerのbuildのtargetで流す。
  main checkoutがdagqのsource（[Source repository](source-repository.md)）でなければ、runtimeの知るe2eが無いので流さずに着地へ進む。
- **期間限定で外したケース**（[ADR-t1582-1](../../adr/2026-10-04-t1582-1-temporarily-leave-broker-and-cmux-only-e2e-cases-out.md)）は流れず`skipped`にも出ない（一覧は[testの制約](../../development/testing.md#e2e)）。
- **同時に1本**（決定4）: 1つのsupervisorのrunは1本ずつ流し、順番を待つrunはslotとleaseを持ったまま待つ。
  processをまたいではhostの全queueで共通のlock（`install::e2e_lock_path`）を取り、自動更新の関門と`install`の関門も同じlockを待つ。
- **関門の印**（決定5、[ADR-t1165-1](../../adr/2026-09-30-t1165-1-e2e-gate-reruns-failed-e2e-once-and-records-quarantined-failures.md)）: 落ちたtestは名前で1回だけ流し直し、なお落ちたtestを印で判定する（`e2e_verdict::judge`）。
  印はworktreeでなくlanding branchのcommitの`.config/e2e-quarantine.toml`から読むので、workerが足した印は効かない。
  runが変えたtestのファイルの印と、runのtaskが直す印は外す（`run_e2e::marks_for_run`）。
  差分が読めなければ印を効かせない。
- **落ちたとき**（決定3）: 印で通らない失敗はrunを`needs_session`にし（`code: e2e_failed`）、[`needs_session`](needs-session.md#needs_session)のresume（`ResumeKind::E2e`）がlogを読んで直すよう頼む。
  [Landing recheck](landing-recheck.md)が着地しないと見つけたheadは、e2eを流さず（間なら流した後に）`needs_session`にする。
- **cmuxが答えないとき**: 実cmuxを要るe2eだけを外して残りで判定し、外したtestと理由を`skipped`に残す（流せない理由にしない）。
- **流せないとき**: 始められない・上限で止めた・流し直しが上限を過ぎたときは、変更のせいとは言えないのでworkerに返さず、slotとleaseを持ったまま待って流し直す（`outcome: unavailable`）。
  続けば`status`のattentionに`check the e2e host`が出て、流せたら消える。
  待つ間にdrainか引き継ぎに入れば、leaseを外して`review and integrate`に残す。
- **引き継ぎ**: 流している段は組み立て直せないので、handoffは終わるのを待ち、始まっていないrunは組み立て直せる待ちの段に置く。
- **環境**（ADR-t1233-2のConsequences）: この工程はworkerが書いたコードをhostで動かすので、起こしたprocessのenvを消し、許す名前と`[run.env]`と関門が置くものだけを渡す。
  資格情報は許す名前に当たっても渡さず、例外はcmuxのsocketのpasswordだけ。
  許す名前・接頭辞と理由は`e2e_gate.rs`の`PASSED_ENV`・`PASSED_PREFIXES`、絞り方と資格情報の判定はprogramのreviewと共通の`passed_env.rs`の`PassedEnv`・`credential`が持つ。
  落とし穴: 許す名前が足りない版が着地して固定バイナリが入れ替わると、runのe2eが始められず`check the e2e host`が続くか、道具が見つからず`e2e_failed`でworkerに返り、自動更新の関門も落ちうる。
  そのときはlogの`== the env passed`の行とe2eの出力で足りない名前を見て、`dagq install --rollback`で戻し、許す名前を足すtaskを登録する。
- **見え方**: `stats`の`land_phases`の`e2e_wait`と`e2e`（[Stats](stats.md)）、`timeline`の空白の理由（[Timeline](timeline.md)）。
