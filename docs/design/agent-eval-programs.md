---
id: design-agent-eval-programs
type: design
title: agentのevalのprogramのreviewの当て方（ケースに当てるprogramの選び方・baseで落ちるprogramを除くこと・実行の枠・incompleteにする場合）
status: current
created: 2026-11-06
scope: runtime
related:
  - design-agent-eval
  - adr-t1728-1
  - adr-t1895-2
  - adr-t2300-1
  - design-supervisor-lifecycle-review-programs
---

# agentのevalのprogramのreviewの当て方

[agentのeval](agent-eval.md)のケースに、本番のreviewと同じ段を当てる約束。

- evalは各ケースに本番のreviewと同じ段を当てる（判断は`domain::agent_eval::programs`）。
  周の定義とケースを読むlanding branchのcommitから、programの一覧とscriptを読み（[ADR-t1895-2](../adr/2026-10-06-t1895-2-program-reviews-are-fast-format-checks-read-from-the-landing-branch.md)。一覧の設定・実行・envとbackendは[プログラムのreview](supervisor-lifecycle/review-programs.md)と[Run environment](supervisor-lifecycle/run-environment.md)が持ち、ここに写さない）、pathがケースの差分に当たるものを、ケースのtreeをcwdに設定の順に流す。
  ケースのpatchがscriptや設定を変えても、流れるのはcommitの中身である。
- 選んだprogramは、先にケースの`base_commit`のtree（ケースのtreeと同じものを隠す）で設定の順に流し、0以外で終わったものはそのケースのtreeで流さない（古いbaseは今の規則に既に違反しうるが、本番のreviewのbaseのlanding branchはCIが通すため。[ADR-t2300-1](../adr/2026-10-21-t2300-1-eval-leaves-out-programs-a-cases-base-fails.md)）。
  baseで0のものだけをケースのtreeで流すので、ケースを止めるのは差分が足した違反だけである。
- built-inのprogramはケースに当てない（runの変更が足す行を着地の前に見るもので、ケースのpatchはそれに当たらない）。
- programはケースの1回の枠の中でagentのjobの前に1本ずつ流し、周の同時数の上限を超えない。
  providerの費用には数えない。
  全部exit 0ならagentのjobを起動する。
- ケースのtreeで1本でも0以外で終わったケースは、残りのprogramとagentを起動せず、agentの成績の分母から外し、`agent_eval_finished`に記録する。
- programの起動の失敗（reviewのactorのbackendがcontainerのときを含む）か時間切れのケースがあれば（baseのtreeでも同じ）、新しい実行を起動せず、周を`incomplete`で閉じ、`passed`を`false`にする。
- ケースごとのprogramの結果は`agent_eval_case_checked`に記録し、周を引き取ったsupervisorは止まったケースのagentを起動しない。
- programが受け持つ規則コードはagentの定義から外すか狭め、ケースの`expected`は書き換えない（programが止めたケースは分母から外れ、ラベルの変更は人・inbox・plannerに限る。[ADR-t1728-1](../adr/2026-10-06-t1728-1-agent-definitions-cases-and-eval-as-a-queue-service-use-case.md)の決定6・13）。
