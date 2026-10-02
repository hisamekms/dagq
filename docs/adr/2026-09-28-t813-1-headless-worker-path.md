---
id: adr-t813-1
type: adr
title: worker の非対話の経路を足し、1 turn を 1 回の非対話の呼び出しにして、answer・revise・resume・催促を同じ session の resume の呼び出しで送り、process を cmux の workspace の session wrapper の中で動かす。非対話で起きる止まりは ADR-0047 の 3 層で扱い、新しい actor は足さない（ADR-0027 決定 1・2・4、ADR-0071 決定 1・2・6・16、ADR-0047 決定 30・31・37・39・40 を amends）
status: accepted
created: 2026-09-28
updated: 2026-09-28
accepted_on: 2026-09-28
amends:
  - adr-0027 decision 1
  - adr-0027 decision 2
  - adr-0027 decision 4
  - adr-0071 decision 1
  - adr-0071 decision 2
  - adr-0071 decision 6
  - adr-0071 decision 16
  - adr-0047 decision 30
  - adr-0047 decision 31
  - adr-0047 decision 37
  - adr-0047 decision 39
  - adr-0047 decision 40
amended_by:
  - adr-t1340-1
  - adr-t1404-1
owners:
  - hisamekms
tags:
  - runtime
  - supervisor
  - provider
related:
  - adr-0004
  - adr-0027
  - adr-0047
  - adr-0071
  - adr-t598-1
  - adr-t728-1
  - adr-t813-2
  - adr-t813-3
  - plan-headless-worker-spike
---

# ADR-t813-1: worker の非対話の経路を足し、1 turn を 1 回の非対話の呼び出しにして、answer・revise・resume・催促を同じ session の resume の呼び出しで送り、process を cmux の workspace の session wrapper の中で動かす。非対話で起きる止まりは ADR-0047 の 3 層で扱い、新しい actor は足さない（ADR-0027 決定 1・2・4、ADR-0071 決定 1・2・6・16、ADR-0047 決定 30・31・37・39・40 を amends）

## Context

今の worker は Claude Code の対話の session を cmux の terminal で動かし、画面の判定（入力欄・作業中・既知のダイアログ・認証）、Stop hook の idle の印、`/exit`、画面への打ち込みで制御している。2026-09-27 に人が planner に Codex でも worker を動かしたいと依頼し、続けて、非対話の実行を前提に計画を組み直すこと、Claude も非対話に移す計画があることを指示した（goal 57）。planner の測定（note 26997）と spike（[headless-worker-spike](../plans/headless-worker-spike.md)、task 812）で、`claude -p` と `codex exec` はどちらも 1 回の呼び出しが 1 turn で、process の終了が turn の終わり、JSONL の出力に turn の結果・usage・失敗が出て、session の id で resume できると分かった。Codex の画面の判定を作らずに済み、Claude も同じ経路に乗せられる。

2026-09-27 に人は「ask は人判断必須のものだけになる？ そのための別のアクターはいらない？」と問い、planner は「ask は人が要る理由のものだけにし、actor は足さない」と答えた。

## Decision

1. **非対話の経路。** worker の 1 turn を 1 回の非対話の呼び出し（`claude -p` / `codex exec`）にし、turn の結果は process の終了と JSONL の出力（最後の結果の event、exit code）で読む。画面の判定・idle の印・`/exit`・画面への打ち込みは使わない。receipt・commit・evidence の規則と、validating・review・integrate の流れは対話の経路と同じにする。
2. **続きは同じ session の resume の呼び出しで送る。** worker の質問への answer、review の revise、merge-tree の衝突の解消依頼、`needs_session` の resume、receipt の書き直しなどの催促、`queue_hold` の後の「続けて」は、どれも同じ session の id の resume の呼び出しの prompt にする。生きている terminal への打ち込みはしない。送った文が処理されたかは、その呼び出しの turn が始まり終わったことで分かるので、入力欄を読む確認とEnterの送り直し（ADR-0047 決定 31）は非対話の run では行わない（決定 31 を amends）。
3. **process は cmux の workspace の session wrapper の中で動かす。** 呼び出しは今の worker と同じく run の workspace の session wrapper が起動し、出力をその terminal に見せ、終了と exit code を run の dir に残す。これで supervisor の再起動や exec の引き継ぎを越えて turn が生き、adopt と引き継ぎは今の wrapper の規則で組み立て直せる。supervisor の子にする案は退ける（下の Alternatives）。turn を止めるときは、呼び出しの process group ごと止める（Codex は SIGTERM で子を残すため）。
4. **人は worker の terminal に打ち込めなくなる。代わりは ask と answer。** 非対話の run の terminal は出力を見せるだけで、入力を受けない。人が worker に伝えることは、worker が開いた `worker_question` への answer、review の `approve_landing` の `send_back`、`stalled` などの ask への answer で行い、runtime がそれを resume の prompt にして送る。人が直接手を入れたいときは今の `dagq-recover` の手順（run を手放して worktree で作業する）を使う。
5. **ADR-0027 との関係（決定 1・2・4 を amends）。** 非対話の run では「review の間 session を開いたまま待つ」は「review の間 process は無く、session の id と worktree を持ったまま待つ」と読み替える。revise（決定 2）と merge-tree の衝突の解消依頼（決定 4）は `cmux send` ではなく resume の呼び出しで送る。`/exit` と workspace の close の時点（決定 1）は、非対話では最後の turn の終わりで process が無くなるので `/exit` が無く、workspace の close だけを同じ時点で行う。revise の回数と verdict の扱いは変えない。
6. **ADR-0071 との関係（決定 1・2・6・16 を amends）。** 非対話の run は人の答えを待つ間 process を持たない。待ちになるのは turn が `worker_question` を開いて終わったとき（`Session` / `Revise` / `Resume` の段）だけで、`answer_prompt` / `stalled` / `stuck_exit` の待ち（画面とダイアログと `/exit` に由来するもの）は非対話では起きない（決定 1 の表）。待ちは答えが書かれたか ask が close されたときに終わり（決定 2）、戻り待ちになって slot が空いたら resume の呼び出しで答えを送る。待ちの間の見張り（決定 6）は lease を持ったまま、process の代わりに ask と run の記録だけを見る。決定 9 の復旧 job が escalate して開いた `stalled` の ask も、turn は終わっているか止めてあるので、同じく process の無い待ちにし、答えで終えて戻り待ちにする（答えの文を resume の prompt に載せる）。`worker_question` を開いた turn は process の終了で終わるので、決定 16 の「答えを送った後の idle だけで段を終える」は「答えを載せた resume の turn の終わりで段を判定する」と読み替える。slot を空けること、待ちの数の上限、戻る順は変えない。
7. **対話の経路は Claude の既定に残る。** 対話の経路とその決定（ADR-0027・ADR-0071・ADR-0047 の画面・idle・`/exit`・ダイアログの規則）は対話の run にそのまま効く。Claude の run は既定で対話の経路を使い、非対話の経路は選んだときだけ使う。Codex の run は非対話の経路だけを使う。Claude の run の経路は task ごとに選べ、指定が無ければ対話にする（綴りは design）。Claude の既定を非対話に切り替えるかは、両経路の測定の後に別の ADR で決める。provider の選び方と、provider と経路の記録は [ADR-t813-2](2026-09-28-t813-2-provider-per-task-and-mutual-fallback.md) が決める。
8. **起動の確かめ。** runtime は turn の最初の出力（Claude の init の permission の mode、Codex の thread の id）を読み、頼んだ設定で始まっていなければ turn を止めて起動の失敗として記録する（haiku は `auto` を黙って `default` にする）。これは provider が使えないことではなく設定の誤りなので、provider の切り替え（ADR-t813-2）には回さず、一般的な失敗として決定 9 の 3 層で扱う。
9. **人以外の判断の置き場所。** 非対話の run でも ask は人が要る理由（ADR-0047 決定 41 の `reason_category`）に当たるものだけにし、新しい actor は足さない。対話の session に対して runtime と復旧 job が人を経ずに行っていた判断（Enter の送り直し・既知のダイアログ・`/exit`・`prompt_waiting`・`stuck_exit`・`idle_process`・`long_background`）は非対話では起きない。代わりに起きる次の状況を、ADR-0047 の 3 層で扱う（決定 37・39・40 を amends し、非対話の run の行と alert と操作を足す）。
   - **receipt も ask も無い turn の終わり**（process は正常に終わった）: runtime が決まった文面の催促を resume の呼び出しで決まった回数だけ送る（決定 30 の「一度だけ促す」を、非対話の run では決まった回数に読み替える。決定 30 を amends）。なお続けば復旧 job の alert にする。
   - **出力の途絶え**: heartbeat を出す provider（Claude）だけ、一定時間 stream が無ければ復旧 job の alert にする。heartbeat の無い provider（Codex）は process が生きていることと時間の上限で見る。
   - **時間の上限**: runtime が turn の時間の上限を持ち、超えたら turn を止めて（process group ごと）復旧 job の alert にする。
   - **permission の拒否が続いて進まない**: 出力の拒否の記録から runtime が数え、決まった回数を超えたら復旧 job の alert にする。
   - **turn の失敗**（非 0 の終了、失敗の結果）: 一般的な失敗は今の `failed` / `interrupted` と同じく復旧 job にかける。認証と利用上限は [ADR-t813-2](2026-09-28-t813-2-provider-per-task-and-mutual-fallback.md) の切り替えに回す。
   - 復旧 job が非対話の run で選べる操作は、指示つきの resume（`send_instruction` と `resume` を resume の呼び出しにしたもの）、turn を止める、`retry` / `retry_inherit`、`wait`。前提の再検査と上限（alert の種類ごとに 3 回）と `escalate` の規則は決定 40 のまま。turn の終わりに run の worktree を cwd に持つ process が残っていれば記録し、止めるのは `stop_processes` と同じく pid で行う。
   - 決めきれないときだけ、今の kind の ask（`stalled` / `decide`）で人に聞く。

event の kind・欄名・flag・催促の回数・時間の上限・途絶えの閾値は後続の実装 task が [docs/design/](../design/) に書く。

## Alternatives

- **process を supervisor の子にする**: 実装は単純だが、supervisor の再起動（`down` / `up`、crash）で turn が死ぬか孤児になり、人は出力を見られない。exec の引き継ぎは pid を保つが、再起動は越えられない。wrapper はすでに adopt の規則を持つ。
- **Codex の対話の TUI に画面の判定を作る**: Claude と同じ量の画面の判定・ダイアログ・idle の仕組みを Codex にも作ることになり、TUI の変化に弱い。非対話の出力は構造がある。
- **非対話の run の判断のために新しい actor（非対話の run の世話役）を足す**: runtime の決まった規則と復旧 job で足り、ask を人の判断に限る原則（決定 41）と役割の数を増やさない方針（ADR-0047 決定 1）に反する。
- **Claude の既定をすぐ非対話にする**: 対話の経路の実績（画面の判定の規則、自動修正の数）に対して、非対話の経路はまだ測っていない。測定の後に決める。

## Consequences

- Codex の画面の判定は作らない。非対話の run では入力欄の確認・ダイアログ・`/exit` の自動修正が起きず、対応する alert（`stuck_exit`・`prompt_waiting`・`idle_process`）も出ない。
- 人は非対話の worker に terminal で指示できない。手を入れるのは ask の answer か `dagq-recover` になる。
- 待ちの間は process が無いので、待ちの run が host の資源を使わない。
- 対話と非対話の 2 つの経路を runtime が持ち続ける。ADR-0027・ADR-0071・ADR-0047 の該当の決定は、対話の run にはそのまま、非対話の run にはこの ADR の読み替えで効く。
