---
id: design-measurement-ledger
type: design
title: 計測の台帳（台帳の形・畳む関数・台帳を作る係・共通の読み取り関数）
status: current
created: 2026-11-18
scope: runtime
related:
  - design-measurement
  - design-measurement-missing
  - design-measurement-environment
  - design-supervisor-lifecycle-commands
  - adr-t1662-1
  - adr-t1662-2
  - adr-t4644-1
  - adr-t4652-1
---

# 計測の台帳

[計測](measurement.md)の記録から台帳を作り、読み手が共通の関数で読むための責務と約束を持つ。
記録の区分は[SSOTとビュー](measurement.md#ssotとビュー)、区間の考え方は[区間とタグ](measurement.md#区間とタグ)を読む。

## 台帳の形

台帳は対象（run・task・session・queue・node）ごとに1行（ADR-t1662-1決定10）。
区切りの値は列、フローで形が変わるものはJSON。
行の項目と種類ごとの中身は`LedgerRow`と`domain::ledger`の各moduleのdoc comment、作り直しの規則は`ledger::rebuilds`が持つ。

- eventはどれか1つの行の記録になるが、写さないkindのうちqueue行に属し被覆の始まりより後のeventは、省略が有効な間はどの行の記録にもならず窓の端だけに効き、共通の読み取り関数が補う（[ADR-t4644-1](../adr/2026-11-09-t4644-1-omit-unread-queue-events-and-read-window-edges.md)）。
  run・task・sessionに属する同じkindのeventは今どおりその行の記録で、行の持ち主の入口は`domain::ledger::owner`、写さないkindの一覧の入口は`domain::ledger::queue`。

## 畳む関数

区間の列を作る関数は1つで（ADR-t1662-1決定2）、台帳を作る係と、走っているrunを見る読み手（`status`・`timeline`・`forecast`）が共有する（`domain::ledger::fold`）。
読み手は`run_phase_changed`の名前とタグを出し、記録の無いrunは`legacy`にする（各moduleのdoc comment）。
入力はEventStoreのeventと、それから作るclaimの前の区間の行・今の時刻だけで、同じ入力からは同じ出力を返す。

- runとtaskの畳み方は`domain::ledger::fold`、runのstepの内側の層は`domain::ledger::steps`のdoc commentが持つ。
- claimの後の事実は`domain::after_claim`が作り、statsの`MeasureTrack`も同じものを使う。

## 台帳を作る係

係の入力・状態・増分の読み取りと書き込みは`domain::ledger::builder`のdocが持つ（[ADR-t4652-1](../adr/2026-11-09-t4652-1-ledger-writes-bounded-by-pass-time-and-r-split-by-kind.md)）。

- 着地の処理の中では作らず、runtimeの制御（claim・slot・resume・着地）は台帳を読まない。
- runは終わりから猶予の後で、telemetryのtokenの失効と資源の最後の窓の記録の後にだけ確定する（[実行環境と資源](measurement-environment.md#台帳との順序)）。
- storeの印は、その位置までの全入力が行の記録か写さないqueueのeventの被覆で記録済みである約束を持つ。
- 古い版の作り直しは`builder`のdocが持つ。
  行の持ち主を変える版上げは直らない。
- 読み書きに失敗してもrunとtaskの結果を変えない。
- 1要求の行は1つのtransactionで書かれ、印は覆う最後の組と同じ書き込みに載り、巨大な単独行も飛ばされず原子的に書かれる。

## 共通の読み取り関数

stats・kpi・reportの全ての欄は、行を窓と絞り込みで選んで返す1つの関数（`domain::ledger::window`）だけから作る。
窓の端と、読み手が窓の問いで宣言する窓の中の境界（kpiの期間と比較の区切り・event idのcursorの時刻・statsの頁の始まり）の補いは(C, S]の写さないqueueのeventを越えず、ほかの値の取り方を変えず、読む量は境界ごとに端の近くに限る（入口は`domain::ledger::window`と`window::bounds`）。
統計のmoduleはこの関数が返す値だけを取り、StateStore・run dir・process・Gitを読まない。
窓の当て方・絞り込み・旧方式と走っているrunの扱い・欄ごとの元の行は`domain::ledger::window`、今の名前の意味の違いは`ledger::read`のdoc commentが持つ。

- kpiの工程の名前と、hostの設定の目標の読み替えは`domain::kpi::window`のmodule docが持つ（ADR-t1662-1）。
- 新しい出力は所要時間とslotを握った時間の2本でタグごとの内訳とunattributedを出し、`change`と`area`で層に分ける。

### 欠測と集約の契約

値の無い窓と入力ごとの集約の約束は[欠測と集約の契約](measurement-missing.md)が持つ。
