---
id: adr-t1063-1
type: adr
title: worker 以外の headless の job の provider を役割ごとの設定で選び（既定 claude）、job は権限を意図で渡して provider の実装が自分の仕組みに訳し、最終の返答の text を受け取る。使えない provider からは worker と同じ条件でもう一方に切り替えて起動し直し、控えは provider ごとのまま、どの provider・model で動いたかを全ての job で記録する（ADR-t813-2 決定 6 を amends）
status: accepted
created: 2026-09-29
updated: 2026-09-29
accepted_on: 2026-09-29
amends:
  - adr-t813-2 decision 6
owners:
  - hisamekms
tags:
  - runtime
  - supervisor
  - provider
related:
  - adr-t813-2
  - adr-t813-3
  - adr-0079
  - adr-0047
  - adr-t728-1
  - adr-t598-1
  - plan-codex-headless-jobs-spike
  - design-supervisor-lifecycle-actor-model
  - design-supervisor-lifecycle-goal-review
  - design-supervisor-lifecycle-queue-hold
---

# ADR-t1063-1: worker 以外の headless の job の provider を役割ごとの設定で選び（既定 claude）、job は権限を意図で渡して provider の実装が自分の仕組みに訳し、最終の返答の text を受け取る。使えない provider からは worker と同じ条件でもう一方に切り替えて起動し直し、控えは provider ごとのまま、どの provider・model で動いたかを全ての job で記録する（ADR-t813-2 決定 6 を amends）

## Context

[ADR-t813-2](2026-09-28-t813-2-provider-per-task-and-mutual-fallback.md) は worker の provider を task ごとに選べるようにし、Context と決定 6 の 3 つめの項で、review・復旧・plan review・goal review の job、observer、runtime の planner を「切り替え先の無い Claude だけの役割」として、Claude の控えがこれらを止めると決めた。job のコードは Claude Code の道具名で権限を書き、Claude の出力をそのまま verdict として読んでいる。

2026-09-29 に人は、worker 以外の headless の job も Codex で動かせるようにし、(1) 影響の小さい goal review から始める、(2) job は provider を知らない（interface と実装の関係）、(3) どの provider・model で動いたかを後から見分けて Claude と比べられることを必須にする、と決めた（goal 73）。planner と inbox は対話の session なので対象外。

spike（[codex-headless-jobs-spike](../plans/codex-headless-jobs-spike.md)、codex-cli 0.155.1、本番 queue の goal 47）で次が分かった。

- Codex の読み取りだけの sandbox の中で、goal review が使う dagq の読み取りは全て sandbox の外と同じ出力になり、追加の設定は要らなかった。書き込みは sandbox が拒んだ（1.）
- job の process に置いた env（役割と queue）はそのまま sandbox の中の `dagq` に渡り、CLI の role の policy は Claude の job と同じに効く（2.）
- verdict は最後の返答の text として取り出せ、同じ prompt で Codex は schema どおりの verdict を 2 回返した。prompt は Claude の plugin・skill・hook に頼っていない（3.・5.）
- 実際の model は Codex の JSONL の出力には無く、Codex が書く rollout か人向けの header にだけある（3.）
- Codex は許された道具に限らず sandbox の中で任意の読み取りのコマンドを打つ。「読み取りだけ」という意図は同じで、表し方が違う（5.）
- 認証・利用上限・実行ファイルが無い場合は再現していないが、worker の分類（ADR-t813-2 決定 2）がそのまま当たる見込み（4.）

## Decision

1. **job の provider は役割ごとの設定で選ぶ。** headless の job（review・復旧・plan review・goal review・observer と、[ADR-t996-1](2026-09-29-t996-1-supervisor-runs-throughput-review-jobs-and-reports-to-inbox.md) のスループットの見直しの job）の provider は job のコードでなく、役割ごとの model / effort（[ADR-0079](0079-record-task-weight-predictions-and-trial-model-effort-selection.md) 決定 7）と同じ役割の設定の表で選ぶ。値は `claude` / `codex`、既定は `claude` で、設定しなければ今と同じに起動し振る舞う。model の値は provider ごとの名前として読む。その役割を動かせる実装を持たない provider を設定したときは、起動せずに設定の誤りとして知らせる。
2. **権限は意図で渡し、出力は最終の返答の text で受け取る。** job は provider に、Claude Code の道具名でなく意図（読み取りだけ、dagq の読み取りを打てる、など）を渡し、provider の実装が自分の仕組みに訳す（Claude は許す道具の一覧、Codex は読み取りだけの sandbox と、role と queue を持つ env）。この ADR で Codex に乗せる job（goal review）の sandbox は読み取りだけにし、bypass の flag を使わない。queue に書く役割（observer など）の sandbox は、それを乗せる task が決定 3 とともに決め直す。job は provider によらない最終の返答の text を受け取り、provider の出力の形（Codex の JSONL など）から取り出すのは provider の実装が行う。verdict の検査と適用は provider によらず同じにする。状態を変えるコマンドを job が打てないことは、sandbox と [ADR-t728-1](2026-09-27-t728-1-trust-domains-actors-and-default-deny-capability-authorization.md) の CLI の policy の両方が守る。
3. **Codex に対応の無い Claude の設定は、読み取りだけの sandbox が代わりを果たす job では持たせない。** Claude の job に付ける設定（hook と拒むコマンドの一覧）は、書き込みと他の process への signal を防ぐためのもので、読み取りだけの sandbox がそれを拒む。Codex の job にはその代わりを作らず、持たせなかったことを design と記録に残す。sandbox が代わりを果たさない役割（queue に書く observer など）を Codex に乗せるときは、その task が扱いを決め直す。
4. **使えない provider からは、worker と同じ条件でもう一方に切り替えて起動し直す。** 「使えない」は ADR-t813-2 決定 2 と同じ（実行ファイルが無い・起動できない・認証が要る・利用上限や rate limit）で、一般的な失敗（非 0 の終了、時間の上限、permission の拒否、verdict の parse の失敗、model の誤りなど）では決定 3 のとおり切り替えない。job は 1 回きりの呼び出しなので、切り替え先では同じ prompt で新しく起動し直す。切り替え先はその役割を動かせる実装を持つ provider に限り、無ければ今の Claude の job と同じく控えで待つ。切り替えと理由は記録に残す。失敗の分類は provider の実装が共通の分類に訳す。
5. **控えは provider ごとのまま、job は使える provider があれば止めない（ADR-t813-2 決定 6 を amends）。** 決定 6 の 3 つめの項の「切り替え先の無い Claude だけの役割」を、「設定と実装の上で Claude でしか動けない役割」と読み替える。ある役割の job を止めるのは、その役割の provider と決定 4 の切り替え先の両方が控えられたとき（切り替え先が無ければ、その役割の provider が控えられたとき）だけにする。Claude の控えで今の `queue_hold` の ask を開く条件（Claude でしか動けない役割があること）は変えない。runtime の planner は対話で Codex に乗らないので、当面 Claude の控えは今までどおり ask を開き、変わるのは、Codex で動ける job がその間も止まらないことである。worker についての決定 6 の他の項は変えない。
6. **どの provider・model で動いたかを全ての job で記録する。** 全ての headless の job の起動の記録に、provider と分かる範囲の実際の model（読めなければ要求した model と、読めなかったこと）と、session（Codex なら thread）の id を残し、goal review にもそれらを残す。stats / kpi の job の集計（所要時間・失敗率・verdict の分布）を provider で分けて読め、役割ごとに設定された provider を status か doctor で見られるようにする。これは必須で、provider を足す task は記録の欠けたまま着地しない。
7. **広げる順は goal review → observer → plan review → review → 復旧にする。** goal review は着地の流れの外で、道具は読み取りだけ、verdict は runtime が再検査して適用し、失敗しても goal が開いたままなので最初にする。observer は finding と ask を queue に書くので、Codex の sandbox から queue へ書く経路（worker の `dagq ask` と同じ問題、task 890）が入った後にする。plan review・review・復旧は着地と復旧の流れに入るので、goal review の記録を Claude と比べてから順に広げる。スループットの見直しの job の順はそれを乗せる task が決める。goal 73 は goal review だけを乗せる。

欄名・設定の key・flag・event の kind・既定値と、Codex の起動の引数・出力の取り出し方・model の読み取り方は、後続の実装の task が [docs/design/](../design/) に書く。

## Alternatives

- **job のコードで provider を決める（goal review は Codex、など）**: 比べるために切り替えるたびに build が要り、人の「provider は透過」に合わない。
- **Codex の job にも道具名の一覧を訳して渡す**: Codex には道具を名前で絞る仕組みが無く、sandbox が意図をそのまま表せる（spike 1.・5.）。
- **Claude の hook と拒むコマンドの一覧を Codex の rules などで作り直す**: 読み取りだけの sandbox で防ぐものが無く、保つものが増えるだけ。
- **Codex が使えないときは切り替えず止める**: goal review は止まっても goal が開いたままで害は小さいが、worker の相互フォールバックと扱いが割れ、review などに広げたときに着地が止まる。
- **Claude の控えは今までどおり全ての job を止める**: Codex で動ける役割まで人を待ち、フォールバックの意味が無くなる。
- **model を記録しない、または要求した値だけを記録する**: 人の決定（観測可能性は必須）に合わない。Codex は既定の model に任せると要求した値が無い。

## Consequences

- Codex の job は Claude より遅い見込み（spike 5. で goal 47 の 1 件が Claude 37 秒、Codex 55〜73 秒）。比べるのは決定 6 の記録で行う。
- 起動した側の env は Codex の job の全てのコマンドに見える（spike 2.）ので、supervisor の env に secret を置かない前提は Claude の job と同じ。
- 時間の上限で Codex を止めたとき、止め方によっては走っていたコマンドが残る（spike 4.）。job の process と子孫を pid で止める今の方法を Codex でも使い、実装の task が確かめる。
- 未ログイン・利用上限・実行ファイルが無い場合の Codex の job の出力は未確認で、判定の文言は実物を見た時点で design と実装を直す（ADR-t813-2 の Consequences と同じ）。
- [queue-hold](../design/supervisor-lifecycle/queue-hold.md) の ask を開く条件は、役割に Codex を設定する実装の task が決定 5 のとおりに直す。
