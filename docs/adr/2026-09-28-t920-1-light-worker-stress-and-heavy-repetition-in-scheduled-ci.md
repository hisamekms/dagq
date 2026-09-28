---
id: adr-t920-1
type: adr
title: workerの手元のstressをあからさまに不安定なtestを止める軽い見張りに縮め、重い繰り返しをGitHub Actionsの1日1回の定時実行に移す（task 767の決定を変える）
status: accepted
created: 2026-09-28
updated: 2026-09-28
accepted_on: 2026-09-28
owners:
  - hisamekms
tags:
  - testing
  - worker
  - ci
related:
  - adr-t768-1
  - adr-0076
  - adr-t598-1
---

# ADR-t920-1: workerのstressを軽い見張りに縮め、重い繰り返しをCIの定時実行に移す

## Context

task 767（2026-09-27に人がplannerと決めた。ADRではなくAGENTS.mdの「変更後に必ず通す」のstressの項が持つ）は、workerが足した・変えたtestを手元で20周（1周が長いtestを含むときは時間の上限つき）繰り返してから receipt を書くと決めた。理由は、不安定なtestが着地した後に別のtaskの`integrate`の検証で落ち、そのrunがresumeされて1件あたり7〜16分遅れていたことだった。

その後、次の2つが変わった。

- task 767の後、runtimeのtaskのworkerの手元のtestの時間（statsの`work_breakdown.secs.test`）の中央値が150秒から442秒に伸びた。stressはhostの負荷の下で流すものなので、それ自体がhostのloadを押し上げ、loadによるclaimの保留（2026-09-28午前は時間の47%）を増やした。着地の数は27日の昼の7.3件/時から28日午前の4.2件/時に落ちた（goal 62）。
- [ADR-t768-1](2026-09-27-t768-1-rerun-failed-tests-once-and-land-again-on-flaky-only.md)により、着地の検証で落ちたtestが全てFLAKYならworkerをresumeせずに着地を1回やり直す。不安定なtestが着地したときの代償（task 767の理由の1件7〜16分のresume）は、検証をもう1周する分に小さくなった。

stressの検出力の大半は、多くの周回と高い負荷で稀な失敗を引き出す部分にあり、そこはrunの着地を待たせる必要がない。

## Decision

1. **workerの手元のstressは、あからさまに不安定なtest（数周で落ちるもの）を着地前に止める軽い見張りに縮める。** 対象（runのdiffで足した・変えた`tests/it`・`src/`の`#[cfg(test)]`・`crates/`のtest。e2eとpluginは除く）と、落ちたら原因を直してから receipt を書くことは変えない。周回と時間の上限はtask 767より小さくし、具体的な数値はAGENTS.mdに書く。
2. **重い繰り返しはGitHub Actionsの定時実行（1日1回）に移す。** 対象はmainで直近の期間に足した・変えた`tests/it`と`src/`の`#[cfg(test)]`のtest（e2eとpluginは除く）。20周以上をnextestの高い並列度で流し、testどうしのCPUの取り合いで負荷を作る。落ちたらGitHubのissueで人に知らせ（同じtestは既存のissueに追記）、plannerがtaskにする。hostのloadもqueueのslotも使わない。
3. **効果は変更の前後で、workerのtestの時間（statsの`work_breakdown.secs.test`）と、`integrate`の検証のflaky・resumeの件数で見る。** flakyによる着地の失敗（ADR-t768-1のやり直しで救えずresumeになるもの）が増えたら、workerの周回を戻す。

## Alternatives

- **今のまま（20周・長いtestは5分）**: 検出力は高いが、workerの時間とhostのloadを押し上げ、全てのtaskの着地を遅らせる。ADR-t768-1の後は、防ぐ代償より払う代償の方が大きい。
- **workerのstressを全部やめる**: 最も速いが、数周で落ちるような明らかに不安定なtestまでmainに入り、他のtaskの着地を毎回やり直させる。数周の見張りは安い。
- **定時実行だけにする（workerの見張りも無くす）**: 上と同じく、明らかな不安定さの発見が着地の後の最大1日遅れになり、その間の他のtaskの着地を乱す。
- **supervisorのtimerのjob（observerのような）にする**: hostで流すのでloadの問題が戻り、queueのslotや`integrate`と取り合う。GitHub Actionsならhostの外で、失敗の記録（issue）も人に見える場所に残る。

## Consequences

- workerの手元のtestの時間が縮み、hostのloadとloadによるclaimの保留が下がる見込み。前後比較は決定3の指標で行う。
- 稀にしか落ちない不安定なtestは着地してから見つかる。着地の検証で落ちてもADR-t768-1のやり直しで多くは救われ、定時実行のissueからplannerが直すtaskにする。
- 定時実行のworkflowは後続のciのtaskが作る（goal 62）。それが着地するまでは、重い繰り返しはどこでも流れない期間がある。
- 周回・時間の上限の数値とコマンドはAGENTS.mdの「変更後に必ず通す」とworkerの節が持つ。
