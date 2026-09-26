---
id: adr-t615-1
type: adr
title: 着地先のbranchとpushのremoteとpushするかをrepositoryごとにdagq.tomlで決められるようにし、指定が無ければdefault branchを推定し、解決できなければupで止める（ADR-0008決定3・4・6・7・8、ADR-0047決定26、ADR-0054決定7をamends）
status: accepted
created: 2026-09-27
updated: 2026-09-27
accepted_on: 2026-09-27
amends:
  - adr-0008 decision 3
  - adr-0008 decision 4
  - adr-0008 decision 6
  - adr-0008 decision 7
  - adr-0008 decision 8
  - adr-0047 decision 26
  - adr-0054 decision 7
owners:
  - hisamekms
tags:
  - runtime
  - git
  - operations
related:
  - adr-0008
  - adr-0047
  - adr-0054
  - adr-t598-1
  - adr-t614-1
  - design-supervisor-lifecycle-landing-branch
  - design-supervisor-lifecycle-run-environment
  - design-supervisor-lifecycle-integrate
---

# ADR-t615-1: 着地先のbranchとpushのremoteとpushするかをrepositoryごとにdagq.tomlで決められるようにし、指定が無ければdefault branchを推定し、解決できなければupで止める（ADR-0008決定3・4・6・7・8、ADR-0047決定26、ADR-0054決定7をamends）

## Context

dagqはこのrepository自身の開発でしか使われてこなかったので、着地先のbranchと、着地後のpushの先が固定されている（goal 52の(1)）。

- [ADR-0008](0008-merge-queue-squash-landing.md)は`integrate`がrunをmainへ着地させると決め、mainの進め方を`refs/heads/main`の`update-ref`か、mainをcheckoutしているworktreeの`merge --ff-only`とした。ADR-0008の決定は番号の無い箇条書きなので、このADRでは箇条の順に1から数える（決定3は「手順」、決定4は「mainの進め方」、決定6は衝突と再検証の失敗、決定7は`failed` receipt、決定8は「`main`を進める前のエラー」）。
- [ADR-0054](0054-run-lease-ownership-parallel-supervisors-and-recover.md)決定7は、claimのたびに`refs/heads/main`を読み直してbase commitにすると決めた。
- [ADR-0047](0047-irregularities-in-three-layers-recovery-job-ask-reasons-and-goal-review.md)決定26は、着地後にmainを`origin`へpushし、`origin`が無ければ`push_skipped`にすると決めた。止めるのは`integrate`の`--no-push`だけで、supervisorの着地には止める設定が無い。

実装も`refs/heads/main`と`origin`を固定の文字列で持つので、default branchが`master`のrepositoryでは`up`・`supervise`・`integrate`・`plan`・`stats`・`review`・`rebind`（repositoryを調べる入口が`refs/heads/main`を先に読む）がrev-parseの失敗で止まり、pushしたくないrepository（共有のremoteへ直接pushしない運用、手元だけで試す場合）では着地のたびに意図しないpushが走る。

人は2026-09-27に、着地は今までどおりdefault branchへ直接行ってpushし、remoteとpushしないことを`dagq.toml`で指定できるようにすると決めた。PRで着地させる運用は範囲外とした。

## Decision

1. **着地先のbranch・pushのremote・pushするかを、repositoryの`dagq.toml`で指定できる。** 指定はrepositoryにcommitされ、main checkoutの作業ファイルから読む（`[run.env]`と同じ場所。人ごと・hostごとの設定にはしない）。着地のやり方（1 task = 1 squash commitのfast-forward、rebaseと再検証、衝突の`needs_session`）は変えず、変えるのはどのbranchを進め、どこへpushするかだけである。
2. **branchの指定が無ければ、default branchを推定する。** 順に、pushのremoteのHEAD（remoteのdefault branch）が指すbranch、`main`、`master`の、ローカルに在る最初のものを使う。どれも無ければ推定せず、解決できないとする。推定は使うたびにその時点のrepositoryで行い、queueに保存しない。
3. **pushは既定で行い、止められる。** remoteの指定が無ければ`origin`で、その`origin`が無ければ今までどおりpushせずに`push_skipped`を記録する（remoteの無いrepositoryで使えるようにするため）。remoteを明示して、そのremoteが無いのは設定の誤りとして扱い、黙って飛ばさない。pushしないと指定したrepositoryではremoteを見ずに`push_skipped`を記録する。pushの失敗の扱い（runは`integrated`のまま、inboxのattention）は変えない。
4. **解決できなければ`up`のpreflightで止め、`doctor`で見せる。** `up`はsupervisorを起動する前に、branch・remote・pushの解決の結果を確かめ、branchが解決できない、明示したbranchが無い、pushする設定で明示したremoteが無い、`dagq.toml`の書式が誤っているときは、supervisorを起動せず`dagq.toml`での指定を案内して止める。`up`と`doctor`は解決したbranch・remote・pushと、それぞれをどこから決めたか（指定か推定か）を出す。preflightを通らない状態は`supervise`・`integrate`・`plan`・`stats`でもerrorにし、既定のbranchを黙って仮定しない。
5. **既存のqueueは設定なしで今までどおり動く。** `dagq.toml`に指定の無いrepositoryで、remoteのHEADが`main`を指すか`main`が在れば着地先は`main`、pushは`origin`へ行う。dagq自身のrepositoryは何も足さずに今までと同じ振る舞いになる。
6. **amendsの中身。** ADR-0008決定3・4・6・7・8とADR-0054決定7の「main」「`refs/heads/main`」は、決定1〜2で解決した着地先のbranchに読み替える（進め方・読み直し・squashの手順はそのまま）。ADR-0047決定26の「mainを`origin`へpushする」「`origin`が無ければ`push_skipped`」は、決定3のとおり、解決した着地先のbranchを解決したremoteへpushし、pushしない指定か、remoteを指定しないで`origin`が無いときに`push_skipped`とする、に変える。`--no-push`は残す。

上の3つのADRのほかにも、accepted のADRの本文で着地の対象としての「main」を書いた箇所（ADR-0027決定4のmerge-tree、ADR-0049決定2の着地とpush、ADR-0029決定3〜5のscopeの基点、ADR-0047決定24のresumeの依頼文・決定37と38のreceiptの書き直しの条件・決定40のjobに許さない操作、ADR-0067決定3、ADR-0068決定1〜3・6のrecheck、ADR-0069決定3・10のmainの履歴、ADR-0071決定14、ADR-0073決定17のきっかけ）は、main・originを選ぶ決定ではなく、着地先を指す呼び名として使っている。これらは本文を変えず、「main」を着地先のbranchと読む。欄名・既定値・推定の順・eventのpayload・errorの文言は[Landing branch](../design/supervisor-lifecycle/landing-branch.md)が持つ。

## Alternatives

- **PRを作って着地させる運用**: 人が範囲外とした。reviewと着地の仕組み（review job、単一の着地slot、merge-treeの事前判定）がremoteのmergeを待つ形に変わる。
- **指定が無ければ常に`main`**: master のrepositoryで設定を書かないと動かず、goal 52の目標（設定の無いmasterのrepositoryでの完走）に届かない。
- **`git symbolic-ref HEAD`（main checkoutが今checkoutしているbranch）**: 人がmain checkoutで別のbranchに切り替えるだけで着地先が変わる。
- **設定を人ごと・hostごとのファイルに置く**: 着地先はrepositoryの運用で、同じrepositoryを使う人とhostで同じであるべき。
- **推定の結果をqueueに保存する**: remoteのdefault branchを変えたときに古い値が残り、保存の更新の規則が要る。

## Consequences

- default branchが`master`でremoteの無いrepositoryで、設定なしに着地できる（pushは`push_skipped`）。
- `dagq.toml`に新しい表が増えるので、それを知らない旧バイナリは未知の表として拒む。表を足すのは、それを知るバイナリに入れ替えた後にする。
- 実装は別のtaskで行う。実装が入るまでは`refs/heads/main`と`origin`の固定のまま動く。
