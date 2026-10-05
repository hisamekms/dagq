---
id: adr-t1818-2
type: adr
title: 復旧jobの足すoptionは種類を宣言させ、runtimeの操作や完了・cancelと紛らわしいものはruntimeのoptionに寄せるかaskから外し、残るjobのoptionは復旧jobにもう一度渡ることをoptionごとに示し、ADR-t1818-1の経路への引き渡しを許された操作に足す（ADR-0047決定40とADR-t813-1決定9をamends）
status: accepted
created: 2026-10-05
updated: 2026-10-05
accepted_on: 2026-10-05
amends:
  - adr-0047 decision 40
  - adr-t813-1 decision 9
owners:
  - hisamekms
tags:
  - runtime
  - supervisor
  - operations
related:
  - adr-0047
  - adr-t609-1
  - adr-t813-1
  - adr-t451-1
  - adr-t1818-1
  - design-supervisor-lifecycle-triage
  - design-supervisor-lifecycle-background-recovery-job
---

# ADR-t1818-2: 復旧jobの足すoptionは種類を宣言させ、runtimeの操作や完了・cancelと紛らわしいものはruntimeのoptionに寄せるかaskから外し、残るjobのoptionは復旧jobにもう一度渡ることをoptionごとに示し、ADR-t1818-1の経路への引き渡しを許された操作に足す（ADR-0047決定40とADR-t813-1決定9をamends）

## Context

[ADR-0047](0047-irregularities-in-three-layers-recovery-job-ask-reasons-and-goal-review.md)決定40のescalateは、askのoptionsを「今のkindのもの（`decide`は`retry` / `resume` / `cancel`）にjobの`options`を足す」と決めている。決定40は[ADR-t609-1](2026-09-27-t609-1-failed-live-recovery-job-opens-the-alert-ask.md)（生きているrunのjobの失敗をalertのaskにする）と[ADR-t813-1](2026-09-28-t813-1-headless-worker-path.md)決定9（非対話のrunで選べる操作）がamendsしており、escalateのoptionsの規則と許された操作の一覧の枠はそのまま残っている。

jobのoptionは自由な文で、runtimeは`retry` / `resume` / `cancel`以外の答えを何も動かさずに復旧jobへもう一度渡す（[Triage](../design/supervisor-lifecycle/triage.md)の9）。task 1217のrun（commitの無いreceiptで`commit_mismatch`）では、jobが「Mark task 1217 done (or cancel it as obsolete)」を足し（ask 436）、inboxはそれを完了かcancelの操作と読んで選び、`triage_decided`（`action: recover`）で何も動かなかった。questionの全体の説明に「jobの足したoptionは復旧jobにもう一度渡る」とあっても、optionの文がruntimeの操作に見えれば読み違える。

goal 141のBは、紛らわしいoptionを出さず、残るjobのoptionが何をするかをoptionごとに分かるようにすることを求める。Aの「baseがすでに受け入れ条件を満たす」の経路（以下「ADR-t1818-1の経路」）は[ADR-t1818-1](2026-10-05-t1818-1-worker-claims-already-satisfied-and-runtime-closes-on-a-verified-snapshot.md)が決める。

## Decision

決定40のescalateのoptionsの規則と許された操作の一覧を次のとおり改める。

1. **jobは足すoptionごとに種類を宣言し、runtimeは宣言で寄せ、語で外す。** jobの`options`の各要素は、今のkindのruntimeの答え（`decide`なら`retry` / `resume` / `cancel`）のどれを指すか、復旧jobに渡し直すものかを宣言する。
   - runtimeの答えを指すoptionは、jobの文ではなくruntimeのそのoptionに寄せる（重ねず、jobの文は推奨の説明としてquestionに載せる）。今のask・runで適用できない答えを指すものはaskから外す。
   - 種類の無いもの・知らない種類のものはaskから外す。
   - 復旧jobに渡し直すと宣言したoptionでも、文がruntimeの操作（retry・resume・cancel）や「完了にする」「doneにする」と紛らわしければaskから外す。ただし、決定3の引き渡しを次のラウンドのjobに頼むと宣言したoptionは、その旨の表示（決定2）で残す。宣言を正とし、語の検査はjobの宣言の誤りを拾う柵として持つ（語の一覧はdesign）。
   - 外したoptionと理由は記録し、jobの見立てとしてquestionに残してよいが、選べるoptionにはしない。
2. **残るjobのoptionは、ask上でoptionごとに「復旧jobにもう一度渡る（runtimeは何も動かさない）」と分かる形にする。** questionの全体の説明だけに頼らず、各optionの表示にそれを示す。引き渡しを頼むoptionは、選ぶと復旧jobがもう一度走って引き渡しを選び、閉じるかはADR-t1818-1の判定が決めることを示す。表示の形はdesignが持つ。
3. **許された操作に「ADR-t1818-1の経路への引き渡し」を足す。** 決定40の許された操作の一覧への追加で、非対話のrunで選べる操作（ADR-t813-1決定9）にも同じに足す。
   - 対象は、workerが主張を書けずに失敗したrunのうち、commitの無いreceiptで`commit_mismatch`になり、baseが受け入れ条件を満たすとjobが見立てたもの。
   - 前提（runtimeが適用の時点で再検査し、崩れていればverdict全体を`escalate`として扱う決定40の規則のまま）: runがtaskの最新のrunで`failed`、leaseが無く、失敗がreceiptの`commit`がbaseを指す`commit_mismatch`で、runのbranchに`base_commit`より先のcommitが無い（捨てる成果が無い）。
   - 適用すると、runtimeはそのrunをADR-t1818-1の経路（snapshotのverify、review jobの判定、`high`のpassで閉じ、それ以外は人のask）に回す。jobは閉じる判断をせず、主張を渡すだけにする。receiptに残った確かめとjobの見立てが根拠に代わる（ADR-t1818-1決定1の例外）。
   - 人は`decide`のaskでこれを直接の答えとしては選ばない。人が選べるのは決定2の引き渡しを頼むjobのoptionで、答えは今までどおり復旧jobに渡り（Triageの9）、次のラウンドのjobがこの操作を選ぶ。
4. **ほかは変えない。** 決定40のverdictの形、他の許された操作と許されない操作（taskのcancelをjobが選べないことを含む）、自信が無いときの扱い、jobの失敗の扱い（ADR-t609-1のamendsを含む）、ADR-t813-1決定9の他の操作は変えない。ADR-0047決定3（failedのrunを復旧jobにかけ、`decide`の答え`retry` / `resume` / `cancel`をsupervisorが適用する）と決定41（人が要る理由の分類）も変えない。

## Alternatives

- **語だけで判定する**: jobの文は自由で言い回しが多く、取りこぼしも誤検出も出る。宣言を正にし、語は柵にとどめる。
- **宣言だけで判定する**: jobが宣言を誤ると、ask 436と同じ読み違いが残る。外すのは安全側（jobに渡し直す経路と見立てはquestionに残る）なので、語の柵を足す。
- **jobのoptionを全部やめる**: 人の答えをjobに渡し直す経路（verifyを直してから`retry_inherit`させるなど）が使えなくなる。
- **「完了にする」をruntimeの新しい直接の操作にする**: verifyとreviewの判定を経ずに閉じることになり、ADR-t1818-1の決定と食い違う。
- **引き渡しを`decide`のruntimeの答えにして人が直接選べるようにする**: ADR-0047決定3の答えの集合を変えることになる。jobに渡し直す今の経路で同じことができ、前提の見立てもjobが持つ。
- **引き渡しを足さない**: 主張と見られるrunがworkerの書き方の違いだけで人のaskに残り、Aの経路があっても使えない。

## Consequences

- 人とinboxは、選んだoptionが何も動かさないのに動いたと読むことが無くなる。
- 復旧jobのverdictのschemaとpromptに、optionの種類と引き渡しの操作が加わる。
- 実装はgoal 141の実装のtaskが行い、optionの種類の綴り・語の一覧・askの表示・eventの欄は[Triage](../design/supervisor-lifecycle/triage.md)と[生きているsessionの復旧job](../design/supervisor-lifecycle/background-recovery-job.md)に書く。
